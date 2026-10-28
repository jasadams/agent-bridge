//! Durable network state. All network I/O stays outside these database operations.
use crate::{
    db::{Db, now, valid_name},
    network::{
        trust::Permission,
        wire::{
            Envelope, Operation, PAGE_SIZE, Reference, RemoteAgent, StatusEvent, WireRequest,
            WireResponse,
        },
    },
    protocol::{MAX_BODY, MAX_FRAME, Message},
};
use anyhow::{Context, Result, bail, ensure};
use libp2p::Multiaddr;
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkConfig {
    pub enabled: bool,
    pub listen: String,
    pub lan: bool,
}

#[derive(Debug)]
struct Capacity(&'static str);
impl std::fmt::Display for Capacity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Capacity {}

pub fn retryable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Capacity>().is_some() || error.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(), Some(rusqlite::Error::SqliteFailure(e, _)) if matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    })
}

fn bounded_detail(mut value: String) -> String {
    let mut limit = value.len().min(2000);
    while !value.is_char_boundary(limit) {
        limit -= 1;
    }
    value.truncate(limit);
    value
}

fn canonical_peer(peer: &str) -> Result<()> {
    let parsed: libp2p_identity::PeerId = peer.parse().context("Invalid peer identity")?;
    ensure!(parsed.to_string() == peer, "Noncanonical peer identity");
    Ok(())
}

fn validate_agent(agent: &RemoteAgent) -> Result<()> {
    ensure!(valid_name(&agent.name), "Invalid remote agent name");
    crate::clients::validate_remote_session(&agent.provider, &agent.session)?;
    Ok(())
}

fn remote(agent: &crate::protocol::Agent) -> RemoteAgent {
    RemoteAgent {
        name: agent.name.clone(),
        provider: agent.provider.clone(),
        session: agent.session.clone(),
    }
}

pub fn validate_address(address: &str, listening: bool) -> Result<Multiaddr> {
    if !listening && address.contains("/p2p-circuit/") {
        return super::relay_config::validate_circuit(address, None);
    }
    super::relay_config::validate_direct(address, listening)
}

impl Db {
    pub fn network_config(&self) -> Result<NetworkConfig> {
        let enabled: String = self.conn.query_row(
            "SELECT value FROM settings WHERE key='network_enabled'",
            [],
            |r| r.get(0),
        )?;
        let listen = self.conn.query_row(
            "SELECT value FROM settings WHERE key='network_listen'",
            [],
            |r| r.get(0),
        )?;
        Ok(NetworkConfig {
            enabled: enabled == "true",
            listen,
            lan: self.conn.query_row(
                "SELECT value FROM settings WHERE key='network_lan'",
                [],
                |r| r.get::<_, String>(0),
            )? == "true",
        })
    }

    pub fn set_network_config(&self, enabled: bool, listen: Option<&str>) -> Result<()> {
        self.set_network_mode(enabled, listen, None)
    }

    pub fn set_network_mode(
        &self,
        enabled: bool,
        listen: Option<&str>,
        lan: Option<bool>,
    ) -> Result<()> {
        if let Some(listen) = listen {
            validate_address(listen, true)?;
        }
        let tx = self.conn.unchecked_transaction()?;
        if let Some(listen) = listen {
            tx.execute(
                "UPDATE settings SET value=? WHERE key='network_listen'",
                [listen],
            )?;
        }
        tx.execute(
            "UPDATE settings SET value=? WHERE key='network_enabled'",
            [enabled.to_string()],
        )?;
        if let Some(lan) = lan {
            tx.execute(
                "UPDATE settings SET value=? WHERE key='network_lan'",
                [lan.to_string()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    fn require_peer(&self, peer: &str) -> Result<()> {
        canonical_peer(peer)?;
        let trusted: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE peer_id=? AND revoked=0)",
            [peer],
            |r| r.get(0),
        )?;
        ensure!(trusted, "Peer is unknown or revoked");
        Ok(())
    }

    pub fn set_peer_address(&self, peer: &str, address: &str) -> Result<()> {
        validate_address(address, false)?;
        let peer = self.peer(peer)?;
        if address.contains("/p2p-circuit/") {
            super::relay_config::configured_circuit(address, &peer.peer_id, &self.relay_nodes()?)?;
        }
        self.require_peer(&peer.peer_id)?;
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM peer_addresses", [], |r| r.get(0))?;
        let exists: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM peer_addresses WHERE peer_id=?)",
            [&peer.peer_id],
            |r| r.get(0),
        )?;
        ensure!(
            exists || count < 64,
            "At most 64 explicit peers may have addresses"
        );
        self.conn.execute("INSERT INTO peer_addresses VALUES(?,?) ON CONFLICT(peer_id) DO UPDATE SET address=excluded.address", params![peer.peer_id,address])?;
        Ok(())
    }

