use agent_bridge::{db::Db, migrations::SCHEMA_VERSION, protocol::Provider};
use rusqlite::Connection;

// This fixture is the unversioned schema shipped before migration support.
const LEGACY: &str = "
CREATE TABLE agents(name TEXT PRIMARY KEY,provider TEXT NOT NULL,session TEXT NOT NULL,endpoint TEXT NOT NULL,enabled INTEGER NOT NULL);
CREATE TABLE messages(id TEXT PRIMARY KEY,sender TEXT NOT NULL,recipient TEXT NOT NULL,recipient_session TEXT NOT NULL,body TEXT NOT NULL,parent TEXT,hops INTEGER NOT NULL,state TEXT NOT NULL,detail TEXT,created INTEGER NOT NULL,updated INTEGER NOT NULL);
CREATE INDEX messages_delivery ON messages(state,recipient,created);
CREATE TABLE settings(key TEXT PRIMARY KEY,value TEXT NOT NULL);
CREATE TABLE native_codex(session TEXT PRIMARY KEY,display_name TEXT);
CREATE TABLE native_claude(session TEXT PRIMARY KEY,display_name TEXT);
INSERT INTO settings VALUES('paused','true');
INSERT INTO agents VALUES('user','inbox','user','',1),('receiver','inbox','receiver-session','',1);
INSERT INTO native_codex VALUES('codex-session','A title');
INSERT INTO native_claude VALUES('claude-session','Another title');
INSERT INTO messages VALUES('queued-id','user','receiver','receiver-session','hello',NULL,0,'queued',NULL,10,10),('sending-id','user','receiver','receiver-session','in flight',NULL,0,'sending',NULL,11,11);
";

#[test]
fn legacy_database_migrates_without_losing_messages_or_registrations() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(LEGACY).unwrap();
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert!(db.paused().unwrap());
    assert_eq!(db.agent("receiver").unwrap().session, "receiver-session");
    assert_eq!(db.get("queued-id").unwrap().state, "queued");
    assert_eq!(db.get("sending-id").unwrap().state, "uncertain");
    assert_eq!(
        db.display_name(&Provider::Codex, "codex-session")
            .unwrap()
            .as_deref(),
        Some("A title")
    );
    assert_eq!(
        db.display_name(&Provider::Claude, "claude-session")
            .unwrap()
            .as_deref(),
        Some("Another title")
    );
    assert!(db.peers().unwrap().is_empty());
    drop(db);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        SCHEMA_VERSION
    );
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.history(&None, 100).unwrap().len(), 2);
}

#[test]
fn future_schema_is_rejected_before_journal_or_data_changes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(LEGACY).unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    let before = std::fs::read(&path).unwrap();
    assert!(Db::open(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value::<String, _>(None, "journal_mode", |r| r.get(0))
            .unwrap(),
        "delete"
    );
    assert_eq!(
        conn.query_row::<String, _, _>(
            "SELECT state FROM messages WHERE id='sending-id'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        "sending"
    );
}

#[test]
fn migration_failure_rolls_back_schema_and_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE peers(unexpected TEXT);")
        .unwrap();
    drop(conn);
    assert!(Db::open(&path).is_err());
    let conn = Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='agents'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
}

#[test]
fn version_one_database_adds_trust_tables_without_reinitializing_settings() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(LEGACY).unwrap();
    conn.pragma_update(None, "user_version", 1).unwrap();
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert!(db.paused().unwrap());
    assert!(db.peers().unwrap().is_empty());
    assert_eq!(db.history(&None, 100).unwrap().len(), 2);
}

