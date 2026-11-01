use agent_bridge::{
    db::Db,
    protocol::{Agent, Provider},
    topics::{AgentRef, TopicLimits, TopicPublication, TopicRef},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::Connection;

fn local() -> String {
    libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string()
}
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn author() -> AgentRef {
    AgentRef {
        bridge_id: None,
        agent_id: "inbox-user".into(),
    }
}
fn request(topic: &TopicRef, body: &str) -> TopicPublication {
    TopicPublication {
        topic: topic.clone(),
        id: id(),
        author: author(),
        body: body.into(),
        parent: None,
        mentions: vec![],
    }
}
fn publish(
    db: &mut Db,
    local: &str,
    topic: &TopicRef,
    text: &str,
) -> agent_bridge::topics::PublishResult {
    db.topic_publish(local, &request(topic, text), &TopicLimits::default())
        .unwrap()
}
fn memory() -> Db {
    Db::open(std::path::Path::new(":memory:")).unwrap()
}

#[test]
fn catalog_and_topics_preserve_exact_legacy_registry_sessions() {
    let mut db = memory();
    let local = local();
    let limits = TopicLimits::default();
    let topic = db.topic_create(&local, "Compatibility", &limits).unwrap();
    let uppercase = "ABCDEFAB-CDEF-4ABC-8ABC-ABCDEFABCDEF";
    let manual = "legacy\nmanual";
    for (name, provider, session, enabled) in [
        ("native-alias", Provider::Codex, uppercase, true),
        ("manual-alias", Provider::Inbox, manual, false),
    ] {
        db.register(
            &Agent {
                name: name.into(),
                provider,
                session: session.into(),
                endpoint: String::new(),
                enabled,
            },
            false,
        )
        .unwrap();
    }
    let catalog = db.topic_agents(&local).unwrap();
    for agent_id in [format!("codex-{uppercase}"), format!("inbox-{manual}")] {
        assert!(
            catalog
                .iter()
                .any(|entry| entry.reference.agent_id == agent_id)
        );
    }
    let mut publication = request(&topic.reference, "Preserve exact provenance");
    publication.author.agent_id = format!("codex-{uppercase}");
    publication.mentions = vec![AgentRef {
        bridge_id: None,
        agent_id: format!("inbox-{manual}"),
    }];
    let post = db
        .topic_publish(&local, &publication, &limits)
        .unwrap()
        .post;
    assert_eq!(post.author.agent_id, format!("codex-{uppercase}"));
    assert_eq!(post.mentions[0].agent_id, format!("inbox-{manual}"));
    publication.id = id();
    publication.author.agent_id = format!("codex-{}", uppercase.to_lowercase());
    assert!(db.topic_publish(&local, &publication, &limits).is_err());
}

#[test]
fn lifecycle_retry_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let local = local();
    let limits = TopicLimits::default();
    let a = db.topic_create(&local, "Engineering", &limits).unwrap();
    let b = db.topic_create(&local, "Engineering", &limits).unwrap();
    assert_ne!(a.reference, b.reference);
    assert_eq!(a.reference.bridge_id.as_deref(), Some(local.as_str()));
    let mut req = request(&a.reference, "hello");
    let post = db.topic_publish(&local, &req, &limits).unwrap().post;
    assert_eq!(post.sequence, 1);
    db.topic_archive(&local, &a.reference, true).unwrap();
    assert_eq!(db.topic_list(&local, false).unwrap().len(), 1);
    assert!(db.topic_publish(&local, &req, &limits).unwrap().duplicate);
    req.topic = b.reference.clone();
    assert!(
        db.topic_publish(&local, &req, &limits)
            .unwrap_err()
            .to_string()
            .contains("topic_conflict")
    );
    req.topic = a.reference.clone();
    req.id = id();
    assert!(
        db.topic_publish(&local, &req, &limits)
            .unwrap_err()
            .to_string()
            .contains("topic_archived")
    );
    db.topic_archive(&local, &a.reference, false).unwrap();
    db.topic_rename(&local, &a.reference, "Delivery").unwrap();
    let first = db
        .topic_read(&local, &a.reference, Some(""), None, 1)
        .unwrap();
    drop(db);
    let mut db = Db::open(&path).unwrap();
    publish(&mut db, &local, &a.reference, "later");
    let next = db
        .topic_read(&local, &a.reference, Some(&first.next_cursor), None, 1)
        .unwrap();
    assert_eq!(next.posts[0].sequence, 2);
    assert_eq!(next.high_water, 2);
    assert_eq!(
        db.topic_inspect(&local, &a.reference).unwrap().name,
        "Delivery"
    );
}

