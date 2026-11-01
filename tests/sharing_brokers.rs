//! Session sharing changes policy, not identity or durable delivery semantics.
mod support;

use agent_bridge::{
    broker::{self, Broker},
    protocol::{Agent, Provider, Request},
};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::{task::JoinHandle, time::timeout};

struct Running {
    broker: Broker,
    task: JoinHandle<anyhow::Result<()>>,
}
impl Running {
    fn start(state: &Path) -> Self {
        let bound = Broker::bind(state.into(), "/missing/codex".into()).unwrap();
        Self {
            broker: bound.broker.clone(),
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
    fn share(&self, peer: &str, agent: &str, shared: bool) -> Value {
        self.request(Request::PeerShare {
            peer: peer.into(),
            agent: agent.into(),
            shared,
        })
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
    fn connection(&self, peer: &str) -> Value {
        self.request(Request::PeerConnections)
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["peer_id"] == peer)
            .unwrap()
            .clone()
    }
    fn send(&self, recipient: &str, body: &str, parent: Option<String>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.request(Request::Send {
            id: id.clone(),
            sender: "user".into(),
            recipient: recipient.into(),
            body: body.into(),
            parent,
        });
        id
    }
    async fn listening(&self) -> String {
        wait("QUIC listener", || {
            self.request(Request::Status)["network"]["listening"]
                .as_array()?
                .first()
                .cloned()
        })
        .await
        .as_str()
        .unwrap()
        .into()
    }
}
async fn wait(label: &str, mut inspect: impl FnMut() -> Option<Value>) -> Value {
    timeout(Duration::from_secs(30), async {
        loop {
            if let Some(value) = inspect() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
}
fn pair(a: &Running, b: &Running) -> (String, String) {
    let a_id = a.request(Request::Identity)["peer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b_id = b.request(Request::Identity)["peer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    a.request(Request::PeerPair {
        peer_id: b_id.clone(),
        fingerprint: b_id.clone(),
        alias: Some("bob".into()),
    });
    b.request(Request::PeerPair {
        peer_id: a_id.clone(),
        fingerprint: a_id.clone(),
        alias: Some("alice".into()),
    });
    (a_id, b_id)
}
async fn connect(a: &Running, b: &Running) {
    for bridge in [a, b] {
        bridge.request(Request::NetworkConfigure {
            enabled: true,
            listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
        });
    }
    a.request(Request::PeerAddress {
        peer: "bob".into(),
        address: b.listening().await,
    });
    b.request(Request::PeerAddress {
        peer: "alice".into(),
        address: a.listening().await,
    });
}
async fn directory(a: &Running, name: &str) {
    wait("permitted remote directory", || {
        a.request(Request::RemoteList)
            .as_array()?
            .iter()
            .find(|agent| agent["name"] == name)
            .cloned()
    })
    .await;
}
async fn received(bridge: &Running, body: &str) -> Value {
    wait("received message", || {
        bridge.history().into_iter().find(|row| row["body"] == body)
    })
    .await
}
async fn state(bridge: &Running, id: &str, wanted: &str) -> Value {
    wait("delivery status", || {
        let message = bridge.request(Request::Get { id: id.into() });
        (message["state"] == wanted).then_some(message)
    })
    .await
}
fn inbox(bridge: &Running, name: &str, session: &str, replace: bool) {
    bridge.request(Request::Register {
        agent: Agent {
            name: name.into(),
            provider: Provider::Inbox,
            session: session.into(),
            endpoint: String::new(),
            enabled: true,
        },
        replace,
    });
}
fn temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ab-share-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[tokio::test]
async fn shared_sessions_exchange_delivered_replies_and_enforce_unshare_across_restart() {
    let root = temp();
    let a = Running::start(&root.path().join("a"));
    let b_path = root.path().join("b");
    let b = Running::start(&b_path);
    let (a_id, b_id) = pair(&a, &b);
    assert_eq!(a.connection(&b_id)["state"], "network_disabled");
    assert!(
        a.request(Request::SharedSessions)
            .as_array()
            .unwrap()
            .is_empty()
    );
    inbox(&b, "private", "private-session", false);
    assert_eq!(
        a.share("bob", "user", true)["grants"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        b.share("alice", "user", true)["grants"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    connect(&a, &b).await;
    directory(&a, "user").await;
    directory(&b, "user").await;
    wait("connected state", || {
        (a.connection(&b_id)["state"] == "connected").then_some(Value::Null)
    })
    .await;
    let status = a.connection(&b_id);
    assert_eq!(status["trusted"], true);
    assert_eq!(status["path"], "direct");
    assert!(status["transport"]["direct_connections"].as_u64().unwrap() > 0);

    assert_eq!(status["shared_sessions"][0]["shared"], true);
    assert!(status["pairing"].as_array().unwrap().is_empty());
    assert!(!status.to_string().contains("verification_code"));
    assert_eq!(a.request(Request::RemoteList).as_array().unwrap().len(), 1);
    assert!(
        a.broker
            .request(Request::Send {
                id: uuid::Uuid::new_v4().to_string(),
                sender: "user".into(),
                recipient: "bob/private".into(),
                body: "must remain private".into(),
                parent: None
            })
            .is_err()
    );

    let original = a.send("bob/user", "Shared session hello", None);
    let incoming = received(&b, "Shared session hello").await;
    let local_id = incoming["id"].as_str().unwrap().to_owned();
    state(&b, &local_id, "delivered").await;
    state(&a, &original, "delivered").await;
    let reply_id = b.send("alice/user", "Shared session reply", Some(local_id));
    let reply = received(&a, "Shared session reply").await;
    assert_eq!(reply["parent"], original);
    assert_eq!(reply["hops"], 1);
    state(&b, &reply_id, "delivered").await;

    b.request(Request::Pause { paused: true });
    let queued_id = a.send("bob/user", "Queued before stop sharing", None);
    let queued = received(&b, "Queued before stop sharing").await;
    assert_eq!(queued["state"], "pending");
    // No await between revocation of the bundle and enqueue: A's directory is
    // deliberately stale, but the destination must recheck current policy.
    b.share("alice", "user", false);
    let denied_id = a.send("bob/user", "Denied after unshare", None);
    b.request(Request::Pause { paused: false });
    let stored = rusqlite::Connection::open_with_flags(
        b_path.join("messages.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    wait("queued message blocked after unshare", || {
        let blocked: bool = stored
            .query_row(
                "SELECT state='blocked' FROM messages WHERE id=?",
                [queued["id"].as_str().unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        blocked.then_some(Value::Null)
    })
    .await;
    state(&b, queued["id"].as_str().unwrap(), "pending").await;
    state(&a, &queued_id, "pending").await;
    let rejected = state(&a, &denied_id, "failed").await;
    assert_eq!(rejected["transfer"]["state"], "rejected");
    assert!(
        b.history()
            .iter()
            .all(|row| row["body"] != "Denied after unshare")
    );
    a.share("bob", "user", false);
    assert!(
        a.broker
            .request(Request::Send {
                id: uuid::Uuid::new_v4().to_string(),
                sender: "user".into(),
                recipient: "bob/user".into(),
                body: "No source grant".into(),
                parent: None
            })
            .is_err()
    );
    a.share("bob", "user", true);
    b.share("alice", "user", true);
    assert_eq!(b.request(Request::SharedSessions)[0]["shared"], true);
    b.request(Request::NetworkConfigure {
        enabled: false,
        listen: None,
    });
    assert_eq!(b.connection(&a_id)["state"], "network_disabled");
    b.stop().await;
    support::wait_for_shutdown(&b_path).await;
    let b = Running::start(&b_path);
    assert_eq!(b.request(Request::SharedSessions)[0]["shared"], true);
    connect(&a, &b).await;
    directory(&a, "user").await;
    let after_restart = a.send("bob/user", "Shared grants survived restart", None);
    let incoming = received(&b, "Shared grants survived restart").await;
    state(&b, incoming["id"].as_str().unwrap(), "delivered").await;
    state(&a, &after_restart, "delivered").await;
    b.request(Request::PeerRevoke {
        peer: "alice".into(),
    });
    assert_eq!(b.connection(&a_id)["state"], "revoked");
    assert!(b.connection(&a_id)["path"].is_null());
    assert!(b.connection(&a_id)["transport"].is_null());

    assert_eq!(b.connection(&a_id)["trusted"], false);
    assert!(
        b.request(Request::SharedSessions)
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        b.broker
            .request(Request::PeerShare {
                peer: "alice".into(),
                agent: "user".into(),
                shared: true
            })
            .is_err()
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn replacing_shared_registration_does_not_authorize_its_new_session() {
    let root = temp();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    pair(&a, &b);
    inbox(&b, "worker", "old-session", false);
    a.share("bob", "user", true);
    b.share("alice", "worker", true);
    connect(&a, &b).await;
    directory(&a, "worker").await;
    inbox(&b, "worker", "replacement-session", true);
    let rows = b.request(Request::SharedSessions);
    assert_eq!(rows[0]["session"], "old-session");
    assert_eq!(rows[0]["stale"], true);
    assert_eq!(rows[0]["enabled"], false);
    let denied_id = a.send("bob/worker", "No permission inheritance", None);
    state(&a, &denied_id, "failed").await;
    assert!(
        b.history()
            .iter()
            .all(|row| row["body"] != "No permission inheritance")
    );
    // Removing stale grants by immutable identity leaves the new registration private.
    b.request(Request::PeerUnshareSession {
        peer: "alice".into(),
        provider: Provider::Inbox,
        session: "old-session".into(),
    });
    assert_eq!(b.request(Request::SharedSessions), json!([]));
    b.share("alice", "worker", true);
    directory(&a, "worker").await;
    // Wait for the directory to identify the replacement, rather than the stale route.
    wait("replacement directory", || {
        a.request(Request::RemoteList)
            .as_array()?
            .iter()
            .find(|agent| agent["session"] == "replacement-session")
            .cloned()
    })
    .await;
    let id = a.send("bob/worker", "Explicitly shared replacement", None);
    let incoming = received(&b, "Explicitly shared replacement").await;
    state(&b, incoming["id"].as_str().unwrap(), "delivered").await;
    state(&a, &id, "delivered").await;
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn connection_view_distinguishes_pending_pairing_without_disclosing_comparison_codes() {
    let root = temp();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let a_id = a.request(Request::Identity)["peer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let b_id = b.request(Request::Identity)["peer_id"]
        .as_str()
        .unwrap()
        .to_owned();
    for bridge in [&a, &b] {
        bridge.request(Request::NetworkConfigure {
            enabled: true,
            listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
        });
    }
    a.listening().await;
    let address = b.listening().await;
    b.request(Request::LanPairingOpen { seconds: 120 });
    let started = a.request(Request::LanPairingConnect {
        peer: b_id.clone(),
        address: Some(address),
        seconds: 120,
    });
    let id = started["id"].as_str().unwrap().to_owned();
    wait("pending request", || {
        b.request(Request::LanPairingList)["requests"]
            .as_array()?
            .iter()
            .find(|row| row["id"] == id && row["state"] == "awaiting_accept")
            .cloned()
    })
    .await;
    assert_eq!(a.connection(&b_id)["state"], "pairing_pending");
    assert_eq!(b.connection(&a_id)["state"], "pairing_pending");
    assert_eq!(b.connection(&a_id)["connected"], false);
    assert_eq!(b.connection(&a_id)["trusted"], false);
    b.request(Request::LanPairingAccept { id: id.clone() });
    let compared = wait("comparison code", || {
        b.request(Request::LanPairingList)["requests"]
            .as_array()?
            .iter()
            .find(|row| row["id"] == id && row["state"] == "compare_code")
            .cloned()
    })
    .await;
    let code = compared["verification_code"].as_str().unwrap();
    let summary = b.connection(&a_id).to_string();
    assert!(!summary.contains("verification_code"));
    assert!(!summary.contains(code));
    assert!(!summary.contains("endpoint"));
    a.request(Request::LanPairingCancel { id: id.clone() });
    b.request(Request::LanPairingCancel { id });
    assert_eq!(a.connection(&b_id)["state"], "unpaired");
    assert_eq!(b.connection(&a_id)["state"], "unpaired");
    assert_eq!(a.request(Request::SharedSessions), json!([]));
    assert_eq!(b.request(Request::SharedSessions), json!([]));
    a.stop().await;
    b.stop().await;
}