    pub fn network_peers(&self) -> Result<Vec<(String, String)>> {
        Ok(self.conn.prepare("SELECT a.peer_id,a.address FROM peer_addresses a JOIN peers p ON p.peer_id=a.peer_id WHERE p.revoked=0 ORDER BY a.peer_id")?.query_map([], |r|Ok((r.get(0)?,r.get(1)?)))?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn remote_agents(&self) -> Result<Value> {
        self.remote_agents_filtered(None)
    }

    pub fn remote_agents_for(&self, peer: &str) -> Result<Value> {
        ensure!(
            !peer.trim().is_empty(),
            "Choose a nonempty peer alias or ID"
        );
        let peer = self.peer(peer)?;
        self.require_peer(&peer.peer_id)?;
        self.remote_agents_filtered(Some(&peer.peer_id))
    }

    fn remote_agents_filtered(&self, peer: Option<&str>) -> Result<Value> {
        let rows = self.conn.prepare("SELECT r.peer_id,p.alias,r.agent FROM remote_agents r JOIN peers p ON p.peer_id=r.peer_id WHERE p.revoked=0 AND (?1 IS NULL OR r.peer_id=?1) ORDER BY r.peer_id,r.name")?.query_map([peer], |r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = Vec::new();
        for (peer, alias, agent) in rows {
            let agent: RemoteAgent = serde_json::from_str(&agent)?;
            let metadata = self.peer(&peer)?;
            result.push(json!({"peer_id":peer,"peer_alias":alias,"machine_name":metadata.machine_name,"machine_display_name":metadata.display_name,"name":agent.name,"provider":agent.provider,"session":agent.session,"qualified_name":format!("{}/{}",alias.as_deref().unwrap_or(&peer),agent.name)}));
        }
        Ok(json!(result))
    }

    pub fn replace_remote_agents(&self, peer: &str, agents: &[RemoteAgent]) -> Result<()> {
        self.require_peer(peer)?;
        ensure!(
            agents.len() <= PAGE_SIZE,
            "Remote directory exceeds 100 entries"
        );
        let mut names = HashSet::new();
        for agent in agents {
            validate_agent(agent)?;
            ensure!(names.insert(&agent.name), "Duplicate remote agent name");
        }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM remote_agents WHERE peer_id=?", [peer])?;
        for agent in agents {
            tx.execute(
                "INSERT INTO remote_agents VALUES(?,?,?)",
                params![peer, agent.name, serde_json::to_string(agent)?],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn export_agents(&self, peer: &str) -> Result<Vec<RemoteAgent>> {
        self.require_peer(peer)?;
        let mut result = Vec::new();
        for agent in self.listed_agents(false)? {
            if self.peer_allowed(peer, &agent.provider, &agent.session, Permission::Discover)? {
                result.push(remote(&agent));
            }
        }
        ensure!(
            result.len() <= PAGE_SIZE,
            "More than 100 agents are exported; narrow discovery grants"
        );
        Ok(result)
    }

    pub fn send_remote(
        &self,
        local_peer: &str,
        id: &str,
        sender: &str,
        recipient: &str,
        body: &str,
        parent: &Option<String>,
    ) -> Result<Message> {
        canonical_peer(local_peer)?;
        uuid::Uuid::parse_str(id).context("Message ID must be a UUID")?;
        ensure!(
            !body.trim().is_empty() && body.len() <= MAX_BODY,
            "Message must contain 1–65536 bytes"
        );
        if let Some(old) = self.get_optional(id)? {
            let request: Option<(String,String)> = self.conn.query_row("SELECT requested_sender,requested_recipient FROM outbound_transfers WHERE id=?", [id], |r|Ok((r.get(0)?,r.get(1)?))).optional()?;
            ensure!(
                request
                    .as_ref()
                    .is_some_and(|(s, r)| s == sender && r == recipient)
                    && old.body == body
                    && old.parent == *parent,
                "Message ID already exists with different contents"
            );
            return Ok(old);
        }
        let source = self.resolve_agent(sender)?;
        let (peer_name, agent_name) = recipient
            .split_once('/')
            .context("Remote destination must be peer/agent")?;
        let peer = self.peer(peer_name)?;
        ensure!(
            peer.peer_id != local_peer,
            "Cannot send a remote message to this bridge"
        );
        ensure!(
            self.peer_allowed(
                &peer.peer_id,
                &source.provider,
                &source.session,
                Permission::Send
            )?,
            "Sending permission is not granted"
        );
        let (destination, reference, hops) = if let Some(parent_id) = parent {
            let previous = self.get(parent_id)?;
            ensure!(
                previous.recipient == source.name && previous.recipient_session == source.session,
                "Only the original recipient session can reply"
            );
            let encoded: String = self
                .conn
                .query_row(
                    "SELECT envelope FROM inbound_transfers WHERE local_id=?",
                    [parent_id],
                    |r| r.get(0),
                )
                .optional()?
                .context("Remote replies require a received remote parent")?;
            let previous: Envelope = serde_json::from_str(&encoded)?;
            ensure!(
                previous.origin_peer == peer.peer_id && previous.source.name == agent_name,
                "Reply destination does not match the parent sender"
            );
            (
                previous.source,
                Some(Reference {
                    peer_id: previous.origin_peer,
                    id: previous.id,
                }),
                previous.hops + 1,
            )
        } else {
            let encoded: String = self
                .conn
                .query_row(
                    "SELECT agent FROM remote_agents WHERE peer_id=? AND name=?",
                    params![peer.peer_id, agent_name],
                    |r| r.get(0),
                )
                .optional()?
                .context("Unknown remote session; fetch its permitted directory first")?;
            (serde_json::from_str(&encoded)?, None, 0)
        };
        ensure!(hops <= 8, "Conversation reached the eight-hop limit");
        let envelope = Envelope {
            origin_peer: local_peer.into(),
            id: id.into(),
            source: remote(&source),
            destination_peer: peer.peer_id.clone(),
            destination,
            body: body.into(),
            parent: reference,
            hops,
        };
        ensure!(
            serde_json::to_vec(&WireRequest::new(Operation::Send {
                envelope: Box::new(envelope.clone())
            }))?
            .len()
                <= MAX_FRAME,
            "Serialized network envelope exceeds 256 KiB"
        );
        self.check_capacity(&peer.peer_id, false)?;
        let tx = self.conn.unchecked_transaction()?;
        let t = now();
        tx.execute(
            "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7,'queued',NULL,?8,?8)",
            params![
                id,
                source.name,
                format!("{}/{}", peer.peer_id, envelope.destination.name),
                envelope.destination.session,
                body,
                parent,
                hops,
                t
            ],
        )?;
        tx.execute("INSERT INTO outbound_transfers(id,peer_id,envelope,state,requested_sender,requested_recipient) VALUES(?,?,?,'pending',?,?)", params![id,peer.peer_id,serde_json::to_string(&envelope)?,sender,recipient])?;
        tx.commit()?;
        self.get(id)
    }

    fn check_capacity(&self, peer: &str, inbound: bool) -> Result<()> {
        let total: i64 = self.conn.query_row(
            "SELECT count(*) FROM messages WHERE state='queued'",
            [],
            |r| r.get(0),
        )?;
        if total >= 1000 {
            return Err(Capacity("Queue is full (1000 pending messages)").into());
        }
        let sql = if inbound {
            "SELECT count(*) FROM inbound_transfers t JOIN messages m ON m.id=t.local_id WHERE t.peer_id=? AND m.state IN ('queued','sending')"
        } else {
            "SELECT count(*) FROM outbound_transfers WHERE peer_id=? AND state IN ('pending','awaiting_receipt')"
        };
        let count: i64 = self.conn.query_row(sql, [peer], |r| r.get(0))?;
        if count >= 100 {
            return Err(Capacity("Peer queue is full (100 pending messages)").into());
        }
        Ok(())
    }

    pub fn pending_transfers(&self, peer: &str, limit: usize) -> Result<Vec<Envelope>> {
        self.require_peer(peer)?;
        if self.paused()? {
            return Ok(Vec::new());
        }
        let rows = self.conn.prepare("SELECT id,envelope FROM outbound_transfers WHERE peer_id=? AND state IN ('pending','awaiting_receipt') ORDER BY rowid")?.query_map([peer], |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut result = Vec::new();
        for (id, encoded) in rows {
            let envelope: Envelope = serde_json::from_str(&encoded)?;
            if !self.peer_allowed(
                peer,
                &envelope.source.provider,
                &envelope.source.session,
                Permission::Send,
            )? {
                self.conn.execute("UPDATE outbound_transfers SET state='blocked',detail='Sending permission was revoked' WHERE id=?", [&id])?;
                self.conn.execute("UPDATE messages SET state='blocked',detail='Sending permission was revoked',updated=? WHERE id=?", params![now(),id])?;
                continue;
            }
            if result.len() < limit.min(100) {
                result.push(envelope);
            }
        }
        Ok(result)
    }

    pub fn mark_transfer_attempt(&self, id: &str) -> Result<()> {
        let envelope = self.outbound(id)?.context("Unknown outbound transfer")?;
        ensure!(
            self.peer_allowed(
                &envelope.destination_peer,
                &envelope.source.provider,
                &envelope.source.session,
                Permission::Send
            )?,
            "Sending permission was revoked"
        );
        ensure!(self.conn.execute("UPDATE outbound_transfers SET state='awaiting_receipt' WHERE id=? AND state IN ('pending','awaiting_receipt')", [id])? == 1, "Transfer is no longer pending");
        Ok(())
    }

    fn outbound(&self, id: &str) -> Result<Option<Envelope>> {
        let encoded: Option<String> = self
            .conn
            .query_row(
                "SELECT envelope FROM outbound_transfers WHERE id=?",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        encoded
            .map(|s| serde_json::from_str(&s).map_err(Into::into))
            .transpose()
    }

    pub fn transfer_result(&self, peer: &str, id: &str, response: &WireResponse) -> Result<()> {
        self.require_peer(peer)?;
        let envelope = self.outbound(id)?.context("Unknown outbound transfer")?;
        ensure!(
            envelope.destination_peer == peer,
            "Receipt came from another peer"
        );
        match response {
            WireResponse::Accepted { id: accepted } => {
                ensure!(accepted == id, "Receipt identifies another message");
                self.conn.execute("UPDATE outbound_transfers SET state='accepted',detail=NULL WHERE id=? AND state IN ('pending','awaiting_receipt')", [id])?;
            }
            WireResponse::Rejected { reason, retryable } => {
                ensure!(reason.len() <= 2000, "Rejection reason is too long");
                if !retryable {
                    let tx = self.conn.unchecked_transaction()?;
                    let changed = tx.execute("UPDATE outbound_transfers SET state='rejected',detail=? WHERE id=? AND state IN ('pending','awaiting_receipt')", params![reason,id])?;
                    if changed == 1 {
                        tx.execute("UPDATE messages SET state='failed',detail=?,updated=? WHERE id=? AND state='queued'", params![reason,now(),id])?;
                    }
                    tx.commit()?;
                }
            }
            _ => bail!("Unexpected response to message transfer"),
        }
        Ok(())
    }

    pub fn transfer_info(&self, id: &str) -> Result<Option<Value>> {
        let row: Option<(String, Option<String>, Option<i64>)> = self
            .conn
            .query_row(
                "SELECT state,detail,observed_at FROM outbound_transfers WHERE id=?",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        Ok(row.map(|(state,detail,observed_at)|json!({"state":state,"detail":detail,"observed_at":observed_at})))
    }

    pub fn accept_remote(
        &self,
        local_peer: &str,
        authenticated_peer: &str,
        envelope: &Envelope,
    ) -> Result<()> {
        self.require_peer(authenticated_peer)?;
        canonical_peer(local_peer)?;
        ensure!(
            serde_json::to_vec(&WireRequest::new(Operation::Send {
                envelope: Box::new(envelope.clone())
            }))?
            .len()
                <= MAX_FRAME,
            "Serialized network envelope exceeds 256 KiB"
        );
        ensure!(
            envelope.origin_peer == authenticated_peer
                && envelope.destination_peer == local_peer
                && authenticated_peer != local_peer,
            "Envelope identity does not match authenticated endpoints"
        );
        uuid::Uuid::parse_str(&envelope.id).context("Remote message ID must be a UUID")?;
        validate_agent(&envelope.source)?;
        validate_agent(&envelope.destination)?;
        ensure!(
            !envelope.body.trim().is_empty()
                && envelope.body.len() <= MAX_BODY
                && envelope.hops <= 8,
            "Invalid message body or hop count"
        );
        let existing: Option<String> = self
            .conn
            .query_row(
                "SELECT envelope FROM inbound_transfers WHERE peer_id=? AND origin_id=?",
                params![authenticated_peer, envelope.id],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            ensure!(
                serde_json::from_str::<Envelope>(&existing)? == *envelope,
                "Message ID already accepted with different contents"
            );
            return Ok(());
        }
        let destination = self.agent(&envelope.destination.name)?;
        ensure!(
            destination.enabled && remote(&destination) == envelope.destination,
            "Destination session was replaced or disabled"
        );
        ensure!(
            self.peer_allowed(
                authenticated_peer,
                &destination.provider,
                &destination.session,
                Permission::Receive
            )?,
            "Receiving permission is not granted"
        );
        let parent_local = if let Some(parent) = &envelope.parent {
            ensure!(
                parent.peer_id == local_peer,
                "Reply parent belongs to another bridge"
            );
            let previous = self.outbound(&parent.id)?.context("Unknown reply parent")?;
            ensure!(
                previous.destination_peer == authenticated_peer
                    && previous.destination == envelope.source
                    && previous.source == envelope.destination
                    && envelope.hops == previous.hops + 1,
                "Reply endpoints or hop count do not match the parent"
            );
            Some(parent.id.clone())
        } else {
            ensure!(envelope.hops == 0, "New conversations start at hop zero");
            None
        };
        let minute_ago = now().saturating_sub(60_000);
        let recent: i64 = self.conn.query_row("SELECT count(*) FROM inbound_transfers t JOIN messages m ON m.id=t.local_id WHERE t.peer_id=? AND m.created>=?", params![authenticated_peer,minute_ago], |r|r.get(0))?;
        if recent >= 60 {
            return Err(Capacity("Peer message rate limit reached (60 per minute)").into());
        }
        let recipient_recent: i64 = self.conn.query_row("SELECT count(*) FROM inbound_transfers t JOIN messages m ON m.id=t.local_id WHERE t.peer_id=? AND t.provider=? AND t.session=? AND m.created>=?", params![authenticated_peer,destination.provider.label(),destination.session,minute_ago], |r|r.get(0))?;
        if recipient_recent >= 30 {
            return Err(
                Capacity("Recipient message rate limit reached (30 per peer per minute)").into(),
            );
        }
        self.check_capacity(authenticated_peer, true)?;
        let local_id = uuid::Uuid::new_v4().to_string();
        let tx = self.conn.unchecked_transaction()?;
        let encoded = serde_json::to_string(envelope)?;
        check_retained_capacity(&tx, authenticated_peer, envelope.body.len() + encoded.len())?;
        let t = now();
        tx.execute(
            "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7,'queued',NULL,?8,?8)",
            params![
                local_id,
                format!("{}/{}", authenticated_peer, envelope.source.name),
                destination.name,
                destination.session,
                envelope.body,
                parent_local,
                envelope.hops,
                t
            ],
        )?;
        tx.execute(
            "INSERT INTO inbound_transfers VALUES(?,?,?,?,?,?)",
            params![
                authenticated_peer,
                envelope.id,
                local_id,
                encoded,
                destination.provider.label(),
                destination.session
            ],
        )?;
        tx.execute("INSERT INTO peer_events(peer_id,origin_id,state,detail,updated) VALUES(?,?,'queued',NULL,?)", params![authenticated_peer,envelope.id,t])?;
        tx.commit()?;
        Ok(())
    }

    pub fn sync_events(&self, peer: &str, after: u64) -> Result<Vec<StatusEvent>> {
        self.require_peer(peer)?;
        let after = i64::try_from(after).context("Invalid status cursor")?;
        let candidates = self.conn.prepare("SELECT sequence,origin_id,state,detail,updated FROM peer_events WHERE peer_id=? AND sequence>? ORDER BY sequence LIMIT 100")?.query_map(params![peer,after], |r|Ok(StatusEvent { sequence:r.get::<_,i64>(0)? as u64,id:r.get(1)?,state:r.get(2)?,detail:r.get::<_,Option<String>>(3)?.map(bounded_detail),updated:r.get(4)? }))?.collect::<rusqlite::Result<Vec<_>>>()?;
        let mut events = Vec::new();
        for event in candidates {
            events.push(event);
            // JSON control characters expand to six bytes. Count the complete
            // response, rather than assuming the unescaped detail byte limit fits.
            if serde_json::to_vec(&WireResponse::Status {
                events: events.clone(),
            })?
            .len()
                > MAX_FRAME
            {
                events.pop();
                break;
            }
        }
        Ok(events)
    }

    pub fn sync_cursor(&self, peer: &str) -> Result<u64> {
        self.require_peer(peer)?;
        let cursor: i64 = self
            .conn
            .query_row(
                "SELECT sequence FROM peer_cursors WHERE peer_id=?",
                [peer],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        u64::try_from(cursor).context("Invalid stored status cursor")
    }

    pub fn apply_events(&self, peer: &str, events: &[StatusEvent]) -> Result<()> {
        self.require_peer(peer)?;
        ensure!(events.len() <= PAGE_SIZE, "Status page exceeds 100 entries");
        let mut cursor = self.sync_cursor(peer)?;
        let initial = cursor;
        let tx = self.conn.unchecked_transaction()?;
        let mut previous = 0;
        for event in events {
            ensure!(
                event.sequence > previous
                    && event.sequence <= i64::MAX as u64
                    && event.updated >= 0,
                "Status sequences must be strictly increasing"
            );
            previous = event.sequence;
            ensure!(
                [
                    "queued",
                    "sending",
                    "submitted",
                    "acknowledged",
                    "failed",
                    "uncertain",
                    "cancelled",
                    "blocked"
                ]
                .contains(&event.state.as_str())
                    && event.detail.as_ref().is_none_or(|s| s.len() <= 2000),
                "Invalid remote delivery status"
            );
            if event.sequence <= initial {
                continue;
            }
            let destination: Option<String> = tx
                .query_row(
                    "SELECT peer_id FROM outbound_transfers WHERE id=?",
                    [&event.id],
                    |r| r.get(0),
                )
                .optional()?;
            ensure!(
                destination.as_deref() == Some(peer),
                "Status refers to an unknown message or another peer"
            );
            tx.execute(
                "UPDATE messages SET state=?,detail=?,updated=? WHERE id=?",
                params![event.state, event.detail, event.updated, event.id],
            )?;
            tx.execute("UPDATE outbound_transfers SET state='accepted',detail=NULL,observed_at=? WHERE id=?", params![now(),event.id])?;
            cursor = event.sequence;
        }
        tx.execute("INSERT INTO peer_cursors VALUES(?,?) ON CONFLICT(peer_id) DO UPDATE SET sequence=excluded.sequence", params![peer,i64::try_from(cursor)?])?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn block_revoked_inbound(&self) -> Result<()> {
        self.conn.execute("UPDATE messages SET state='blocked',detail='Remote receiving policy or destination changed',updated=? WHERE state='queued' AND id IN (SELECT t.local_id FROM inbound_transfers t WHERE NOT EXISTS(SELECT 1 FROM peers p JOIN peer_grants g ON g.peer_id=p.peer_id WHERE p.peer_id=t.peer_id AND p.revoked=0 AND g.provider=t.provider AND g.session=t.session AND g.permission='receive' AND EXISTS(SELECT 1 FROM agents a WHERE a.name=messages.recipient AND a.provider=t.provider AND a.session=t.session AND a.enabled=1)))", [now()])?;
        Ok(())
    }

    pub(crate) fn cancel_transfer(&self, id: &str) -> Result<Option<Message>> {
        let state: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM outbound_transfers WHERE id=?",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            return Ok(None);
        };
        ensure!(
            state == "pending",
            "Remote acceptance may already have happened; attempted transfers cannot be cancelled locally"
        );
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE outbound_transfers SET state='cancelled' WHERE id=?",
            [id],
        )?;
        tx.execute(
            "UPDATE messages SET state='cancelled',updated=? WHERE id=?",
            params![now(), id],
        )?;
        tx.commit()?;
        Ok(Some(self.get(id)?))
    }

    pub(crate) fn retry_transfer(&self, id: &str) -> Result<Option<Message>> {
        let state: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM outbound_transfers WHERE id=?",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(state) = state else {
            return Ok(None);
        };
        ensure!(
            ["rejected", "blocked"].contains(&state.as_str()),
            "Remote native delivery must be retried at the destination; accepted or ambiguous transfers cannot replay native execution"
        );
        let envelope = self.outbound(id)?.context("Unknown outbound transfer")?;
        ensure!(
            self.peer_allowed(
                &envelope.destination_peer,
                &envelope.source.provider,
                &envelope.source.session,
                Permission::Send
            )?,
            "Sending permission is not granted"
        );
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE outbound_transfers SET state='pending',detail=NULL WHERE id=?",
            [id],
        )?;
        tx.execute(
            "UPDATE messages SET state='queued',detail=NULL,updated=? WHERE id=?",
            params![now(), id],
        )?;
        tx.commit()?;
        Ok(Some(self.get(id)?))
    }
}

// Bound retained remote history, not just currently pending work. Payload bytes
// include both stored body copies; row caps also bound context/event overhead.
const PEER_HISTORY_BYTES: i64 = 64 * 1024 * 1024;
const GLOBAL_HISTORY_BYTES: i64 = 256 * 1024 * 1024;
const PEER_HISTORY_MESSAGES: i64 = 8192;
const GLOBAL_HISTORY_MESSAGES: i64 = 32768;
fn check_retained_capacity(
    tx: &rusqlite::Transaction<'_>,
    peer: &str,
    additional: usize,
) -> Result<()> {
    let (peer_rows, peer_bytes): (i64, i64) = tx
        .query_row(
            "SELECT messages,bytes FROM inbound_usage WHERE peer_id=?",
            [peer],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?
        .unwrap_or((0, 0));
    let (total_rows, total_bytes): (i64, i64) = tx.query_row(
        "SELECT coalesce(sum(messages),0),coalesce(sum(bytes),0) FROM inbound_usage",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if peer_rows >= PEER_HISTORY_MESSAGES
        || total_rows >= GLOBAL_HISTORY_MESSAGES
        || peer_bytes > PEER_HISTORY_BYTES - additional as i64
        || total_bytes > GLOBAL_HISTORY_BYTES - additional as i64
    {
        return Err(Capacity("Retained inbound history quota reached; owner action is required (history and deduplication records are preserved)").into());
    }
    Ok(())
}
