//! Production brokers use only registered circuit routes, with no LAN discovery.
use agent_bridge::{
    broker::{self, Broker},
    network::relay_node::{self, Limits, RunningNode},
    protocol::{Agent, Provider, Request},
};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;
struct Running {
    broker: Broker,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl Running {
    fn start(path: &std::path::Path) -> Self {
        let bound = Broker::bind(path.into(), "/missing/codex".into()).unwrap();
        Self {
            broker: bound.broker.clone(),
            task: tokio::spawn(broker::serve(bound)),
        }
    }
    fn request(&self, req: Request) -> Value {
        self.broker.request(req).unwrap()
    }
    fn id(&self) -> String {
        self.request(Request::Identity)["peer_id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn peers(&self) -> Value {
        self.request(Request::PeersList)
    }
    fn attempts(&self) -> Value {
        self.request(Request::LanPairingList)
    }
    async fn state(&self, id: &str, state: &str) -> Value {
        wait(state, || {
            self.attempts()["requests"]
                .as_array()?
                .iter()
                .find(|s| s["id"] == id && s["state"] == state)
                .cloned()
        })
        .await
    }
    fn share(&self, peer: &str) {
        self.request(Request::PeerShare {
            peer: peer.into(),
            agent: "user".into(),
            shared: true,
        });
    }
    fn send(&self, to: &str, body: &str, parent: Option<String>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.request(Request::Send {
            id: id.clone(),
            sender: "user".into(),
            recipient: to.into(),
            body: body.into(),
            parent,
        });
        id
    }
    fn history(&self) -> Value {
        self.request(Request::History {
            agent: None,
            limit: 100,
        })
    }
    async fn message(&self, body: &str) -> Value {
        wait("received message", || {
            self.history()
                .as_array()?
                .iter()
                .find(|m| m["body"] == body)
                .cloned()
        })
        .await
    }
    async fn delivery(&self, id: &str, state: &str) -> Value {
        wait("message state", || {
            let m = self.request(Request::Get { id: id.into() });
            (m["state"] == state).then_some(m)
        })
        .await
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn wait(label: &str, mut inspect: impl FnMut() -> Option<Value>) -> Value {
    timeout(Duration::from_secs(40), async {
        loop {
            if let Some(v) = inspect() {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timeout waiting {label}"))
}
async fn node() -> (RunningNode, String) {
    let node = relay_node::start(
        libp2p::identity::Keypair::generate_ed25519(),
        "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        None,
        Limits::default(),
    )
    .unwrap();
    let address = wait("relay listener", || {
        node.status.borrow()["listening"]
            .as_array()?
            .first()
            .cloned()
    })
    .await;
    let address = format!("{}/p2p/{}", address.as_str().unwrap(), node.peer_id);
    (node, address)
}
fn configure(bridge: &Running, addresses: &[&str]) {
    for address in addresses {
        bridge.request(Request::RelayAdd {
            address: (*address).into(),
        });
    }
    bridge.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
    });
}
async fn discover(a: &Running, target: &str) {
    wait("public rendezvous", || {
        a.request(Request::PeersDiscover)
            .as_array()?
            .iter()
            .find(|p| p["peer_id"] == target)
            .cloned()
    })
    .await;
}
async fn pair(a: &Running, b: &Running) -> String {
    b.request(Request::LanPairingOpen { seconds: 120 });
    let start = a.request(Request::LanPairingConnect {
        peer: b.id(),
        address: None,
        seconds: 120,
    });
    let id = start["id"].as_str().unwrap().to_owned();
    b.state(&id, "awaiting_accept").await;
    b.request(Request::LanPairingAccept { id: id.clone() });
    let av = a.state(&id, "compare_code").await;
    let bv = b.state(&id, "compare_code").await;
    assert_eq!(av["verification_code"], bv["verification_code"]);
    assert!(a.peers().as_array().unwrap().is_empty());
    assert!(b.peers().as_array().unwrap().is_empty());
    let code = av["verification_code"].as_str().unwrap().to_owned();
    a.request(Request::LanPairingConfirm {
        id: id.clone(),
        code: code.clone(),
    });
    b.request(Request::LanPairingConfirm {
        id: id.clone(),
        code,
    });
    a.state(&id, "paired").await;
    b.state(&id, "paired").await;
    id
}
async fn directory(bridge: &Running, target: &str) {
    wait("permitted remote directory", || {
        bridge
            .request(Request::RemoteList)
            .as_array()?
            .iter()
            .find(|row| row["peer_id"] == target && row["name"] == "user")
            .cloned()
    })
    .await;
}

#[tokio::test]
async fn public_discovery_pairs_and_exchanges_private_delivered_replies_only_through_relay() {
    let root = tempfile::Builder::new()
        .prefix("ab-relay-brokers-")
        .tempdir_in("/tmp")
        .unwrap();
    let (node, address) = node().await;
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let aid = a.id();
    let bid = b.id();
    configure(&a, &[&address]);
    configure(&b, &[&address]);
    discover(&a, &bid).await;
    discover(&b, &aid).await;
    assert_eq!(
        a.request(Request::Status)["network"]["configured"]["lan"],
        false
    );
    assert!(a.peers().as_array().unwrap().is_empty());
    assert!(b.peers().as_array().unwrap().is_empty());
    assert!(
        a.request(Request::RelayList)
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["peer_id"] != aid)
    );
    pair(&a, &b).await;
    assert_eq!(a.peers()[0]["grants"], serde_json::json!([]));
    assert_eq!(a.peers()[0]["peer_id"], bid);
    assert_eq!(b.peers()[0]["peer_id"], aid);
    assert!(
        a.peers()
            .as_array()
            .unwrap()
            .iter()
            .all(|peer| peer["peer_id"] != node.peer_id.to_string())
    );
    b.request(Request::Register {
        agent: Agent {
            name: "private".into(),
            provider: Provider::Inbox,
            session: "private".into(),
            endpoint: String::new(),
            enabled: true,
        },
        replace: false,
    });
    a.share(&bid);
    b.share(&aid);
    directory(&a, &bid).await;
    directory(&b, &aid).await;
    assert!(
        a.request(Request::RemoteList)
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["name"] != "private")
    );
    let original = a.send(&format!("{bid}/user"), "Relayed hello", None);
    let incoming = b.message("Relayed hello").await;
    let local = incoming["id"].as_str().unwrap().to_owned();
    b.delivery(&local, "delivered").await;
    a.delivery(&original, "delivered").await;
    let replyid = b.send(&format!("{aid}/user"), "Relayed reply", Some(local));
    let reply = a.message("Relayed reply").await;
    assert_eq!(reply["parent"], original);
    b.delivery(&replyid, "delivered").await;
    assert!(
        node.status.borrow()["circuits_opened"].as_u64().unwrap() > 0,
        "node must actually forward traffic"
    );
    assert!(
        a.request(Request::Status)["network"]["discovered"]
            .as_array()
            .unwrap()
            .is_empty(),
        "no LAN hints"
    );
    b.request(Request::PeerRevoke { peer: aid.clone() });
    assert_eq!(b.peers()[0]["revoked"], true);
    assert_eq!(b.peers()[0]["grants"], serde_json::json!([]));
    let blocked = a.send(&format!("{bid}/user"), "Revoked relay traffic", None);
    wait("revoked endpoint disconnected", || {
        let status = a.request(Request::Status);
        status["network"]["connected"]
            .as_array()
            .is_some_and(|ids| ids.iter().all(|id| id != &bid))
            .then_some(Value::Null)
    })
    .await;
    // A dropped connection cannot convey the remote owner's policy decision.
    // The durable source intent may remain pending, but cannot be delivered.
    assert_ne!(
        a.request(Request::Get { id: blocked })["state"],
        "delivered"
    );
    assert!(
        b.history()
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["body"] != "Revoked relay traffic")
    );
    a.request(Request::NetworkConfigure {
        enabled: false,
        listen: None,
    });
    assert!(
        a.request(Request::PeersDiscover)
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(a.request(Request::RelayList).as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn configured_independent_nodes_provide_routes_when_an_operator_is_unavailable() {
    let root = tempfile::Builder::new()
        .prefix("ab-relay-multi-")
        .tempdir_in("/tmp")
        .unwrap();
    let (bad, badaddress) = node().await;
    drop(bad);
    let (good, goodaddress) = node().await;
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    configure(&a, &[&badaddress, &goodaddress]);
    configure(&b, &[&badaddress, &goodaddress]);
    discover(&a, &b.id()).await;
    discover(&b, &a.id()).await;
    pair(&a, &b).await;
    a.share(&b.id());
    b.share(&a.id());
    directory(&a, &b.id()).await;
    let id = a.send(&format!("{}/user", b.id()), "Independent operator", None);
    let msg = b.message("Independent operator").await;
    b.delivery(msg["id"].as_str().unwrap(), "delivered").await;
    a.delivery(&id, "delivered").await;
    assert!(good.status.borrow()["circuits_opened"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn relay_restart_recovers_reservations_and_durable_messages_without_pairing_again() {
    let root = tempfile::Builder::new()
        .prefix("ab-relay-restart-")
        .tempdir_in("/tmp")
        .unwrap();
    let key = libp2p::identity::Keypair::generate_ed25519();
    let relay = relay_node::start(
        key.clone(),
        "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        None,
        Limits::default(),
    )
    .unwrap();
    let listen = wait("relay listener", || {
        relay.status.borrow()["listening"]
            .as_array()?
            .first()
            .cloned()
    })
    .await;
    let listen = listen.as_str().unwrap().to_owned();
    let address = format!("{listen}/p2p/{}", relay.peer_id);
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    configure(&a, &[&address]);
    configure(&b, &[&address]);
    discover(&a, &b.id()).await;
    discover(&b, &a.id()).await;
    pair(&a, &b).await;
    a.share(&b.id());
    b.share(&a.id());
    directory(&a, &b.id()).await;
    directory(&b, &a.id()).await;
    // Disable the destination as well: an established direct path can survive
    // relay loss, while this case must exercise offline durable recovery.
    b.request(Request::NetworkConfigure {
        enabled: false,
        listen: None,
    });
    relay.shutdown().await;
    let original = a.send(&format!("{}/user", b.id()), "Survives relay restart", None);
    let restarted = timeout(Duration::from_secs(5), async {
        loop {
            match relay_node::start(
                key.clone(),
                listen.parse().unwrap(),
                None,
                Limits::default(),
            ) {
                Ok(node) => break node,
                // The previous successful configuration is unchanged. QUIC may
                // release its underlying endpoint after the swarm task stops;
                // retry the bind within this bounded restart deadline.
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("Old QUIC listener must be released before restart");
    b.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
    });
    let incoming = b.message("Survives relay restart").await;
    b.delivery(incoming["id"].as_str().unwrap(), "delivered")
        .await;
    a.delivery(&original, "delivered").await;
    assert_eq!(
        b.history()
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["body"] == "Survives relay restart")
            .count(),
        1
    );
    assert_eq!(a.peers()[0]["grants"].as_array().unwrap().len(), 3);
    let mut last_observation = Value::Null;
    timeout(Duration::from_secs(40), async {
        loop {
            let relay_status = restarted.status.borrow().clone();
            last_observation = serde_json::json!({
                "relay":relay_status,
                "source_network":a.request(Request::Status)["network"],
                "destination_network":b.request(Request::Status)["network"],
            });
            if relay_status["reservations"].as_u64().unwrap_or_default() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!("timeout waiting restored relay reservations: {last_observation:#}")
    });
    assert!(
        a.request(Request::Status)["network"]["paths"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path["peer_id"] == b.id())
    );
}
