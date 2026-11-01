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
    // Bound actual delivery work, not retained history or ambiguous attempts.
    // Uncertain attempts are never automatically replayed and consume no worker.
    let pending: i64 = conn.query_row("SELECT count(*) FROM messages m JOIN message_context c ON c.id=m.id WHERE NOT EXISTS(SELECT 1 FROM outbound_transfers o WHERE o.id=m.id) AND m.recipient_session=?1 AND (?2 IS NULL OR m.id<>?2) AND (json_extract(c.destination,'$.provider')='codex' OR json_extract(c.destination,'$.provider') IS NULL) AND m.state IN('queued','sending')", params![session,exclude], |r| r.get(0))?;
    if pending >= 64 {
        return Err(Capacity("Codex session is full (64 pending deliveries across aliases); wait for pending work before sending or retrying").into());
    }
    Ok(())
}

impl Db {
    #[cfg(test)]
    pub(crate) fn codex_notice(
        &self,
        id: &str,
        recipient: &str,
        session: &str,
        content: &str,
    ) -> Result<Message> {
        self.codex_event_notice(id, recipient, session, content, None)
    }

    pub(crate) fn codex_event_notice(
        &self,
        id: &str,
        recipient: &str,
        session: &str,
        content: &str,
        token: Option<&str>,
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
        if let Some(token) = token {
            self.event_control_record(id, token)?;
        }
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
    fn submitted_history_without_agent_receipts_does_not_block_next_handoff() {
        let (_root, mut db, one, two) = setup();
        for _ in 0..64 {
            let message = send(&db, &one);
            assert_eq!(db.claim().unwrap().unwrap().0.id, message.id);
            db.finish(&message.id, "submitted", "native accepted")
                .unwrap();
        }
        let next = send(&db, &two);
        assert_eq!(db.claim().unwrap().unwrap().0.id, next.id);
    }
    #[test]
    fn aliases_share_pending_limits_and_retry_excludes_itself() {
        let (_root, mut db, one, two) = setup();
        let first = send(&db, &one);
        let claimed = db.claim().unwrap().unwrap();
        assert_eq!(claimed.0.id, first.id);
        send(&db, &two);
        assert!(db.claim().unwrap().is_none());
        db.finish(&first.id, "submitted", "native accepted")
            .unwrap();
        let active = db.claim().unwrap().unwrap().0;
        for _ in 0..63 {
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
        assert!(error.to_string().contains("64 pending"));
        assert_eq!(
            db.send(&first.id, "user", &one.name, "peer input", &None)
                .unwrap()
                .id,
            first.id
        );
        db.finish(&active.id, "submitted", "native accepted")
            .unwrap();
        let fresh = send(&db, &two);
        db.conn
            .execute("UPDATE messages SET state='failed' WHERE id=?", [&fresh.id])
            .unwrap();
        assert_eq!(db.retry(&fresh.id, false).unwrap().state, "queued");
    }
    #[test]
    fn event_controls_are_not_peer_history_and_restart_never_replays_them() {
        let (root, mut db, one, _) = setup();
        let peer = send(&db, &one);
        let event = db
            .codex_notice(
                &uuid::Uuid::new_v4().to_string(),
                &one.name,
                &one.session,
                "event control",
            )
            .unwrap();
        db.event_control_record(&event.id, "broker-one/subscription-one")
            .unwrap();
        assert!(db.event_control_record(&event.id, "replacement").is_err());
        assert_eq!(
            db.event_controls().unwrap(),
            vec![(event.id.clone(), "broker-one/subscription-one".into())]
        );
        assert_eq!(
            db.history(&None, 100)
                .unwrap()
                .iter()
                .map(|m| &m.id)
                .collect::<Vec<_>>(),
            vec![&peer.id]
        );
        let page = db.history_since(&None, None, 1).unwrap();
        assert_eq!(page.messages[0].id, peer.id);
        assert!(!page.has_more);
        assert!(
            db.history_since(&None, Some(&page.cursor), 1)
                .unwrap()
                .messages
                .is_empty()
        );
        let admitted = db
            .codex_notice(
                &uuid::Uuid::new_v4().to_string(),
                &one.name,
                &one.session,
                "already attempted",
            )
            .unwrap();
        db.event_control_record(&admitted.id, "broker-one/subscription-one")
            .unwrap();
        db.conn
            .execute(
                "UPDATE messages SET state='sending' WHERE id=?",
                [&admitted.id],
            )
            .unwrap();
        db.event_control_revoke(&admitted.id).unwrap();
        assert_eq!(db.get(&admitted.id).unwrap().state, "sending");
        drop(db);
        db = Db::open(&root.path().join("bridge.db")).unwrap();
        assert_eq!(db.get(&event.id).unwrap().state, "failed");
        assert_eq!(db.get(&admitted.id).unwrap().state, "uncertain");
        assert_eq!(db.claim().unwrap().unwrap().0.id, peer.id);
        assert!(db.claim().unwrap().is_none());
    }

    #[test]
    fn ordinary_alias_unregister_does_not_close_canonical_session_or_accept_stale_native_close() {
        let (_root, db, one, two) = setup();
        db.unregister(&one.name, &one.session).unwrap();
        assert!(!db.agent(&one.name).unwrap().enabled);
        assert!(db.agent(&two.name).unwrap().enabled);
        assert!(
            db.close_native(&two.name, "wrong-session", &two.endpoint)
                .is_err()
        );
        assert!(db.agent(&two.name).unwrap().enabled);
        db.close_native(&two.name, &two.session, &two.endpoint)
            .unwrap();
        assert!(!db.agent(&two.name).unwrap().enabled);
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

    #[test]
    fn unknown_history_is_not_pending_capacity_and_is_never_replayed_on_restart() {
        let (root, mut db, one, _) = setup();
        let mut ids = Vec::new();
        for _ in 0..64 {
            let message = send(&db, &one);
            db.claim().unwrap().unwrap();
            db.finish(&message.id, "uncertain", "lost confirmation")
                .unwrap();
            ids.push(message.id);
        }
        let fresh = send(&db, &one);
        drop(db);
        let mut db = Db::open(&root.path().join("bridge.db")).unwrap();
        assert_eq!(db.claim().unwrap().unwrap().0.id, fresh.id);
        assert!(db.claim().unwrap().is_none());
        for id in ids {
            assert_eq!(db.get(&id).unwrap().state, "uncertain");
            assert!(db.retry(&id, false).is_err());
        }
    }

    #[test]
    fn historical_outcomes_use_immutable_provider_not_live_alias_or_detail() {
        let (_root, mut db, mut one, _) = setup();
        let message = send(&db, &one);
        db.claim().unwrap().unwrap();
        db.finish(&message.id, "submitted", "arbitrary legacy text")
            .unwrap();
        one.provider = Provider::Claude;
        one.endpoint = "/tmp/unused.sock".into();
        db.register(&one, true).unwrap();
        assert_eq!(
            db.delivery_state(&db.get(&message.id).unwrap()).unwrap(),
            "delivered"
        );
        db.conn.execute("UPDATE message_context SET destination=json_set(destination,'$.provider','claude') WHERE id=?", [&message.id]).unwrap();
        assert_eq!(
            db.delivery_state(&db.get(&message.id).unwrap()).unwrap(),
            "unknown"
        );
        db.conn.execute("UPDATE message_context SET destination=json_remove(destination,'$.provider') WHERE id=?", [&message.id]).unwrap();
        assert_eq!(
            db.delivery_state(&db.get(&message.id).unwrap()).unwrap(),
            "unknown"
        );
        // Historical receipt evidence remains evidence, never a task completion.
        db.conn
            .execute(
                "UPDATE messages SET state='acknowledged' WHERE id=?",
                [&message.id],
            )
            .unwrap();
        assert_eq!(
            db.delivery_state(&db.get(&message.id).unwrap()).unwrap(),
            "delivered"
        );
        assert_eq!(db.counts().unwrap()["delivered"], 1);
    }
}
