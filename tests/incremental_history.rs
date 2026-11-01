use agent_bridge::{
    broker::Broker,
    db::Db,
    protocol::{Agent, Provider, Request},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::json;

fn agent(name: &str) -> Agent {
    Agent {
        name: name.into(),
        provider: Provider::Inbox,
        session: uuid::Uuid::new_v4().to_string(),
        endpoint: String::new(),
        enabled: true,
    }
}

fn send(db: &mut Db, recipient: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    db.send(&id, "user", recipient, "body", &None).unwrap();
    id
}

#[test]
fn pages_preserve_insertion_order_across_ties_restart_and_vacuum() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("messages.db");
    let mut db = Db::open(&path).unwrap();
    db.register(&agent("worker"), false).unwrap();
    let first = send(&mut db, "worker");
    let second = send(&mut db, "worker");
    let third = send(&mut db, "worker");
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE messages SET created=1, updated=1", [])
        .unwrap();
    let first_page = db.history_since(&None, Some(""), 2).unwrap();
    assert_eq!(first_page.messages[0].id, first);
    assert_eq!(first_page.messages[1].id, second);
    assert!(first_page.has_more);
    drop(db);
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute_batch("VACUUM")
        .unwrap();
    let mut db = Db::open(&path).unwrap();
    let fourth = send(&mut db, "worker");
    let second_page = db
        .history_since(&None, Some(&first_page.cursor), 2)
        .unwrap();
    assert_eq!(second_page.messages[0].id, third);
    assert_eq!(second_page.messages[1].id, fourth);
    assert!(!second_page.has_more);
    let empty = db
        .history_since(&None, Some(&second_page.cursor), 2)
        .unwrap();
    assert!(empty.messages.is_empty());
    assert_eq!(empty.cursor, second_page.cursor);
    assert!(!empty.has_more);
    // Retrying the same cursor produces the same page, not a consumed stream.
    let retry = db
        .history_since(&None, Some(&first_page.cursor), 2)
        .unwrap();
    assert_eq!(retry.cursor, second_page.cursor);
    assert_eq!(retry.messages[0].id, third);
}

#[test]
fn empty_cursor_sees_future_inserts_but_does_not_replay_state_updates() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Db::open(&directory.path().join("messages.db")).unwrap();
    db.register(&agent("worker"), false).unwrap();
    let initial = db.history_since(&None, None, 1).unwrap();
    assert!(initial.messages.is_empty());
    let id = send(&mut db, "worker");
    let page = db.history_since(&None, Some(&initial.cursor), 1).unwrap();
    assert_eq!(page.messages[0].id, id);
    assert!(!page.has_more);
    db.claim().unwrap().unwrap();
    db.finish(&id, "submitted", "native accepted").unwrap();
    let next = db.history_since(&None, Some(&page.cursor), 1).unwrap();
    assert!(next.messages.is_empty());
    assert_eq!(db.get(&id).unwrap().state, "submitted");
}

#[test]
fn scope_filter_paginates_without_skips_and_rejects_other_scopes() {
    let directory = tempfile::tempdir().unwrap();
    let mut db = Db::open(&directory.path().join("messages.db")).unwrap();
    db.register(&agent("worker"), false).unwrap();
    db.register(&agent("other"), false).unwrap();
    send(&mut db, "other");
    let first = send(&mut db, "worker");
    send(&mut db, "other");
    let second = send(&mut db, "worker");
    let scope = Some("worker".into());
    let page = db.history_since(&scope, None, 1).unwrap();
    assert_eq!(page.messages[0].id, first);
    assert!(page.has_more);
    let next = db.history_since(&scope, Some(&page.cursor), 1).unwrap();
    assert_eq!(next.messages[0].id, second);
    assert!(!next.has_more);
    assert!(db.history_since(&None, Some(&page.cursor), 1).is_err());
    assert!(
        db.history_since(&Some("other".into()), Some(&page.cursor), 1)
            .is_err()
    );
}

#[test]
fn malformed_unsupported_missing_and_oversized_cursors_are_errors() {
    let directory = tempfile::tempdir().unwrap();
    let db = Db::open(&directory.path().join("messages.db")).unwrap();
    let encode = |value: serde_json::Value| URL_SAFE_NO_PAD.encode(value.to_string());
    let cursors = [
        "not-a-cursor".into(),
        "x".repeat(1025),
        encode(json!({"version":2,"agent":null,"after":null})),
        encode(json!({"version":1,"agent":null,"after":"missing"})),
    ];
    for cursor in cursors {
        assert!(db.history_since(&None, Some(&cursor), 1).is_err());
    }
    assert!(db.history_since(&None, None, 0).is_err());
    assert!(db.history_since(&None, None, 101).is_err());
}

#[tokio::test]
async fn broker_adds_metadata_and_keeps_legacy_history_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let server = Broker::bind(directory.path().into(), "/missing/codex".into()).unwrap();
    let broker = &server.broker;
    broker
        .request(Request::Register {
            agent: agent("worker"),
            replace: false,
        })
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    broker
        .request(Request::Send {
            id: id.clone(),
            sender: "user".into(),
            recipient: "worker".into(),
            body: "body".into(),
            parent: None,
        })
        .unwrap();
    let page = broker
        .request(Request::HistorySince {
            agent: Some("worker".into()),
            cursor: Some(String::new()),
            limit: 1,
        })
        .unwrap();
    assert_eq!(page["messages"][0]["id"], id);
    assert!(page["messages"][0].get("source_machine").is_some());
    assert_eq!(page["has_more"], false);
    let cursor = page["cursor"].as_str().unwrap().to_owned();
    let user_scope = broker
        .request(Request::HistorySince {
            agent: Some("user".into()),
            cursor: None,
            limit: 1,
        })
        .unwrap();
    assert_eq!(user_scope["messages"][0]["id"], id);
    assert!(
        broker
            .request(Request::HistorySince {
                agent: Some("user".into()),
                cursor: Some(cursor),
                limit: 1,
            })
            .is_err()
    );
    let legacy = broker
        .request(Request::History {
            agent: Some("worker".into()),
            limit: 1,
        })
        .unwrap();
    assert_eq!(legacy.as_array().unwrap().len(), 1);
    assert_eq!(legacy[0]["id"], id);

    let session = uuid::Uuid::new_v4().to_string();
    broker
        .observe_codex(&json!({"id":session,"name":"Friendly worker","status":{"type":"idle"}}))
        .unwrap();
    let native_id = uuid::Uuid::new_v4().to_string();
    broker
        .request(Request::Send {
            id: native_id.clone(),
            sender: "user".into(),
            recipient: "Friendly worker".into(),
            body: "native body".into(),
            parent: None,
        })
        .unwrap();
    let alias_page = broker
        .request(Request::HistorySince {
            agent: Some("codex:Friendly worker".into()),
            cursor: None,
            limit: 1,
        })
        .unwrap();
    assert_eq!(alias_page["messages"][0]["id"], native_id);
    let canonical_page = broker
        .request(Request::HistorySince {
            agent: Some(format!("codex-{session}")),
            cursor: Some(alias_page["cursor"].as_str().unwrap().into()),
            limit: 1,
        })
        .unwrap();
    assert!(canonical_page["messages"].as_array().unwrap().is_empty());
}
