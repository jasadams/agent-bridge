use agent_bridge::{
    db::Db,
    network::{
        trust::Permission,
        wire::{Envelope, RemoteAgent, supported_providers},
    },
    protocol::{Agent, Provider},
};

const SESSION: &str = "ses_0123456789abcdefghijklmnop";
const OTHER: &str = "ses_abcdefghijklmnopqrstuvwxyz";
fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn peer() -> String {
    libp2p_identity::Keypair::generate_ed25519()
        .public()
        .to_peer_id()
        .to_string()
}
fn agent(session: &str) -> Agent {
    Agent {
        name: "native".into(),
        provider: Provider::OpenCode,
        session: session.into(),
        endpoint: "/unused/plugin.sock".into(),
        enabled: true,
    }
}
// Store contracts inject liveness records directly; native endpoint/probe contracts
// are exercised separately by the real-socket two-broker tests.
fn fixture_lease(path: &std::path::Path, session: &str) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute(
        "INSERT INTO opencode_leases VALUES(?1,?2,?3,'/unused/plugin.sock',?4,1) ON CONFLICT(session) DO UPDATE SET generation=excluded.generation,instance=excluded.instance,expires=excluded.expires,ready=excluded.ready",
        rusqlite::params![
            session,
            id(),
            id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as i64
                + 20_000
        ],
    )
    .unwrap();
}
fn remote(session: &str) -> RemoteAgent {
    RemoteAgent {
        name: "native".into(),
        provider: Provider::OpenCode,
        session: session.into(),
    }
}
struct Pair {
    _root: tempfile::TempDir,
    a: Db,
    b: Db,
    aid: String,
    bid: String,
}
impl Pair {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let mut a = Db::open(&root.path().join("a")).unwrap();
        let mut b = Db::open(&root.path().join("b")).unwrap();
        let aid = peer();
        let bid = peer();
        a.pair_peer(&bid, &bid, Some("bob")).unwrap();
        b.pair_peer(&aid, &aid, Some("alice")).unwrap();
        fixture_lease(&root.path().join("a"), SESSION);
        fixture_lease(&root.path().join("b"), OTHER);
        a.register(&agent(SESSION), false).unwrap();
        b.register(&agent(OTHER), false).unwrap();
        a.replace_remote_agents(&bid, &[remote(OTHER)]).unwrap();
        b.replace_remote_agents(&aid, &[remote(SESSION)]).unwrap();
        Self {
            _root: root,
            a,
            b,
            aid,
            bid,
        }
    }
    fn share(&mut self) {
        self.a
            .share_peer_session(&self.bid, "native", true)
            .unwrap();
        self.b
            .share_peer_session(&self.aid, "native", true)
            .unwrap();
    }
    fn send(&self) -> Envelope {
        let mid = id();
        self.a
            .send_remote(&self.aid, &mid, "native", "bob/native", "question", &None)
            .unwrap();
        self.a
            .pending_transfers(&self.bid, 100)
            .unwrap()
            .into_iter()
            .find(|e| e.id == mid)
            .unwrap()
    }
}

