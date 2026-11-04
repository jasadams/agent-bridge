use agent_bridge::{
    db::Db,
    migrations::SCHEMA_VERSION,
    network::{identity::Identity, invitation, trust::Permission},
};
use rusqlite::{Connection, params};
use std::{fs, os::unix::fs::PermissionsExt};

fn identity(root: &std::path::Path, name: &str) -> Identity {
    let path = root.join(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    Identity::load_or_create(&path).unwrap()
}

#[test]
fn accepted_invitation_persists_identity_address_and_replay_protection_without_grants() {
    let root = tempfile::tempdir().unwrap();
    let issuer = identity(root.path(), "issuer");
    let receiver = identity(root.path(), "receiver");
    let fingerprint = issuer.info().fingerprint;
    let token =
        invitation::create(&issuer, &["/ip4/192.168.1.20/udp/44001/quic-v1".into()], 60).unwrap();
    let path = root.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let peer = db
        .accept_invitation(
            &token,
            &fingerprint,
            Some("workstation"),
            &receiver.info().peer_id,
        )
        .unwrap();
    assert_eq!(peer.peer_id, fingerprint);
    assert!(peer.grants.is_empty());
    assert_eq!(
        db.network_peers().unwrap(),
        vec![(
            fingerprint.clone(),
            "/ip4/192.168.1.20/udp/44001/quic-v1".into()
        )]
    );
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert!(
        db.accept_invitation(
            &token,
            &fingerprint,
            Some("other"),
            &receiver.info().peer_id
        )
        .is_err()
    );
    assert_eq!(db.peer("workstation").unwrap().peer_id, fingerprint);
    assert!(db.peer("other").is_err());
}

#[test]
fn replay_cannot_restore_revoked_trust_and_new_invitation_restores_without_grants() {
    let root = tempfile::tempdir().unwrap();
    let issuer = identity(root.path(), "issuer");
    let receiver = identity(root.path(), "receiver");
    let fingerprint = issuer.info().fingerprint;
    let local = receiver.info().peer_id;
    let mut db = Db::open(&root.path().join("db")).unwrap();
    let first = invitation::create(&issuer, &[], 60).unwrap();
    db.accept_invitation(&first, &fingerprint, None, &local)
        .unwrap();
    db.grant_peer(&fingerprint, "user", Permission::Receive, true)
        .unwrap();
    db.revoke_peer(&fingerprint).unwrap();
    assert!(
        db.accept_invitation(&first, &fingerprint, None, &local)
            .is_err()
    );
    assert!(db.peer(&fingerprint).unwrap().revoked);
    let fresh = invitation::create(&issuer, &[], 60).unwrap();
    let peer = db
        .accept_invitation(&fresh, &fingerprint, None, &local)
        .unwrap();
    assert!(!peer.revoked);
    assert!(peer.grants.is_empty());
    assert!(
        db.accept_invitation(
            &invitation::create(&receiver, &[], 60).unwrap(),
            &local,
            None,
            &local
        )
        .is_err()
    );
}

#[test]
fn alias_collision_rolls_back_pairing_and_does_not_consume_invitation() {
    let root = tempfile::tempdir().unwrap();
    let issuer = identity(root.path(), "issuer");
    let receiver = identity(root.path(), "receiver");
    let existing = identity(root.path(), "existing").info().peer_id;
    let fingerprint = issuer.info().fingerprint;
    let local = receiver.info().peer_id;
    let token = invitation::create(&issuer, &[], 60).unwrap();
    let mut db = Db::open(&root.path().join("db")).unwrap();
    db.pair_peer(&existing, &existing, Some("workstation"))
        .unwrap();
    assert!(
        db.accept_invitation(&token, &fingerprint, Some("workstation"), &local)
            .is_err()
    );
    assert_eq!(db.peers().unwrap().len(), 1);
    assert_eq!(db.peer("workstation").unwrap().peer_id, existing);
    db.accept_invitation(&token, &fingerprint, Some("second"), &local)
        .unwrap();
    assert_eq!(db.peer("second").unwrap().peer_id, fingerprint);
}

#[test]
fn invitation_hints_do_not_overwrite_manual_peer_address() {
    let root = tempfile::tempdir().unwrap();
    let issuer = identity(root.path(), "issuer");
    let local = identity(root.path(), "receiver").info().peer_id;
    let fingerprint = issuer.info().fingerprint;
    let mut db = Db::open(&root.path().join("db")).unwrap();
    db.pair_peer(&fingerprint, &fingerprint, None).unwrap();
    db.set_peer_address(&fingerprint, "/ip4/192.168.1.30/udp/44001/quic-v1")
        .unwrap();
    let token =
        invitation::create(&issuer, &["/ip4/192.168.1.20/udp/44001/quic-v1".into()], 60).unwrap();
    db.accept_invitation(&token, &fingerprint, None, &local)
        .unwrap();
    assert_eq!(
        db.network_peers().unwrap()[0].1,
        "/ip4/192.168.1.30/udp/44001/quic-v1"
    );
}

#[test]
fn schema_three_upgrade_preserves_existing_network_configuration_and_data() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let db = Db::open(&path).unwrap();
    db.set_network_config(true, Some("/ip4/127.0.0.1/udp/44001/quic-v1"))
        .unwrap();
    db.pause(true).unwrap();
    drop(db);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("fixtures/native-metadata-v10.sql"))
        .unwrap();
    conn.execute_batch("DROP TRIGGER IF EXISTS inbound_usage_insert; DROP TRIGGER IF EXISTS inbound_usage_delete; DROP TABLE inbound_usage; DROP TRIGGER IF EXISTS message_context_insert; DROP TABLE message_context; DROP INDEX messages_monitor_order; DROP TABLE relay_nodes; DROP TABLE pairing_offers; DROP TABLE pairing_joined; DROP TABLE consumed_invitations; DELETE FROM settings WHERE key IN ('network_lan','machine_name'); ALTER TABLE peers DROP COLUMN machine_name; PRAGMA user_version=3;").unwrap();
    conn.execute("INSERT INTO messages VALUES('preserved','user','user','user','body',NULL,0,'submitted',NULL,1,1)", []).unwrap();
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert!(db.network_config().unwrap().enabled);
    assert_eq!(
        db.network_config().unwrap().listen,
        "/ip4/127.0.0.1/udp/44001/quic-v1"
    );
    assert!(db.paused().unwrap());
    assert_eq!(db.get("preserved").unwrap().body, "body");
    let conn = Connection::open(path).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT value FROM settings WHERE key='network_lan'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        "false"
    );
}

