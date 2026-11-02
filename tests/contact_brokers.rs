#[path = "support/claude_directory.rs"]
mod claude_directory;

use agent_bridge::{
    broker::{BoundServer, Broker},
    network::{contact, identity::Identity},
    protocol::Request,
};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};
use tokio::time::timeout;

fn broker(path: &Path) -> BoundServer {
    let mut bound = Broker::bind(path.into(), "/missing/codex".into()).unwrap();
    let claude = claude_directory::write_cli(path, &json!([]));
    bound.broker = bound.broker.clone().with_claude_binary(claude);
    bound
}
fn enable(broker: &Broker) {
    broker
        .request(Request::NetworkConfigure {
            enabled: true,
            listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
        })
        .unwrap();
}
fn relay() -> String {
    let peer = libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id();
    format!("/dns4/relay.example.com/tcp/443/p2p/{peer}")
}
fn invite(root: &Path, relays: &[String]) -> String {
    let identity = Identity::load_or_create(&root.join("issuer")).unwrap();
    contact::create(&identity, "untrusted-display-name", relays, 120)
        .unwrap()
        .link
}
fn connect(link: &str, consent: bool, seconds: u64) -> Request {
    Request::ContactConnect {
        link: link.into(),
        use_invite_relays: consent,
        seconds,
    }
}
fn relays(broker: &Broker) -> Value {
    broker.request(Request::RelayList).unwrap()
}
fn assert_no_pairing_or_permissions(broker: &Broker, state: &Path) {
    assert_eq!(broker.request(Request::PeersList).unwrap(), json!([]));
    let pairing = broker.request(Request::LanPairingList).unwrap();
    assert_eq!(pairing["requests"], json!([]));
    assert_eq!(pairing["open"], false);
    let db = rusqlite::Connection::open(state.join("messages.sqlite3")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM peer_grants", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn contact_adoption_requires_explicit_consent_and_cancellation_prevents_sas() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("receiver");
    let server = broker(&state);
    let b = &server.broker;
    enable(b);
    let route = relay();
    let link = invite(root.path(), std::slice::from_ref(&route));
    let error = b
        .request_async(connect(&link, false, 120))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("--use-invite-relays"));
    assert_eq!(relays(b), json!([]));
    assert_no_pairing_or_permissions(b, &state);

    // Poll the request until it is awaiting the absent networking worker. A
    // timeout on &mut Future leaves that future alive, so close races the real
    // suspended continuation rather than merely dropping the operation.
    let pending = b.request_async(connect(&link, true, 120));
    tokio::pin!(pending);
    assert!(
        timeout(Duration::from_millis(60), &mut pending)
            .await
            .is_err()
    );
    assert_eq!(relays(b)[0]["address"], route);
    assert_no_pairing_or_permissions(b, &state);
    b.request(Request::LanPairingClose).unwrap();
    let error = timeout(Duration::from_secs(1), &mut pending)
        .await
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"));
    assert_no_pairing_or_permissions(b, &state);

    // The same, already-configured infrastructure does not need a second opt-in.
    let pending = b.request_async(connect(&link, false, 120));
    tokio::pin!(pending);
    assert!(
        timeout(Duration::from_millis(60), &mut pending)
            .await
            .is_err()
    );
    b.request(Request::LanPairingClose).unwrap();
    assert!(
        timeout(Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cancelled")
    );
    assert_no_pairing_or_permissions(b, &state);
}

#[tokio::test]
async fn invalid_contacts_and_lifetimes_cannot_change_configuration_or_authority() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("receiver");
    let server = broker(&state);
    let b = &server.broker;
    let link = invite(root.path(), &[relay()]);
    // A valid signed link and operator consent still do not opt into networking.
    assert!(b.request_async(connect(&link, true, 120)).await.is_err());
    assert_eq!(relays(b), json!([]));
    let db = agent_bridge::db::Db::open(&state.join("messages.sqlite3")).unwrap();
    assert!(!db.network_config().unwrap().enabled);
    drop(db);
    assert!(b.request(Request::ContactCreate { seconds: 120 }).is_err());
    assert_no_pairing_or_permissions(b, &state);

    enable(b);
    let before = relays(b);
    let mut tampered = link.clone().into_bytes();
    let offset = contact::PREFIX.len() + 30;
    tampered[offset] = if tampered[offset] == b'A' { b'B' } else { b'A' };
    for invalid in [
        "agent-bridge://connect/invalid".to_owned(),
        String::from_utf8(tampered).unwrap(),
        format!("{link}?untrusted=1"),
    ] {
        assert!(b.request_async(connect(&invalid, true, 120)).await.is_err());
        assert_eq!(relays(b), before);
        assert_no_pairing_or_permissions(b, &state);
    }
    for seconds in [0, 29, 601, u64::MAX] {
        assert!(
            b.request_async(connect(&link, true, seconds))
                .await
                .is_err()
        );
        assert!(b.request(Request::ContactCreate { seconds }).is_err());
        assert!(
            b.request_async(Request::ContactLocation {
                location: "localhost:443".into(),
                seconds
            })
            .await
            .is_err()
        );
        assert_eq!(relays(b), before);
        assert_no_pairing_or_permissions(b, &state);
    }
}

#[tokio::test]
async fn contact_operator_capacity_failure_rolls_back_the_entire_import() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("receiver");
    let server = broker(&state);
    let b = &server.broker;
    enable(b);
    for _ in 0..3 {
        b.request(Request::RelayAdd { address: relay() }).unwrap();
    }
    let before = relays(b);
    let incoming = vec![relay(), relay()];
    let link = invite(root.path(), &incoming);
    let error = b
        .request_async(connect(&link, true, 120))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("four independent relays"));
    assert_eq!(
        relays(b),
        before,
        "failed bulk import must not retain its first operator"
    );
    assert_no_pairing_or_permissions(b, &state);
}
