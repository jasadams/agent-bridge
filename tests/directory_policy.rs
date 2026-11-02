use agent_bridge::{
    db::Db,
    migrations,
    protocol::{Agent, Provider},
};
use rusqlite::{Connection, params};

fn agent(name: &str, session: &str, enabled: bool) -> Agent {
    Agent {
        name: name.into(),
        provider: Provider::Codex,
        session: session.into(),
        endpoint: String::new(),
        enabled,
    }
}

fn policy_count(conn: &Connection, session: &str) -> i64 {
    conn.query_row(
        "SELECT count(*) FROM agent_owner_policy WHERE session=?",
        [session],
        |row| row.get(0),
    )
    .unwrap()
}

#[test]
fn migration_preserves_all_legacy_flags_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    drop(Db::open(&path).unwrap());
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "DROP TABLE agent_owner_policy; PRAGMA user_version=19;
        INSERT INTO agents VALUES('legacy-on','codex','old-on','',1);
        INSERT INTO agents VALUES('legacy-off','claude','old-off','',0);
        INSERT INTO agents VALUES('legacy-plugin','opencode','old-plugin','',0);",
    )
    .unwrap();
    migrations::migrate(&mut conn).unwrap();
    let rows: i64 = conn.query_row(
        "SELECT count(*) FROM agents a JOIN agent_owner_policy p USING(name,provider,session) WHERE a.enabled=p.enabled",
        [], |row| row.get(0),
    ).unwrap();
    assert_eq!(rows, 4); // Includes the pre-existing local inbox.
    conn.execute("UPDATE agents SET enabled=0 WHERE name='legacy-on'", [])
        .unwrap();
    migrations::migrate(&mut conn).unwrap();
    let db = Db::open(&path).unwrap();
    assert!(
        db.owner_policy_enabled(&Provider::Codex, "old-on", "legacy-on")
            .unwrap()
    );
    assert!(
        !db.owner_policy_enabled(&Provider::Claude, "old-off", "legacy-off")
            .unwrap()
    );
    assert!(
        !db.owner_policy_enabled(&Provider::OpenCode, "old-plugin", "legacy-plugin")
            .unwrap()
    );
    assert_eq!(policy_count(&conn, "old-on"), 1);
}

#[test]
fn native_observation_and_offline_are_not_owner_intent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let canonical = format!("codex-{session}");
    db.observe_codex(&session, Some("first"), true).unwrap();
    assert_eq!(policy_count(&conn, &session), 0);
    db.codex_offline(Some(&session)).unwrap();
    assert!(!db.agent(&canonical).unwrap().enabled);
    assert!(
        db.owner_policy_enabled(&Provider::Codex, &session, &canonical)
            .unwrap()
    );
    db.observe_codex(&session, Some("renamed"), true).unwrap();
    assert!(db.agent(&canonical).unwrap().enabled);
    assert_eq!(policy_count(&conn, &session), 0);
    db.unregister(&canonical, &session).unwrap();
    db.observe_codex(&session, None, true).unwrap();
    assert!(!db.agent(&canonical).unwrap().enabled);
    assert!(
        !db.owner_policy_enabled(&Provider::Codex, &session, &canonical)
            .unwrap()
    );
    assert_eq!(policy_count(&conn, &session), 1);
}

#[test]
fn alias_precedence_and_replacement_are_exact_identity_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let canonical = format!("codex-{session}");
    db.register(&agent("denied", &session, false), false)
        .unwrap();
    assert!(
        !db.owner_policy_enabled(&Provider::Codex, &session, &canonical)
            .unwrap()
    );
    db.observe_codex(&session, None, true).unwrap();
    assert!(!db.agent(&canonical).unwrap().enabled);
    db.register(&agent("allowed", &session, true), false)
        .unwrap();
    assert!(
        db.owner_policy_enabled(&Provider::Codex, &session, &canonical)
            .unwrap()
    );
    assert!(
        !db.owner_policy_enabled(&Provider::Codex, &session, "denied")
            .unwrap()
    );
    db.observe_codex(&session, None, true).unwrap();
    assert!(db.agent(&canonical).unwrap().enabled);
    db.unregister(&canonical, &session).unwrap();
    assert!(
        !db.owner_policy_enabled(&Provider::Codex, &session, &canonical)
            .unwrap()
    );
    assert!(
        db.owner_policy_enabled(&Provider::Codex, &session, "allowed")
            .unwrap()
    );
    db.observe_codex(&session, None, true).unwrap();
    assert!(!db.agent(&canonical).unwrap().enabled);

    let replacement = uuid::Uuid::new_v4().to_string();
    db.register(&agent("denied", &replacement, true), true)
        .unwrap();
    assert!(
        db.owner_policy_enabled(&Provider::Codex, &replacement, "denied")
            .unwrap()
    );
    assert!(
        !db.owner_policy_enabled(&Provider::Codex, &session, "denied")
            .unwrap()
    );
    db.unregister("denied", &session).unwrap(); // Stale UUID cannot disable replacement.
    assert!(db.agent("denied").unwrap().enabled);
    assert!(
        db.owner_policy_enabled(&Provider::Codex, &replacement, "denied")
            .unwrap()
    );
    assert!(
        db.owner_policy_enabled(&Provider::Claude, &session, "denied")
            .unwrap()
    );
}