#[test]
fn invitation_record_limit_is_bounded_and_expired_records_are_pruned() {
    let root = tempfile::tempdir().unwrap();
    let issuer = identity(root.path(), "issuer");
    let local = identity(root.path(), "receiver").info().peer_id;
    let fingerprint = issuer.info().fingerprint;
    let path = root.path().join("db");
    let mut db = Db::open(&path).unwrap();
    db.pair_peer(&fingerprint, &fingerprint, None).unwrap();
    let token = invitation::create(&issuer, &[], 60).unwrap();
    let expires = invitation::verify(&token, &fingerprint).unwrap().expires_at;
    let mut conn = Connection::open(&path).unwrap();
    let tx = conn.transaction().unwrap();
    for _ in 0..1024 {
        tx.execute(
            "INSERT INTO consumed_invitations VALUES(?,?,?)",
            params![fingerprint, uuid::Uuid::new_v4().to_string(), expires],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    assert!(
        db.accept_invitation(&token, &fingerprint, None, &local)
            .is_err()
    );
    conn.execute("UPDATE consumed_invitations SET expires_at=0", [])
        .unwrap();
    db.accept_invitation(&token, &fingerprint, None, &local)
        .unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM consumed_invitations", [], |r| r
            .get(0))
            .unwrap(),
        1
    );
}