#[test]
fn sharing_is_explicit_provider_and_session_scoped_and_legacy_export_stays_filtered() {
    let mut p = Pair::new();
    assert!(
        p.a.export_agents_supported(&p.bid, &supported_providers())
            .unwrap()
            .is_empty()
    );
    assert!(
        p.a.send_remote(&p.aid, &id(), "native", "bob/native", "private", &None)
            .is_err()
    );
    p.a.grant_peer(&p.bid, "native", Permission::Discover, true)
        .unwrap();
    assert_eq!(
        p.a.export_agents_supported(&p.bid, &supported_providers())
            .unwrap(),
        vec![remote(SESSION)]
    );
    assert!(p.a.export_agents(&p.bid).unwrap().is_empty());
    assert!(
        p.a.send_remote(&p.aid, &id(), "native", "bob/native", "private", &None)
            .is_err()
    );
    p.share();
    for permission in [Permission::Discover, Permission::Send, Permission::Receive] {
        assert!(
            p.a.peer_allowed(&p.bid, &Provider::OpenCode, SESSION, permission)
                .unwrap()
        );
        assert!(
            !p.a.peer_allowed(&p.bid, &Provider::OpenCode, OTHER, permission)
                .unwrap()
        );
        assert!(
            !p.a.peer_allowed(&p.bid, &Provider::Inbox, SESSION, permission)
                .unwrap()
        );
    }
    let envelope = p.send();
    p.b.share_peer_session(&p.aid, "native", false).unwrap();
    assert!(p.b.accept_remote(&p.bid, &p.aid, &envelope).is_err());
    p.b.share_peer_session(&p.aid, "native", true).unwrap();
    p.b.accept_remote(&p.bid, &p.aid, &envelope).unwrap();
}

#[test]
fn title_rename_disable_and_replacement_never_move_grants_or_queued_destination() {
    let mut p = Pair::new();
    p.share();
    p.a.observe_session_name(&Provider::OpenCode, SESSION, Some("Renamed"))
        .unwrap();
    assert_eq!(
        p.a.export_agents_supported(&p.bid, &supported_providers())
            .unwrap(),
        vec![remote(SESSION)]
    );
    let envelope = p.send();
    p.b.accept_remote(&p.bid, &p.aid, &envelope).unwrap();
    let mut disabled = agent(OTHER);
    disabled.enabled = false;
    p.b.register(&disabled, false).unwrap();
    assert!(p.b.share_peer_session(&p.aid, "native", true).is_err());
    assert!(p.b.claim().unwrap().is_none());
    p.b.share_peer_session(&p.aid, "native", false).unwrap();
    p.b.register(&agent(SESSION), true).unwrap();
    assert!(
        !p.b.peer_allowed(&p.aid, &Provider::OpenCode, SESSION, Permission::Receive)
            .unwrap()
    );
    assert!(p.b.claim().unwrap().is_none());
    let mut fresh = envelope;
    fresh.id = id();
    assert!(p.b.accept_remote(&p.bid, &p.aid, &fresh).is_err());
    assert_eq!(p.b.history(&None, 10).unwrap().len(), 1);
}

#[test]
fn replies_pin_native_identity_without_a_directory_and_reject_replaced_sources() {
    let mut p = Pair::new();
    p.share();
    let envelope = p.send();
    p.b.accept_remote(&p.bid, &p.aid, &envelope).unwrap();
    let parent = p.b.history(&None, 1).unwrap()[0].id.clone();
    p.b.replace_remote_agents(&p.aid, &[]).unwrap();
    p.b.send_remote(
        &p.bid,
        &id(),
        "native",
        "alice/native",
        "answer",
        &Some(parent.clone()),
    )
    .unwrap();
    let reply = p.b.pending_transfers(&p.aid, 1).unwrap().pop().unwrap();
    assert_eq!(reply.destination, remote(SESSION));
    assert_eq!(reply.source, remote(OTHER));
    p.a.accept_remote(&p.aid, &p.bid, &reply).unwrap();
    assert!(
        p.a.history(&None, 10)
            .unwrap()
            .iter()
            .any(|m| m.parent.as_deref() == Some(envelope.id.as_str()))
    );
    let mut forged = reply;
    forged.id = id();
    forged.source.session = SESSION.into();
    assert!(p.a.accept_remote(&p.aid, &p.bid, &forged).is_err());
    p.b.register(&agent(SESSION), true).unwrap();
    p.b.share_peer_session(&p.aid, "native", true).unwrap();
    assert!(
        p.b.send_remote(
            &p.bid,
            &id(),
            "native",
            "alice/native",
            "wrong session",
            &Some(parent)
        )
        .is_err()
    );
}