#[test]
fn bounded_bidirectional_pages_and_empty_anchors() {
    let mut db = memory();
    let local = local();
    let a = db
        .topic_create(&local, "A", &TopicLimits::default())
        .unwrap();
    let empty = db.topic_read(&local, &a.reference, None, None, 2).unwrap();
    assert!(empty.posts.is_empty());
    assert!(!empty.has_older);
    for _ in 0..5 {
        publish(&mut db, &local, &a.reference, "x");
    }
    let latest = db.topic_read(&local, &a.reference, None, None, 2).unwrap();
    assert_eq!(
        latest.posts.iter().map(|p| p.sequence).collect::<Vec<_>>(),
        vec![4, 5]
    );
    assert!(!latest.has_more);
    assert!(latest.has_older);
    publish(&mut db, &local, &a.reference, "append");
    let older = db
        .topic_read(&local, &a.reference, None, Some(&latest.previous_cursor), 2)
        .unwrap();
    assert_eq!(
        older.posts.iter().map(|p| p.sequence).collect::<Vec<_>>(),
        vec![2, 3]
    );
    assert!(older.has_more);
    assert!(older.has_older);
    assert_eq!(older.high_water, 6);
    let oldest = db
        .topic_read(&local, &a.reference, None, Some(&older.previous_cursor), 2)
        .unwrap();
    assert_eq!(oldest.posts[0].sequence, 1);
    assert!(!oldest.has_older);
    let before_first = db
        .topic_read(&local, &a.reference, None, Some(&oldest.previous_cursor), 2)
        .unwrap();
    assert!(before_first.posts.is_empty());
    assert!(!before_first.has_older);
    assert!(before_first.has_more);
    let resume = db
        .topic_read(
            &local,
            &a.reference,
            Some(&before_first.next_cursor),
            None,
            2,
        )
        .unwrap();
    assert_eq!(resume.posts[0].sequence, 1);
    let latest = db.topic_read(&local, &a.reference, None, None, 2).unwrap();
    let poll = db
        .topic_read(&local, &a.reference, Some(&latest.next_cursor), None, 2)
        .unwrap();
    assert!(poll.posts.is_empty());
    assert_eq!(poll.next_cursor, latest.next_cursor);
    publish(&mut db, &local, &a.reference, "new");
    let resumed = db
        .topic_read(&local, &a.reference, Some(&poll.next_cursor), None, 2)
        .unwrap();
    assert_eq!(resumed.posts.len(), 1);
    assert_eq!(resumed.posts[0].sequence, 7);
}

#[test]
fn cursor_validation() {
    let mut db = memory();
    let local = local();
    let a = db
        .topic_create(&local, "A", &TopicLimits::default())
        .unwrap();
    let b = db
        .topic_create(&local, "B", &TopicLimits::default())
        .unwrap();
    publish(&mut db, &local, &a.reference, "x");
    let first = db
        .topic_read(&local, &a.reference, Some(""), None, 1)
        .unwrap();
    for (cursor, before) in [
        (Some(first.next_cursor.as_str()), None),
        (None, Some(first.previous_cursor.as_str())),
    ] {
        assert!(
            db.topic_read(&local, &b.reference, cursor, before, 2)
                .is_err()
        );
    }
    assert!(
        db.topic_read(&local, &a.reference, Some(""), Some(""), 2)
            .is_err()
    );
    for limit in [0, 101] {
        assert!(
            db.topic_read(&local, &a.reference, None, None, limit)
                .is_err()
        );
    }
    for (version, bridge, after) in [
        (2, local.clone(), 0),
        (1, local.clone(), 2),
        (1, super_local(), 0),
        (1, local.clone(), -1),
    ] {
        let c=URL_SAFE_NO_PAD.encode(serde_json::to_vec(&serde_json::json!({"version":version,"bridge_id":bridge,"topic_id":a.reference.topic_id,"after":after})).unwrap());
        assert!(
            db.topic_read(&local, &a.reference, Some(&c), None, 2)
                .is_err()
        );
        assert!(
            db.topic_read(&local, &a.reference, None, Some(&c), 2)
                .is_err()
        );
    }
    assert!(
        db.topic_read(&local, &a.reference, Some("!"), None, 2)
            .is_err()
    );
}
fn super_local() -> String {
    local()
}

