use agent_bridge::{
    db::Db,
    network::{
        identity::Identity,
        relay_config::{rendezvous_routes, validate_circuit, validate_relay},
    },
};
use serde_json::json;
fn peer() -> String {
    libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string()
}
fn address(id: &str, port: u16) -> String {
    format!("/ip4/127.0.0.1/udp/{port}/quic-v1/p2p/{id}")
}
#[test]
fn pinned_operators_persist_are_bounded_and_do_not_create_identity_trust_or_networking() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let ids: Vec<_> = (0..5).map(|_| peer()).collect();
    for (i, id) in ids.iter().take(4).enumerate() {
        db.add_relay(&address(id, 4001 + i as u16)).unwrap();
    }
    assert!(db.add_relay(&address(&ids[4], 4010)).is_err());
    assert!(db.peers().unwrap().is_empty());
    assert!(!db.network_config().unwrap().enabled);
    assert!(!root.path().join("identity.key").exists());
    db.add_relay(&address(&ids[0], 5000)).unwrap();
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert_eq!(db.relay_nodes().unwrap().len(), 4);
    assert!(
        db.relay_nodes()
            .unwrap()
            .iter()
            .any(|r| r.address == address(&ids[0], 5000))
    );
    db.remove_relay(&ids[0]).unwrap();
    assert!(db.remove_relay(&ids[0]).is_err());
    db.add_relay(&address(&ids[4], 4010)).unwrap();
}
#[test]
fn rendezvous_accepts_only_configured_relay_and_exact_final_identity() {
    let relay = peer();
    let other = peer();
    let target = peer();
    let local = peer();
    let r = validate_relay(&address(&relay, 4001)).unwrap();
    let good = format!("{}/p2p-circuit/p2p/{target}", r.address);
    let forged = format!("{}/p2p-circuit/p2p/{other}", r.address);
    let badoperator = format!("{}/p2p-circuit/p2p/{target}", address(&other, 4001));
    let records = json!([
        {"peer_id":target,"addresses":[forged,badoperator,"/ip4/8.8.8.8/udp/4001/quic-v1",good]},
        {"peer_id":local,"addresses":[format!("{}/p2p-circuit/p2p/{local}",r.address)]},
        {"peer_id":"invalid","addresses":[]}
    ]);
    let routes = rendezvous_routes(&records, &[r], &local);
    assert_eq!(routes.len(), 1);
    assert_eq!(routes[&target].len(), 1);
    assert_eq!(routes[&target][0].to_string(), good);
    assert!(validate_circuit(&good, Some(&other)).is_err());
}
#[test]
fn malformed_missing_identity_and_unscoped_circuit_routes_are_rejected() {
    let id = peer();
    let target = peer();
    for a in [
        "/ip4/127.0.0.1/udp/4001/quic-v1".into(),
        format!("/dns/EXAMPLE.com/udp/4001/quic-v1/p2p/{id}"),
        address(&id, 0),
        format!("/ip4/0.0.0.0/udp/4001/quic-v1/p2p/{id}"),
    ] {
        assert!(validate_relay(&a).is_err(), "{a}");
    }
    assert!(validate_circuit(&format!("{}/p2p-circuit", address(&id, 4001)), None).is_err());
    assert!(
        validate_circuit(
            &format!(
                "{}/p2p-circuit/p2p/{target}/p2p-circuit/p2p/{id}",
                address(&id, 4001)
            ),
            None
        )
        .is_err()
    );
    let root = tempfile::tempdir().unwrap();
    let mut db = Db::open(&root.path().join("db")).unwrap();
    db.pair_peer(&target, &target, None).unwrap();
    assert!(
        db.set_peer_address(
            &target,
            &format!("{}/p2p-circuit/p2p/{id}", address(&id, 4001))
        )
        .is_err()
    );
    let circuit = format!("{}/p2p-circuit/p2p/{target}", address(&id, 4001));
    assert!(db.set_peer_address(&target, &circuit).is_err());
    db.add_relay(&address(&id, 4001)).unwrap();
    db.set_peer_address(&target, &circuit).unwrap();
    db.remove_relay(&id).unwrap();
    assert!(
        agent_bridge::network::relay_config::configured_circuit(
            &circuit,
            &target,
            &db.relay_nodes().unwrap()
        )
        .is_err()
    );
}
#[test]
fn schema_six_upgrade_preserves_grants_and_identity_and_adds_empty_relay_config() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    std::fs::create_dir(&state).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    let id = Identity::load_or_create(&state).unwrap().info().peer_id;
    let path = state.join("db");
    let mut db = Db::open(&path).unwrap();
    let other = peer();
    db.pair_peer(&other, &other, Some("other")).unwrap();
    db.share_peer_session("other", "user", true).unwrap();
    db.pause(true).unwrap();
    drop(db);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("fixtures/native-metadata-v10.sql"))
        .unwrap();
    conn.execute_batch("DROP TRIGGER IF EXISTS inbound_usage_insert; DROP TRIGGER IF EXISTS inbound_usage_delete; DROP TABLE inbound_usage; DROP TRIGGER IF EXISTS message_context_insert; DROP TABLE message_context; DROP INDEX messages_monitor_order; DROP TABLE relay_nodes; PRAGMA user_version=6;")
        .unwrap();
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.peer("other").unwrap().grants.len(), 3);
    assert!(db.paused().unwrap());
    assert!(db.relay_nodes().unwrap().is_empty());
    assert_eq!(Identity::load_or_create(&state).unwrap().info().peer_id, id);
}

