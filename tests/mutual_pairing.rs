use agent_bridge::{
    db::Db,
    network::{identity::Identity, trust::Permission},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, params};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
fn identity(root: &Path, name: &str) -> Identity {
    let path = root.join(name);
    fs::create_dir(&path).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    Identity::load_or_create(&path).unwrap()
}
#[test]
fn mutual_pairing_persists_both_keys_no_grants_and_signed_address_hints() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let b = identity(root.path(), "b");
    let path = root.path().join("a.db");
    let mut da = Db::open(&path).unwrap();
    let mut db = Db::open(&root.path().join("b.db")).unwrap();
    let offer = da
        .pairing_offer(&a, &["/ip4/192.168.1.2/udp/4000/quic-v1".into()], 120)
        .unwrap();
    drop(da);
    let mut da = Db::open(&path).unwrap();
    let joined = db
        .pairing_join(
            &b,
            &offer.offer,
            &a.info().fingerprint,
            Some("a"),
            &["/ip4/192.168.1.3/udp/4000/quic-v1".into()],
        )
        .unwrap();
    assert!(joined.peer.grants.is_empty());
    assert!(da.peers().unwrap().is_empty());
    let peer = da
        .pairing_finish(&a, &joined.response, &b.info().fingerprint, Some("b"))
        .unwrap();
    assert!(peer.grants.is_empty());
    assert_eq!(da.peer("b").unwrap().peer_id, b.info().peer_id);
    assert_eq!(db.peer("a").unwrap().peer_id, a.info().peer_id);
    assert_eq!(
        da.network_peers().unwrap()[0].1,
        "/ip4/192.168.1.3/udp/4000/quic-v1"
    );
    assert_eq!(
        db.network_peers().unwrap()[0].1,
        "/ip4/192.168.1.2/udp/4000/quic-v1"
    );
    assert!(!da.network_config().unwrap().enabled);
    assert!(!db.network_config().unwrap().enabled);
}
#[test]
fn one_issuer_offer_accepts_exactly_one_responder_across_restart() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let b = identity(root.path(), "b");
    let c = identity(root.path(), "c");
    let path = root.path().join("a.db");
    let mut da = Db::open(&path).unwrap();
    let mut db = Db::open(&root.path().join("b.db")).unwrap();
    let mut dc = Db::open(&root.path().join("c.db")).unwrap();
    let offer = da.pairing_offer(&a, &[], 60).unwrap();
    let rb = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, None, &[])
        .unwrap();
    let rc = dc
        .pairing_join(&c, &offer.offer, &a.info().fingerprint, None, &[])
        .unwrap();
    // Knowing a stolen offer cannot pass confirmation of the intended responder.
    assert!(
        da.pairing_finish(&a, &rc.response, &b.info().fingerprint, None)
            .is_err()
    );
    assert!(da.peers().unwrap().is_empty());
    da.pairing_finish(&a, &rb.response, &b.info().fingerprint, None)
        .unwrap();
    drop(da);
    let mut da = Db::open(&path).unwrap();
    assert!(
        da.pairing_finish(&a, &rc.response, &c.info().fingerprint, None)
            .is_err()
    );
    assert!(
        da.pairing_finish(&a, &rb.response, &b.info().fingerprint, None)
            .is_err()
    );
    assert_eq!(da.peers().unwrap().len(), 1);
}
#[test]
fn replay_cannot_restore_revoked_trust_and_fresh_pairing_has_no_grants() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let b = identity(root.path(), "b");
    let mut da = Db::open(&root.path().join("a.db")).unwrap();
    let path = root.path().join("b.db");
    let mut db = Db::open(&path).unwrap();
    let offer = da.pairing_offer(&a, &[], 60).unwrap();
    let reply = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, None, &[])
        .unwrap();
    da.pairing_finish(&a, &reply.response, &b.info().fingerprint, None)
        .unwrap();
    da.grant_peer(&b.info().peer_id, "user", Permission::Receive, true)
        .unwrap();
    db.grant_peer(&a.info().peer_id, "user", Permission::Receive, true)
        .unwrap();
    da.revoke_peer(&b.info().peer_id).unwrap();
    db.revoke_peer(&a.info().peer_id).unwrap();
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert!(
        db.pairing_join(&b, &offer.offer, &a.info().fingerprint, None, &[])
            .is_err()
    );
    assert!(
        da.pairing_finish(&a, &reply.response, &b.info().fingerprint, None)
            .is_err()
    );
    assert!(db.peer(&a.info().peer_id).unwrap().revoked);
    assert!(da.peer(&b.info().peer_id).unwrap().revoked);
    let offer = da.pairing_offer(&a, &[], 60).unwrap();
    let reply = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, None, &[])
        .unwrap();
    let peer = da
        .pairing_finish(&a, &reply.response, &b.info().fingerprint, None)
        .unwrap();
    assert!(peer.grants.is_empty());
    assert!(reply.peer.grants.is_empty());
}
#[test]
fn cancellation_wrong_issuer_wrong_fingerprint_and_self_pairing_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let b = identity(root.path(), "b");
    let c = identity(root.path(), "c");
    let mut da = Db::open(&root.path().join("a.db")).unwrap();
    let mut db = Db::open(&root.path().join("b.db")).unwrap();
    let offer = da.pairing_offer(&a, &[], 60).unwrap();
    assert!(
        da.pairing_join(&a, &offer.offer, &a.info().fingerprint, None, &[])
            .is_err()
    );
    assert!(
        db.pairing_join(&b, &offer.offer, &c.info().fingerprint, None, &[])
            .is_err()
    );
    let reply = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, None, &[])
        .unwrap();
    assert!(
        da.pairing_finish(&c, &reply.response, &b.info().fingerprint, None)
            .is_err()
    );
    da.pairing_cancel(&offer.nonce).unwrap();
    assert!(da.pairing_cancel(&offer.nonce).is_err());
    assert!(
        da.pairing_finish(&a, &reply.response, &b.info().fingerprint, None)
            .is_err()
    );
    assert!(da.peers().unwrap().is_empty());
}
#[test]
fn alias_conflicts_rollback_nonce_consumption_in_both_directions() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let b = identity(root.path(), "b");
    let c = identity(root.path(), "c");
    let mut da = Db::open(&root.path().join("a.db")).unwrap();
    let mut db = Db::open(&root.path().join("b.db")).unwrap();
    da.pair_peer(&c.info().peer_id, &c.info().fingerprint, Some("occupied"))
        .unwrap();
    db.pair_peer(&c.info().peer_id, &c.info().fingerprint, Some("occupied"))
        .unwrap();
    let offer = da.pairing_offer(&a, &[], 60).unwrap();
    assert!(
        db.pairing_join(
            &b,
            &offer.offer,
            &a.info().fingerprint,
            Some("occupied"),
            &[]
        )
        .is_err()
    );
    let joined = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, Some("a"), &[])
        .unwrap();
    assert!(
        da.pairing_finish(
            &a,
            &joined.response,
            &b.info().fingerprint,
            Some("occupied")
        )
        .is_err()
    );
    da.pairing_finish(&a, &joined.response, &b.info().fingerprint, Some("b"))
        .unwrap();
}
#[test]
fn tampering_every_signed_field_and_cross_domain_tokens_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let b = identity(root.path(), "b");
    let mut da = Db::open(&root.path().join("a.db")).unwrap();
    let mut db = Db::open(&root.path().join("b.db")).unwrap();
    let offer = da.pairing_offer(&a, &[], 60).unwrap();
    let joined = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, None, &[])
        .unwrap();
    for (token, is_offer) in [(&offer.offer, true), (&joined.response, false)] {
        let original: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(token).unwrap()).unwrap();
        for field in original["payload"]
            .as_object()
            .unwrap()
            .keys()
            .chain(std::iter::once(&"signature".to_string()))
        {
            let mut value = original.clone();
            if field == "signature" {
                value[field] = serde_json::json!(vec![0u8; 64]);
            } else {
                value["payload"][field] = serde_json::Value::Null;
            }
            let token = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap());
            if is_offer {
                assert!(
                    db.pairing_join(&b, &token, &a.info().fingerprint, None, &[])
                        .is_err()
                );
            } else {
                assert!(
                    da.pairing_finish(&a, &token, &b.info().fingerprint, None)
                        .is_err()
                );
            }
        }
    }
    let legacy = agent_bridge::network::invitation::create(&a, &[], 60).unwrap();
    assert!(
        db.pairing_join(&b, &legacy, &a.info().fingerprint, None, &[])
            .is_err()
    );
    assert!(
        da.pairing_finish(&a, &offer.offer, &b.info().fingerprint, None)
            .is_err()
    );
    assert!(
        db.pairing_join(&b, &"x".repeat(16385), &a.info().fingerprint, None, &[])
            .is_err()
    );
}
#[test]
fn persisted_offer_expiry_is_rechecked_and_pending_records_are_bounded() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let b = identity(root.path(), "b");
    let path = root.path().join("a.db");
    let mut da = Db::open(&path).unwrap();
    let mut db = Db::open(&root.path().join("b.db")).unwrap();
    let offer = da.pairing_offer(&a, &[], 60).unwrap();
    let reply = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, None, &[])
        .unwrap();
    let mut conn = Connection::open(&path).unwrap();
    conn.execute("UPDATE pairing_offers SET expires_at=0", [])
        .unwrap();
    assert!(
        da.pairing_finish(&a, &reply.response, &b.info().fingerprint, None)
            .is_err()
    );
    let tx = conn.transaction().unwrap();
    tx.execute("DELETE FROM pairing_offers", []).unwrap();
    for _ in 0..1024 {
        tx.execute(
            "INSERT INTO pairing_offers VALUES(?,?,?)",
            params![
                uuid::Uuid::new_v4().to_string(),
                "bounded",
                offer.expires_at
            ],
        )
        .unwrap();
    }
    tx.commit().unwrap();
    assert!(da.pairing_offer(&a, &[], 60).is_err());
    conn.execute("UPDATE pairing_offers SET expires_at=0", [])
        .unwrap();
    assert!(da.pairing_offer(&a, &[], 60).is_ok());
    assert!(da.pairing_offer(&a, &[], 29).is_err());
    assert!(da.pairing_offer(&a, &[], 601).is_err());
}