#[test]
fn reply_cannot_cross_provider_after_name_and_native_id_are_reused() {
    let mut p = Pair::new();
    p.share();
    let envelope = p.send();
    p.b.accept_remote(&p.bid, &p.aid, &envelope).unwrap();
    let parent = p.b.history(&None, 1).unwrap()[0].id.clone();
    let replacement = Agent {
        provider: Provider::Inbox,
        ..agent(OTHER)
    };
    p.b.register(&replacement, true).unwrap();
    p.b.share_peer_session(&p.aid, "native", true).unwrap();
    assert!(
        p.b.send_remote(
            &p.bid,
            &id(),
            "native",
            "alice/native",
            "wrong provider",
            &Some(parent)
        )
        .is_err()
    );
    assert!(p.b.pending_transfers(&p.aid, 100).unwrap().is_empty());
}

#[test]
fn malformed_directories_and_envelopes_are_rejected_atomically() {
    let mut p = Pair::new();
    p.share();
    let envelope = p.send();
    for invalid in [
        "",
        "ses_short",
        "ses_0123456789abcdefghijklmno_",
        "ses_0123456789abcdefghijklmnop\n",
        "ses_é123456789abcdefghijklmnop",
    ] {
        assert!(
            p.a.replace_remote_agents(&p.bid, &[remote(OTHER), remote(invalid)])
                .is_err()
        );
        for source in [true, false] {
            let mut bad = envelope.clone();
            bad.id = id();
            if source {
                bad.source.session = invalid.into();
            } else {
                bad.destination.session = invalid.into();
            }
            assert!(p.b.accept_remote(&p.bid, &p.aid, &bad).is_err());
        }
        assert!(
            p.a.unshare_peer_session(&p.bid, &Provider::OpenCode, invalid)
                .is_err()
        );
    }
    // Failed refresh cannot destroy the previously permitted, exact destination.
    assert_eq!(p.send().destination, remote(OTHER));
    let mut wrong_provider = envelope.clone();
    wrong_provider.id = id();
    wrong_provider.source.provider = Provider::Claude;
    assert!(p.b.accept_remote(&p.bid, &p.aid, &wrong_provider).is_err());
    assert!(p.b.history(&None, 10).unwrap().is_empty());
}

// Strict provider/agent/directory schema pinned to pre-OpenCode commit 8fde538.
// Keeping an enum here is essential: a generic JSON fixture would miss the break.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyProvider {
    Claude,
    Codex,
    Inbox,
}
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyAgent {
    name: String,
    provider: LegacyProvider,
    session: String,
}
#[derive(Debug, serde::Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum LegacyDirectory {
    Directory { agents: Vec<LegacyAgent> },
}

#[test]
fn pre_opencode_strict_directory_reader_keeps_all_supported_clients_discoverable() {
    let mut p = Pair::new();
    p.share();
    for provider in [Provider::Claude, Provider::Codex] {
        let native = Agent {
            name: provider.label().into(),
            provider,
            session: id(),
            endpoint: String::new(),
            enabled: true,
        };
        p.a.register(&native, false).unwrap();
        p.a.share_peer_session(&p.bid, &native.name, true).unwrap();
    }
    p.a.share_peer_session(&p.bid, "user", true).unwrap();
    let all =
        p.a.export_agents_supported(&p.bid, &supported_providers())
            .unwrap();
    assert_eq!(all.len(), 4);
    let unfiltered =
        serde_json::to_value(agent_bridge::network::wire::WireResponse::Directory { agents: all })
            .unwrap();
    assert!(serde_json::from_value::<LegacyDirectory>(unfiltered).is_err());
    let filtered = serde_json::to_value(agent_bridge::network::wire::WireResponse::Directory {
        agents: p.a.export_agents(&p.bid).unwrap(),
    })
    .unwrap();
    let LegacyDirectory::Directory { agents } = serde_json::from_value(filtered).unwrap();
    assert_eq!(agents.len(), 3);
    for agent in agents {
        match agent.provider {
            LegacyProvider::Claude | LegacyProvider::Codex => {
                uuid::Uuid::parse_str(&agent.session).unwrap();
                assert!(matches!(agent.name.as_str(), "claude" | "codex"));
            }
            LegacyProvider::Inbox => {
                assert_eq!(agent.name, "user");
                assert_eq!(agent.session, "user");
            }
        }
    }
    assert!(
        serde_json::from_str::<RemoteAgent>(
            r#"{"name":"unknown","provider":"future","session":"unvalidated"}"#
        )
        .is_err()
    );
}