#[test]
fn quota_and_callback_atomicity() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let local = local();
    let limits = TopicLimits {
        max_topics: 1,
        max_posts_per_topic: 1,
        max_total_posts: 1,
        max_total_body_bytes: 3,
    };
    let topic = db.topic_create(&local, "A", &limits).unwrap();
    assert!(db.topic_create(&local, "B", &limits).is_err());
    let req = request(&topic.reference, "abc");
    let failed = db.topic_publish_with(&local, &req, &limits, |tx, post| {
        tx.execute("INSERT INTO settings VALUES('topic_test', ?)", [&post.id])?;
        anyhow::bail!("rollback")
    });
    assert!(failed.is_err());
    assert_eq!(
        db.topic_inspect(&local, &topic.reference)
            .unwrap()
            .high_water,
        0
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row(
            "SELECT count(*) FROM settings WHERE key='topic_test'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    let post = db.topic_publish(&local, &req, &limits).unwrap().post;
    assert_eq!(post.sequence, 1);
    assert!(
        db.topic_publish_with(&local, &req, &limits, |_, _| panic!("retry callback"))
            .unwrap()
            .duplicate
    );
    assert!(
        db.topic_publish(&local, &request(&topic.reference, "x"), &limits)
            .unwrap_err()
            .to_string()
            .contains("topic_capacity")
    );
    for sql in [
        "UPDATE topic_posts SET body='changed'",
        "DELETE FROM topic_posts",
    ] {
        assert!(conn.execute(sql, []).is_err());
    }
}

#[test]
fn independent_quota_bounds_count_archived_bytes() {
    for limits in [
        TopicLimits {
            max_posts_per_topic: 1,
            ..TopicLimits::default()
        },
        TopicLimits {
            max_total_posts: 1,
            ..TopicLimits::default()
        },
        TopicLimits {
            max_total_body_bytes: 4,
            ..TopicLimits::default()
        },
    ] {
        let mut db = memory();
        let local = local();
        let a = db.topic_create(&local, "A", &limits).unwrap();
        let b = db.topic_create(&local, "B", &limits).unwrap();
        let req = request(&a.reference, "😀");
        db.topic_publish(&local, &req, &limits).unwrap();
        let target = if limits.max_posts_per_topic == 1 {
            a.reference.clone()
        } else {
            db.topic_archive(&local, &a.reference, true).unwrap();
            b.reference
        };
        assert!(
            db.topic_publish(&local, &request(&target, "x"), &limits)
                .unwrap_err()
                .to_string()
                .contains("topic_capacity")
        );
        assert!(db.topic_publish(&local, &req, &limits).unwrap().duplicate);
    }
}

#[test]
fn exact_identity_mentions_parent_and_catalog() {
    let mut db = memory();
    let local = local();
    let limits = TopicLimits::default();
    let session = id();
    let canonical = AgentRef {
        bridge_id: None,
        agent_id: format!("codex-{session}"),
    };
    for name in ["alias", "alias2"] {
        db.register(
            &Agent {
                name: name.into(),
                provider: Provider::Codex,
                session: session.clone(),
                endpoint: String::new(),
                enabled: true,
            },
            false,
        )
        .unwrap();
    }
    db.observe_session_name(&Provider::Codex, &session, Some("Current"))
        .unwrap();
    let topic = db.topic_create(&local, "A", &limits).unwrap();
    let other = db.topic_create(&local, "B", &limits).unwrap();
    let mut req = request(&topic.reference, "hi");
    req.author = canonical.clone();
    req.mentions = vec![author(), author()];
    let p = db.topic_publish(&local, &req, &limits).unwrap().post;
    assert_eq!(p.author_display_name, "Current");
    assert_eq!(p.mentions.len(), 1);
    req.author.bridge_id = Some(local.clone());
    req.mentions = vec![author()];
    assert!(db.topic_publish(&local, &req, &limits).unwrap().duplicate);
    let mut reply = request(&other.reference, "reply");
    reply.parent = Some(p.id.clone());
    assert!(db.topic_publish(&local, &reply, &limits).is_err());
    reply.topic = topic.reference.clone();
    assert_eq!(
        db.topic_publish(&local, &reply, &limits)
            .unwrap()
            .post
            .parent,
        Some(p.id.clone())
    );
    for agent in [
        AgentRef {
            bridge_id: None,
            agent_id: "alias".into(),
        },
        AgentRef {
            bridge_id: Some(super_local()),
            agent_id: "inbox-user".into(),
        },
    ] {
        let mut bad = request(&topic.reference, "hi");
        bad.author = agent;
        assert!(db.topic_publish(&local, &bad, &limits).is_err());
    }
    db.unregister("alias", &session).unwrap();
    db.unregister("alias2", &session).unwrap();
    assert!(db.topic_publish(&local, &req, &limits).unwrap().duplicate);
    let mut new = req.clone();
    new.id = id();
    assert!(db.topic_publish(&local, &new, &limits).is_err());
    db.observe_session_name(&Provider::Codex, &session, Some("Renamed"))
        .unwrap();
    assert_eq!(
        db.topic_read(&local, &topic.reference, Some(""), None, 100)
            .unwrap()
            .posts[0]
            .author_display_name,
        "Current"
    );
    let catalog = db.topic_agents(&local).unwrap();
    assert_eq!(catalog.len(), 2);
    assert_eq!(
        catalog
            .iter()
            .find(|a| a.reference.agent_id == canonical.agent_id)
            .unwrap()
            .display_name,
        "Renamed"
    );
    db.register(
        &Agent {
            name: "alias".into(),
            provider: Provider::Inbox,
            session: "different".into(),
            endpoint: String::new(),
            enabled: true,
        },
        true,
    )
    .unwrap();
    let catalog = db.topic_agents(&local).unwrap();
    assert!(
        catalog
            .iter()
            .any(|a| a.reference.agent_id == "inbox-different")
    );
    assert!(
        catalog
            .iter()
            .any(|a| a.reference.agent_id == canonical.agent_id)
    );
}

#[test]
fn validation_and_concurrent_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let local = local();
    let limits = TopicLimits::default();
    for name in ["", " space", "space ", "line\n"] {
        assert!(db.topic_create(&local, name, &limits).is_err());
    }
    assert!(db.topic_create(&local, &"😀".repeat(100), &limits).is_ok());
    assert!(db.topic_create(&local, &"a".repeat(101), &limits).is_err());
    let topic = db.topic_create(&local, "Concurrent", &limits).unwrap();
    for body in ["".to_string(), " \n".to_string(), "x".repeat(65537)] {
        assert!(
            db.topic_publish(&local, &request(&topic.reference, &body), &limits)
                .is_err()
        );
    }
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let local = local.clone();
            let topic = topic.reference.clone();
            std::thread::spawn(move || {
                let mut db = Db::open(&path).unwrap();
                for _ in 0..5 {
                    publish(&mut db, &local, &topic, "concurrent");
                }
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    let page = db
        .topic_read(&local, &topic.reference, Some(""), None, 100)
        .unwrap();
    assert_eq!(
        page.posts.iter().map(|p| p.sequence).collect::<Vec<_>>(),
        (1..=20).collect::<Vec<_>>()
    );
}

#[test]
fn strict_refs_uuid_and_normalized_conflicts() {
    let mut db = memory();
    let local = local();
    let limits = TopicLimits::default();
    let topic = db.topic_create(&local, "A", &limits).unwrap();
    let remote = TopicRef {
        bridge_id: Some(super_local()),
        topic_id: topic.reference.topic_id.clone(),
    };
    assert!(
        db.topic_inspect(&local, &remote)
            .unwrap_err()
            .to_string()
            .contains("topic_not_local")
    );
    assert!(db.topic_rename(&local, &remote, "B").is_err());
    assert!(db.topic_archive(&local, &remote, true).is_err());
    assert!(db.topic_read(&local, &remote, None, None, 1).is_err());
    assert!(
        db.topic_inspect(
            &local,
            &TopicRef {
                bridge_id: None,
                topic_id: id()
            }
        )
        .unwrap_err()
        .to_string()
        .contains("topic_not_found")
    );
    let mut req = request(&topic.reference, "body");
    req.mentions = vec![author()];
    let original = db.topic_publish(&local, &req, &limits).unwrap();
    let mut equivalent = req.clone();
    equivalent.topic.bridge_id = Some(local.clone());
    equivalent.author.bridge_id = Some(local.clone());
    equivalent.mentions.push(AgentRef {
        bridge_id: Some(local.clone()),
        agent_id: "inbox-user".into(),
    });
    assert_eq!(
        db.topic_publish(&local, &equivalent, &limits).unwrap().post,
        original.post
    );
    for mutation in 0..4 {
        let mut bad = req.clone();
        match mutation {
            0 => bad.body.push('!'),
            1 => bad.mentions.clear(),
            2 => bad.parent = Some(id()),
            _ => bad.author.agent_id = "inbox-other".into(),
        }
        assert!(
            db.topic_publish(&local, &bad, &limits)
                .unwrap_err()
                .to_string()
                .contains("topic_conflict")
        );
    }
    req.id = req.id.to_uppercase();
    assert!(db.topic_publish(&local, &req, &limits).is_err());
    let mut req = request(&topic.reference, "body");
    req.mentions.push(AgentRef {
        bridge_id: None,
        agent_id: "inbox-unknown".into(),
    });
    assert!(
        db.topic_publish(&local, &req, &limits)
            .unwrap_err()
            .to_string()
            .contains("topic_agent_unknown")
    );
    req.mentions.clear();
    req.parent = Some(id());
    assert!(db.topic_publish(&local, &req, &limits).is_err());
}

#[test]
fn mention_bounds_immutable_history_and_callback_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let local = local();
    let limits = TopicLimits::default();
    let topic = db.topic_create(&local, "A", &limits).unwrap();
    let mut req = request(&topic.reference, &"x".repeat(65536));
    for n in 0..101 {
        let session = format!("manual{n:03}");
        db.register(
            &Agent {
                name: session.clone(),
                provider: Provider::Inbox,
                session: session.clone(),
                endpoint: String::new(),
                enabled: false,
            },
            false,
        )
        .unwrap();
        req.mentions.push(AgentRef {
            bridge_id: None,
            agent_id: format!("inbox-{session}"),
        });
    }
    assert!(db.topic_publish(&local, &req, &limits).is_err());
    req.mentions.pop();
    req.mentions.reverse();
    assert!(
        db.topic_publish_with(&local, &req, &limits, |_, _| anyhow::bail!("reject"))
            .is_err()
    );
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row("SELECT count(*) FROM topic_mentions", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    let post = db.topic_publish(&local, &req, &limits).unwrap().post;
    assert_eq!(post.sequence, 1);
    assert_eq!(post.mentions.len(), 100);
    assert!(post.mentions.windows(2).all(|pair| pair[0] < pair[1]));
    for sql in [
        "UPDATE topic_mentions SET agent_id='inbox-user'",
        "DELETE FROM topic_mentions",
    ] {
        assert!(conn.execute(sql, []).is_err());
    }
    req.mentions.sort();
    assert!(db.topic_publish(&local, &req, &limits).unwrap().duplicate);
}

#[test]
fn catalog_bound_and_current_metadata_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let local = local();
    let limits = TopicLimits::default();
    let session = id();
    db.register(
        &Agent {
            name: "alias".into(),
            provider: Provider::Claude,
            session: session.clone(),
            endpoint: "offline".into(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    db.observe_session_name(&Provider::Claude, &session, Some("Snapshot"))
        .unwrap();
    let topic = db.topic_create(&local, "A", &limits).unwrap();
    let mut req = request(&topic.reference, "body");
    req.author.agent_id = format!("claude-{session}");
    db.topic_publish(&local, &req, &limits).unwrap();
    db.observe_session_name(&Provider::Claude, &session, None)
        .unwrap();
    assert_eq!(
        db.topic_agents(&local)
            .unwrap()
            .iter()
            .find(|a| a.reference.agent_id == req.author.agent_id)
            .unwrap()
            .display_name,
        "Snapshot"
    );
    let conn = Connection::open(&path).unwrap();
    conn.execute("DELETE FROM agents WHERE name='alias'", [])
        .unwrap();
    assert_eq!(
        db.topic_read(&local, &topic.reference, None, None, 10)
            .unwrap()
            .posts[0]
            .author_display_name,
        "Snapshot"
    );
    for n in 0..1000 {
        conn.execute(
            "INSERT INTO agents VALUES(?, 'inbox', ?, '', 0)",
            rusqlite::params![format!("a{n}"), format!("s{n}")],
        )
        .unwrap();
    }
    assert!(
        db.topic_agents(&local)
            .unwrap_err()
            .to_string()
            .contains("topic_capacity")
    );
}

#[test]
fn concurrent_identical_uuid_only_one_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let local = local();
    let topic = db
        .topic_create(&local, "A", &TopicLimits::default())
        .unwrap();
    let req = request(&topic.reference, "retry");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
    let threads: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            let local = local.clone();
            let req = req.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let mut db = Db::open(&path).unwrap();
                barrier.wait();
                db.topic_publish(&local, &req, &TopicLimits::default())
                    .unwrap()
                    .duplicate
            })
        })
        .collect();
    let duplicates: usize = threads
        .into_iter()
        .map(|thread| usize::from(thread.join().unwrap()))
        .sum();
    assert_eq!(duplicates, 3);
    assert_eq!(
        db.topic_inspect(&local, &topic.reference)
            .unwrap()
            .high_water,
        1
    );
}
