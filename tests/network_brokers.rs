mod support;

use agent_bridge::{
    broker::{self, Broker},
    network::trust::Permission,
    protocol::{Agent, Provider, Request},
};
use serde_json::Value;
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixListener,
    task::JoinHandle,
    time::timeout,
};

struct Running {
    broker: Broker,
    state: PathBuf,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    fn start(state: &Path) -> Self {
        let bound = Broker::bind(state.into(), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        Self {
            broker,
            state: state.into(),
            task: tokio::spawn(broker::serve(bound)),
        }
    }

    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }

    fn request(&self, request: Request) -> Value {
        self.broker.request(request).unwrap()
    }

    fn identity(&self) -> String {
        self.request(Request::Identity)["peer_id"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn history(&self) -> Vec<Value> {
        self.request(Request::History {
            agent: None,
            limit: 100,
        })
        .as_array()
        .unwrap()
        .clone()
    }

    async fn listening(&self) -> String {
        wait(|| {
            let status = self.request(Request::Status);
            status["network"]["listening"]
                .as_array()
                .and_then(|addresses| addresses.first())
                .cloned()
        })
        .await
        .as_str()
        .unwrap()
        .to_owned()
    }
}

async fn wait(mut inspect: impl FnMut() -> Option<Value>) -> Value {
    timeout(Duration::from_secs(40), async {
        loop {
            if let Some(value) = inspect() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("broker network condition timed out")
}

fn grant(bridge: &Running, peer: &str, agent: &str) {
    for permission in [Permission::Discover, Permission::Receive, Permission::Send] {
        bridge.request(Request::PeerGrant {
            peer: peer.into(),
            agent: agent.into(),
            permission,
            allow: true,
        });
    }
}

fn pair(alice: &Running, bob: &Running) -> (String, String) {
    let alice_id = alice.identity();
    let bob_id = bob.identity();
    alice.request(Request::PeerPair {
        peer_id: bob_id.clone(),
        fingerprint: bob_id.clone(),
        alias: Some("bob".into()),
    });
    bob.request(Request::PeerPair {
        peer_id: alice_id.clone(),
        fingerprint: alice_id.clone(),
        alias: Some("alice".into()),
    });
    (alice_id, bob_id)
}

async fn connect(alice: &Running, bob: &Running) {
    for bridge in [alice, bob] {
        bridge.request(Request::NetworkConfigure {
            enabled: true,
            listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
        });
    }
    let alice_address = alice.listening().await;
    let bob_address = bob.listening().await;
    alice.request(Request::PeerAddress {
        peer: "bob".into(),
        address: bob_address,
    });
    bob.request(Request::PeerAddress {
        peer: "alice".into(),
        address: alice_address,
    });
}

async fn directory(bridge: &Running, name: &str) {
    wait(|| {
        bridge
            .request(Request::RemoteList)
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["name"] == name)
            .cloned()
    })
    .await;
}

fn inbox(bridge: &Running, name: &str) {
    bridge.request(Request::Register {
        agent: Agent {
            name: name.into(),
            provider: Provider::Inbox,
            session: format!("{name}-session"),
            endpoint: String::new(),
            enabled: true,
        },
        replace: false,
    });
}

async fn message_state(bridge: &Running, id: &str, state: &str) -> Value {
    wait(|| {
        let message = bridge.request(Request::Get { id: id.into() });
        (message["state"] == state).then_some(message)
    })
    .await
}

fn temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ab-net-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[tokio::test]
async fn two_brokers_deliver_once_to_native_socket_and_sync_delivery_and_reply() {
    let root = temp();
    let alice = Running::start(&root.path().join("a"));
    let bob = Running::start(&root.path().join("b"));
    let (alice_id, _) = pair(&alice, &bob);
    let native_dir = root.path().join("native");
    std::fs::create_dir(&native_dir).unwrap();
    std::fs::set_permissions(&native_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let native_path = native_dir.join("inbox.sock");
    let native = UnixListener::bind(&native_path).unwrap();
    std::fs::set_permissions(&native_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    bob.request(Request::Register {
        agent: Agent {
            name: "reviewer".into(),
            provider: Provider::Claude,
            session: uuid::Uuid::new_v4().to_string(),
            endpoint: native_path.to_string_lossy().into_owned(),
            enabled: true,
        },
        replace: false,
    });
    grant(&alice, "bob", "user");
    grant(&bob, "alice", "reviewer");
    connect(&alice, &bob).await;
    directory(&alice, "reviewer").await;
    let id = uuid::Uuid::new_v4().to_string();
    let body = "Remote quotes ' and literal $(nothing)\nUnicode 🦀";
    for _ in 0..2 {
        broker::rpc(
            &alice.state,
            &Request::Send {
                id: id.clone(),
                sender: "user".into(),
                recipient: "bob/reviewer".into(),
                body: body.into(),
                parent: None,
            },
        )
        .await
        .unwrap();
    }
    let (stream, _) = timeout(Duration::from_secs(20), native.accept())
        .await
        .unwrap()
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    let frame: Value = serde_json::from_str(&line).unwrap();
    let local_id = frame["msg_id"].as_str().unwrap().to_owned();
    assert_ne!(local_id, id);
    let content = frame["message"]["content"].as_str().unwrap();
    assert!(content.starts_with("<cross-session-message from-name=\""));
    assert!(content.contains(body));
    assert_eq!(frame["from"], format!("{alice_id}/user"));
    assert!(content.contains(&format!("Reply to: {alice_id}/user")));
    assert!(!content.contains("Agent Bridge"));
    let original = message_state(&alice, &id, "unknown").await;
    assert_eq!(original["transfer"]["state"], "accepted");
    let reply_id = uuid::Uuid::new_v4().to_string();
    broker::rpc(
        &bob.state,
        &Request::Send {
            id: reply_id.clone(),
            sender: "reviewer".into(),
            recipient: format!("{alice_id}/user"),
            body: "Reviewed".into(),
            parent: Some(local_id),
        },
    )
    .await
    .unwrap();
    let reply = wait(|| {
        alice
            .history()
            .into_iter()
            .find(|message| message["body"] == "Reviewed")
    })
    .await;
    assert_eq!(reply["parent"], id);
    assert_eq!(reply["hops"], 1);
    message_state(&alice, reply["id"].as_str().unwrap(), "delivered").await;
    assert!(
        timeout(Duration::from_millis(200), native.accept())
            .await
            .is_err(),
        "duplicate native delivery"
    );
    assert_eq!(
        bob.history()
            .iter()
            .filter(|message| message["body"] == body)
            .count(),
        1
    );
    alice.stop().await;
    bob.stop().await;
}

#[tokio::test]
async fn queued_transfer_survives_origin_restart_and_destination_offline_period() {
    let root = temp();
    let alice_path = root.path().join("a");
    let bob_path = root.path().join("b");
    let alice = Running::start(&alice_path);
    let bob = Running::start(&bob_path);
    pair(&alice, &bob);
    inbox(&bob, "reviewer");
    grant(&alice, "bob", "user");
    grant(&bob, "alice", "reviewer");
    connect(&alice, &bob).await;
    directory(&alice, "reviewer").await;
    let bob_address = bob.listening().await;
    // Retain the selected port across restart, and disable source transport before
    // enqueueing so this checks durable offline work rather than an in-flight race.
    bob.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some(bob_address.clone()),
    });
    alice.request(Request::NetworkConfigure {
        enabled: false,
        listen: None,
    });
    wait(|| (alice.request(Request::Status)["network"]["enabled"] == false).then_some(Value::Null))
        .await;
    bob.stop().await;
    let id = uuid::Uuid::new_v4().to_string();
    alice.request(Request::Send {
        id: id.clone(),
        sender: "user".into(),
        recipient: "bob/reviewer".into(),
        body: "Persist while offline".into(),
        parent: None,
    });
    assert_eq!(
        alice.request(Request::Get { id: id.clone() })["transfer"]["state"],
        "pending"
    );
    alice.stop().await;
    support::wait_for_shutdown(&alice_path).await;
    let alice = Running::start(&alice_path);
    assert_eq!(
        alice.request(Request::Get { id: id.clone() })["transfer"]["state"],
        "pending"
    );
    alice.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
    });
    support::wait_for_shutdown(&bob_path).await;
    let bob = Running::start(&bob_path);
    assert_eq!(bob.listening().await, bob_address);
    let alice_address = alice.listening().await;
    bob.request(Request::PeerAddress {
        peer: "alice".into(),
        address: alice_address,
    });
    let received = wait(|| {
        bob.history()
            .into_iter()
            .find(|message| message["body"] == "Persist while offline")
    })
    .await;
    message_state(&bob, received["id"].as_str().unwrap(), "unknown").await;
    message_state(&alice, &id, "unknown").await;
    assert_eq!(
        bob.history()
            .iter()
            .filter(|message| message["body"] == "Persist while offline")
            .count(),
        1
    );
    assert_eq!(
        alice.request(Request::Get { id })["transfer"]["state"],
        "accepted"
    );
    alice.stop().await;
    bob.stop().await;
}

#[tokio::test]
async fn revocation_blocks_a_durably_received_message_before_native_submission() {
    let root = temp();
    let alice = Running::start(&root.path().join("a"));
    let bob = Running::start(&root.path().join("b"));
    pair(&alice, &bob);
    inbox(&bob, "reviewer");
    grant(&alice, "bob", "user");
    grant(&bob, "alice", "reviewer");
    bob.request(Request::Pause { paused: true });
    connect(&alice, &bob).await;
    directory(&alice, "reviewer").await;
    let id = uuid::Uuid::new_v4().to_string();
    alice.request(Request::Send {
        id: id.clone(),
        sender: "user".into(),
        recipient: "bob/reviewer".into(),
        body: "Block before native submission".into(),
        parent: None,
    });
    let received = wait(|| {
        bob.history()
            .into_iter()
            .find(|message| message["body"] == "Block before native submission")
    })
    .await;
    assert_eq!(received["state"], "pending");
    bob.request(Request::PeerRevoke {
        peer: "alice".into(),
    });
    bob.request(Request::Pause { paused: false });
    let stored = rusqlite::Connection::open_with_flags(
        root.path().join("b/messages.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    wait(|| {
        let blocked: bool = stored
            .query_row(
                "SELECT state='blocked' FROM messages WHERE id=?",
                [received["id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        blocked.then_some(Value::Null)
    })
    .await;
    message_state(&bob, received["id"].as_str().unwrap(), "pending").await;
    assert!(
        bob.request(Request::RemoteList)
            .as_array()
            .unwrap()
            .is_empty()
    );
    alice.stop().await;
    bob.stop().await;
}
