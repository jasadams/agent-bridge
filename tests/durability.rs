use agent_bridge::{
    db::Db,
    protocol::{Agent, Provider},
};

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn agent(name: &str) -> Agent {
    Agent {
        name: name.into(),
        provider: Provider::Inbox,
        session: id(),
        endpoint: String::new(),
        enabled: true,
    }
}

#[test]
fn duplicate_send_is_idempotent_but_conflicting_reuse_is_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let dbpath = temp.path().join("db");
    let mut db = Db::open(&dbpath).unwrap();
    db.register(&agent("worker"), false).unwrap();
    let mid = id();
    db.send(&mid, "user", "worker", "first", &None).unwrap();
    assert_eq!(
        db.send(&mid, "user", "worker", "first", &None).unwrap().id,
        mid
    );
    assert!(db.send(&mid, "user", "worker", "different", &None).is_err());
    assert_eq!(db.history(&None, 50).unwrap().len(), 1);
}

#[test]
fn crash_preserves_queued_and_quarantines_in_flight() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("db");
    let mut db = Db::open(&path).unwrap();
    db.register(&agent("worker"), false).unwrap();
    let first = id();
    let second = id();
    db.send(&first, "user", "worker", "first", &None).unwrap();
    db.send(&second, "user", "worker", "second", &None).unwrap();
    assert_eq!(db.claim().unwrap().unwrap().0.id, first);
    drop(db);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.get(&first).unwrap().state, "uncertain");
    assert_eq!(db.get(&second).unwrap().state, "queued");
    assert!(db.retry(&first, false).is_err());
    db.retry(&first, true).unwrap();
}

#[test]
fn recipient_replacement_never_redirects_pending_work() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Db::open(&temp.path().join("db")).unwrap();
    let old = agent("worker");
    db.register(&old, false).unwrap();
    let mid = id();
    db.send(&mid, "user", "worker", "for old session", &None)
        .unwrap();
    let new = agent("worker");
    assert!(db.register(&new, false).is_err());
    db.register(&new, true).unwrap();
    assert_eq!(db.get(&mid).unwrap().state, "failed");
    assert!(db.retry(&mid, false).is_err());
    assert!(db.claim().unwrap().is_none());
    // A delayed SessionEnd from the previous session cannot unregister its replacement.
    db.unregister("worker", &old.session).unwrap();
    assert!(db.agent("worker").unwrap().enabled);
}

#[test]
fn one_in_flight_per_recipient_preserves_fifo_without_blocking_other_agents() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Db::open(&temp.path().join("db")).unwrap();
    db.register(&agent("a"), false).unwrap();
    db.register(&agent("b"), false).unwrap();
    let a1 = id();
    let a2 = id();
    let b1 = id();
    db.send(&a1, "user", "a", "a1", &None).unwrap();
    db.send(&a2, "user", "a", "a2", &None).unwrap();
    db.send(&b1, "user", "b", "b1", &None).unwrap();
    assert_eq!(db.claim().unwrap().unwrap().0.id, a1);
    assert_eq!(db.claim().unwrap().unwrap().0.id, b1);
    assert!(db.claim().unwrap().is_none());
    db.finish(&a1, "submitted", "sent").unwrap();
    assert_eq!(db.claim().unwrap().unwrap().0.id, a2);
}

#[test]
fn native_completion_prevents_normal_retries_but_allows_explicit_duplicate_override() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Db::open(&temp.path().join("db")).unwrap();
    db.register(&agent("worker"), false).unwrap();
    let mid = id();
    db.send(&mid, "user", "worker", "message", &None).unwrap();
    db.claim().unwrap();
    db.finish(&mid, "submitted", "transport finished later")
        .unwrap();
    assert_eq!(db.get(&mid).unwrap().state, "submitted");
    assert!(db.claim().unwrap().is_none());
    assert!(db.retry(&mid, false).is_err());
    db.retry(&mid, true).unwrap();
    assert_eq!(db.claim().unwrap().unwrap().0.id, mid);
}

#[test]
fn pause_and_offline_registration_hold_work_until_resume() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Db::open(&temp.path().join("db")).unwrap();
    let worker = agent("worker");
    db.register(&worker, false).unwrap();
    db.send(&id(), "user", "worker", "wait", &None).unwrap();
    db.pause(true).unwrap();
    assert!(db.claim().unwrap().is_none());
    db.pause(false).unwrap();
    db.unregister("worker", &worker.session).unwrap();
    assert!(db.claim().unwrap().is_none());
    db.register(&worker, false).unwrap();
    assert!(db.claim().unwrap().is_some());
}

#[test]
fn replies_are_correlated_and_bounded() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Db::open(&temp.path().join("db")).unwrap();
    db.register(&agent("a"), false).unwrap();
    db.register(&agent("b"), false).unwrap();
    let mut parent = id();
    db.send(&parent, "a", "b", "start", &None).unwrap();
    assert!(
        db.send(&id(), "a", "b", "forged parent", &Some(parent.clone()))
            .is_err()
    );
    let (mut sender, mut recipient) = ("b", "a");
    for hop in 1..=100 {
        let next = id();
        assert_eq!(
            db.send(&next, sender, recipient, "reply", &Some(parent))
                .unwrap()
                .hops,
            hop
        );
        parent = next;
        std::mem::swap(&mut sender, &mut recipient);
    }
    let error = db
        .send(&id(), sender, recipient, "too far", &Some(parent))
        .unwrap_err();
    assert!(error.to_string().contains("100-hop limit"));
}

#[test]
fn submitted_requires_explicit_duplicate_override_and_queued_can_cancel() {
    let temp = tempfile::tempdir().unwrap();
    let mut db = Db::open(&temp.path().join("db")).unwrap();
    db.register(&agent("worker"), false).unwrap();
    let mid = id();
    db.send(&mid, "user", "worker", "message", &None).unwrap();
    db.claim().unwrap();
    db.finish(&mid, "submitted", "sent").unwrap();
    assert!(db.retry(&mid, false).is_err());
    assert!(db.cancel(&mid).is_err());
    db.retry(&mid, true).unwrap();
    db.cancel(&mid).unwrap();
    assert!(db.claim().unwrap().is_none());
}