#[test]
fn invalid_persisted_identity_cannot_gain_or_remove_session_grants() {
    let mut p = Pair::new();
    let conn = rusqlite::Connection::open(p._root.path().join("a")).unwrap();
    for invalid in [
        "ses_short",
        "ses_0123456789abcdefghijklmno_",
        "not-a-session",
    ] {
        // Simulate untrusted historical metadata; registration itself rejects it.
        conn.execute("UPDATE agents SET session=? WHERE name='native'", [invalid])
            .unwrap();
        for permission in [Permission::Discover, Permission::Send, Permission::Receive] {
            for allow in [true, false] {
                assert!(p.a.grant_peer(&p.bid, "native", permission, allow).is_err());
            }
        }
        for shared in [true, false] {
            assert!(p.a.share_peer_session(&p.bid, "native", shared).is_err());
        }
        assert!(p.a.peer(&p.bid).unwrap().grants.is_empty());
    }
}

fn schema_eleven_grants(path: &std::path::Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(path).unwrap();
    // Recreate the exact pre-OpenCode grant constraint and numbered schema.
    conn.execute_batch(
        "DROP TABLE board_lifecycle; DROP TABLE board_message_receipts; DROP TABLE codex_notices; DROP TRIGGER opencode_enqueue_bound;
DROP TRIGGER opencode_retry_bound;
DROP TRIGGER opencode_enqueue_binding;
DROP TABLE opencode_deliveries;
DROP TABLE opencode_leases;
ALTER TABLE peer_grants RENAME TO upgraded_grants;
CREATE TABLE peer_grants(
 peer_id TEXT NOT NULL REFERENCES peers(peer_id) ON DELETE CASCADE,
 provider TEXT NOT NULL CHECK(provider IN ('claude','codex','inbox')),
 session TEXT NOT NULL,
 permission TEXT NOT NULL CHECK(permission IN ('discover','send','receive')),
 PRIMARY KEY(peer_id,provider,session,permission));
INSERT INTO peer_grants SELECT * FROM upgraded_grants;
DROP TABLE upgraded_grants;
CREATE INDEX fixture_grants_session ON peer_grants(provider,session);
CREATE TABLE grant_audit(permission TEXT);
CREATE TRIGGER fixture_grant_audit AFTER INSERT ON peer_grants BEGIN INSERT INTO grant_audit VALUES(NEW.permission); END;
PRAGMA user_version=11;",
    )
    .unwrap();
    conn
}

