//! Explicit bundles of per-session grants. Names resolve once and never transfer access.
use super::trust::Peer;
use crate::{db::Db, protocol::Provider};
use anyhow::{Result, ensure};
use rusqlite::{TransactionBehavior, params};
use serde_json::{Value, json};

impl Db {
    /// Atomically share (or stop sharing) one provider/session with a trusted peer.
    /// Existing independent grants for that session are included in the bundle;
    /// stopping sharing removes all three, without affecting any other session.
    pub fn share_peer_session(&mut self, peer: &str, agent: &str, shared: bool) -> Result<Peer> {
        let peer = self.peer(peer)?;
        let agent = self.resolve_agent(agent)?;
        ensure!(
            agent.provider != Provider::OpenCode,
            "OpenCode sessions are local-only; remote sharing is not implemented"
        );
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let trusted: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE peer_id=? AND revoked=0)",
            [&peer.peer_id],
            |row| row.get(0),
        )?;
        ensure!(trusted, "Peer is revoked; pair again before sharing");
        // Recheck the resolved registration and policy inside the write transaction.
        let current: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE name=? AND provider=? AND session=? AND (?=0 OR enabled=1))",
            params![agent.name, agent.provider.label(), agent.session, shared],
            |row| row.get(0),
        )?;
        ensure!(
            current,
            "Session registration changed or is disabled; select an enabled session before sharing"
        );
        if shared {
            for permission in ["discover", "send", "receive"] {
                tx.execute(
                    "INSERT OR IGNORE INTO peer_grants(peer_id,provider,session,permission) VALUES(?,?,?,?)",
                    params![peer.peer_id, agent.provider.label(), agent.session, permission],
                )?;
            }
        } else {
            tx.execute(
                "DELETE FROM peer_grants WHERE peer_id=? AND provider=? AND session=?",
                params![peer.peer_id, agent.provider.label(), agent.session],
            )?;
        }
        tx.commit()?;
        self.peer(&peer.peer_id)
    }

    /// Remove grants by immutable session identity even after its registration disappears.
    pub fn unshare_peer_session(
        &mut self,
        peer: &str,
        provider: &Provider,
        session: &str,
    ) -> Result<Peer> {
        ensure!(
            !session.is_empty() && session.len() <= 200,
            "Session ID must contain 1–200 bytes"
        );
        let peer = self.peer(peer)?;
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "DELETE FROM peer_grants WHERE peer_id=? AND provider=? AND session=?",
            params![peer.peer_id, provider.label(), session],
        )?;
        tx.commit()?;
        self.peer(&peer.peer_id)
    }

    /// Include partial grants and sessions whose registrations have disappeared.
    /// This is a local administrative view and never exposes adapter endpoints.
    pub fn shared_sessions(&self) -> Result<Value> {
        let mut statement = self.conn.prepare(
            "SELECT DISTINCT peer_id,provider,session FROM peer_grants ORDER BY peer_id,provider,session",
        )?;
        let groups = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = Vec::with_capacity(groups.len());
        for (peer_id, provider, session) in groups {
            let peer = self.peer(&peer_id)?;
            let mut statement = self.conn.prepare(
                "SELECT name,enabled FROM agents WHERE provider=? AND session=? ORDER BY name",
            )?;
            let local = statement
                .query_map(params![provider, session], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut statement = self.conn.prepare(
                "SELECT permission FROM peer_grants WHERE peer_id=? AND provider=? AND session=? ORDER BY permission",
            )?;
            let permissions = statement
                .query_map(params![peer_id, provider, session], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            result.push(json!({
                "peer_id":peer_id,
                "peer_alias":peer.alias,
                "machine_name":peer.machine_name,
                "machine_display_name":peer.display_name,
                "provider":provider,
                "session":session,
                "local_names":local.iter().map(|(name,_)| name).collect::<Vec<_>>(),
                "enabled":local.iter().any(|(_,enabled)| *enabled),
                "stale":local.is_empty(),
                "shared":permissions.len()==3,
                "permissions":permissions,
            }));
        }
        Ok(json!(result))
    }
}
