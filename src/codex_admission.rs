//! Bounded Codex work in the existing broker queue. No native I/O or retry here.
use crate::{
    db::Db,
    protocol::{Agent, Message, Provider},
};
use anyhow::{Result, ensure};
use rusqlite::{Connection, params};

#[derive(Debug)]
pub(crate) struct Capacity(pub &'static str);
impl std::fmt::Display for Capacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Capacity {}

pub(crate) fn check(
    conn: &Connection,
    provider: &Provider,
    session: &str,
    exclude: Option<&str>,
) -> Result<()> {
    if *provider != Provider::Codex {
        return Ok(());
    }
    let global: i64 = conn.query_row(
        "SELECT count(*) FROM messages WHERE state='queued' AND (?1 IS NULL OR id<>?1)",
        [exclude],
        |r| r.get(0),
    )?;
    if global >= 1000 {
        return Err(Capacity("Queue is full (1000 pending messages)").into());
    }
    // Captured provider is immutable across alias replacement. Older unknown
    // provenance counts conservatively, but is never silently rewritten.
    // Controls need no ack: accepted controls cease using an unresolved slot.
    let pending: i64 = conn.query_row("SELECT count(*) FROM messages m JOIN message_context c ON c.id=m.id WHERE NOT EXISTS(SELECT 1 FROM outbound_transfers o WHERE o.id=m.id) AND m.recipient_session=?1 AND (?2 IS NULL OR m.id<>?2) AND (json_extract(c.destination,'$.provider')='codex' OR json_extract(c.destination,'$.provider') IS NULL) AND (m.state IN('queued','sending','uncertain') OR (m.state='submitted' AND NOT EXISTS(SELECT 1 FROM codex_notices n WHERE n.id=m.id)))", params![session,exclude], |r| r.get(0))?;
    if pending >= 64 {
        return Err(Capacity("Codex session is full (64 unresolved messages across aliases); resolve pending receipts before sending or retrying").into());
    }
    Ok(())
}

impl Db {
    pub(crate) fn codex_notice(
        &self,
        id: &str,
        recipient: &str,
        session: &str,
        content: &str,
    ) -> Result<Message> {
        let agent = self.agent(recipient)?;
        ensure!(
            agent.provider == Provider::Codex && agent.enabled && agent.session == session,
            "Codex control notice requires the unchanged enabled exact local registration"
        );
        let tx = self.conn.unchecked_transaction()?;
        let existing = self.get_optional(id)?;
        if existing.is_some() {
            ensure!(
                self.codex_is_notice(id)?,
                "Notice ID already belongs to an ordinary message"
            );
        }
        let message = self.send(id, "user", recipient, content, &None)?;
        tx.execute("INSERT OR IGNORE INTO codex_notices VALUES(?)", [id])?;
        tx.commit()?;
        Ok(message)
    }

    pub(crate) fn codex_is_notice(&self, id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM codex_notices WHERE id=?)",
            [id],
            |r| r.get(0),
        )?)
    }

    pub(crate) fn codex_retry_admission(&self, message: &Message, target: &Agent) -> Result<()> {
        let captured: Option<String> = self.conn.query_row(
            "SELECT json_extract(destination,'$.provider') FROM message_context WHERE id=?",
            [&message.id],
            |r| r.get(0),
        )?;
        if target.provider != Provider::Codex && captured.as_deref() != Some("codex") {
            return Ok(());
        }
        ensure!(
            target.provider == Provider::Codex && captured.as_deref() == Some("codex"),
            "Original Codex provider identity cannot be verified; do not replay this message"
        );
        check(
            &self.conn,
            &target.provider,
            &message.recipient_session,
            Some(&message.id),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Provider;
    fn setup() -> (tempfile::TempDir, Db, Agent, Agent) {
        let root = tempfile::tempdir().unwrap();
        let mut db = Db::open(&root.path().join("bridge.db")).unwrap();
        let one = Agent {
            name: "one".into(),
            provider: Provider::Codex,
            session: "123e4567-e89b-12d3-a456-426614174000".into(),
            endpoint: String::new(),
            enabled: true,
        };
        let mut two = one.clone();
        two.name = "two".into();
        db.register(&one, false).unwrap();
        db.register(&two, false).unwrap();
        (root, db, one, two)
    }
    fn send(db: &Db, target: &Agent) -> Message {
        db.send(
            &uuid::Uuid::new_v4().to_string(),
            "user",
            &target.name,
            "peer input",
            &None,
        )
        .unwrap()
    }
    #[test]
    fn aliases_share_pending_and_unacknowledged_limits_and_retry_excludes_itself() {
        let (_root, mut db, one, two) = setup();
        let first = send(&db, &one);
        let claimed = db.claim().unwrap().unwrap();
        assert_eq!(claimed.0.id, first.id);
        send(&db, &two);
        assert!(db.claim().unwrap().is_none());
        db.finish(&first.id, "submitted", "native accepted")
            .unwrap();
        assert!(db.claim().unwrap().is_some());
        for _ in 0..62 {
            send(&db, &one);
        }
        let error = db
            .send(
                &uuid::Uuid::new_v4().to_string(),
                "user",
                &two.name,
                "full",
                &None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("64 unresolved"));
        assert_eq!(
            db.send(&first.id, "user", &one.name, "peer input", &None)
                .unwrap()
                .id,
            first.id
        );
        db.ack(&first.id, &one.name).unwrap();
        let fresh = send(&db, &two);
        db.conn
            .execute("UPDATE messages SET state='failed' WHERE id=?", [&fresh.id])
            .unwrap();
        assert_eq!(db.retry(&fresh.id, false).unwrap().state, "queued");
    }
    #[test]
    fn typed_controls_keep_no_ack_semantics_but_pending_controls_are_bounded() {
        let (_root, mut db, one, two) = setup();
        for _ in 0..70 {
            let id = uuid::Uuid::new_v4().to_string();
            let message = db
                .codex_notice(&id, &one.name, &one.session, "raw control")
                .unwrap();
            assert!(db.codex_is_notice(&message.id).unwrap());
            let (claimed, _) = db.claim().unwrap().unwrap();
            assert_eq!(claimed.body, "raw control");
            db.finish(&id, "submitted", "native accepted; no ack required")
                .unwrap();
        }
        for _ in 0..64 {
            db.codex_notice(
                &uuid::Uuid::new_v4().to_string(),
                &two.name,
                &two.session,
                "pending control",
            )
            .unwrap();
        }
        assert!(
            db.codex_notice(
                &uuid::Uuid::new_v4().to_string(),
                &one.name,
                &one.session,
                "full"
            )
            .is_err()
        );
        assert!(
            db.codex_notice(
                &uuid::Uuid::new_v4().to_string(),
                &one.name,
                "changed",
                "rebind"
            )
            .is_err()
        );
        db.pause(true).unwrap();
        assert!(db.claim().unwrap().is_none());
    }
    #[test]
    fn immutable_provider_survives_alias_rebinding_and_unknown_legacy_is_not_replayed() {
        let (_root, mut db, mut one, _two) = setup();
        let old = send(&db, &one);
        one.provider = Provider::Claude;
        one.endpoint = "/tmp/unused.sock".into();
        db.register(&one, true).unwrap();
        assert!(db.retry(&old.id, false).is_err());
        db.conn
            .execute("UPDATE messages SET state='queued' WHERE id=?", [&old.id])
            .unwrap();
        assert!(db.claim().unwrap().is_none());
        one.provider = Provider::Codex;
        one.endpoint.clear();
        db.register(&one, true).unwrap();
        db.conn.execute("UPDATE message_context SET destination=json_remove(destination,'$.provider') WHERE id=?",[&old.id]).unwrap();
        db.conn
            .execute("UPDATE messages SET state='failed' WHERE id=?", [&old.id])
            .unwrap();
        assert!(db.retry(&old.id, false).is_err());
        db.conn
            .execute("UPDATE messages SET state='queued' WHERE id=?", [&old.id])
            .unwrap();
        assert!(db.claim().unwrap().is_none());
    }
    #[test]
    fn retry_respects_global_capacity_and_failed_capacity_check_is_atomic() {
        let (_root, db, one, _) = setup();
        let old = send(&db, &one);
        db.conn
            .execute("UPDATE messages SET state='failed' WHERE id=?", [&old.id])
            .unwrap();
        for _ in 0..1000 {
            db.send(
                &uuid::Uuid::new_v4().to_string(),
                "one",
                "user",
                "inbox",
                &None,
            )
            .unwrap();
        }
        assert!(db.retry(&old.id, false).is_err());
        assert_eq!(db.get(&old.id).unwrap().state, "failed");
        assert!(
            db.codex_notice(
                &uuid::Uuid::new_v4().to_string(),
                &one.name,
                &one.session,
                "full"
            )
            .is_err()
        );
    }
    #[test]
    fn uncertain_ordinary_and_control_attempts_never_automatically_replay() {
        let (_root, mut db, one, two) = setup();
        let first = send(&db, &one);
        db.claim().unwrap().unwrap();
        db.finish(&first.id, "uncertain", "lost native receipt")
            .unwrap();
        assert!(db.retry(&first.id, false).is_err());
        assert!(db.claim().unwrap().is_none());
        let control = db
            .codex_notice(
                &uuid::Uuid::new_v4().to_string(),
                &two.name,
                &two.session,
                "notice",
            )
            .unwrap();
        db.claim().unwrap().unwrap();
        db.finish(&control.id, "uncertain", "lost native receipt")
            .unwrap();
        assert!(db.retry(&control.id, false).is_err());
        assert!(db.claim().unwrap().is_none());
        assert_eq!(db.get(&first.id).unwrap().state, "uncertain");
        assert_eq!(db.get(&control.id).unwrap().state, "uncertain");
    }
}
