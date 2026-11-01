use agent_bridge::{
    db::Db,
    protocol::{Agent, Message, Provider},
};

#[test]
fn low_level_state_mapping_does_not_rewrite_persisted_message_serialization() {
    for (stored, public) in [
        ("queued", "pending"),
        ("sending", "pending"),
        ("blocked", "pending"),
        ("submitted", "delivered"),
        ("acknowledged", "delivered"),
        ("uncertain", "unknown"),
        ("cancelled", "failed"),
        ("failed", "failed"),
    ] {
        let message = Message {
            id: "message-id".into(),
            sender: "user".into(),
            recipient: "worker".into(),
            recipient_session: "worker-session".into(),
            body: "original body".into(),
            parent: None,
            hops: 0,
            state: stored.into(),
            detail: Some("original detail".into()),
            created: 1,
            updated: 2,
        };
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(
            value["state"], stored,
            "Raw serialization cannot infer native proof"
        );
        assert_eq!(agent_bridge::protocol::delivery_state(stored), public);
        assert_eq!(value["body"], "original body");
        assert_eq!(value["detail"], "original detail");
        assert_eq!(
            message.state, stored,
            "serialization must not mutate storage"
        );
    }
}

#[test]
fn historical_acknowledged_messages_remain_terminal_without_an_ack_api() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.sqlite3");
    let mut db = Db::open(&path).unwrap();
    db.register(
        &Agent {
            name: "worker".into(),
            provider: Provider::Inbox,
            session: "worker-session".into(),
            endpoint: String::new(),
            enabled: true,
        },
        false,
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    db.send(&id, "user", "worker", "historical receipt", &None)
        .unwrap();
    // Only historical compatibility fixtures may create the retired disk state.
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE messages SET state='acknowledged' WHERE id=?", [&id])
        .unwrap();
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.get(&id).unwrap().state, "acknowledged");
    assert!(db.claim().unwrap().is_none());
    assert!(db.retry(&id, true).is_err());
    let history = db.history(&None, 10).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(db.counts().unwrap()["delivered"], 1);
}
