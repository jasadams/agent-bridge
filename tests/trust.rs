use agent_bridge::{
    db::Db,
    network::trust::Permission,
    protocol::{Agent, Provider},
};

fn peer_id() -> String {
    libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string()
}

fn agent(name: &str, session: &str) -> Agent {
    Agent {
        name: name.into(),
        provider: Provider::Inbox,
        session: session.into(),
        endpoint: String::new(),
    }
}

#[test]
fn grants_are_directional_pinned_and_revoked_without_cache() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    db.register(&agent("reviewer", "original"), false).unwrap();
    let peer = peer_id();
    db.pair_peer(&peer, &peer, Some("workstation")).unwrap();
    assert!(
        !db.peer_allowed(&peer, &Provider::Inbox, "original", Permission::Receive)
            .unwrap()
    );
    let granted = db
        .grant_peer("workstation", "reviewer", Permission::Receive, true)
        .unwrap();
    assert_eq!(granted.grants.len(), 1);
    assert!(
        !db.peer_allowed(
            "workstation",
            &Provider::Inbox,
            "original",
            Permission::Receive
        )
        .unwrap()
    );
    assert!(
        db.peer_allowed(&peer, &Provider::Inbox, "original", Permission::Receive)
            .unwrap()
    );
    assert!(
        !db.peer_allowed(&peer, &Provider::Inbox, "original", Permission::Send)
            .unwrap()
    );
    assert!(
        !db.peer_allowed(&peer, &Provider::Inbox, "original", Permission::Discover)
            .unwrap()
    );
    assert!(
        !db.peer_allowed(&peer, &Provider::Codex, "original", Permission::Receive)
            .unwrap()
    );
    assert!(
        !db.peer_allowed(
            &peer_id(),
            &Provider::Inbox,
            "original",
            Permission::Receive
        )
        .unwrap()
    );
    db.register(&agent("reviewer", "replacement"), true)
        .unwrap();
    assert!(
        !db.peer_allowed(&peer, &Provider::Inbox, "replacement", Permission::Receive)
            .unwrap()
    );
    assert!(
        db.peer_allowed(&peer, &Provider::Inbox, "original", Permission::Receive)
            .unwrap()
    );
    db.grant_peer(&peer, "reviewer", Permission::Send, true)
        .unwrap();
    drop(db);
    let mut db = Db::open(&path).unwrap();
    assert!(
        db.peer_allowed(&peer, &Provider::Inbox, "replacement", Permission::Send)
            .unwrap()
    );
    db.revoke_peer("workstation").unwrap();
    assert!(
        !db.peer_allowed(&peer, &Provider::Inbox, "replacement", Permission::Send)
            .unwrap()
    );
    assert!(db.peer(&peer).unwrap().revoked);
    assert!(db.peer(&peer).unwrap().grants.is_empty());
    assert!(
        db.grant_peer(&peer, "reviewer", Permission::Send, true)
            .is_err()
    );
    db.pair_peer(&peer, &peer, None).unwrap();
    assert!(!db.peer(&peer).unwrap().revoked);
    assert!(db.peer(&peer).unwrap().grants.is_empty());
}

#[test]
fn pairing_rejects_fingerprint_mismatch_and_alias_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    let first = peer_id();
    let second = peer_id();
    assert!(db.pair_peer(&first, &second, Some("workstation")).is_err());
    assert!(db.pair_peer("invalid", "invalid", None).is_err());
    assert!(db.peers().unwrap().is_empty());
    db.pair_peer(&first, &first, Some("workstation")).unwrap();
    assert!(db.pair_peer(&second, &second, Some("workstation")).is_err());
    assert!(db.pair_peer(&second, &second, Some(&first)).is_err());
    assert_eq!(db.peers().unwrap().len(), 1);
    assert_eq!(db.peer("workstation").unwrap().peer_id, first);
    db.grant_peer(&first, "user", Permission::Discover, true)
        .unwrap();
    db.pair_peer(&first, &first, None).unwrap();
    assert_eq!(db.peer(&first).unwrap().grants.len(), 1);
    db.grant_peer(&first, "user", Permission::Discover, false)
        .unwrap();
    assert!(db.peer(&first).unwrap().grants.is_empty());
}

#[test]
fn unregistering_a_route_keeps_saved_grants_until_explicit_policy_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    db.register(&agent("reviewer", "session"), false).unwrap();
    let peer = peer_id();
    db.pair_peer(&peer, &peer, None).unwrap();
    db.grant_peer(&peer, "reviewer", Permission::Receive, true)
        .unwrap();
    db.unregister("reviewer", "session").unwrap();
    assert!(
        db.peer_allowed(&peer, &Provider::Inbox, "session", Permission::Receive)
            .unwrap()
    );
    db.grant_peer(&peer, "reviewer", Permission::Send, true)
        .unwrap();
    db.grant_peer(&peer, "reviewer", Permission::Receive, false)
        .unwrap();
}
