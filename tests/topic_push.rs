//! Durable topic push contract against isolated state, not the owner's broker.
use agent_bridge::{
    db::Db,
    protocol::{Agent, Provider},
    topics::{AgentRef, TopicLimits, TopicPublication},
};

fn bridge() -> String {
    libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string()
}

#[test]
fn duplicate_publish_and_restart_do_not_duplicate_opted_in_work() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("messages.sqlite3");
    let local = bridge();
    let mut db = Db::open(&path).unwrap();
    let recipient_session = uuid::Uuid::new_v4().to_string();
    db.register(
        &Agent {
            name: "recipient".into(),
            provider: Provider::Claude,
            session: recipient_session.clone(),
            endpoint: "/tmp/test.sock".into(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    let topic = db
        .topic_create(&local, "Updates", &TopicLimits::default())
        .unwrap();
    let agent = AgentRef {
        bridge_id: None,
        agent_id: format!("claude-{recipient_session}"),
    };
    db.topic_push_set(&local, &topic.reference, &agent, "all")
        .unwrap();
    let publication = TopicPublication {
        topic: topic.reference.clone(),
        id: uuid::Uuid::new_v4().to_string(),
        author: AgentRef {
            bridge_id: None,
            agent_id: "inbox-user".into(),
        },
        body: "A post".into(),
        parent: None,
        mentions: vec![agent.clone()],
    };
    let original = db
        .topic_publish(&local, &publication, &TopicLimits::default())
        .unwrap();
    assert!(!original.duplicate);
    assert!(
        db.topic_publish(&local, &publication, &TopicLimits::default())
            .unwrap()
            .duplicate
    );
    let before = db
        .topic_push_diagnostics(&local, &topic.reference, &agent)
        .unwrap();
    assert_eq!(before.pending, 1);
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert!(
        db.topic_publish(&local, &publication, &TopicLimits::default())
            .unwrap()
            .duplicate
    );
    assert_eq!(
        db.topic_push_diagnostics(&local, &topic.reference, &agent)
            .unwrap()
            .pending,
        1
    );
}

#[test]
fn manual_default_and_mode_switch_do_not_schedule_historical_posts() {
    let local = bridge();
    let mut db = Db::open(std::path::Path::new(":memory:")).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    db.register(
        &Agent {
            name: "recipient".into(),
            provider: Provider::Claude,
            session: session.clone(),
            endpoint: "/tmp/test.sock".into(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    let topic = db
        .topic_create(&local, "Updates", &TopicLimits::default())
        .unwrap();
    let recipient = AgentRef {
        bridge_id: None,
        agent_id: format!("claude-{session}"),
    };
    let mut post = TopicPublication {
        topic: topic.reference.clone(),
        id: uuid::Uuid::new_v4().to_string(),
        author: AgentRef {
            bridge_id: None,
            agent_id: "inbox-user".into(),
        },
        body: "Before subscription".into(),
        parent: None,
        mentions: vec![],
    };
    db.topic_publish(&local, &post, &TopicLimits::default())
        .unwrap();
    assert_eq!(
        db.topic_push_get(&local, &topic.reference, &recipient)
            .unwrap()
            .mode,
        "manual"
    );
    db.topic_push_set(&local, &topic.reference, &recipient, "all")
        .unwrap();
    assert_eq!(
        db.topic_push_diagnostics(&local, &topic.reference, &recipient)
            .unwrap()
            .pending,
        0
    );
    post.id = uuid::Uuid::new_v4().to_string();
    post.body = "After subscription".into();
    db.topic_publish(&local, &post, &TopicLimits::default())
        .unwrap();
    assert_eq!(
        db.topic_push_diagnostics(&local, &topic.reference, &recipient)
            .unwrap()
            .pending,
        1
    );
    db.topic_push_set(&local, &topic.reference, &recipient, "manual")
        .unwrap();
    assert_eq!(
        db.topic_push_diagnostics(&local, &topic.reference, &recipient)
            .unwrap()
            .pending,
        0
    );
}

#[test]
fn restart_after_claim_preserves_uncertainty_without_automatic_replay() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("messages.sqlite3");
    let local = bridge();
    let mut db = Db::open(&path).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    db.register(
        &Agent {
            name: "recipient".into(),
            provider: Provider::Claude,
            session: session.clone(),
            endpoint: "/tmp/test.sock".into(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    let topic = db
        .topic_create(&local, "Recovery", &TopicLimits::default())
        .unwrap();
    let recipient = AgentRef {
        bridge_id: None,
        agent_id: format!("claude-{session}"),
    };
    db.topic_push_set(&local, &topic.reference, &recipient, "all")
        .unwrap();
    let post = TopicPublication {
        topic: topic.reference.clone(),
        id: uuid::Uuid::new_v4().to_string(),
        author: AgentRef {
            bridge_id: None,
            agent_id: "inbox-user".into(),
        },
        body: "Uncertain push".into(),
        parent: None,
        mentions: vec![],
    };
    db.topic_publish(&local, &post, &TopicLimits::default())
        .unwrap();
    let (claimed, _) = db.claim().unwrap().unwrap();
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.get(&claimed.id).unwrap().state, "uncertain");
    assert!(db.claim().unwrap().is_none());
    assert_eq!(
        db.topic_push_diagnostics(&local, &topic.reference, &recipient)
            .unwrap()
            .unknown,
        1
    );
    assert!(
        db.topic_publish(&local, &post, &TopicLimits::default())
            .unwrap()
            .duplicate
    );
    assert!(db.claim().unwrap().is_none());
}
