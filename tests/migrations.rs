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

fn user_version(conn: &Connection) -> i64 {
    conn.pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

#[test]
fn legacy_database_migrates_without_losing_messages_or_inbox_rows() {
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
    assert!(db.peers().unwrap().is_empty());
    drop(db);
    let conn = Connection::open(&path).unwrap();
    assert_eq!(user_version(&conn), SCHEMA_VERSION);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM agents", [], |r| r.get(0))
            .unwrap(),
        2
    );
    for dropped in ["session_metadata", "remote_agents"] {
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM sqlite_schema WHERE name=?",
                [dropped],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
    }
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
    assert_eq!(user_version(&conn), 0);
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

// Reconstitute the previous numbered schema, including native rows older
// binaries wrote, then migrate the whole chain through v20.
fn version_ten_fixture(conn: &mut Connection) {
    // Start from the current schema, then reconstruct the pre-v20 catalog and
    // version. This exercises the real v20 migration without duplicating every
    // historical migration in the fixture.
    agent_bridge::migrations::migrate(conn).unwrap();
    conn.execute_batch(
        "DROP TABLE route_aliases;
DROP TABLE claude_bindings;
DROP TABLE agents;
CREATE TABLE agents(name TEXT PRIMARY KEY,provider TEXT NOT NULL,session TEXT NOT NULL,endpoint TEXT NOT NULL,enabled INTEGER NOT NULL);
CREATE TABLE session_metadata(provider TEXT NOT NULL,session TEXT NOT NULL,display_name TEXT,PRIMARY KEY(provider,session));
CREATE TABLE remote_agents(peer_id TEXT NOT NULL REFERENCES peers(peer_id),name TEXT NOT NULL,agent TEXT NOT NULL,PRIMARY KEY(peer_id,name));
INSERT INTO session_metadata VALUES('claude','97e8d81e-073d-4cde-a017-4a8255b7b3b8','duplicate'),('codex','97e8d81e-073d-4cde-a017-4a8255b7b3b8','duplicate');
INSERT INTO agents VALUES('user','inbox','user','',1),('claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8','claude','97e8d81e-073d-4cde-a017-4a8255b7b3b8','claude.sock',1),('codex-97e8d81e-073d-4cde-a017-4a8255b7b3b8','codex','97e8d81e-073d-4cde-a017-4a8255b7b3b8','',0),('manual','claude','97e8d81e-073d-4cde-a017-4a8255b7b3b8','claude.sock',1);
INSERT INTO peers(peer_id,alias,revoked,machine_name) VALUES('fixture-peer','friend',0,'machine');
INSERT INTO peer_grants VALUES('fixture-peer','claude','97e8d81e-073d-4cde-a017-4a8255b7b3b8','send'),('fixture-peer','codex','97e8d81e-073d-4cde-a017-4a8255b7b3b8','discover');
INSERT INTO messages VALUES('pending','user','claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8','97e8d81e-073d-4cde-a017-4a8255b7b3b8','hello',NULL,0,'queued',NULL,10,10),('acked','claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8','user','user','reply','pending',0,'acknowledged',NULL,11,12);
INSERT INTO message_context VALUES('pending','{}','{}'),('acked','{}','{}');
PRAGMA user_version=19;",
    )
    .unwrap();
}

fn durable_rows(conn: &Connection) -> Vec<Vec<String>> {
    [
        "SELECT quote(id)||quote(sender)||quote(recipient)||quote(recipient_session)||quote(body)||quote(parent)||quote(state)||quote(updated) FROM messages ORDER BY id",
        "SELECT quote(peer_id)||quote(alias)||quote(revoked)||quote(machine_name) FROM peers ORDER BY peer_id",
        "SELECT quote(peer_id)||quote(provider)||quote(session)||quote(permission) FROM peer_grants ORDER BY peer_id,provider,session,permission",
        "SELECT quote(id)||quote(source)||quote(destination) FROM message_context ORDER BY id",
    ].iter().map(|sql| conn.prepare(sql).unwrap().query_map([], |r| r.get(0)).unwrap().collect::<rusqlite::Result<Vec<String>>>().unwrap()).collect()
}

#[test]
fn version_twenty_preserves_immutable_rows_and_retires_the_native_catalog() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let mut conn = Connection::open(&path).unwrap();
    version_ten_fixture(&mut conn);
    let before = durable_rows(&conn);
    for _ in 0..2 {
        agent_bridge::migrations::migrate(&mut conn).unwrap();
        assert_eq!(durable_rows(&conn), before);
    }
    // Only the manually configured Inbox row remains in agents.
    let agents: Vec<String> = conn
        .prepare("SELECT name FROM agents ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(agents, ["user"]);
    // Noncanonical explicit rows become stable aliases. Canonical provider IDs
    // are reserved and resolve without a row.
    let alias: (String, String, String) = conn
        .query_row("SELECT name,provider,session FROM route_aliases", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(
        alias,
        (
            "manual".into(),
            "claude".into(),
            "97e8d81e-073d-4cde-a017-4a8255b7b3b8".into()
        )
    );
    let binding: (String, String) = conn
        .query_row("SELECT session,endpoint FROM claude_bindings", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(
        binding,
        (
            "97e8d81e-073d-4cde-a017-4a8255b7b3b8".into(),
            "claude.sock".into()
        )
    );
    for dropped in ["session_metadata", "remote_agents"] {
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM sqlite_schema WHERE name=?",
                [dropped],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
    }
    for trigger in [
        "message_context_insert",
        "opencode_enqueue_bound",
        "opencode_enqueue_binding",
    ] {
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT count(*) FROM sqlite_schema WHERE type='trigger' AND name=?",
                [trigger],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
    }
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert_eq!(db.agent("manual").unwrap().session, alias.2);
    assert_eq!(
        db.agent("claude-97e8d81e-073d-4cde-a017-4a8255b7b3b8")
            .unwrap()
            .endpoint,
        "claude.sock"
    );
    assert!(db.agent("duplicate").is_err());
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
    };
    db.register(&agent, false).unwrap();
    let identifier = uuid::Uuid::new_v4().to_string();
    db.send_names(&identifier, "user", &agent.name, "legacy", &None)
        .unwrap();
    drop(db);
    let conn = Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE message_context SET destination=json_remove(destination,'$.provider') WHERE id=?",
        [&identifier],
    )
    .unwrap();
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS topic_push_failed_gap; DROP TABLE topic_push_work; DROP TABLE topic_push_preferences; DROP TABLE topic_mentions; DROP TABLE topic_posts; DROP TABLE topics; DROP TABLE board_lifecycle; DROP TABLE board_message_receipts; DROP TABLE codex_notices;
         DROP TABLE IF EXISTS route_aliases; DROP TABLE IF EXISTS claude_bindings; DROP TABLE agents;
         CREATE TABLE agents(name TEXT PRIMARY KEY,provider TEXT NOT NULL,session TEXT NOT NULL,endpoint TEXT NOT NULL,enabled INTEGER NOT NULL);
         INSERT INTO agents VALUES('user','inbox','user','',1),('native','codex','123e4567-e89b-12d3-a456-426614174000','',1);
         CREATE TABLE IF NOT EXISTS session_metadata(provider TEXT NOT NULL,session TEXT NOT NULL,display_name TEXT,PRIMARY KEY(provider,session));
         CREATE TABLE IF NOT EXISTS remote_agents(peer_id TEXT NOT NULL,name TEXT NOT NULL,agent TEXT NOT NULL,PRIMARY KEY(peer_id,name));
         PRAGMA user_version=13;",
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

#[test]
fn version_twenty_one_drops_only_the_removed_registration_table_by_prefix() {
    let mut conn = Connection::open_in_memory().unwrap();
    agent_bridge::migrations::migrate(&mut conn).unwrap();
    let matching = |conn: &Connection| -> Vec<String> {
        conn.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name GLOB 'opencode_l*'",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
    };
    assert!(matching(&conn).is_empty());
    // An upgraded-from-20 database carries exactly one removed registration table.
    conn.execute_batch("CREATE TABLE opencode_lx(session TEXT); PRAGMA user_version=20;")
        .unwrap();
    assert_eq!(matching(&conn), ["opencode_lx"]);
    agent_bridge::migrations::migrate(&mut conn).unwrap();
    assert!(matching(&conn).is_empty());
    conn.query_row("SELECT count(*) FROM opencode_deliveries", [], |_| Ok(()))
        .unwrap();
}
