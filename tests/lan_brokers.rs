//! Opt-in tests for real multicast discovery. They use two isolated brokers on
//! one host and do not replace acceptance on two physical machines.
mod support;

use agent_bridge::{
    broker::{self, Broker},
    network::trust::Permission,
    protocol::{Agent, Provider, Request},
};
use libp2p::{Multiaddr, multiaddr::Protocol};
use serde_json::Value;
use std::{path::Path, time::Duration};
use tokio::{task::JoinHandle, time::timeout};

struct Running {
    broker: Broker,
    task: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Running {
    fn start(state: &Path) -> Self {
        let bound = Broker::bind(state.into(), "/missing/codex".into()).unwrap();
        Self {
            broker: bound.broker.clone(),
            task: Some(tokio::spawn(broker::serve(bound))),
        }
    }

    async fn stop(mut self) {
        let task = self.task.take().unwrap();
        task.abort();
        let _ = task.await;
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

    async fn discovered(&self, peer_id: &str) -> Value {
        wait("peer discovery", || {
            self.request(Request::PeersDiscover)
                .as_array()
                .unwrap()
                .iter()
                .find(|peer| peer["peer_id"] == peer_id)
                .cloned()
        })
        .await
    }

    async fn lan_address(&self) -> String {
        wait("LAN listener", || {
            self.request(Request::Status)["network"]["listening"]
                .as_array()?
                .iter()
                .find(|address| {
                    address
                        .as_str()
                        .and_then(|address| address.parse::<Multiaddr>().ok())
                        .is_some_and(|address| {
                            address.iter().any(|part| {
                                matches!(part, Protocol::Ip4(ip) if ip.is_private() && !ip.is_loopback())
                            })
                        })
                })
                .cloned()
        })
        .await
        .as_str()
        .unwrap()
        .to_owned()
    }

    async fn directory(&self) -> Value {
        wait("permitted remote directory", || {
            let directory = self.request(Request::RemoteList);
            (directory.as_array()?.len() == 1 && directory[0]["name"] == "user")
                .then_some(directory)
        })
        .await
    }

    async fn state(&self, id: &str, state: &str) -> Value {
        wait("delivery status", || {
            let message = self.request(Request::Get { id: id.into() });
            (message["state"] == state).then_some(message)
        })
        .await
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn wait(label: &str, mut inspect: impl FnMut() -> Option<Value>) -> Value {
    timeout(Duration::from_secs(60), async {
        loop {
            if let Some(value) = inspect() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}; requires a multicast-capable LAN"))
}

fn grant_user(bridge: &Running, peer: &str) {
    for permission in [Permission::Discover, Permission::Receive, Permission::Send] {
        bridge.request(Request::PeerGrant {
            peer: peer.into(),
            agent: "user".into(),
            permission,
            allow: true,
        });
    }
}

fn send(bridge: &Running, destination: &str, body: &str, parent: Option<String>) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    bridge.request(Request::Send {
        id: id.clone(),
        sender: "user".into(),
        recipient: destination.into(),
        body: body.into(),
        parent,
    });
    id
}

#[tokio::test]
#[ignore = "requires a multicast-capable LAN interface"]
async fn discovered_peers_pair_exchange_and_reconnect_without_manual_addresses() {
    let root = tempfile::Builder::new()
        .prefix("ab-lan-")
        .tempdir_in("/tmp")
        .unwrap();
    let alice = Running::start(&root.path().join("a"));
    let bob_path = root.path().join("b");
    let bob = Running::start(&bob_path);
    let alice_id = alice.identity();
    let bob_id = bob.identity();
    for bridge in [&alice, &bob] {
        bridge.request(Request::Register {
            agent: Agent {
                name: "private-agent".into(),
                provider: Provider::Inbox,
                session: "private-session".into(),
                endpoint: String::new(),
                enabled: true,
            },
            replace: false,
        });
        bridge.request(Request::NetworkLan { listen: None });
    }
    let found_bob = alice.discovered(&bob_id).await;
    let found_alice = bob.discovered(&alice_id).await;
    for (found, id) in [(&found_bob, &bob_id), (&found_alice, &alice_id)] {
        assert_eq!(found["fingerprint"], id.as_str());
        assert_eq!(found["trusted"], false);
        assert!(!found["addresses"].as_array().unwrap().is_empty());
        assert!(found.get("agents").is_none());
        assert!(found.get("session").is_none());
        assert!(found.get("endpoint").is_none());
    }
    for bridge in [&alice, &bob] {
        assert!(
            bridge
                .request(Request::PeersList)
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            bridge
                .request(Request::RemoteList)
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(bridge.history().is_empty());
    }
    assert!(
        alice
            .broker
            .request(Request::Send {
                id: uuid::Uuid::new_v4().to_string(),
                sender: "user".into(),
                recipient: format!("{bob_id}/user"),
                body: "Discovery must not grant access".into(),
                parent: None,
            })
            .is_err()
    );
    // The test already obtained both identities from their isolated brokers;
    // the human workflow verifies these complete fingerprints out of band.
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
    assert!(
        alice
            .request(Request::RemoteList)
            .as_array()
            .unwrap()
            .is_empty()
    );
    grant_user(&alice, "bob");
    grant_user(&bob, "alice");
    let alice_directory = alice.directory().await;
    let bob_directory = bob.directory().await;
    assert_eq!(alice_directory[0]["qualified_name"], "bob/user");
    assert_eq!(bob_directory[0]["qualified_name"], "alice/user");
    assert!(
        alice_directory
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent["name"] != "private-agent")
    );

    let id = send(&alice, "bob/user", "LAN exchange", None);
    let received = wait("LAN message", || {
        bob.history()
            .into_iter()
            .find(|message| message["body"] == "LAN exchange")
    })
    .await;
    let local_id = received["id"].as_str().unwrap();
    bob.state(local_id, "delivered").await;
    let delivered = alice.state(&id, "delivered").await;
    assert_eq!(delivered["transfer"]["state"], "accepted");
    send(
        &bob,
        &format!("{alice_id}/user"),
        "LAN reply",
        Some(local_id.into()),
    );
    let reply = wait("correlated LAN reply", || {
        alice
            .history()
            .into_iter()
            .find(|message| message["body"] == "LAN reply")
    })
    .await;
    assert_eq!(reply["parent"], id);
    assert_eq!(reply["hops"], 1);

    let old_address = bob.lan_address().await;
    let old_port = old_address
        .parse::<Multiaddr>()
        .unwrap()
        .iter()
        .find_map(|part| {
            if let Protocol::Udp(port) = part {
                Some(port)
            } else {
                None
            }
        })
        .unwrap();
    alice.request(Request::NetworkConfigure {
        enabled: false,
        listen: None,
    });
    wait("origin networking disable", || {
        (alice.request(Request::Status)["network"]["enabled"] == false).then_some(Value::Null)
    })
    .await;
    bob.stop().await;
    // Reserve the old UDP port so the restarted broker must advertise a new
    // one; this tests address refresh rather than luck in ephemeral allocation.
    let old_port_reservation = timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(socket) =
                std::net::UdpSocket::bind((std::net::Ipv4Addr::UNSPECIFIED, old_port))
            {
                break socket;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("stopped broker retained its QUIC port");
    let offline_id = send(&alice, "bob/user", "Queued across LAN address change", None);
    assert_eq!(
        alice.request(Request::Get {
            id: offline_id.clone()
        })["transfer"]["state"],
        "pending"
    );
    support::wait_for_shutdown(&bob_path).await;
    let bob = Running::start(&bob_path);
    assert_eq!(bob.identity(), bob_id);
    let new_address = bob.lan_address().await;
    assert_ne!(old_address, new_address);
    alice.request(Request::NetworkLan { listen: None });
    wait("new LAN address discovery", || {
        alice
            .request(Request::PeersDiscover)
            .as_array()?
            .iter()
            .find(|peer| {
                peer["peer_id"] == bob_id
                    && peer["addresses"].as_array().is_some_and(|addresses| {
                        addresses
                            .iter()
                            .any(|address| address == &Value::String(new_address.clone()))
                    })
            })
            .cloned()
    })
    .await;
    let recovered = wait("offline LAN message", || {
        bob.history()
            .into_iter()
            .find(|message| message["body"] == "Queued across LAN address change")
    })
    .await;
    bob.state(recovered["id"].as_str().unwrap(), "delivered")
        .await;
    alice.state(&offline_id, "delivered").await;
    assert_eq!(
        bob.history()
            .iter()
            .filter(|message| message["body"] == "Queued across LAN address change")
            .count(),
        1
    );
    drop(old_port_reservation);

    alice.request(Request::PeerRevoke { peer: "bob".into() });
    assert!(
        alice
            .request(Request::RemoteList)
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        alice
            .broker
            .request(Request::Send {
                id: uuid::Uuid::new_v4().to_string(),
                sender: "user".into(),
                recipient: "bob/user".into(),
                body: "Revoked discovery cannot restore access".into(),
                parent: None,
            })
            .is_err()
    );
    wait("revoked peer disconnect", || {
        let status = alice.request(Request::Status);
        status["network"]["connected"]
            .as_array()
            .is_some_and(|peers| !peers.iter().any(|peer| peer == &bob_id))
            .then_some(Value::Null)
    })
    .await;
    for peer in alice.request(Request::PeersDiscover).as_array().unwrap() {
        if peer["peer_id"] == bob_id {
            assert_eq!(peer["revoked"], true);
            assert_eq!(peer["trusted"], false);
        }
    }
    for bridge in [&alice, &bob] {
        bridge.request(Request::NetworkConfigure {
            enabled: false,
            listen: None,
        });
        wait("LAN disable cleanup", || {
            let status = bridge.request(Request::Status);
            (status["network"]["enabled"] == false
                && bridge
                    .request(Request::PeersDiscover)
                    .as_array()
                    .unwrap()
                    .is_empty())
            .then_some(Value::Null)
        })
        .await;
    }
    alice.stop().await;
    bob.stop().await;
}