#[test]
fn dual_carrier_adoption_is_atomic_and_migrates_old_routes() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let id = peer();
    {
        let db = Db::open(&path).unwrap();
        drop(db);
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("fixtures/native-metadata-v10.sql"))
        .unwrap();
    conn.execute_batch("DROP TRIGGER IF EXISTS inbound_usage_insert; DROP TRIGGER IF EXISTS inbound_usage_delete; DROP TABLE inbound_usage; DROP TRIGGER IF EXISTS message_context_insert; DROP TABLE message_context; DROP INDEX messages_monitor_order; DROP TABLE relay_nodes; CREATE TABLE relay_nodes(peer_id TEXT PRIMARY KEY,address TEXT NOT NULL UNIQUE); PRAGMA user_version=7;").unwrap();
    conn.execute(
        "INSERT INTO relay_nodes VALUES(?,?)",
        rusqlite::params![id, address(&id, 4001)],
    )
    .unwrap();
    drop(conn);
    let mut db = Db::open(&path).unwrap();
    let tcp = format!("/dns4/localhost/tcp/4001/p2p/{id}");
    db.add_relay(&tcp).unwrap();
    assert_eq!(db.relay_nodes().unwrap().len(), 2);
    let before = db.relay_nodes().unwrap();
    let additions: Vec<_> = (0..4).map(|_| address(&peer(), 4002)).collect();
    assert!(db.add_relays(&additions).is_err());
    assert_eq!(db.relay_nodes().unwrap(), before);
    assert!(db.peers().unwrap().is_empty());
    assert!(!db.network_config().unwrap().enabled);
    db.remove_relay(&id).unwrap();
    assert!(db.relay_nodes().unwrap().is_empty());
}
#[test]
fn first_contact_locations_are_explicit_and_canonical() {
    use agent_bridge::network::relay_config::contact_location;
    for (input, expected) in [
        ("localhost:4001", "/dns/localhost/tcp/4001"),
        ("tcp://192.0.2.1:4001", "/ip4/192.0.2.1/tcp/4001"),
        (
            "quic://[2001:db8::1]:4001",
            "/ip6/2001:db8::1/udp/4001/quic-v1",
        ),
    ] {
        assert_eq!(contact_location(input).unwrap().to_string(), expected);
    }
    for input in [
        "example.com",
        "user@example.com:4001",
        "EXAMPLE.com:4001",
        "example.com:0",
        "example.com:4001/path",
        "https://example.com:4001",
        "example.com:4001?x=y",
        "0.0.0.0:4001",
    ] {
        assert!(contact_location(input).is_err(), "{input}");
    }
}
