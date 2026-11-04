use agent_bridge::{
    db::Db,
    network::trust::Permission,
    protocol::{Agent, Provider},
};
use rusqlite::Connection;

fn identity() -> String {
    libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string()
}
fn register(db: &mut Db, name: &str, session: &str, replace: bool) {
    db.register(
        &Agent {
            name: name.into(),
            provider: Provider::Inbox,
            session: session.into(),
            endpoint: String::new(),
        },
        replace,
    )
    .unwrap();
}

#[test]
fn bundle_is_explicit_idempotent_and_does_not_change_other_session_grants() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let peer = identity();
    db.pair_peer(&peer, &peer, Some("machine-b")).unwrap();
    register(&mut db, "selected", "original", false);
    register(&mut db, "private", "other", false);
    db.grant_peer("machine-b", "private", Permission::Send, true)
        .unwrap();
    assert!(
        !db.peer_allowed(&peer, &Provider::Inbox, "original", Permission::Send)
            .unwrap()
    );
    let first = db
        .share_peer_session("machine-b", "selected", true)
        .unwrap();
    assert_eq!(first.grants.len(), 4);
    assert_eq!(
        db.share_peer_session("machine-b", "selected", true)
            .unwrap()
            .grants
            .len(),
        4
    );
    let rows = db.shared_sessions().unwrap();
    let selected = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session"] == "original")
        .unwrap();
    assert_eq!(selected["shared"], true);
    assert_eq!(
        selected["permissions"],
        serde_json::json!(["discover", "receive", "send"])
    );
    let private = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session"] == "other")
        .unwrap();
    assert_eq!(private["shared"], false);
    drop(db);
    let mut db = Db::open(&path).unwrap();
    let removed = db
        .share_peer_session("machine-b", "selected", false)
        .unwrap();
    assert_eq!(removed.grants.len(), 1);
    assert!(
        db.peer_allowed(&peer, &Provider::Inbox, "other", Permission::Send)
            .unwrap()
    );
    assert!(
        !db.peer_allowed(&peer, &Provider::Inbox, "original", Permission::Receive)
            .unwrap()
    );
}

#[test]
fn bundle_rolls_back_every_permission_when_an_insert_fails() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let peer = identity();
    db.pair_peer(&peer, &peer, None).unwrap();
    Connection::open(&path).unwrap().execute_batch(
        "CREATE TRIGGER reject_receive BEFORE INSERT ON peer_grants WHEN NEW.permission='receive' BEGIN SELECT RAISE(ABORT,'test rejection'); END;",
    ).unwrap();
    assert!(db.share_peer_session(&peer, "user", true).is_err());
    assert!(db.peer(&peer).unwrap().grants.is_empty());
}

#[test]
fn session_replacement_cannot_inherit_sharing_and_stale_grants_remain_visible() {
    let root = tempfile::tempdir().unwrap();
    let mut db = Db::open(&root.path().join("db")).unwrap();
    let peer = identity();
    db.pair_peer(&peer, &peer, Some("machine-b")).unwrap();
    register(&mut db, "worker", "old", false);
    db.share_peer_session("machine-b", "worker", true).unwrap();
    register(&mut db, "worker", "new", true);
    assert!(
        !db.peer_allowed(&peer, &Provider::Inbox, "new", Permission::Receive)
            .unwrap()
    );
    let rows = db.shared_sessions().unwrap();
    assert_eq!(rows[0]["session"], "old");
    assert_eq!(rows[0]["stale"], true);
    assert_eq!(rows[0]["enabled"], false);
    assert_eq!(rows[0]["local_names"], serde_json::json!([]));
    // Unsharing the replacement cannot accidentally remove an old session grant.
    db.share_peer_session("machine-b", "worker", false).unwrap();
    assert_eq!(db.peer(&peer).unwrap().grants.len(), 3);
    register(&mut db, "old-session", "old", false);
    db.unregister("old-session", "old").unwrap();
    db.share_peer_session("machine-b", "old-session", false)
        .unwrap();
    assert!(db.shared_sessions().unwrap().as_array().unwrap().is_empty());
}

#[test]
fn disabled_session_can_be_unshared_but_not_shared_and_revocation_clears_everything() {
    let root = tempfile::tempdir().unwrap();
    let mut db = Db::open(&root.path().join("db")).unwrap();
    let peer = identity();
    db.pair_peer(&peer, &peer, None).unwrap();
    register(&mut db, "worker", "original", false);
    register(&mut db, "alias", "original", false);
    db.share_peer_session(&peer, "worker", true).unwrap();
    assert_eq!(
        db.shared_sessions().unwrap()[0]["local_names"],
        serde_json::json!(["alias", "worker"])
    );
    db.unregister("worker", "original").unwrap();
    assert!(db.share_peer_session(&peer, "worker", true).is_err());
    db.share_peer_session(&peer, "worker", false).unwrap();
    assert!(db.peer(&peer).unwrap().grants.is_empty());
    db.share_peer_session(&peer, "alias", true).unwrap();
    db.revoke_peer(&peer).unwrap();
    assert!(db.shared_sessions().unwrap().as_array().unwrap().is_empty());
    assert!(db.share_peer_session(&peer, "alias", true).is_err());
    assert!(db.share_peer_session(&identity(), "alias", true).is_err());
}

#[test]
fn stale_session_identity_removal_does_not_touch_replacement_grants() {
    let root = tempfile::tempdir().unwrap();
    let mut db = Db::open(&root.path().join("db")).unwrap();
    let peer = identity();
    db.pair_peer(&peer, &peer, Some("machine-b")).unwrap();
    register(&mut db, "worker", "old", false);
    db.share_peer_session("machine-b", "worker", true).unwrap();
    register(&mut db, "worker", "replacement", true);
    db.share_peer_session("machine-b", "worker", true).unwrap();
    assert_eq!(db.peer("machine-b").unwrap().grants.len(), 6);
    db.unshare_peer_session("machine-b", &Provider::Inbox, "old")
        .unwrap();
    let rows = db.shared_sessions().unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 1);
    assert_eq!(rows[0]["session"], "replacement");
    assert!(
        db.peer_allowed(&peer, &Provider::Inbox, "replacement", Permission::Receive)
            .unwrap()
    );
    assert!(
        db.unshare_peer_session("machine-b", &Provider::Inbox, "")
            .is_err()
    );
    assert!(
        db.unshare_peer_session("machine-b", &Provider::Inbox, &"x".repeat(201))
            .is_err()
    );
    db.revoke_peer("machine-b").unwrap();
    assert!(
        db.unshare_peer_session("machine-b", &Provider::Inbox, "replacement")
            .unwrap()
            .grants
            .is_empty()
    );
}
