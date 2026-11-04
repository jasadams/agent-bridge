//! Isolated real broker + private socket exercises the production Claude adapter.
//! This is a fixture socket, not qualification of an installed native client.
use agent_bridge::{
    broker::{self, Broker},
    protocol::{Provider, Request},
    topics::{AgentRef, TopicRef},
};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::{io::AsyncReadExt, net::UnixListener, time::timeout};

async fn native_input(listener: &UnixListener, duration: Duration) -> Option<String> {
    timeout(duration, async {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).await.unwrap();
            if !bytes.is_empty() {
                return String::from_utf8(bytes).unwrap();
            }
        }
    })
    .await
    .ok()
}

#[tokio::test]
async fn opt_in_topic_push_uses_guarded_native_adapter_and_untrusted_context() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.path().join("native.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone();
    broker
        .request(Request::Register {
            name: "topic".into(),
            provider: Provider::Inbox,
            session: "unrelated".into(),

            replace: false,
        })
        .unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    broker
        .request(Request::Register {
            name: "receiver".into(),
            provider: Provider::Claude,
            session: session.clone(),
            replace: false,
        })
        .unwrap();
    broker
        .request(Request::ClaudeBind {
            session: session.clone(),
            endpoint: socket.to_string_lossy().into_owned(),
        })
        .unwrap();
    let server = tokio::spawn(broker::serve(bound));
    let created = broker
        .request(Request::TopicCreate {
            name: "Native context".into(),
        })
        .unwrap();
    let topic: TopicRef = serde_json::from_value(created["reference"].clone()).unwrap();
    let agent = AgentRef {
        bridge_id: None,
        agent_id: format!("claude-{session}"),
    };
    let publish = |id: String| Request::TopicPublish {
        topic: topic.clone(),
        id,
        author: AgentRef {
            bridge_id: None,
            agent_id: "inbox-user".into(),
        },
        body: "hello from topic".into(),
        parent: None,
        mentions: vec![agent.clone()],
    };
    broker
        .request(publish(uuid::Uuid::new_v4().to_string()))
        .unwrap();
    assert!(
        native_input(&listener, Duration::from_millis(200))
            .await
            .is_none()
    );
    broker
        .request(Request::TopicPushSet {
            topic: topic.clone(),
            agent: agent.clone(),
            mode: "mentions".into(),
        })
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    broker.request(publish(id.clone())).unwrap();
    let text = native_input(&listener, Duration::from_secs(5))
        .await
        .expect("expected native input");
    let native_frame: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(native_frame["from"], "@topic");
    let conn = rusqlite::Connection::open(root.path().join("state/messages.sqlite3")).unwrap();
    let source:String=conn.query_row("SELECT c.source FROM topic_push_work w JOIN message_context c ON c.id=w.message_id WHERE w.post_id=?",[&id],|row|row.get(0)).unwrap();
    let source: serde_json::Value = serde_json::from_str(&source).unwrap();
    assert_eq!(source["kind"], "topic_push");
    assert!(
        source["agent"].is_null() && source["provider"].is_null() && source["session"].is_null()
    );
    assert!(text.contains("untrusted data"), "{text}");
    assert!(text.contains(&id), "{text}");
    assert!(text.contains("hello from topic"), "{text}");
    let diagnostics = timeout(Duration::from_secs(5), async {
        loop {
            let value = broker
                .request(Request::TopicPushGet {
                    topic: topic.clone(),
                    agent: agent.clone(),
                })
                .unwrap();
            if value["delivery"]["unknown"] == 1 {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(diagnostics["delivery"]["items"][0]["state"], "unknown");
    broker.request(publish(id)).unwrap();
    assert!(
        native_input(&listener, Duration::from_millis(300))
            .await
            .is_none()
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn stale_enabled_socket_failure_records_one_durable_gap() {
    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.path().join("offline.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone();
    let session = uuid::Uuid::new_v4().to_string();
    broker
        .request(Request::Register {
            name: "stale".into(),
            provider: Provider::Claude,
            session: session.clone(),
            replace: false,
        })
        .unwrap();
    broker
        .request(Request::ClaudeBind {
            session: session.clone(),
            endpoint: socket.to_string_lossy().into_owned(),
        })
        .unwrap();
    drop(listener); // Registration stays enabled; the socket now refuses connections.
    let server = tokio::spawn(broker::serve(bound));
    let created = broker
        .request(Request::TopicCreate {
            name: "Offline".into(),
        })
        .unwrap();
    let topic: TopicRef = serde_json::from_value(created["reference"].clone()).unwrap();
    let agent = AgentRef {
        bridge_id: None,
        agent_id: format!("claude-{session}"),
    };
    broker
        .request(Request::TopicPushSet {
            topic: topic.clone(),
            agent: agent.clone(),
            mode: "all".into(),
        })
        .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let request = || Request::TopicPublish {
        topic: topic.clone(),
        id: id.clone(),
        author: AgentRef {
            bridge_id: None,
            agent_id: "inbox-user".into(),
        },
        body: "Read after reconnect".into(),
        parent: None,
        mentions: vec![],
    };
    assert!(
        !broker.request(request()).unwrap()["duplicate"]
            .as_bool()
            .unwrap()
    );
    let observed = timeout(Duration::from_secs(5), async {
        loop {
            let value = broker
                .request(Request::TopicPushGet {
                    topic: topic.clone(),
                    agent: agent.clone(),
                })
                .unwrap();
            if value["delivery"]["failed"] == 1 {
                break value;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        observed["gap_count"], 1,
        "offline delivery must record a catch-up gap"
    );
    assert_eq!(observed["last_gap_sequence"], 1);
    assert!(
        broker.request(request()).unwrap()["duplicate"]
            .as_bool()
            .unwrap()
    );
    assert_eq!(
        broker
            .request(Request::TopicPushGet {
                topic: topic.clone(),
                agent: agent.clone()
            })
            .unwrap()["gap_count"],
        1
    );
    server.abort();
    let _ = server.await;
}