#[test]
fn exact_join_retry_recovers_original_response_after_restart_without_changing_alias_or_identity() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let b = identity(root.path(), "b");
    let c = identity(root.path(), "c");
    let mut da = Db::open(&root.path().join("a.db")).unwrap();
    let path = root.path().join("b.db");
    let mut db = Db::open(&path).unwrap();
    let offer = da.pairing_offer(&a, &[], 60).unwrap();
    let joined = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, Some("a"), &[])
        .unwrap();
    drop(db);
    let mut db = Db::open(&path).unwrap();
    let retry = db
        .pairing_join(&b, &offer.offer, &a.info().fingerprint, Some("a"), &[])
        .unwrap();
    assert_eq!(retry.response, joined.response);
    assert!(
        db.pairing_join(&b, &offer.offer, &a.info().fingerprint, Some("other"), &[])
            .is_err()
    );
    assert!(
        db.pairing_join(&c, &offer.offer, &a.info().fingerprint, None, &[])
            .is_err()
    );
    db.revoke_peer(&a.info().peer_id).unwrap();
    assert!(
        db.pairing_join(&b, &offer.offer, &a.info().fingerprint, None, &[])
            .is_err()
    );
}

#[test]
fn schema_five_upgrade_preserves_identity_policy_and_installs_empty_pairing_state() {
    let root = tempfile::tempdir().unwrap();
    let a = identity(root.path(), "a");
    let path = root.path().join("db");
    let mut db = Db::open(&path).unwrap();
    db.pair_peer(&a.info().peer_id, &a.info().fingerprint, Some("saved"))
        .unwrap();
    db.grant_peer("saved", "user", Permission::Receive, true)
        .unwrap();
    db.pause(true).unwrap();
    drop(db);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("fixtures/native-metadata-v10.sql"))
        .unwrap();
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS inbound_usage_insert; DROP TRIGGER IF EXISTS inbound_usage_delete; DROP TABLE inbound_usage; DROP TRIGGER IF EXISTS message_context_insert; DROP TABLE message_context; DROP INDEX messages_monitor_order; DROP TABLE relay_nodes; DROP TABLE pairing_offers; DROP TABLE pairing_joined; PRAGMA user_version=5;",
    )
    .unwrap();
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert!(db.paused().unwrap());
    assert_eq!(db.peer("saved").unwrap().grants.len(), 1);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM pairing_offers", [], |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM pairing_joined", [], |r| r.get(0))
            .unwrap(),
        0
    );
}
