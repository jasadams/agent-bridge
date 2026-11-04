use agent_bridge::{
    db::Db,
    network::{identity::Identity, machine, trust::Permission},
    protocol::{Agent, Provider},
};
use rusqlite::Connection;
use std::{fs, os::unix::fs::PermissionsExt};

fn id() -> String {
    libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string()
}

#[test]
fn machine_name_defaults_once_persists_and_rejects_unsafe_names() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let db = Db::open(&path).unwrap();
    assert_eq!(db.machine_name().unwrap(), machine::default_name());
    db.set_machine_name("machine-b").unwrap();
    for invalid in [
        "",
        ".",
        "..",
        "work machine",
        "a/b",
        "machine-b\n",
        "machine-b\x1b[0m",
        "\u{202e}machine-b",
    ] {
        assert!(
            db.set_machine_name(invalid).is_err(),
            "accepted {invalid:?}"
        );
    }
    assert!(db.set_machine_name(&"a".repeat(101)).is_err());
    assert_eq!(db.machine_name().unwrap(), "machine-b");
    drop(db);
    assert_eq!(
        Db::open(&path).unwrap().machine_name().unwrap(),
        "machine-b"
    );
}

#[test]
fn authenticated_names_never_change_aliases_or_grants_and_require_current_trust() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    let peer = id();
    assert!(db.remember_peer_name(&peer, "machine-b").is_err());
    db.pair_peer(&peer, &peer, Some("my-server")).unwrap();
    db.grant_peer(&peer, "user", Permission::Receive, true)
        .unwrap();
    let grants = serde_json::to_value(db.peer(&peer).unwrap().grants).unwrap();
    db.remember_peer_name(&peer, "machine-b").unwrap();
    db.remember_peer_name(&peer, "renamed-machine-b").unwrap();
    assert!(db.remember_peer_name("my-server", "spoof").is_err());
    let record = db.peer(&peer).unwrap();
    assert_eq!(record.alias.as_deref(), Some("my-server"));
    assert_eq!(record.display_name, "my-server");
    assert_eq!(record.machine_name.as_deref(), Some("renamed-machine-b"));
    assert_eq!(serde_json::to_value(record.grants).unwrap(), grants);
    assert_eq!(db.suggest_peer_alias(&peer, "other").unwrap(), "my-server");
    db.revoke_peer(&peer).unwrap();
    assert!(db.remember_peer_name(&peer, "spoof").is_err());
    assert!(db.peer(&peer).unwrap().revoked);
}

#[test]
fn duplicate_names_are_disambiguated_and_alias_suggestions_are_collision_safe() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    let first = id();
    let second = id();
    db.pair_peer(&first, &first, None).unwrap();
    db.pair_peer(&second, &second, None).unwrap();
    db.remember_peer_name(&first, "machine-b").unwrap();
    db.remember_peer_name(&second, "machine-b").unwrap();
    let a = db.peer(&first).unwrap();
    let b = db.peer(&second).unwrap();
    assert!(a.display_name.starts_with("machine-b ["));
    assert!(b.display_name.starts_with("machine-b ["));
    assert_ne!(a.display_name, b.display_name);
    let suggestion = db.suggest_peer_alias(&second, "machine-b").unwrap();
    assert!(suggestion.starts_with("machine-b-"));
    assert!(second.ends_with(suggestion.strip_prefix("machine-b-").unwrap()));
    db.pair_peer(&second, &second, Some(&suggestion)).unwrap();
    assert_eq!(db.peer(&second).unwrap().display_name, suggestion);
    assert!(db.peer(&first).unwrap().alias.is_none());
    let third = id();
    assert_ne!(db.suggest_peer_alias(&third, &first).unwrap(), first);
    let long_name = "a".repeat(100);
    let owner = id();
    db.pair_peer(&owner, &owner, Some(&long_name)).unwrap();
    let suggested = db.suggest_peer_alias(&third, &long_name).unwrap();
    assert!(suggested.len() <= 100);
    let occupied = id();
    db.pair_peer(&occupied, &occupied, Some(&suggested))
        .unwrap();
    let alternate = db.suggest_peer_alias(&third, &long_name).unwrap();
    assert_ne!(alternate, suggested);
    assert!(alternate.len() <= 100);
}

#[test]
fn renames_preserve_identity_immutable_queued_envelopes_and_remote_routing() {
    let dir = tempfile::tempdir().unwrap();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let identity = Identity::load_or_create(dir.path()).unwrap().info();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let peer = id();
    db.pair_peer(&peer, &peer, Some("workstation")).unwrap();
    db.grant_peer(&peer, "user", Permission::Send, true)
        .unwrap();
    db.remember_peer_name(&peer, "machine-b").unwrap();
    db.register(
        &Agent {
            name: "reviewer".into(),
            provider: Provider::Inbox,
            session: "pinned".into(),
            endpoint: String::new(),
        },
        false,
    )
    .unwrap();
    let mid = uuid::Uuid::new_v4().to_string();
    db.send_remote_names(
        &identity.peer_id,
        &mid,
        "user",
        "workstation/reviewer",
        "hello",
        &None,
    )
    .unwrap();
    let conn = Connection::open(&path).unwrap();
    let before: String = conn
        .query_row(
            "SELECT envelope FROM outbound_transfers WHERE id=?",
            [&mid],
            |r| r.get(0),
        )
        .unwrap();
    db.set_machine_name("renamed-local").unwrap();
    db.remember_peer_name(&peer, "renamed-remote").unwrap();
    let after: String = conn
        .query_row(
            "SELECT envelope FROM outbound_transfers WHERE id=?",
            [&mid],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        Identity::load_or_create(dir.path()).unwrap().info(),
        identity
    );
    assert_eq!(db.get(&mid).unwrap().recipient, format!("{peer}/reviewer"));
    assert_eq!(db.get(&mid).unwrap().recipient_session, "pinned");
}

#[test]
fn schema_four_upgrade_preserves_data_and_initializes_only_missing_name() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    db.register(
        &Agent {
            name: "receiver".into(),
            provider: Provider::Inbox,
            session: "same-session".into(),
            endpoint: String::new(),
        },
        false,
    )
    .unwrap();
    let mid = uuid::Uuid::new_v4().to_string();
    db.send_names(&mid, "user", "receiver", "preserved", &None)
        .unwrap();
    let peer = id();
    db.pair_peer(&peer, &peer, Some("chosen-alias")).unwrap();
    db.grant_peer(&peer, "user", Permission::Send, true)
        .unwrap();
    drop(db);
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(include_str!("fixtures/native-metadata-v10.sql"))
        .unwrap();
    conn.execute_batch("DROP TRIGGER IF EXISTS inbound_usage_insert; DROP TRIGGER IF EXISTS inbound_usage_delete; DROP TABLE inbound_usage; DROP TRIGGER IF EXISTS message_context_insert; DROP TABLE message_context; DROP INDEX messages_monitor_order; DROP TABLE relay_nodes; DROP TABLE pairing_offers; DROP TABLE pairing_joined; ALTER TABLE peers DROP COLUMN machine_name; DELETE FROM settings WHERE key='machine_name'; PRAGMA user_version=4;").unwrap();
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.machine_name().unwrap(), machine::default_name());
    assert_eq!(db.get(&mid).unwrap().body, "preserved");
    assert_eq!(db.get(&mid).unwrap().state, "queued");
    assert_eq!(
        db.peer(&peer).unwrap().alias.as_deref(),
        Some("chosen-alias")
    );
    assert_eq!(db.peer(&peer).unwrap().grants.len(), 1);
    assert!(db.peer(&peer).unwrap().machine_name.is_none());
}
