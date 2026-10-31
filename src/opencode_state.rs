//! Durable local OpenCode ordering, admission, and liveness. No native I/O here.
use crate::{
    db::{Db, now},
    protocol::{Agent, Message, Provider},
};
use anyhow::{Context, Result, ensure};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const LEASE_MS: i64 = 20_000;

pub(crate) fn native_id(namespace: &str, session: &str, id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(namespace.as_bytes());
    hash.update(b"\0");
    hash.update(session.as_bytes());
    hash.update(b"\0");
    hash.update(id.to_ascii_lowercase().as_bytes());
    use std::fmt::Write;
    let mut result = String::from("msg_");
    for byte in hash.finalize() {
        write!(&mut result, "{byte:02x}").expect("Writing hexadecimal to a String");
    }
    result
}

#[derive(Clone, Debug)]
pub(crate) struct Dispatch {
    pub frame: Value,
    pub generation: String,
    pub attempt: String,
    pub endpoint: String,
}
impl Db {
    pub(crate) fn opencode_alias_admission(&self, agent: &Agent) -> Result<()> {
        let valid: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM opencode_leases WHERE session=?1 AND endpoint=?2 AND expires>?3)",params![agent.session,agent.endpoint,now()],|r|r.get(0))?;
        ensure!(
            valid,
            "OpenCode alias requires the current owning live plugin lease"
        );
        Ok(())
    }
    /// Hidden typed controls retain ordinary native uncertainty barriers. Expose
    /// bounded recovery handles without turning them into peer-message history.
    pub(crate) fn event_recovery(&self, agent: Option<&Agent>) -> Result<Value> {
        const LIMIT: usize = 16;
        let Some(agent) = agent.filter(|agent| agent.provider == Provider::OpenCode) else {
            return Ok(json!({"total":0,"truncated":false,"items":[]}));
        };
        let predicate = "FROM messages m JOIN opencode_deliveries d ON d.id=m.id JOIN message_context c ON c.id=m.id WHERE d.session=?1 AND json_extract(c.destination,'$.provider')='opencode' AND d.phase<>'queued' AND m.state IN('queued','sending','uncertain') AND (json_extract(c.destination,'$.control_notice')=1 OR json_extract(c.destination,'$.event_control') IS NOT NULL OR d.content=m.body)";
        let total: i64 = self.conn.query_row(
            &format!("SELECT count(*) {predicate}"),
            [&agent.session],
            |row| row.get(0),
        )?;
        let mut query=self.conn.prepare(&format!("SELECT m.id,m.state,(d.phase='reconcile' AND m.state IN('queued','sending')),EXISTS(SELECT 1 FROM agents a WHERE a.name=m.recipient AND a.provider='opencode' AND a.session=d.session),EXISTS(SELECT 1 FROM agents a JOIN opencode_leases l ON l.session=a.session WHERE a.name=m.recipient AND a.provider='opencode' AND a.session=d.session AND a.enabled=1 AND a.endpoint=l.endpoint AND l.expires>?2) {predicate} ORDER BY m.rowid LIMIT ?3"))?;
        let items=query.query_map(params![agent.session,now(),LIMIT as i64],|row| {
            Ok(json!({"id":row.get::<_,String>(0)?,"state":row.get::<_,String>(1)?,"reconciliation_pending":row.get::<_,bool>(2)?,"route_matches":row.get::<_,bool>(3)?,"lease_available":row.get::<_,bool>(4)?}))
        })?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut recovery = json!({"total":total,"truncated":total>LIMIT as i64,"items":items});
        if total > 0 {
            recovery["instruction"] = json!(
                "Delivery may remain blocked by uncertain native insertion. Inspect each ID, then explicitly run agent-bridge --state-dir SAME_BROKER_STATE_DIR retry ID (default allow_duplicate=false). Retry reads existing native proof only; missing proof remains uncertain. Queued reconciliation has not necessarily run; after restart or lease change, explicitly retry the same ID again if it remains queued. Route or lease mismatches may require restoring the original exact session registration before retry."
            );
        }
        Ok(recovery)
    }

    pub(crate) fn opencode_notice_content(&self, id: &str, content: &str) -> Result<()> {
        let changed = self.conn.execute("UPDATE opencode_deliveries SET content=?1 WHERE id=?2 AND phase='queued' AND (content IS NULL OR content=?1)",params![content,id])?;
        ensure!(
            changed == 1,
            "OpenCode notice ID already belongs to another delivery"
        );
        self.conn.execute("UPDATE message_context SET destination=json_set(destination,'$.control_notice',1) WHERE id=?", [id])?;
        Ok(())
    }
    pub(crate) fn opencode_tick(&self, time: i64) -> Result<()> {
        self.conn.execute("UPDATE agents SET enabled=0 WHERE provider='opencode' AND EXISTS(SELECT 1 FROM opencode_leases l WHERE l.session=agents.session AND l.endpoint=agents.endpoint AND l.expires<=?1)", [time])?;
        // Queued is proof of no native attempt (including an explicit deferred
        // receipt). Bound waiting by queue capacity and the exact owning lease,
        // not elapsed native work/permission time. Keep schema 13's deadline
        // column as compatibility metadata; it no longer authorizes expiry.
        self.conn.execute("UPDATE messages SET state='failed',detail='OpenCode lease/generation expired; explicitly retry after live registration',updated=?1 WHERE state='queued' AND id IN(SELECT d.id FROM opencode_deliveries d LEFT JOIN opencode_leases l ON l.session=d.session WHERE d.phase='queued' AND (l.expires<=?1 OR l.generation<>d.generation OR l.session IS NULL))", [time])?;
        Ok(())
    }

    /// Unresolved OpenCode receipts would otherwise block their session queue
    /// forever. Promotion is a read-only reconciliation: it can only prove an
    /// insertion that already happened, never submit again, so per-session order
    /// and the dedup guarantee are preserved. Each promotion is atomic (savepoint)
    /// and non-fatal; the delivery row is CAS-guarded and rebound to the live
    /// lease generation exactly like an explicit retry.
    pub(crate) fn opencode_sweep_uncertain(&self, time: i64) -> Result<usize> {
        let mut rows = self.conn.prepare(
            "SELECT d.id,d.reconcile_attempts FROM opencode_deliveries d
             JOIN messages m ON m.id=d.id AND m.recipient_session=d.session
             JOIN agents a ON a.name=m.recipient AND a.provider='opencode' AND a.session=d.session AND a.enabled=1
             JOIN opencode_leases l ON l.session=d.session AND l.endpoint=a.endpoint
             WHERE d.phase IN ('uncertain','dispatched') AND m.state='uncertain'
               AND l.expires>?1 AND l.ready=1
               AND (d.reconcile_at IS NULL OR d.reconcile_at<=?1)
             ORDER BY m.rowid LIMIT 64",
        )?;
        let candidates = rows
            .query_map([time], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(rows);
        let mut promoted = 0;
        for (id, attempts) in candidates {
            // min(300s, 2^attempts); attempts reset only on a terminal outcome.
            let seconds = 2i64
                .checked_pow(u32::try_from(attempts.clamp(0, 8)).unwrap_or(8))
                .unwrap_or(300)
                .min(300);
            self.conn.execute_batch("SAVEPOINT opencode_sweep")?;
            let result = (|| -> Result<bool> {
                let changed = self.conn.execute(
                    "UPDATE opencode_deliveries SET phase='reconcile',generation=(SELECT generation FROM opencode_leases WHERE session=opencode_deliveries.session),reconcile_attempts=reconcile_attempts+1,reconcile_at=?1 WHERE id=?2 AND phase IN ('uncertain','dispatched')",
                    params![time + seconds * 1000, id],
                )?;
                if changed == 0 {
                    return Ok(false);
                }
                // Queue-capacity triggers may reject this; the savepoint keeps
                // the delivery row committed with the message row.
                self.conn.execute(
                    "UPDATE messages SET state='queued',detail='OpenCode receipt unresolved; automatic read-only reconciliation',updated=?1 WHERE id=?2 AND state='uncertain'",
                    params![time, id],
                )?;
                Ok(true)
            })();
            match result {
                Ok(true) => {
                    self.conn.execute_batch("RELEASE opencode_sweep")?;
                    promoted += 1;
                }
                Ok(false) => self.conn.execute_batch("RELEASE opencode_sweep")?,
                Err(error) => {
                    self.conn
                        .execute_batch("ROLLBACK TO opencode_sweep; RELEASE opencode_sweep")?;
                    eprintln!("OpenCode reconciliation sweep skipped {id}: {error:#}");
                }
            }
        }
        Ok(promoted)
    }

    #[cfg(test)]
    pub(crate) fn opencode_observe(&mut self, agent: &Agent, ready: bool) -> Result<Value> {
        self.opencode_observe_at(agent, ready, now())
    }

    pub(crate) fn opencode_observe_at(
        &mut self,
        agent: &Agent,
        ready: bool,
        time: i64,
    ) -> Result<Value> {
        self.opencode_tick(time)?;
        let instance =
            crate::clients::opencode::endpoint_instance(std::path::Path::new(&agent.endpoint))?;
        let old: Option<(String, String, i64)> = self
            .conn
            .query_row(
                "SELECT generation,instance,expires FROM opencode_leases WHERE session=?",
                [&agent.session],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        ensure!(
            old.as_ref()
                .is_none_or(|(_, owner, expires)| *expires <= time || *owner == instance),
            "OpenCode session has an unexpired owning instance"
        );
        let generation = old
            .filter(|(_, owner, expires)| *owner == instance && *expires > time)
            .map(|v| v.0)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO opencode_leases VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(session) DO UPDATE SET generation=excluded.generation,instance=excluded.instance,endpoint=excluded.endpoint,expires=excluded.expires,ready=excluded.ready", params![agent.session,generation,instance,agent.endpoint,time+LEASE_MS,ready])?;
        tx.commit()?;
        self.register(agent, false)?;
        let namespace: String = self.conn.query_row(
            "SELECT value FROM settings WHERE key='opencode_namespace'",
            [],
            |r| r.get(0),
        )?;
        Ok(json!({"agent":agent,"generation":generation,"namespace":namespace}))
    }

    pub(crate) fn opencode_heartbeat(
        &self,
        session: &str,
        endpoint: &str,
        generation: &str,
        ready: bool,
    ) -> Result<Value> {
        self.opencode_tick(now())?;
        let changed = self.conn.execute("UPDATE opencode_leases SET expires=?1,ready=?2 WHERE session=?3 AND endpoint=?4 AND generation=?5 AND expires>?6",params![now()+LEASE_MS,ready,session,endpoint,generation,now()])?;
        ensure!(
            changed == 1,
            "OpenCode heartbeat has expired or belongs to a stale generation"
        );
        self.conn.execute(
            "UPDATE agents SET enabled=1 WHERE provider='opencode' AND session=?1 AND endpoint=?2",
            params![session, endpoint],
        )?;
        Ok(json!({"renewed":true}))
    }

    pub(crate) fn opencode_close(
        &self,
        session: &str,
        endpoint: &str,
        generation: &str,
    ) -> Result<Value> {
        let changed = self.conn.execute("UPDATE opencode_leases SET expires=0,ready=0 WHERE session=?1 AND endpoint=?2 AND generation=?3",params![session,endpoint,generation])?;
        ensure!(changed == 1, "Stale OpenCode close generation");
        self.opencode_tick(now())?;
        Ok(json!({"closed":true}))
    }

    pub(crate) fn opencode_admit(
        &self,
        id: &str,
        session: &str,
        generation: &str,
        instance: &str,
        attempt: &str,
    ) -> Result<Value> {
        self.opencode_tick(now())?;
        let allowed: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM opencode_deliveries d JOIN messages m ON m.id=d.id JOIN agents a ON a.name=m.recipient JOIN opencode_leases l ON l.session=d.session WHERE d.id=?1 AND d.session=?2 AND d.generation=?3 AND d.attempt=?4 AND d.phase='dispatched' AND m.state='sending' AND a.provider='opencode' AND a.session=d.session AND a.enabled=1 AND a.endpoint=l.endpoint AND l.generation=d.generation AND l.instance=?5 AND l.expires>?6 AND l.ready=1 AND NOT EXISTS(SELECT 1 FROM inbound_transfers t WHERE t.local_id=m.id AND NOT EXISTS(SELECT 1 FROM peers p JOIN peer_grants g ON g.peer_id=p.peer_id WHERE p.peer_id=t.peer_id AND p.revoked=0 AND g.provider=t.provider AND g.session=t.session AND g.permission='receive')))",params![id,session,generation,attempt,instance,now()],|r|r.get(0))?;
        ensure!(
            allowed && !self.paused()?,
            "OpenCode native admission denied; claim, pause, policy or generation changed"
        );
        Ok(json!({"admitted":true}))
    }

    pub(crate) fn opencode_cancel_admission(&self, id: &str) -> Result<()> {
        let attempted: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM opencode_deliveries WHERE id=? AND phase<>'queued')",
            [id],
            |r| r.get(0),
        )?;
        ensure!(
            !attempted,
            "Attempted OpenCode reconciliation cannot be cancelled; its native insertion remains unresolved"
        );
        Ok(())
    }

    pub(crate) fn opencode_dispatch_failed(&self, id: &str, detail: &str) -> Result<()> {
        let phase: String = self.conn.query_row(
            "SELECT phase FROM opencode_deliveries WHERE id=?",
            [id],
            |r| r.get(0),
        )?;
        if phase != "queued" {
            self.conn.execute(
                "UPDATE opencode_deliveries SET phase='uncertain' WHERE id=?",
                [id],
            )?;
        }
        self.finish(
            id,
            if phase == "queued" {
                "failed"
            } else {
                "uncertain"
            },
            &format!("OpenCode dispatch stopped before native I/O: {detail}"),
        )
    }

    pub(crate) fn opencode_dispatch(
        &self,
        msg: &Message,
        agent: &Agent,
        content: &str,
    ) -> Result<Dispatch> {
        ensure!(
            agent.provider == Provider::OpenCode,
            "Wrong native provider"
        );
        let (generation, namespace, old_native, old_content, phase): (String,String,String,Option<String>,String) = self.conn.query_row("SELECT generation,namespace,native_id,content,phase FROM opencode_deliveries WHERE id=?",[&msg.id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        let (owner_generation, endpoint): (String, String) = self
            .conn
            .query_row(
                "SELECT generation,endpoint FROM opencode_leases WHERE session=?1 AND expires>?2",
                params![msg.recipient_session, now()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .context("OpenCode session lease expired")?;
        ensure!(
            owner_generation == generation && endpoint == agent.endpoint,
            "OpenCode generation changed before dispatch"
        );
        let reconcile = phase == "reconcile" || phase == "legacy" || phase == "uncertain";
        let native = if old_native == "pending" {
            native_id(&namespace, &agent.session, &msg.id)
        } else {
            old_native
        };
        let content = old_content.unwrap_or_else(|| content.to_owned());
        let attempt = uuid::Uuid::new_v4().to_string();
        self.conn.execute("UPDATE opencode_deliveries SET content=?1,native_id=?2,attempt=?3,phase=?4 WHERE id=?5",params![content,native,attempt,if reconcile {"reconcile"} else {"dispatched"},msg.id])?;
        Ok(Dispatch {
            frame: json!({"op":if reconcile {"reconcile"} else {"deliver"},"session":agent.session,"id":msg.id,"content":content,"namespace":namespace,"generation":generation,"attempt":attempt,"native_message_id":native}),
            generation,
            attempt,
            endpoint,
        })
    }

    pub(crate) fn opencode_finish(
        &self,
        id: &str,
        dispatch: &Dispatch,
        state: &str,
        detail: &str,
    ) -> Result<()> {
        let valid: bool = self.conn.query_row("SELECT EXISTS(SELECT 1 FROM opencode_deliveries d JOIN messages m ON m.id=d.id JOIN opencode_leases l ON l.session=d.session WHERE m.state='sending' AND d.id=?1 AND d.generation=?2 AND d.attempt=?3 AND l.generation=d.generation AND l.endpoint=?4 AND l.expires>?5)",params![id,dispatch.generation,dispatch.attempt,dispatch.endpoint,now()],|r|r.get(0))?;
        if !valid {
            self.conn.execute("UPDATE messages SET state='uncertain',detail='OpenCode generation changed before receipt; read-only reconciliation required',updated=?1 WHERE id=?2 AND state='sending' AND EXISTS(SELECT 1 FROM opencode_deliveries d WHERE d.id=?2 AND d.generation=?3 AND d.attempt=?4)",params![now(),id,dispatch.generation,dispatch.attempt])?;
            self.conn.execute("UPDATE opencode_deliveries SET phase='uncertain' WHERE id=?1 AND generation=?2 AND attempt=?3",params![id,dispatch.generation,dispatch.attempt])?;
            return Ok(());
        }
        let deferred = state == "queued";
        let full = deferred
            && self.conn.query_row(
                "SELECT count(*)>=1000 FROM messages WHERE state='queued' AND id<>?",
                [id],
                |r| r.get::<_, bool>(0),
            )?;
        let next_state = if full { "failed" } else { state };
        let detail = if full {
            "OpenCode deferred before insertion but broker queue is full; explicitly retry after backpressure clears"
        } else {
            detail
        };
        let phase = if deferred {
            "queued"
        } else if state == "submitted" {
            "inserted"
        } else {
            "uncertain"
        };
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE opencode_deliveries SET phase=?1 WHERE id=?2 AND attempt=?3",
            params![phase, id, dispatch.attempt],
        )?;
        // Backoff state is terminal-only: an unresolved receipt keeps its
        // attempt count so retries continue to widen.
        if phase == "inserted" || next_state == "failed" {
            tx.execute(
                "UPDATE opencode_deliveries SET reconcile_attempts=0,reconcile_at=NULL WHERE id=?",
                [id],
            )?;
        }
        // A deferred head stays queued but sleeps until an idle heartbeat/event.
        if deferred {
            tx.execute(
                "UPDATE opencode_leases SET ready=0 WHERE generation=?",
                [&dispatch.generation],
            )?;
        }
        self.finish(id, next_state, detail)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn opencode_retry(&self, id: &str) -> Result<Option<Message>> {
        let old: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT session,phase FROM opencode_deliveries WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((session, phase)) = old else {
            return Ok(None);
        };
        let msg = self.get(id)?;
        ensure!(
            ["failed", "blocked", "uncertain", "submitted"].contains(&msg.state.as_str())
                || (msg.state == "queued" && phase == "reconcile"),
            "OpenCode delivery cannot be retried in its current state"
        );
        let agent = self.agent(&msg.recipient)?;
        ensure!(
            agent.provider == Provider::OpenCode && agent.session == session,
            "OpenCode recipient identity changed; create a new message"
        );
        // An explicitly queued read-only reconciliation can refresh a replaced
        // lease after restart. Its immutable native ID/content and reconcile phase
        // remain intact; a sending attempt cannot be rebound or started twice.
        let generation: String = self
            .conn
            .query_row(
                "SELECT generation FROM opencode_leases WHERE session=?1 AND expires>?2",
                params![session, now()],
                |r| r.get(0),
            )
            .optional()?
            .context("OpenCode retry requires a live session lease")?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE opencode_deliveries SET generation=?1,phase=?2,attempt=NULL WHERE id=?3",
            params![
                generation,
                if phase == "queued" {
                    "queued"
                } else {
                    "reconcile"
                },
                id
            ],
        )?;
        tx.execute("UPDATE messages SET state='queued',detail='OpenCode retry: unattempted work or read-only native reconciliation',updated=? WHERE id=?",params![now(),id])?;
        tx.commit()?;
        Ok(Some(self.get(id)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const SESSION: &str = "ses_0123456789abcdefghijklmnop";
    fn setup() -> (tempfile::TempDir, Db, Agent, String) {
        let root = tempfile::tempdir().unwrap();
        let mut db = Db::open(&root.path().join("db")).unwrap();
        let agent = Agent {
            name: format!("opencode-{SESSION}"),
            provider: Provider::OpenCode,
            session: SESSION.into(),
            endpoint: format!("/tmp/{}.sock", uuid::Uuid::new_v4()),
            enabled: true,
        };
        let result = db.opencode_observe(&agent, true).unwrap();
        let generation = result["generation"].as_str().unwrap().to_owned();
        (root, db, agent, generation)
    }
    fn send(db: &Db, name: &str) -> Message {
        db.send(
            &uuid::Uuid::new_v4().to_string(),
            "user",
            name,
            "test",
            &None,
        )
        .unwrap()
    }
    fn dispatch(db: &mut Db) -> (Message, Dispatch) {
        let (msg, agent) = db.claim().unwrap().unwrap();
        let dispatch = db
            .opencode_dispatch(&msg, &agent, "stable rendered envelope")
            .unwrap();
        (msg, dispatch)
    }
    #[test]
    fn hidden_control_recovery_is_bounded_canonical_and_reports_original_route_mismatch() {
        let (_root, mut db, agent, _generation) = setup();
        let alias = Agent {
            name: "manual-alias".into(),
            ..agent.clone()
        };
        db.register(&alias, false).unwrap();
        for index in 0..18 {
            let recipient = if index % 2 == 0 {
                &agent.name
            } else {
                &alias.name
            };
            let id = uuid::Uuid::new_v4().to_string();
            db.send(
                &id,
                "user",
                recipient,
                "sensitive native control content",
                &None,
            )
            .unwrap();
            db.opencode_notice_content(&id, "sensitive native control content")
                .unwrap();
            db.event_control_record(&id, "retired-subscription")
                .unwrap();
            // Historic attempted records exercise the bounded diagnostic query;
            // this storage fixture makes no native submission/success claim.
            db.conn
                .execute("UPDATE messages SET state='uncertain' WHERE id=?", [&id])
                .unwrap();
            db.conn
                .execute(
                    "UPDATE opencode_deliveries SET phase='uncertain' WHERE id=?",
                    [&id],
                )
                .unwrap();
        }
        let recovery = db.event_recovery(Some(&agent)).unwrap();
        assert_eq!(recovery["total"], 18);
        assert_eq!(recovery["truncated"], true);
        assert_eq!(recovery["items"].as_array().unwrap().len(), 16);
        assert!(
            recovery["items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["route_matches"] == true && item["lease_available"] == true)
        );
        assert!(
            !recovery
                .to_string()
                .contains("sensitive native control content")
        );
        assert!(recovery.to_string().len() < 8192);
        db.register(
            &Agent {
                provider: Provider::Inbox,
                session: "replacement".into(),
                endpoint: String::new(),
                ..alias
            },
            true,
        )
        .unwrap();
        let recovery = db.event_recovery(Some(&agent)).unwrap();
        assert_eq!(recovery["total"], 18);
        assert!(
            recovery["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["route_matches"] == false && item["lease_available"] == false)
        );
        assert!(db.history(&None, 100).unwrap().is_empty());
    }

    #[test]
    fn canonical_alias_fifo_deferred_and_uncertain_heads_remain_barriers() {
        let (_root, mut db, agent, generation) = setup();
        let mut alias = agent.clone();
        alias.name = "alias".into();
        db.register(&alias, false).unwrap();
        let first = send(&db, &agent.name);
        let second = send(&db, &alias.name);
        let (msg, attempt) = dispatch(&mut db);
        assert_eq!(msg.id, first.id);
        assert!(db.claim().unwrap().is_none());
        db.opencode_finish(&first.id, &attempt, "queued", "busy")
            .unwrap();
        assert!(db.claim().unwrap().is_none());
        db.opencode_heartbeat(SESSION, &agent.endpoint, &generation, true)
            .unwrap();
        let (_, attempt) = dispatch(&mut db);
        db.opencode_finish(&first.id, &attempt, "uncertain", "lost native receipt")
            .unwrap();
        db.opencode_heartbeat(SESSION, &agent.endpoint, &generation, true)
            .unwrap();
        assert!(db.claim().unwrap().is_none());
        db.retry(&first.id, false).unwrap();
        let (msg, reconcile) = dispatch(&mut db);
        assert_eq!(msg.id, first.id);
        assert_eq!(reconcile.frame["op"], "reconcile");
        assert_eq!(reconcile.frame["content"], "stable rendered envelope");
        db.opencode_finish(&first.id, &reconcile, "submitted", "verified readonly")
            .unwrap();
        assert_eq!(db.claim().unwrap().unwrap().0.id, second.id);
    }
    #[test]
    fn admission_refreshes_pause_and_cancel_and_queue_capacity_includes_retries() {
        let (_root, mut db, agent, _generation) = setup();
        let first = send(&db, &agent.name);
        let (msg, attempt) = dispatch(&mut db);
        let instance =
            crate::clients::opencode::endpoint_instance(std::path::Path::new(&agent.endpoint))
                .unwrap();
        db.pause(true).unwrap();
        assert!(
            db.opencode_admit(
                &msg.id,
                SESSION,
                &attempt.generation,
                &instance,
                &attempt.attempt
            )
            .is_err()
        );
        assert!(db.cancel(&first.id).is_err());
        db.pause(false).unwrap();
        db.opencode_admit(
            &msg.id,
            SESSION,
            &attempt.generation,
            &instance,
            &attempt.attempt,
        )
        .unwrap();
        db.opencode_finish(
            &msg.id,
            &attempt,
            "queued",
            "admission deferred before attempt",
        )
        .unwrap();
        db.cancel(&first.id).unwrap();
        // A cancelled queue head never becomes a native admission.
        assert!(
            db.opencode_admit(
                &msg.id,
                SESSION,
                &attempt.generation,
                &instance,
                &attempt.attempt
            )
            .is_err()
        );
        let failed = send(&db, &agent.name);
        db.conn
            .execute(
                "UPDATE messages SET state='failed' WHERE id=?",
                [&failed.id],
            )
            .unwrap();
        for _ in 0..64 {
            send(&db, &agent.name);
        }
        assert!(
            db.send(
                &uuid::Uuid::new_v4().to_string(),
                "user",
                &agent.name,
                "overflow",
                &None
            )
            .is_err()
        );
        assert!(db.retry(&failed.id, false).is_err());
        assert_eq!(db.get(&failed.id).unwrap().state, "failed");
    }
    #[test]
    fn lease_takeover_fences_queued_work_renew_close_and_late_outcomes() {
        let (_root, mut db, agent, old_generation) = setup();
        let first = send(&db, &agent.name);
        let queued = send(&db, &agent.name);
        let (_, old_attempt) = dispatch(&mut db);
        db.conn
            .execute("UPDATE opencode_leases SET expires=0", [])
            .unwrap();
        db.opencode_tick(now()).unwrap();
        assert_eq!(db.get(&queued.id).unwrap().state, "failed");
        let mut replacement = agent.clone();
        replacement.endpoint = format!("/tmp/{}.sock", uuid::Uuid::new_v4());
        let current = db.opencode_observe(&replacement, true).unwrap();
        let current_generation = current["generation"].as_str().unwrap();
        assert_ne!(old_generation, current_generation);
        assert!(
            db.opencode_heartbeat(SESSION, &agent.endpoint, &old_generation, true)
                .is_err()
        );
        assert!(
            db.opencode_close(SESSION, &agent.endpoint, &old_generation)
                .is_err()
        );
        db.opencode_finish(
            &first.id,
            &old_attempt,
            "submitted",
            "stale insertion receipt",
        )
        .unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "uncertain");
        db.retry(&first.id, true).unwrap();
        let (_, new_attempt) = dispatch(&mut db);
        assert_eq!(new_attempt.frame["op"], "reconcile");
        db.opencode_finish(
            &first.id,
            &old_attempt,
            "submitted",
            "very late previous generation",
        )
        .unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "sending");
        db.opencode_finish(
            &first.id,
            &new_attempt,
            "submitted",
            "current exact readback",
        )
        .unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "submitted");
        db.opencode_finish(&first.id, &old_attempt, "uncertain", "very late error")
            .unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "submitted");
    }
    #[test]
    fn live_unattempted_work_survives_old_busy_deadline_and_restart() {
        for deferred in [false, true] {
            let (root, mut db, agent, generation) = setup();
            let first = send(&db, &agent.name);
            let alias = Agent {
                name: "alias".into(),
                ..agent.clone()
            };
            db.register(&alias, false).unwrap();
            let second = send(&db, &alias.name);
            if deferred {
                let (_, attempt) = dispatch(&mut db);
                db.opencode_finish(&first.id, &attempt, "queued", "busy before native attempt")
                    .unwrap();
            }
            db.opencode_heartbeat(SESSION, &agent.endpoint, &generation, false)
                .unwrap();
            let deadline: i64 = db
                .conn
                .query_row(
                    "SELECT deadline FROM opencode_deliveries WHERE id=?",
                    [&first.id],
                    |r| r.get(0),
                )
                .unwrap();
            // Advance tick deterministically past the old age limit while the
            // exact generation remains live, as continuing heartbeats guarantee.
            db.conn
                .execute(
                    "UPDATE opencode_leases SET expires=?",
                    [deadline + LEASE_MS],
                )
                .unwrap();
            db.opencode_tick(deadline + 1).unwrap();
            assert_eq!(db.get(&first.id).unwrap().state, "queued");
            assert!(db.claim().unwrap().is_none());
            drop(db);
            let mut db = Db::open(&root.path().join("db")).unwrap();
            assert_eq!(db.get(&first.id).unwrap().state, "queued");
            assert!(db.claim().unwrap().is_none());
            db.opencode_heartbeat(SESSION, &agent.endpoint, &generation, true)
                .unwrap();
            let (msg, attempt) = dispatch(&mut db);
            assert_eq!(msg.id, first.id);
            assert_eq!(attempt.frame["op"], "deliver");
            db.opencode_finish(&first.id, &attempt, "submitted", "native inserted")
                .unwrap();
            assert_eq!(db.claim().unwrap().unwrap().0.id, second.id);
        }
    }
    #[test]
    fn expiry_between_claim_and_prepare_fails_one_message_without_stopping_other_clients() {
        let (_root, mut db, agent, _) = setup();
        let first = send(&db, &agent.name);
        let (msg, target) = db.claim().unwrap().unwrap();
        db.conn
            .execute("UPDATE opencode_leases SET expires=0", [])
            .unwrap();
        let error = db.opencode_dispatch(&msg, &target, "body").unwrap_err();
        db.opencode_dispatch_failed(&msg.id, &error.to_string())
            .unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "failed");
        let inbox = Agent {
            name: "unrelated".into(),
            provider: Provider::Inbox,
            session: "unrelated".into(),
            endpoint: String::new(),
            enabled: true,
        };
        db.register(&inbox, false).unwrap();
        let next = send(&db, &inbox.name);
        assert_eq!(db.claim().unwrap().unwrap().0.id, next.id);
    }
    #[test]
    fn broker_restart_and_outcome_commit_failure_preserve_readonly_recovery() {
        let (root, mut db, agent, generation) = setup();
        let first = send(&db, &agent.name);
        let (_, attempt) = dispatch(&mut db);
        db.conn.execute_batch("CREATE TRIGGER fail_outcome BEFORE UPDATE OF state ON messages WHEN NEW.state='submitted' BEGIN SELECT RAISE(ABORT,'injected outcome storage failure'); END;").unwrap();
        assert!(
            db.opencode_finish(
                &first.id,
                &attempt,
                "submitted",
                "native inserted but broker commit failed"
            )
            .is_err()
        );
        db.conn.execute_batch("DROP TRIGGER fail_outcome;").unwrap();
        drop(db);
        let mut db = Db::open(&root.path().join("db")).unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "uncertain");
        assert!(!db.agent(&agent.name).unwrap().enabled);
        db.opencode_heartbeat(SESSION, &agent.endpoint, &generation, true)
            .unwrap();
        db.retry(&first.id, true).unwrap();
        let (_, reconcile) = dispatch(&mut db);
        assert_eq!(reconcile.frame["op"], "reconcile");
        db.opencode_finish(
            &first.id,
            &reconcile,
            "submitted",
            "native inserted exactly once",
        )
        .unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "submitted");
    }
    #[test]
    fn ordinary_native_rendered_content_stays_in_history_but_typed_controls_do_not() {
        let (_root, mut db, agent, _) = setup();
        let peer = send(&db, &agent.name);
        let (claimed, dispatch) = dispatch(&mut db);
        assert_eq!(claimed.id, peer.id);
        assert_ne!(dispatch.frame["content"], peer.body);
        let notice = send(&db, &agent.name);
        db.opencode_notice_content(&notice.id, "raw typed notice")
            .unwrap();
        assert_eq!(
            db.history(&None, 100)
                .unwrap()
                .iter()
                .map(|m| &m.id)
                .collect::<Vec<_>>(),
            vec![&peer.id]
        );
        let page = db.history_since(&None, None, 100).unwrap();
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].id, peer.id);
        // Delivery durability remains available internally, without inventing an ack.
        assert_eq!(db.get(&notice.id).unwrap().state, "queued");
    }

    #[test]
    fn typed_notices_share_canonical_fifo_pause_and_admission() {
        let (_root, mut db, agent, _) = setup();
        let peer = send(&db, &agent.name);
        let notice = send(&db, &agent.name);
        db.opencode_notice_content(&notice.id, "raw typed control notice")
            .unwrap();
        let (_, head) = dispatch(&mut db);
        db.opencode_finish(&peer.id, &head, "queued", "busy")
            .unwrap();
        assert!(db.claim().unwrap().is_none());
        db.pause(true).unwrap();
        assert!(db.claim().unwrap().is_none());
        db.cancel(&notice.id).unwrap();
        assert_eq!(db.get(&notice.id).unwrap().state, "cancelled");
    }
    #[test]
    fn namespaced_native_ids_do_not_collide_between_sessions_or_brokers() {
        assert_ne!(
            native_id("broker-a", SESSION, "same"),
            native_id("broker-b", SESSION, "same")
        );
        assert_ne!(
            native_id("broker-a", SESSION, "same"),
            native_id("broker-a", "other", "same")
        );
        assert_eq!(
            native_id("broker-a", SESSION, "ABC"),
            native_id("broker-a", SESSION, "abc")
        );
    }
    #[test]
    fn historical_unattempted_failure_requires_explicit_retry() {
        let (_root, mut db, agent, generation) = setup();
        let queued = send(&db, &agent.name);
        // A historical broker may already have failed this unattempted ID.
        db.conn
            .execute(
                "UPDATE messages SET state='failed' WHERE id=?",
                [&queued.id],
            )
            .unwrap();
        db.opencode_heartbeat(SESSION, &agent.endpoint, &generation, true)
            .unwrap();
        db.opencode_tick(now()).unwrap();
        assert_eq!(db.get(&queued.id).unwrap().state, "failed");
        assert!(db.claim().unwrap().is_none());
        db.retry(&queued.id, false).unwrap();
        let (_, dispatch) = dispatch(&mut db);
        assert_eq!(dispatch.frame["op"], "deliver");
        assert_eq!(
            dispatch.frame["native_message_id"].as_str().unwrap().len(),
            68
        );
    }
    #[test]
    fn pre_upgrade_submitted_then_old_retry_queued_remains_readonly_after_migration() {
        let (root, db, agent, _) = setup();
        let legacy = db
            .send(
                "12345678-1234-4234-8234-123456789abc",
                "user",
                &agent.name,
                "test",
                &None,
            )
            .unwrap();
        drop(db);
        let path = root.path().join("db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        // Reconstitute schema 12, then exactly the old public retry transition:
        // submitted/uncertain -> queued with erased detail and no attempt record.
        conn.execute_batch("DROP TABLE board_lifecycle; DROP TABLE board_message_receipts; DROP TABLE codex_notices; DROP TRIGGER opencode_enqueue_bound; DROP TRIGGER opencode_retry_bound; DROP TRIGGER opencode_enqueue_binding; DROP TABLE opencode_deliveries; DROP TABLE opencode_leases; PRAGMA user_version=12;").unwrap();
        conn.execute(
            "UPDATE messages SET state='submitted',detail='Native insertion confirmed' WHERE id=?",
            [&legacy.id],
        )
        .unwrap();
        conn.execute(
            "UPDATE messages SET state='queued',detail=NULL,updated=?1 WHERE id=?2",
            params![now(), legacy.id],
        )
        .unwrap();
        conn.execute("UPDATE settings SET value='true' WHERE key='paused'", [])
            .unwrap();
        drop(conn);
        let mut db = Db::open(&path).unwrap();
        assert_eq!(db.get(&legacy.id).unwrap().state, "uncertain");
        db.opencode_observe(&agent, true).unwrap();
        db.pause(false).unwrap();
        let newer = send(&db, &agent.name);
        assert!(db.claim().unwrap().is_none());
        db.retry(&legacy.id, true).unwrap();
        let (msg, reconcile) = dispatch(&mut db);
        assert_eq!(msg.id, legacy.id);
        assert_eq!(reconcile.frame["op"], "reconcile");
        assert_eq!(
            reconcile.frame["native_message_id"],
            format!("msg_{}", legacy.id.replace('-', ""))
        );
        assert_ne!(
            reconcile.frame["native_message_id"],
            native_id(
                reconcile.frame["namespace"].as_str().unwrap(),
                SESSION,
                &legacy.id
            )
        );
        let expected: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/opencode-migrated-frame.json"
        ))
        .unwrap();
        let mut normalized = reconcile.frame.clone();
        for field in ["namespace", "generation", "attempt"] {
            let actual = reconcile.frame[field].as_str().unwrap();
            assert_eq!(uuid::Uuid::parse_str(actual).unwrap().to_string(), actual);
            // Opaque runtime ownership tokens are normalized only after proving
            // the actual migrated producer emits canonical UUIDs, not 'legacy'.
            normalized[field] = expected[field].clone();
        }
        assert_eq!(normalized, expected); // The Bun test sends this exact schema as raw JSON.
        db.opencode_finish(
            &msg.id,
            &reconcile,
            "submitted",
            "Exact old native insertion verified read-only; no new prompt",
        )
        .unwrap();
        assert_eq!(db.claim().unwrap().unwrap().0.id, newer.id);
    }
    #[test]
    fn attempted_reconciliation_cannot_cancel_or_disappear_on_alias_replacement() {
        let (_root, mut db, agent, _) = setup();
        let first = send(&db, &agent.name);
        let later = send(&db, &agent.name);
        let (_, attempt) = dispatch(&mut db);
        db.opencode_finish(&first.id, &attempt, "uncertain", "could still insert")
            .unwrap();
        db.retry(&first.id, false).unwrap();
        assert!(db.cancel(&first.id).is_err());
        let mut replacement = agent.clone();
        replacement.provider = Provider::Inbox;
        replacement.session = "different".into();
        replacement.endpoint = String::new();
        db.register(&replacement, true).unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "uncertain");
        assert_eq!(db.get(&later.id).unwrap().state, "failed");
    }
    #[test]
    fn readonly_reconciliation_can_resolve_multiple_uncertainties_without_deadlock() {
        let (_root, mut db, agent, _) = setup();
        let earlier = send(&db, &agent.name);
        db.conn
            .execute(
                "UPDATE messages SET state='failed' WHERE id=?",
                [&earlier.id],
            )
            .unwrap();
        let first = send(&db, &agent.name);
        let second = send(&db, &agent.name);
        db.conn
            .execute(
                "UPDATE messages SET state='uncertain' WHERE id IN(?1,?2)",
                params![first.id, second.id],
            )
            .unwrap();
        db.conn
            .execute(
                "UPDATE opencode_deliveries SET phase='uncertain' WHERE id IN(?1,?2)",
                params![first.id, second.id],
            )
            .unwrap();
        db.retry(&first.id, false).unwrap();
        db.retry(&second.id, false).unwrap();
        db.retry(&earlier.id, false).unwrap(); // Older row ID cannot outrank an unresolved attempt.
        let (msg, attempt) = dispatch(&mut db);
        assert_eq!(msg.id, first.id);
        assert_eq!(attempt.frame["op"], "reconcile");
        db.opencode_finish(&msg.id, &attempt, "uncertain", "not yet visible")
            .unwrap();
        let (msg, attempt) = dispatch(&mut db);
        assert_eq!(msg.id, second.id);
        assert_eq!(attempt.frame["op"], "reconcile");
    }
    #[test]
    fn retry_and_defer_respect_global_backpressure_without_losing_claim_state() {
        let (_root, mut db, agent, _) = setup();
        let failed = send(&db, &agent.name);
        db.conn
            .execute(
                "UPDATE messages SET state='failed' WHERE id=?",
                [&failed.id],
            )
            .unwrap();
        let sending = send(&db, &agent.name);
        let (_, attempt) = dispatch(&mut db);
        let inbox = Agent {
            name: "sink".into(),
            provider: Provider::Inbox,
            session: "sink".into(),
            endpoint: String::new(),
            enabled: true,
        };
        db.register(&inbox, false).unwrap();
        for _ in 0..1000 {
            send(&db, &inbox.name);
        }
        assert!(db.retry(&failed.id, false).is_err());
        assert_eq!(db.get(&failed.id).unwrap().state, "failed");
        // The INSERT trigger also covers remote enqueue code inside its own transaction.
        assert!(db.conn.execute("INSERT INTO messages VALUES(?1,'user',?2,?3,'remote enqueue',NULL,0,'queued',NULL,?4,?4)",params![uuid::Uuid::new_v4().to_string(),agent.name,SESSION,now()]).is_err());
        db.opencode_finish(&sending.id, &attempt, "queued", "busy before attempt")
            .unwrap();
        assert_eq!(db.get(&sending.id).unwrap().state, "failed");
        let phase: String = db
            .conn
            .query_row(
                "SELECT phase FROM opencode_deliveries WHERE id=?",
                [&sending.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "queued");
    }
    #[test]
    fn revoked_unshared_or_disabled_remote_reconciliation_never_releases_fresh_tail() {
        use crate::network::wire::{Envelope, RemoteAgent};
        for policy_change in ["revoke", "unshare", "disable"] {
            let (_root, mut db, agent, _) = setup();
            let origin = libp2p_identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id()
                .to_string();
            let local = libp2p_identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id()
                .to_string();
            db.pair_peer(&origin, &origin, Some("origin")).unwrap();
            db.share_peer_session(&origin, &agent.name, true).unwrap();
            let alias = Agent {
                name: "local-tail-route".into(),
                ..agent.clone()
            };
            db.register(&alias, false).unwrap();
            let incoming = Envelope {
                origin_peer: origin.clone(),
                id: uuid::Uuid::new_v4().to_string(),
                source: RemoteAgent {
                    name: "source".into(),
                    provider: Provider::Inbox,
                    session: "source".into(),
                },
                destination_peer: local.clone(),
                destination: RemoteAgent {
                    name: agent.name.clone(),
                    provider: agent.provider.clone(),
                    session: agent.session.clone(),
                },
                body: "Unresolved remote native attempt".into(),
                parent: None,
                hops: 0,
            };
            db.accept_remote(&local, &origin, &incoming).unwrap();
            let (old, attempt) = dispatch(&mut db);
            db.opencode_finish(
                &old.id,
                &attempt,
                "uncertain",
                "late native insertion remains possible",
            )
            .unwrap();
            db.retry(&old.id, false).unwrap();
            match policy_change {
                "revoke" => db.revoke_peer(&origin).unwrap(),
                "unshare" => {
                    db.share_peer_session(&origin, &agent.name, false).unwrap();
                }
                _ => db.unregister(&agent.name, &agent.session).unwrap(),
            }
            let tail = send(&db, &alias.name);
            if let Some((claimed, target)) = db.claim().unwrap() {
                // Historical exact readback is local and read-only: it requires
                // no fresh Receive grant and can never become native submission.
                assert_eq!(
                    claimed.id, old.id,
                    "Fresh tail bypassed unresolved policy-blocked attempt"
                );
                let proof = db
                    .opencode_dispatch(&claimed, &target, "stable rendered envelope")
                    .unwrap();
                assert_eq!(proof.frame["op"], "reconcile");
                db.opencode_finish(
                    &claimed.id,
                    &proof,
                    "uncertain",
                    "native insertion remains unverified",
                )
                .unwrap();
            }
            assert_eq!(db.get(&old.id).unwrap().state, "uncertain");
            assert_eq!(db.get(&tail.id).unwrap().state, "queued");
            assert!(
                db.claim().unwrap().is_none(),
                "Unresolved native attempt lost its canonical barrier"
            );
            if policy_change != "disable" {
                assert!(
                    !db.peer_allowed(
                        &origin,
                        &Provider::OpenCode,
                        SESSION,
                        crate::network::trust::Permission::Receive
                    )
                    .unwrap()
                );
            }
        }
    }

    #[test]
    fn uncertain_receipts_sweep_to_read_only_reconcile_without_duplication_or_starvation() {
        let (_root, mut db, agent, _generation) = setup();
        let first = send(&db, &agent.name);
        let second = send(&db, &agent.name);
        let (msg, attempt) = dispatch(&mut db);
        assert_eq!(msg.id, first.id);
        let native = attempt.frame["native_message_id"]
            .as_str()
            .unwrap()
            .to_owned();
        db.opencode_finish(&first.id, &attempt, "uncertain", "lost native receipt")
            .unwrap();
        assert!(db.claim().unwrap().is_none());
        let now = crate::db::now();
        assert_eq!(db.opencode_sweep_uncertain(now).unwrap(), 1);
        let row = |db: &Db| -> (String, i64, Option<i64>) {
            db.conn
                .query_row(
                    "SELECT phase,reconcile_attempts,reconcile_at FROM opencode_deliveries WHERE id=?",
                    [&first.id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .unwrap()
        };
        let (phase, attempts, at) = row(&db);
        assert_eq!(phase, "reconcile");
        assert_eq!(attempts, 1);
        assert_eq!(at, Some(now + 1000));
        // Backoff holds the next sweep until reconcile_at.
        assert_eq!(db.opencode_sweep_uncertain(now + 500).unwrap(), 0);
        let (msg, reconcile) = dispatch(&mut db);
        assert_eq!(msg.id, first.id);
        assert_eq!(reconcile.frame["op"], "reconcile");
        assert_eq!(
            reconcile.frame["native_message_id"].as_str(),
            Some(native.as_str())
        );
        assert_eq!(reconcile.frame["content"], "stable rendered envelope");
        db.opencode_finish(&first.id, &reconcile, "submitted", "verified readonly")
            .unwrap();
        let (phase, attempts, at) = row(&db);
        assert_eq!(phase, "inserted");
        assert_eq!(attempts, 0, "terminal insertion resets backoff");
        assert_eq!(at, None);
        // The follower queued behind the resolved head is no longer starved.
        let (next, _) = dispatch(&mut db);
        assert_eq!(next.id, second.id);
    }

    #[test]
    fn uncertain_sweep_backs_off_and_only_terminal_outcomes_reset_it() {
        let (_root, mut db, agent, _generation) = setup();
        let first = send(&db, &agent.name);
        let (_, attempt) = dispatch(&mut db);
        db.opencode_finish(&first.id, &attempt, "uncertain", "lost native receipt")
            .unwrap();
        let now = crate::db::now();
        assert_eq!(db.opencode_sweep_uncertain(now).unwrap(), 1);
        let (msg, reconcile) = dispatch(&mut db);
        assert_eq!(msg.id, first.id);
        // An unresolved reconciliation keeps the attempt count and widens.
        db.opencode_finish(&first.id, &reconcile, "uncertain", "still unresolved")
            .unwrap();
        let (attempts, at): (i64, Option<i64>) = db
            .conn
            .query_row(
                "SELECT reconcile_attempts,reconcile_at FROM opencode_deliveries WHERE id=?",
                [&first.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempts, 1);
        assert_eq!(at, Some(now + 1000));
        assert_eq!(db.opencode_sweep_uncertain(now).unwrap(), 0);
        assert_eq!(db.opencode_sweep_uncertain(now + 1000).unwrap(), 1);
        let (attempts, at): (i64, Option<i64>) = db
            .conn
            .query_row(
                "SELECT reconcile_attempts,reconcile_at FROM opencode_deliveries WHERE id=?",
                [&first.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(at, Some(now + 1000 + 2000));
    }

    #[test]
    fn sweep_never_overwrites_manual_or_in_flight_reconciliation() {
        let (_root, mut db, agent, _generation) = setup();
        let first = send(&db, &agent.name);
        let (_, attempt) = dispatch(&mut db);
        db.opencode_finish(&first.id, &attempt, "uncertain", "lost native receipt")
            .unwrap();
        db.retry(&first.id, false).unwrap();
        let now = crate::db::now();
        assert_eq!(db.opencode_sweep_uncertain(now).unwrap(), 0);
        let (phase, attempts): (String, i64) = db
            .conn
            .query_row(
                "SELECT phase,reconcile_attempts FROM opencode_deliveries WHERE id=?",
                [&first.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(phase, "reconcile");
        assert_eq!(attempts, 0, "manual retry is left untouched by the sweep");
    }

    #[test]
    fn restart_requalifies_stale_attempts_and_sweeps_after_reobserve() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("db");
        let agent = Agent {
            name: format!("opencode-{SESSION}"),
            provider: Provider::OpenCode,
            session: SESSION.into(),
            endpoint: format!("/tmp/{}.sock", uuid::Uuid::new_v4()),
            enabled: true,
        };
        let first;
        {
            let mut db = Db::open(&path).unwrap();
            db.opencode_observe(&agent, true).unwrap();
            first = send(&db, &agent.name);
            let (msg, attempt) = dispatch(&mut db);
            assert_eq!(msg.id, first.id);
            // Broker stops mid-attempt: the message stays 'sending'.
            assert_eq!(db.get(&first.id).unwrap().state, "sending");
            let _ = attempt;
        }
        let mut db = Db::open(&path).unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "uncertain");
        let phase: String = db
            .conn
            .query_row(
                "SELECT phase FROM opencode_deliveries WHERE id=?",
                [&first.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "uncertain");
        // Plugins reconnect, then the ordinary tick sweeps the stale attempt.
        db.opencode_observe(&agent, true).unwrap();
        let now = crate::db::now();
        db.opencode_sweep_uncertain(now).unwrap();
        let phase: String = db
            .conn
            .query_row(
                "SELECT phase FROM opencode_deliveries WHERE id=?",
                [&first.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(phase, "reconcile");
    }

    #[test]
    fn v16_upgrade_recreates_the_enqueue_trigger_with_explicit_columns() {
        let root = tempfile::tempdir().unwrap();
        let mut db = Db::open(&root.path().join("db")).unwrap();
        // Reconstruct the released v16 shape: the old positional trigger and no
        // reconciliation columns. Db::open must upgrade it in place.
        db.conn
            .execute_batch(
                "ALTER TABLE opencode_deliveries DROP COLUMN reconcile_at;
ALTER TABLE opencode_deliveries DROP COLUMN reconcile_attempts;
DROP TRIGGER opencode_enqueue_binding;
CREATE TRIGGER opencode_enqueue_binding AFTER INSERT ON messages WHEN (SELECT provider FROM agents WHERE name=NEW.recipient)='opencode' BEGIN
 INSERT INTO opencode_deliveries SELECT NEW.id,NEW.recipient_session,COALESCE((SELECT generation FROM opencode_leases WHERE session=NEW.recipient_session),'unleased'),(SELECT value FROM settings WHERE key='opencode_namespace'),'pending',NULL,NULL,'queued',NEW.created+120000;
END;
PRAGMA user_version=16;",
            )
            .unwrap();
        crate::migrations::migrate(&mut db.conn).unwrap();
        let trigger: String = db
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='trigger' AND name='opencode_enqueue_binding'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(trigger.contains("opencode_deliveries(id,session,generation,namespace,native_id,content,attempt,phase,deadline)"));
        let agent = Agent {
            name: format!("opencode-{SESSION}"),
            provider: Provider::OpenCode,
            session: SESSION.into(),
            endpoint: format!("/tmp/{}.sock", uuid::Uuid::new_v4()),
            enabled: true,
        };
        db.opencode_observe(&agent, true).unwrap();
        // The live trigger still enqueues after the upgrade.
        let message = send(&db, &agent.name);
        assert_eq!(db.get(&message.id).unwrap().state, "queued");
    }

    #[test]
    fn sweep_rebinds_a_plugin_restart_to_the_live_generation() {
        let (_root, mut db, agent, _generation) = setup();
        let first = send(&db, &agent.name);
        let (_, attempt) = dispatch(&mut db);
        db.opencode_finish(&first.id, &attempt, "uncertain", "lost native receipt")
            .unwrap();
        // A new plugin instance (new endpoint UUID) takes the session over.
        db.conn
            .execute(
                "UPDATE opencode_leases SET expires=0 WHERE session=?",
                [SESSION],
            )
            .unwrap();
        let replacement = Agent {
            endpoint: format!("/tmp/{}.sock", uuid::Uuid::new_v4()),
            ..agent
        };
        let live = db.opencode_observe(&replacement, true).unwrap();
        let generation = live["generation"].as_str().unwrap().to_owned();
        assert_eq!(db.opencode_sweep_uncertain(crate::db::now()).unwrap(), 1);
        let (phase, bound): (String, String) = db
            .conn
            .query_row(
                "SELECT phase,generation FROM opencode_deliveries WHERE id=?",
                [&first.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(phase, "reconcile");
        assert_eq!(bound, generation);
    }
}
