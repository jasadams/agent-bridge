mod support;

use agent_bridge::{
    broker::{self, Broker},
    protocol::Request,
};
use serde_json::Value;
use std::time::Duration;
use tokio::time::timeout;

struct Running {
    broker: Broker,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl Running {
    fn start(state: &std::path::Path) -> Self {
        let server = Broker::bind(state.into(), "/missing/codex".into()).unwrap();
        Self {
            broker: server.broker.clone(),
            task: tokio::spawn(broker::serve(server)),
        }
    }
    fn request(&self, req: Request) -> Value {
        self.broker.request(req).unwrap()
    }
    fn peers(&self) -> Value {
        self.request(Request::PeersList)
    }
    fn pairing(&self) -> Value {
        self.request(Request::LanPairingList)
    }
    fn identity(&self) -> String {
        self.request(Request::Identity)["peer_id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn address(&self) -> String {
        self.request(Request::NetworkConfigure {
            enabled: true,
            listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
        });
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(address) = self.request(Request::Status)["network"]["listening"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(Value::as_str)
                {
                    break address.to_owned();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn wait_state(&self, id: &str, state: &str) -> Value {
        timeout(Duration::from_secs(12), async {
            loop {
                if let Some(view) = self.pairing()["requests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|v| v["id"] == id && v["state"] == state)
                {
                    break view.clone();
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting {state}: {}", self.pairing()))
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn begin(a: &Running, b: &Running) -> String {
    let _ = a.address().await;
    let address = b.address().await;
    b.request(Request::LanPairingOpen { seconds: 120 });
    let started = a.request(Request::LanPairingConnect {
        peer: b.identity(),
        address: Some(address),
        seconds: 120,
    });
    let id = started["id"].as_str().unwrap().to_owned();
    b.wait_state(&id, "awaiting_accept").await;
    b.request(Request::LanPairingAccept { id: id.clone() });
    id
}

#[tokio::test]
async fn network_pairing_requires_both_owners_and_no_token_copy_or_grants() {
    let root = tempfile::Builder::new()
        .prefix("ab-lan-pair-")
        .tempdir_in("/tmp")
        .unwrap();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let id = begin(&a, &b).await;
    let av = a.wait_state(&id, "compare_code").await;
    let bv = b.wait_state(&id, "compare_code").await;
    let code = av["verification_code"].as_str().unwrap().to_owned();
    assert_eq!(bv["verification_code"], code);
    assert_eq!(a.peers(), serde_json::json!([]));
    assert_eq!(b.peers(), serde_json::json!([]));
    b.request(Request::LanPairingConfirm {
        id: id.clone(),
        code: code.clone(),
    });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(a.peers(), serde_json::json!([]));
    assert_eq!(b.peers(), serde_json::json!([]));
    a.request(Request::LanPairingConfirm {
        id: id.clone(),
        code,
    });
    a.wait_state(&id, "paired").await;
    b.wait_state(&id, "paired").await;
    assert_eq!(a.peers()[0]["peer_id"], b.identity());
    assert_eq!(b.peers()[0]["peer_id"], a.identity());
    assert_eq!(a.peers()[0]["grants"], serde_json::json!([]));
    assert_eq!(b.peers()[0]["grants"], serde_json::json!([]));
    assert!(
        a.request(Request::RemoteList)
            .as_array()
            .unwrap()
            .is_empty()
    );
    a.request(Request::PeerRevoke { peer: b.identity() });
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(a.peers()[0]["revoked"], true);
    assert!(!a.pairing()["open"].as_bool().unwrap());
}

#[tokio::test]
async fn disable_and_restart_drop_incomplete_approval_without_creating_trust() {
    let root = tempfile::Builder::new()
        .prefix("ab-lan-pair-")
        .tempdir_in("/tmp")
        .unwrap();
    let a = Running::start(&root.path().join("a"));
    let bpath = root.path().join("b");
    let b = Running::start(&bpath);
    let id = begin(&a, &b).await;
    let av = a.wait_state(&id, "compare_code").await;
    b.wait_state(&id, "compare_code").await;
    a.request(Request::LanPairingConfirm {
        id: id.clone(),
        code: av["verification_code"].as_str().unwrap().into(),
    });
    b.request(Request::NetworkConfigure {
        enabled: false,
        listen: None,
    });
    assert!(b.pairing()["requests"].as_array().unwrap().is_empty());
    assert!(
        b.broker
            .request(Request::LanPairingConfirm {
                id: id.clone(),
                code: av["verification_code"].as_str().unwrap().into()
            })
            .is_err()
    );
    b.task.abort();
    drop(b);
    support::wait_for_shutdown(&bpath).await;
    let b = Running::start(&bpath);
    assert!(b.pairing()["requests"].as_array().unwrap().is_empty());
    assert_eq!(a.peers(), serde_json::json!([]));
    assert_eq!(b.peers(), serde_json::json!([]));
}

#[tokio::test]
#[ignore = "requires multicast-capable IPv4 LAN"]
async fn discovered_machine_name_pairs_without_manual_address_or_tokens() {
    let root = tempfile::Builder::new()
        .prefix("ab-lan-pair-")
        .tempdir_in("/tmp")
        .unwrap();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let name = format!("qa-pair-{}", uuid::Uuid::new_v4().simple());
    b.request(Request::MachineSetName { name: name.clone() });
    a.request(Request::NetworkLan { listen: None });
    b.request(Request::NetworkLan { listen: None });
    b.request(Request::LanPairingOpen { seconds: 120 });
    timeout(Duration::from_secs(15), async {
        loop {
            if a.request(Request::PeersDiscover)
                .as_array()
                .unwrap()
                .iter()
                .any(|peer| peer["machine_name"] == name)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let id = a.request(Request::LanPairingConnect {
        peer: name,
        address: None,
        seconds: 120,
    })["id"]
        .as_str()
        .unwrap()
        .to_owned();
    b.wait_state(&id, "awaiting_accept").await;
    b.request(Request::LanPairingAccept { id: id.clone() });
    let av = a.wait_state(&id, "compare_code").await;
    let bv = b.wait_state(&id, "compare_code").await;
    assert_eq!(av["verification_code"], bv["verification_code"]);
    let code = av["verification_code"].as_str().unwrap().to_owned();
    a.request(Request::LanPairingConfirm {
        id: id.clone(),
        code: code.clone(),
    });
    b.request(Request::LanPairingConfirm {
        id: id.clone(),
        code,
    });
    a.wait_state(&id, "paired").await;
    b.wait_state(&id, "paired").await;
    assert_eq!(a.peers()[0]["peer_id"], b.identity());
    assert_eq!(b.peers()[0]["peer_id"], a.identity());
}

#[tokio::test]
async fn address_only_tcp_contact_requires_owner_verification_then_persists_pinned_route() {
    let root = tempfile::Builder::new()
        .prefix("ab-contact-")
        .tempdir_in("/tmp")
        .unwrap();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    a.address().await;
    b.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some("/ip4/127.0.0.1/tcp/0".into()),
    });
    let address = timeout(Duration::from_secs(8), async {
        loop {
            if let Some(addr) = b.request(Request::Status)["network"]["listening"]
                .as_array()
                .and_then(|v| v.first())
                .and_then(Value::as_str)
            {
                break addr.to_string();
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .unwrap();
    // The key-only lookup itself needs no open pairing window.
    let started = a
        .broker
        .request_async(Request::ContactLocation {
            location: address.clone(),
            seconds: 120,
        })
        .await
        .unwrap();
    let id = started["id"].as_str().unwrap().to_owned();
    assert_eq!(started["identity_unverified"], true);
    assert!(a.peers().as_array().unwrap().is_empty());
    b.request(Request::LanPairingOpen { seconds: 120 });
    b.wait_state(&id, "awaiting_accept").await;
    b.request(Request::LanPairingAccept { id: id.clone() });
    let av = a.wait_state(&id, "compare_code").await;
    let bv = b.wait_state(&id, "compare_code").await;
    assert_eq!(av["verification_code"], bv["verification_code"]);
    let code = av["verification_code"].as_str().unwrap().to_string();
    a.request(Request::LanPairingConfirm {
        id: id.clone(),
        code: code.clone(),
    });
    assert!(a.peers().as_array().unwrap().is_empty());
    b.request(Request::LanPairingConfirm {
        id: id.clone(),
        code,
    });
    a.wait_state(&id, "paired").await;
    b.wait_state(&id, "paired").await;
    assert_eq!(a.peers()[0]["grants"], serde_json::json!([]));
    let db = agent_bridge::db::Db::open(&root.path().join("a/messages.sqlite3")).unwrap();
    assert_eq!(
        db.network_peers()
            .unwrap()
            .into_iter()
            .find(|(id, _)| id == &b.identity())
            .unwrap()
            .1,
        address
    );
}