#[test]
fn schema_eleven_grants_upgrade_without_loss_and_reopen_idempotently() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let mut db = Db::open(&path).unwrap();
    let paired = peer();
    db.pair_peer(&paired, &paired, None).unwrap();
    db.share_peer_session(&paired, "user", true).unwrap();
    db.register(&agent(SESSION), false).unwrap();
    drop(db);
    let conn = schema_eleven_grants(&path);
    drop(conn);
    for _ in 0..2 {
        let mut db = Db::open(&path).unwrap();
        for permission in [Permission::Discover, Permission::Send, Permission::Receive] {
            assert!(
                db.peer_allowed(&paired, &Provider::Inbox, "user", permission)
                    .unwrap()
            );
        }
        // Reopening a database deliberately does not prove a conversation is
        // still open. Inject a fresh verified-live fixture generation before
        // exercising grant persistence; real probes are covered by network tests.
        assert!(!db.agent("native").unwrap().enabled);
        fixture_lease(&path, SESSION);
        db.register(&agent(SESSION), false).unwrap();
        db.share_peer_session(&paired, "native", true).unwrap();
        assert_eq!(db.peer(&paired).unwrap().grants.len(), 6);
    }
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert!(
        conn.execute(
            "INSERT INTO peer_grants VALUES(?,'future','bad','send')",
            [&paired]
        )
        .is_err()
    );
    assert!(
        conn.execute(
            "INSERT INTO peer_grants VALUES(?,'opencode',?,'admin')",
            rusqlite::params![paired, SESSION]
        )
        .is_err()
    );
    let violations: i64 = conn
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    assert_eq!(
        conn.query_row::<i64, _, _>("SELECT count(*) FROM grant_audit", [], |row| row.get(0))
            .unwrap(),
        3
    );
    assert_eq!(conn.query_row::<i64, _, _>("SELECT count(*) FROM sqlite_schema WHERE name='fixture_grants_session' AND type='index'", [], |row| row.get(0)).unwrap(), 1);
}

#[test]
fn failed_grant_migration_rolls_back_and_retry_preserves_revoked_peer_denial() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("db");
    let live = peer();
    let revoked = peer();
    let mut db = Db::open(&path).unwrap();
    for paired in [&live, &revoked] {
        db.pair_peer(paired, paired, None).unwrap();
        db.share_peer_session(paired, "user", true).unwrap();
    }
    db.revoke_peer(&revoked).unwrap();
    drop(db);
    let conn = schema_eleven_grants(&path);
    // Deliberately retain historical revoked-peer grants and corrupt one row.
    // Production must preserve the former and reject rather than discard the latter.
    for permission in ["discover", "send", "receive"] {
        conn.execute(
            "INSERT INTO peer_grants VALUES(?,'inbox','user',?)",
            rusqlite::params![revoked, permission],
        )
        .unwrap();
    }
    conn.execute_batch("PRAGMA ignore_check_constraints=ON;")
        .unwrap();
    conn.execute(
        "INSERT INTO peer_grants VALUES(?,'future','corrupt','send')",
        [&live],
    )
    .unwrap();
    fn rows(conn: &rusqlite::Connection, sql: &str) -> Vec<String> {
        conn.prepare(sql)
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }
    let grant_sql = "SELECT quote(peer_id)||quote(provider)||quote(session)||quote(permission) FROM peer_grants ORDER BY peer_id,provider,session,permission";
    let schema_sql = "SELECT type||name||coalesce(sql,'') FROM sqlite_schema WHERE tbl_name IN ('peer_grants','grant_audit') ORDER BY type,name";
    let original_grants = rows(&conn, grant_sql);
    let original_schema = rows(&conn, schema_sql);
    let original_audit = rows(&conn, "SELECT permission FROM grant_audit ORDER BY rowid");
    drop(conn);
    assert!(Db::open(&path).is_err());
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        11
    );
    assert_eq!(rows(&conn, grant_sql), original_grants);
    assert_eq!(rows(&conn, schema_sql), original_schema);
    assert_eq!(
        rows(&conn, "SELECT permission FROM grant_audit ORDER BY rowid"),
        original_audit
    );
    assert_eq!(
        conn.query_row::<i64, _, _>(
            "SELECT count(*) FROM sqlite_schema WHERE name='peer_grants_v12'",
            [],
            |row| row.get(0)
        )
        .unwrap(),
        0
    );
    conn.execute("DELETE FROM peer_grants WHERE provider='future'", [])
        .unwrap();
    let valid_grants = rows(&conn, grant_sql);
    drop(conn);
    let db = Db::open(&path).unwrap();
    assert!(db.peer(&revoked).unwrap().revoked);
    for permission in [Permission::Discover, Permission::Send, Permission::Receive] {
        assert!(
            db.peer_allowed(&live, &Provider::Inbox, "user", permission)
                .unwrap()
        );
        assert!(
            !db.peer_allowed(&revoked, &Provider::Inbox, "user", permission)
                .unwrap()
        );
    }
    drop(db);
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        conn.pragma_query_value::<i64, _>(None, "user_version", |row| row.get(0))
            .unwrap(),
        agent_bridge::migrations::SCHEMA_VERSION
    );
    assert_eq!(rows(&conn, grant_sql), valid_grants);
    assert_eq!(
        rows(&conn, "SELECT permission FROM grant_audit ORDER BY rowid"),
        original_audit
    );
}