#[test]
fn stale_unregister_does_not_create_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    db.observe_codex(&session, None, true).unwrap();
    let stale = uuid::Uuid::new_v4().to_string();
    db.unregister(&format!("codex-{session}"), &stale).unwrap();
    db.unregister("unknown", &stale).unwrap();
    let conn = Connection::open(&path).unwrap();
    assert_eq!(policy_count(&conn, &session), 0);
    assert_eq!(policy_count(&conn, &stale), 0);
}

#[test]
fn sibling_owner_denial_blocks_queued_canonical_without_native_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let mut db = Db::open(&dir.path().join("db")).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let canonical = format!("codex-{session}");
    db.register(&agent("allowed", &session, true), false)
        .unwrap();
    db.observe_codex(&session, None, true).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    db.send(&id, "user", &canonical, "queued before denial", &None)
        .unwrap();
    db.unregister("allowed", &session).unwrap();
    assert!(
        !db.owner_policy_enabled(&Provider::Codex, &session, &canonical)
            .unwrap()
    );
    assert!(
        db.claim().unwrap().is_none(),
        "owner denial must block queued canonical delivery without another native refresh"
    );
    assert!(!db.agent(&canonical).unwrap().enabled);
    assert_eq!(db.get(&id).unwrap().state, "queued");
    // Permissive policy is not fresh proof of delivery availability.
    db.register(&agent("allowed", &session, true), false)
        .unwrap();
    assert!(!db.agent(&canonical).unwrap().enabled);
    db.observe_codex(&session, None, true).unwrap();
    assert!(db.agent(&canonical).unwrap().enabled);
    // Register(enabled=false) must clamp the same implicit canonical route too.
    db.register(&agent("allowed", &session, false), false)
        .unwrap();
    assert!(!db.agent(&canonical).unwrap().enabled);
    assert!(db.claim().unwrap().is_none());
}

#[test]
fn explicitly_registered_false_survives_native_refresh_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let session = uuid::Uuid::new_v4().to_string();
    let canonical = format!("codex-{session}");
    let mut db = Db::open(&path).unwrap();
    db.register(&agent(&canonical, &session, false), false)
        .unwrap();
    db.observe_codex(&session, None, true).unwrap();
    assert!(!db.agent(&canonical).unwrap().enabled);
    drop(db);
    let mut db = Db::open(&path).unwrap();
    db.observe_codex(&session, None, true).unwrap();
    assert!(!db.agent(&canonical).unwrap().enabled);
    db.register(&agent(&canonical, &session, true), false)
        .unwrap();
    db.codex_offline(Some(&session)).unwrap();
    db.observe_codex(&session, None, true).unwrap();
    assert!(db.agent(&canonical).unwrap().enabled);
}

#[test]
fn policy_failure_rolls_back_registration_and_unregister() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let conn = Connection::open(&path).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    db.register(&agent("alias", &session, true), false).unwrap();
    let message_id = uuid::Uuid::new_v4().to_string();
    db.send(&message_id, "user", "alias", "keep queued", &None)
        .unwrap();
    conn.execute_batch("CREATE TRIGGER reject_policy BEFORE INSERT ON agent_owner_policy BEGIN SELECT RAISE(ABORT,'policy failure'); END;").unwrap();
    let replacement = uuid::Uuid::new_v4().to_string();
    assert!(
        db.register(&agent("alias", &replacement, true), true)
            .is_err()
    );
    assert_eq!(db.agent("alias").unwrap().session, session);
    assert_eq!(db.get(&message_id).unwrap().state, "queued");
    assert_eq!(policy_count(&conn, &replacement), 0);
    assert!(db.unregister("alias", &session).is_err());
    assert!(db.agent("alias").unwrap().enabled);
    assert!(
        db.owner_policy_enabled(&Provider::Codex, &session, "alias")
            .unwrap()
    );
    let grants: i64 = conn
        .query_row(
            "SELECT count(*) FROM peer_grants WHERE session IN (?1,?2)",
            params![session, replacement],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(grants, 0);
}

#[test]
fn failed_migration_rolls_back_schema_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    drop(Db::open(&path).unwrap());
    let mut conn = Connection::open(&path).unwrap();
    conn.execute_batch("DROP TABLE agent_owner_policy; PRAGMA user_version=19; INSERT INTO agents VALUES('invalid','codex','legacy','',2);").unwrap();
    assert!(migrations::migrate(&mut conn).is_err());
    let version: i64 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 19);
    let tables: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='agent_owner_policy'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
}