// Reconstitute the previous numbered schema, including rows older binaries wrote.
fn version_ten_fixture(conn: &mut Connection) {
    agent_bridge::migrations::migrate(conn).unwrap();
    conn.execute_batch(include_str!("fixtures/native-metadata-v10.sql"))
        .unwrap();
    conn.execute_batch("INSERT INTO native_claude VALUES('97e8d81e-073d-4cde-a017-4a8255b7b3b8','duplicate'),('empty',''),('null',NULL);
INSERT INTO native_codex VALUES('97e8d81e-073d-4cde-a017-4a8255b7b3b8','duplicate'),('empty',''),('null',NULL);
INSERT INTO agents VALUES('claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8','claude','97e8d81e-073d-4cde-a017-4a8255b7b3b8','claude.sock',1),('codex-97e8d81e-073d-4cde-a017-4a8255b7b3b8','codex','97e8d81e-073d-4cde-a017-4a8255b7b3b8','',0),('manual','claude','97e8d81e-073d-4cde-a017-4a8255b7b3b8','claude.sock',1);
INSERT INTO peers(peer_id,alias,revoked,machine_name) VALUES('fixture-peer','friend',0,'machine');
INSERT INTO peer_grants VALUES('fixture-peer','claude','97e8d81e-073d-4cde-a017-4a8255b7b3b8','send'),('fixture-peer','codex','97e8d81e-073d-4cde-a017-4a8255b7b3b8','discover');
INSERT INTO messages VALUES('pending','user','claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8','97e8d81e-073d-4cde-a017-4a8255b7b3b8','hello',NULL,0,'queued',NULL,10,10),('acked','claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8','user','user','reply','pending',0,'acknowledged',NULL,11,12);
PRAGMA user_version=10;").unwrap();
}

fn durable_rows(conn: &Connection) -> Vec<Vec<String>> {
    [
        "SELECT quote(name)||quote(provider)||quote(session)||quote(endpoint)||quote(enabled) FROM agents ORDER BY name",
        "SELECT quote(id)||quote(sender)||quote(recipient)||quote(recipient_session)||quote(body)||quote(parent)||quote(state)||quote(updated) FROM messages ORDER BY id",
        "SELECT quote(peer_id)||quote(alias)||quote(revoked)||quote(machine_name) FROM peers ORDER BY peer_id",
        "SELECT quote(peer_id)||quote(provider)||quote(session)||quote(permission) FROM peer_grants ORDER BY peer_id,provider,session,permission",
        "SELECT quote(id)||quote(source)||quote(destination) FROM message_context ORDER BY id",
    ].iter().map(|sql| conn.prepare(sql).unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<Vec<String>>>().unwrap()).collect()
}

#[test]
fn version_eleven_preserves_metadata_and_all_durable_identities_on_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut conn = Connection::open(&path).unwrap();
    version_ten_fixture(&mut conn);
    let before = durable_rows(&conn);
    for _ in 0..2 {
        agent_bridge::migrations::migrate(&mut conn).unwrap();
        assert_eq!(durable_rows(&conn), before);
        for provider in ["claude", "codex"] {
            for (session, title) in [
                ("97e8d81e-073d-4cde-a017-4a8255b7b3b8", Some("duplicate")),
                ("empty", Some("")),
                ("null", None),
            ] {
                assert_eq!(
                    conn.query_row(
                        "SELECT display_name FROM session_metadata WHERE provider=? AND session=?",
                        [provider, session],
                        |r| r.get::<_, Option<String>>(0)
                    )
                    .unwrap()
                    .as_deref(),
                    title
                );
            }
        }
    }
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert_eq!(
        db.resolve_agent("duplicate").unwrap().name,
        "claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8"
    );
    assert!(db.resolve_agent("codex:duplicate").is_err());
    db.observe_session_name(
        &Provider::Claude,
        "97e8d81e-073d-4cde-a017-4a8255b7b3b8",
        Some("renamed"),
    )
    .unwrap();
    assert_eq!(
        db.resolve_agent("renamed").unwrap().name,
        "claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8"
    );
    assert!(db.resolve_agent("claude:duplicate").is_err());
    drop(db);
    assert_eq!(durable_rows(&Connection::open(path).unwrap()), before);
}