#[test]
fn replies_from_another_alias_of_the_same_native_session_preserve_parent_identity() {
    for provider in [Provider::OpenCode, Provider::Claude, Provider::Codex] {
        let mut p = Pair::new();
        let source_session = if provider == Provider::OpenCode {
            SESSION.into()
        } else {
            id()
        };
        let destination_session = if provider == Provider::OpenCode {
            OTHER.into()
        } else {
            id()
        };
        let source = Agent {
            provider: provider.clone(),
            session: source_session,
            ..agent(SESSION)
        };
        let manual = Agent {
            name: "manual".into(),
            provider: provider.clone(),
            session: destination_session.clone(),
            ..agent(OTHER)
        };
        let canonical = Agent {
            name: format!("{}-{}", provider.label(), destination_session),
            ..manual.clone()
        };
        p.a.register(&source, true).unwrap();
        p.b.register(&manual, false).unwrap();
        p.b.register(&canonical, false).unwrap();
        p.a.register(
            &Agent {
                name: "origin-alias".into(),
                ..source.clone()
            },
            false,
        )
        .unwrap();
        p.a.share_peer_session(&p.bid, &source.name, true).unwrap();
        p.b.share_peer_session(&p.aid, &manual.name, true).unwrap();
        let destination = RemoteAgent {
            name: manual.name.clone(),
            provider: provider.clone(),
            session: destination_session.clone(),
        };
        p.a.replace_remote_agents(&p.bid, std::slice::from_ref(&destination))
            .unwrap();
        let question_id = id();
        p.a.send_remote(
            &p.aid,
            &question_id,
            &source.name,
            "bob/manual",
            "question",
            &None,
        )
        .unwrap();
        let question = p.a.pending_transfers(&p.bid, 1).unwrap().pop().unwrap();
        p.b.accept_remote(&p.bid, &p.aid, &question).unwrap();
        let local_parent = p.b.history(&None, 1).unwrap()[0].id.clone();
        // The plugin's canonical route and owner's manual alias name one session.
        p.b.send_remote(
            &p.bid,
            &id(),
            &canonical.name,
            "alice/native",
            "answer",
            &Some(local_parent),
        )
        .unwrap();
        let reply = p.b.pending_transfers(&p.aid, 1).unwrap().pop().unwrap();
        assert_eq!(reply.source.name, canonical.name);
        assert_eq!(reply.source.provider, destination.provider);
        assert_eq!(reply.source.session, destination.session);
        assert_eq!(reply.destination, question.source);
        assert_eq!(reply.destination_peer, p.aid);
        let reference = reply.parent.as_ref().unwrap();
        assert_eq!(reference.id, question_id);
        assert_eq!(reference.peer_id, p.aid);
        assert_eq!(reply.hops, question.hops + 1);
        p.a.accept_remote(&p.aid, &p.bid, &reply).unwrap();
        let answer =
            p.a.history(&None, 10)
                .unwrap()
                .into_iter()
                .find(|m| m.parent.as_deref() == Some(question_id.as_str()))
                .unwrap();
        assert_eq!(answer.recipient, source.name);
        assert_eq!(answer.recipient_session, source.session);
        // A reply may change its source alias, never redirect the parent's destination.
        let mut redirected = reply;
        redirected.id = id();
        redirected.destination.name = "origin-alias".into();
        assert!(p.a.accept_remote(&p.aid, &p.bid, &redirected).is_err());
    }
}
