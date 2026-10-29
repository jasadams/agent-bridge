//! Numbered, transactional migrations for both legacy and current databases.
use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior};

pub const SCHEMA_VERSION: i64 = 12;

const BASE_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS agents(name TEXT PRIMARY KEY, provider TEXT NOT NULL, session TEXT NOT NULL, endpoint TEXT NOT NULL, enabled INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS messages(id TEXT PRIMARY KEY, sender TEXT NOT NULL, recipient TEXT NOT NULL, recipient_session TEXT NOT NULL, body TEXT NOT NULL, parent TEXT, hops INTEGER NOT NULL, state TEXT NOT NULL, detail TEXT, created INTEGER NOT NULL, updated INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS messages_delivery ON messages(state,recipient,created);
CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS native_codex(session TEXT PRIMARY KEY, display_name TEXT);
CREATE TABLE IF NOT EXISTS native_claude(session TEXT PRIMARY KEY, display_name TEXT);
INSERT OR IGNORE INTO settings VALUES('paused','false');
INSERT OR IGNORE INTO agents VALUES('user','inbox','user','',1);";

const TRUST_SCHEMA: &str = "
CREATE TABLE peers(
 peer_id TEXT PRIMARY KEY NOT NULL,
 alias TEXT UNIQUE,
 revoked INTEGER NOT NULL DEFAULT 0 CHECK(revoked IN (0,1))
);
CREATE TABLE peer_grants(
 peer_id TEXT NOT NULL REFERENCES peers(peer_id) ON DELETE CASCADE,
 provider TEXT NOT NULL CHECK(provider IN ('claude','codex','inbox')),
 session TEXT NOT NULL,
 permission TEXT NOT NULL CHECK(permission IN ('discover','send','receive')),
 PRIMARY KEY(peer_id,provider,session,permission)
);";

const NETWORK_SCHEMA: &str = "
CREATE TABLE peer_addresses(peer_id TEXT PRIMARY KEY REFERENCES peers(peer_id),address TEXT NOT NULL);
CREATE TABLE remote_agents(peer_id TEXT NOT NULL REFERENCES peers(peer_id),name TEXT NOT NULL,agent TEXT NOT NULL,PRIMARY KEY(peer_id,name));
CREATE TABLE outbound_transfers(id TEXT PRIMARY KEY REFERENCES messages(id),peer_id TEXT NOT NULL REFERENCES peers(peer_id),envelope TEXT NOT NULL,state TEXT NOT NULL,detail TEXT,observed_at INTEGER,requested_sender TEXT NOT NULL,requested_recipient TEXT NOT NULL);
CREATE TABLE inbound_transfers(peer_id TEXT NOT NULL REFERENCES peers(peer_id),origin_id TEXT NOT NULL,local_id TEXT NOT NULL UNIQUE REFERENCES messages(id),envelope TEXT NOT NULL,provider TEXT NOT NULL,session TEXT NOT NULL,PRIMARY KEY(peer_id,origin_id));
CREATE TABLE peer_events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,peer_id TEXT NOT NULL REFERENCES peers(peer_id),origin_id TEXT NOT NULL,state TEXT NOT NULL,detail TEXT,updated INTEGER NOT NULL);
CREATE INDEX peer_events_lookup ON peer_events(peer_id,sequence);
CREATE TABLE peer_cursors(peer_id TEXT PRIMARY KEY REFERENCES peers(peer_id),sequence INTEGER NOT NULL DEFAULT 0);
CREATE TRIGGER inbound_status AFTER UPDATE OF state ON messages WHEN NEW.state<>OLD.state BEGIN
 INSERT INTO peer_events(peer_id,origin_id,state,detail,updated) SELECT peer_id,origin_id,NEW.state,NEW.detail,NEW.updated FROM inbound_transfers WHERE local_id=NEW.id;
END;
INSERT OR IGNORE INTO settings VALUES('network_enabled','false');
INSERT OR IGNORE INTO settings VALUES('network_listen','/ip4/127.0.0.1/udp/0/quic-v1');";

const LAN_SCHEMA: &str = "
CREATE TABLE consumed_invitations(
 peer_id TEXT NOT NULL REFERENCES peers(peer_id),
 nonce TEXT NOT NULL,
 expires_at INTEGER NOT NULL,
 PRIMARY KEY(peer_id,nonce)
);
INSERT OR IGNORE INTO settings VALUES('network_lan','false');";

pub fn migrate(conn: &mut Connection) -> Result<()> {
    // Do not change journal mode, recover messages or create tables in an unknown schema.
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        (0..=SCHEMA_VERSION).contains(&version),
        "Unsupported database schema version {version}; supported through {SCHEMA_VERSION}"
    );
    conn.execute_batch(
        "PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;",
    )?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
    ensure!(
        (0..=SCHEMA_VERSION).contains(&version),
        "Unsupported database schema version {version}"
    );
    if version < 1 {
        tx.execute_batch(BASE_SCHEMA)?;
        tx.pragma_update(None, "user_version", 1)?;
    }
    if version < 2 {
        tx.execute_batch(TRUST_SCHEMA)?;
        tx.pragma_update(None, "user_version", 2)?;
    }
    if version < 3 {
        tx.execute_batch(NETWORK_SCHEMA)?;
        tx.pragma_update(None, "user_version", 3)?;
    }
    if version < 4 {
        tx.execute_batch(LAN_SCHEMA)?;
        tx.pragma_update(None, "user_version", 4)?;
    }
    if version < 5 {
        tx.execute_batch("ALTER TABLE peers ADD COLUMN machine_name TEXT;")?;
        tx.execute(
            "INSERT OR IGNORE INTO settings(key,value) VALUES('machine_name',?)",
            [crate::network::machine::default_name()],
        )?;
        tx.pragma_update(None, "user_version", 5)?;
    }
    if version < 6 {
        tx.execute_batch("CREATE TABLE pairing_offers(nonce TEXT PRIMARY KEY,offer TEXT NOT NULL,expires_at INTEGER NOT NULL); CREATE TABLE pairing_joined(peer_id TEXT NOT NULL,nonce TEXT NOT NULL,expires_at INTEGER NOT NULL,local_peer TEXT NOT NULL,offer TEXT NOT NULL,response TEXT NOT NULL,PRIMARY KEY(peer_id,nonce));")?;
        tx.pragma_update(None, "user_version", 6)?;
    }
    if version < 7 {
        tx.execute_batch("CREATE TABLE relay_nodes(peer_id TEXT PRIMARY KEY NOT NULL,address TEXT NOT NULL UNIQUE);")?;
        tx.pragma_update(None, "user_version", 7)?;
    }
    if version < 8 {
        tx.execute_batch("ALTER TABLE relay_nodes RENAME TO relay_nodes_v7; CREATE TABLE relay_nodes(peer_id TEXT NOT NULL,transport TEXT NOT NULL CHECK(transport IN ('quic','tcp')),address TEXT NOT NULL UNIQUE,PRIMARY KEY(peer_id,transport)); INSERT INTO relay_nodes(peer_id,transport,address) SELECT peer_id,'quic',address FROM relay_nodes_v7; DROP TABLE relay_nodes_v7;")?;
        tx.pragma_update(None, "user_version", 8)?;
    }
    if version < 9 {
        tx.execute_batch("CREATE TABLE message_context(id TEXT PRIMARY KEY REFERENCES messages(id),source TEXT NOT NULL,destination TEXT NOT NULL);
CREATE INDEX messages_monitor_order ON messages(created DESC,id DESC);
CREATE TRIGGER message_context_insert AFTER INSERT ON messages BEGIN
 INSERT INTO message_context VALUES(NEW.id,
 json_object('peer_id',NULL,'machine_name',(SELECT value FROM settings WHERE key='machine_name'),'agent',NEW.sender,'provider',(SELECT provider FROM agents WHERE name=NEW.sender),'session',(SELECT session FROM agents WHERE name=NEW.sender)),
 json_object('peer_id',NULL,'machine_name',(SELECT value FROM settings WHERE key='machine_name'),'agent',NEW.recipient,'provider',(SELECT provider FROM agents WHERE name=NEW.recipient AND session=NEW.recipient_session),'session',NEW.recipient_session));
END;
INSERT INTO message_context SELECT id,json_object('peer_id',NULL,'machine_name',NULL,'agent',sender,'provider',NULL,'session',NULL),json_object('peer_id',NULL,'machine_name',NULL,'agent',recipient,'provider',NULL,'session',recipient_session) FROM messages;")?;
        tx.pragma_update(None, "user_version", 9)?;
    }
    if version < 10 {
        tx.execute_batch("CREATE TABLE inbound_usage(peer_id TEXT PRIMARY KEY REFERENCES peers(peer_id),messages INTEGER NOT NULL CHECK(messages>=0),bytes INTEGER NOT NULL CHECK(bytes>=0));
INSERT INTO inbound_usage SELECT t.peer_id,count(*),sum(length(CAST(m.body AS BLOB))+length(CAST(t.envelope AS BLOB))) FROM inbound_transfers t JOIN messages m ON m.id=t.local_id GROUP BY t.peer_id;
CREATE TRIGGER inbound_usage_insert AFTER INSERT ON inbound_transfers BEGIN
 INSERT INTO inbound_usage VALUES(NEW.peer_id,1,length(CAST(NEW.envelope AS BLOB))+(SELECT length(CAST(body AS BLOB)) FROM messages WHERE id=NEW.local_id)) ON CONFLICT(peer_id) DO UPDATE SET messages=messages+1,bytes=bytes+excluded.bytes;
END;
CREATE TRIGGER inbound_usage_delete AFTER DELETE ON inbound_transfers BEGIN
 UPDATE inbound_usage SET messages=messages-1,bytes=bytes-length(CAST(OLD.envelope AS BLOB))-(SELECT length(CAST(body AS BLOB)) FROM messages WHERE id=OLD.local_id) WHERE peer_id=OLD.peer_id;
END;")?;
        tx.pragma_update(None, "user_version", 10)?;
    }
    if version < 11 {
        tx.execute_batch("CREATE TABLE session_metadata(provider TEXT NOT NULL,session TEXT NOT NULL,display_name TEXT,PRIMARY KEY(provider,session));
INSERT INTO session_metadata SELECT 'claude',session,display_name FROM native_claude;
INSERT INTO session_metadata SELECT 'codex',session,display_name FROM native_codex;
DROP TABLE native_claude;
DROP TABLE native_codex;")?;
        tx.pragma_update(None, "user_version", 11)?;
    }
    if version < 12 {
        // SQLite cannot extend a CHECK constraint in place. Preserve every grant
        // and any explicit indexes/triggers while rebuilding the constrained table.
        let auxiliary = {
            let mut statement = tx.prepare(
                "SELECT sql FROM sqlite_schema WHERE tbl_name='peer_grants' AND type IN ('index','trigger') AND sql IS NOT NULL ORDER BY type,name",
            )?;
            statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute_batch(
            "CREATE TABLE peer_grants_v12(
 peer_id TEXT NOT NULL REFERENCES peers(peer_id) ON DELETE CASCADE,
 provider TEXT NOT NULL CHECK(provider IN ('claude','codex','inbox','opencode')),
 session TEXT NOT NULL,
 permission TEXT NOT NULL CHECK(permission IN ('discover','send','receive')),
 PRIMARY KEY(peer_id,provider,session,permission));
INSERT INTO peer_grants_v12 SELECT peer_id,provider,session,permission FROM peer_grants;
DROP TABLE peer_grants;
ALTER TABLE peer_grants_v12 RENAME TO peer_grants;",
        )?;
        for sql in auxiliary {
            tx.execute_batch(&sql)?;
        }
        tx.pragma_update(None, "user_version", 12)?;
    }
    tx.commit()?;
    Ok(())
}