#[test]
fn metadata_copy_failure_rolls_back_prior_copy_and_all_ddl() {
    let mut conn = Connection::open_in_memory().unwrap();
    version_ten_fixture(&mut conn);
    let before = durable_rows(&conn);
    // Failure occurs after creating the shared table and copying Claude rows.
    conn.execute_batch("ALTER TABLE native_codex RENAME COLUMN display_name TO broken;")
        .unwrap();
    assert!(agent_bridge::migrations::migrate(&mut conn).is_err());
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        10
    );
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM sqlite_master WHERE name='session_metadata'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM native_claude", [], |r| r.get(0))
            .unwrap(),
        3
    );
    assert_eq!(durable_rows(&conn), before);
    conn.execute_batch("ALTER TABLE native_codex RENAME COLUMN broken TO display_name;")
        .unwrap();
    agent_bridge::migrations::migrate(&mut conn).unwrap();
    assert_eq!(durable_rows(&conn), before);
}

#[test]
fn interrupted_metadata_copy_leaves_only_the_old_registry_and_can_retry() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut conn = Connection::open(&path).unwrap();
    version_ten_fixture(&mut conn);
    // A sufficiently large copy lets sqlite3_interrupt interrupt real migration
    // work rather than simulating rollback through a test-only production hook.
    conn.execute_batch("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<100000) INSERT INTO native_codex SELECT 'bulk-'||x,'title-'||x FROM n;").unwrap();
    let before = durable_rows(&conn);
    let handle = conn.get_interrupt_handle();
    let finished = Arc::new(AtomicBool::new(false));
    let signal = finished.clone();
    let interrupter = std::thread::spawn(move || {
        while !signal.load(Ordering::Acquire) {
            std::thread::sleep(std::time::Duration::from_millis(1));
            handle.interrupt();
        }
    });
    let result = agent_bridge::migrations::migrate(&mut conn);
    finished.store(true, Ordering::Release);
    interrupter.join().unwrap();
    assert!(result.unwrap_err().to_string().contains("interrupted"));
    assert_eq!(durable_rows(&conn), before);
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |r| r.get(0))
            .unwrap(),
        10
    );
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM sqlite_master WHERE name='session_metadata'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM native_codex", [], |r| r.get(0))
            .unwrap(),
        100003
    );
    drop(conn);
    let mut conn = Connection::open(path).unwrap();
    agent_bridge::migrations::migrate(&mut conn).unwrap();
    assert_eq!(durable_rows(&conn), before);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM session_metadata", [], |r| r.get(0))
            .unwrap(),
        100006
    );
}

#[test]
fn schema_thirteen_unknown_local_codex_queue_is_blocked_without_replay_and_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let agent = agent_bridge::protocol::Agent {
        name: "native".into(),
        provider: Provider::Codex,
        session: "123e4567-e89b-12d3-a456-426614174000".into(),
        endpoint: String::new(),
        enabled: true,
    };
    db.register(&agent, false).unwrap();
    let identifier = uuid::Uuid::new_v4().to_string();
    db.send(&identifier, "user", &agent.name, "legacy", &None)
        .unwrap();
    drop(db);
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE message_context SET destination=json_remove(destination,'$.provider') WHERE id=?",
        [&identifier],
    )
    .unwrap();
    conn.execute_batch(
        "DROP TABLE board_lifecycle; DROP TABLE board_message_receipts; DROP TABLE codex_notices; PRAGMA user_version=13;",
    )
    .unwrap();
    drop(conn);
    for _ in 0..2 {
        let mut db = Db::open(&path).unwrap();
        let message = db.get(&identifier).unwrap();
        assert_eq!(message.state, "blocked");
        assert!(message.detail.unwrap().contains("unknown"));
        assert!(db.retry(&identifier, false).is_err());
        assert!(db.claim().unwrap().is_none());
    }
}
