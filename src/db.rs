use crate::{
    network::trust::{Grant, Peer, Permission},
    protocol::{Agent, MAX_BODY, Message, Provider},
};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Db {
    pub(crate) conn: Connection,
}

pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn message_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        sender: r.get(1)?,
        recipient: r.get(2)?,
        recipient_session: r.get(3)?,
        body: r.get(4)?,
        parent: r.get(5)?,
        hops: r.get(6)?,
        state: r.get(7)?,
        detail: r.get(8)?,
        created: r.get(9)?,
        updated: r.get(10)?,
    })
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

pub(crate) fn pair_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    peer_id: &str,
    fingerprint: &str,
    alias: Option<&str>,
) -> Result<()> {
    let parsed: libp2p_identity::PeerId = peer_id.parse().context("Invalid peer ID")?;
    ensure!(
        parsed.to_string() == peer_id,
        "Peer ID must use its canonical encoding"
    );
    ensure!(
        fingerprint == peer_id,
        "Fingerprint does not match the peer ID"
    );
    if let Some(alias) = alias {
        ensure!(
            valid_name(alias) && alias.parse::<libp2p_identity::PeerId>().is_err(),
            "Peer alias must be a routing name, not a peer ID"
        );
    }
    if let Some(alias) = alias {
        let collision: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE alias=?1 AND peer_id<>?2)",
            params![alias, peer_id],
            |r| r.get(0),
        )?;
        ensure!(!collision, "Peer alias already belongs to another peer");
    }
    // Clear grants when explicitly restoring a revoked identity. Repeating an
    // existing pairing is idempotent and does not broaden permissions.
    tx.execute("DELETE FROM peer_grants WHERE peer_id=? AND EXISTS(SELECT 1 FROM peers WHERE peer_id=? AND revoked=1)", params![peer_id,peer_id])?;
    tx.execute("INSERT INTO peers(peer_id,alias,revoked) VALUES(?1,?2,0) ON CONFLICT(peer_id) DO UPDATE SET alias=COALESCE(excluded.alias,peers.alias),revoked=0", params![peer_id,alias])?;
    Ok(())
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        crate::migrations::migrate(&mut conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO settings(key,value) VALUES('opencode_namespace',?)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        conn.execute("UPDATE opencode_deliveries SET namespace=(SELECT value FROM settings WHERE key='opencode_namespace') WHERE namespace='legacy'", [])?;
        conn.execute("UPDATE agents SET enabled=0 WHERE provider='opencode'", [])?;
        conn.execute("UPDATE messages SET state='uncertain', detail='Service stopped during delivery; inspect the native conversation before retrying.' WHERE state='sending' AND NOT EXISTS(SELECT 1 FROM outbound_transfers o WHERE o.id=messages.id)", [])?;
        Ok(Self { conn })
    }

    /// Pair only after the owner compares this complete fingerprint out of band.
    pub fn pair_peer(
        &mut self,
        peer_id: &str,
        fingerprint: &str,
        alias: Option<&str>,
    ) -> Result<Peer> {
        let tx = self.conn.transaction()?;
        pair_in_transaction(&tx, peer_id, fingerprint, alias)?;
        tx.commit()?;
        self.peer(peer_id)
    }

    /// Consume one locally exchanged invitation without creating agent grants.
    pub fn accept_invitation(
        &mut self,
        token: &str,
        fingerprint: &str,
        alias: Option<&str>,
        local_peer: &str,
    ) -> Result<Peer> {
        let invitation = crate::network::invitation::verify(token, fingerprint)?;
        let local: libp2p_identity::PeerId = local_peer.parse().context("Invalid local peer ID")?;
        ensure!(
            local.to_string() == local_peer,
            "Local peer ID must be canonical"
        );
        ensure!(
            invitation.peer_id != local_peer,
            "Cannot pair this bridge with itself"
        );
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        ensure!(invitation.expires_at > now(), "Invitation expired");
        tx.execute(
            "DELETE FROM consumed_invitations WHERE expires_at<=?",
            [now()],
        )?;
        let used: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM consumed_invitations WHERE peer_id=? AND nonce=?)",
            params![invitation.peer_id, invitation.nonce],
            |r| r.get(0),
        )?;
        ensure!(!used, "Invitation was already accepted on this bridge");
        let count: i64 = tx.query_row("SELECT count(*) FROM consumed_invitations", [], |r| {
            r.get(0)
        })?;
        ensure!(
            count < 1024,
            "Too many active invitation records; wait for earlier invitations to expire"
        );
        pair_in_transaction(&tx, &invitation.peer_id, fingerprint, alias)?;
        if let Some(address) = invitation.addresses.first() {
            let exists: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM peer_addresses WHERE peer_id=?)",
                [&invitation.peer_id],
                |r| r.get(0),
            )?;
            let count: i64 =
                tx.query_row("SELECT count(*) FROM peer_addresses", [], |r| r.get(0))?;
            ensure!(
                exists || count < 64,
                "At most 64 explicit peers may have addresses"
            );
            // A signed hint does not replace a deliberately configured address.
            tx.execute("INSERT INTO peer_addresses(peer_id,address) VALUES(?,?) ON CONFLICT(peer_id) DO NOTHING", params![invitation.peer_id,address])?;
        }
        tx.execute(
            "INSERT INTO consumed_invitations(peer_id,nonce,expires_at) VALUES(?,?,?)",
            params![invitation.peer_id, invitation.nonce, invitation.expires_at],
        )?;
        tx.commit()?;
        self.peer(&invitation.peer_id)
    }

    fn resolve_peer_id(&self, peer: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT peer_id FROM peers WHERE peer_id=?1 OR alias=?1",
                [peer],
                |r| r.get(0),
            )
            .optional()?
            .context("Unknown peer; pair its verified fingerprint first")
    }

    pub fn peer(&self, peer: &str) -> Result<Peer> {
        let peer_id = self.resolve_peer_id(peer)?;
        let (alias, revoked, machine_name): (Option<String>, bool, Option<String>) =
            self.conn.query_row(
                "SELECT alias,revoked,machine_name FROM peers WHERE peer_id=?",
                [&peer_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
        let display_name =
            self.peer_display_name(&peer_id, alias.as_deref(), machine_name.as_deref())?;
        let mut statement = self.conn.prepare("SELECT provider,session,permission FROM peer_grants WHERE peer_id=? ORDER BY provider,session,permission")?;
        let grants = statement
            .query_map([&peer_id], |r| {
                let provider: String = r.get(0)?;
                let permission: String = r.get(2)?;
                Ok((provider, r.get::<_, String>(1)?, permission))
            })?
            .map(|row| {
                let (provider, session, permission) = row?;
                let provider = provider.parse::<Provider>()?;
                let permission = match permission.as_str() {
                    "discover" => Permission::Discover,
                    "send" => Permission::Send,
                    "receive" => Permission::Receive,
                    _ => bail!("Invalid stored grant permission"),
                };
                Ok(Grant {
                    provider,
                    session,
                    permission,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Peer {
            peer_id,
            alias,
            machine_name,
            display_name,
            revoked,
            grants,
        })
    }

    pub fn peers(&self) -> Result<Vec<Peer>> {
        let mut statement = self
            .conn
            .prepare("SELECT peer_id FROM peers ORDER BY peer_id")?;
        let ids = statement
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids.iter().map(|id| self.peer(id)).collect()
    }

    pub fn revoke_peer(&mut self, peer: &str) -> Result<()> {
        let peer_id = self.resolve_peer_id(peer)?;
        let tx = self.conn.transaction()?;
        tx.execute("UPDATE peers SET revoked=1 WHERE peer_id=?", [&peer_id])?;
        tx.execute("DELETE FROM peer_grants WHERE peer_id=?", [&peer_id])?;
        tx.execute("DELETE FROM remote_agents WHERE peer_id=?", [&peer_id])?;
        tx.execute("UPDATE messages SET state=CASE WHEN EXISTS(SELECT 1 FROM opencode_deliveries d WHERE d.id=messages.id AND d.phase<>'queued') THEN 'uncertain' ELSE 'blocked' END,detail='Peer was revoked',updated=? WHERE state='queued' AND (id IN (SELECT local_id FROM inbound_transfers WHERE peer_id=?) OR id IN (SELECT id FROM outbound_transfers WHERE peer_id=? AND state IN ('pending','awaiting_receipt')))", params![now(),peer_id,peer_id])?;
        tx.execute("UPDATE outbound_transfers SET state='blocked',detail='Peer was revoked' WHERE peer_id=? AND state IN ('pending','awaiting_receipt')", [&peer_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn grant_peer(
        &mut self,
        peer: &str,
        agent: &str,
        permission: Permission,
        allow: bool,
    ) -> Result<Peer> {
        let peer = self.peer(peer)?;
        ensure!(
            !peer.revoked,
            "Peer is revoked; verify its fingerprint and pair again first"
        );
        let agent = self.resolve_agent(agent)?;
        crate::clients::validate_remote_session(&agent.provider, &agent.session)?;
        ensure!(
            !allow || agent.enabled,
            "Cannot grant access to a disabled agent"
        );
        if allow {
            self.conn.execute("INSERT OR IGNORE INTO peer_grants(peer_id,provider,session,permission) VALUES(?,?,?,?)", params![peer.peer_id,agent.provider.label(),agent.session,permission.label()])?;
        } else {
            self.conn.execute("DELETE FROM peer_grants WHERE peer_id=? AND provider=? AND session=? AND permission=?", params![peer.peer_id,agent.provider.label(),agent.session,permission.label()])?;
        }
        self.peer(&peer.peer_id)
    }

    /// Every dispatch must query current policy; callers must not cache allows.
    /// Unknown peer identities are denied, rather than implicitly paired.
    pub fn peer_allowed(
        &self,
        peer: &str,
        provider: &Provider,
        session: &str,
        permission: Permission,
    ) -> Result<bool> {
        let Ok(id) = peer.parse::<libp2p_identity::PeerId>() else {
            return Ok(false);
        };
        if id.to_string() != peer {
            return Ok(false);
        }
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM peers p JOIN peer_grants g ON p.peer_id=g.peer_id WHERE p.peer_id=?1 AND p.revoked=0 AND g.provider=?2 AND g.session=?3 AND g.permission=?4 AND EXISTS(SELECT 1 FROM agents a WHERE a.provider=g.provider AND a.session=g.session AND a.enabled=1))", params![peer,provider.label(),session,permission.label()], |r|r.get(0))?)
    }

    pub fn register(&mut self, agent: &Agent, replace: bool) -> Result<()> {
        ensure!(
            valid_name(&agent.name),
            "Agent names use 1–100 letters, digits, dots, underscores or hyphens"
        );
        ensure!(
            agent.name != "user",
            "The name user is reserved for your inbox"
        );
        crate::clients::validate_session(&agent.provider, &agent.session)?;
        if agent.provider == Provider::OpenCode {
            let conflicting: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM agents WHERE provider='opencode' AND session=?1 AND enabled=1 AND endpoint<>?2)",
                params![agent.session, agent.endpoint], |row| row.get(0),
            )?;
            ensure!(
                !conflicting,
                "OpenCode session is already registered by another plugin instance; unregister that exact session first"
            );
        }
        let tx = self.conn.transaction()?;
        let old: Option<(String, String)> = tx
            .query_row(
                "SELECT session,provider FROM agents WHERE name=?",
                [&agent.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        if let Some((session, provider)) = old
            && (session != agent.session || provider != agent.provider.label())
        {
            ensure!(
                replace,
                "Name already belongs to another session; choose another name or explicitly use --replace"
            );
            tx.execute("UPDATE messages SET state='uncertain',detail='Recipient registration changed during attempted OpenCode reconciliation',updated=? WHERE recipient=? AND state='queued' AND id IN(SELECT id FROM opencode_deliveries WHERE phase<>'queued')",params![now(),agent.name])?;
            tx.execute("UPDATE messages SET state='failed',detail='Recipient registration was replaced',updated=? WHERE recipient=? AND state='queued'", params![now(), agent.name])?;
        }
        tx.execute("INSERT INTO agents VALUES(?1,?2,?3,?4,?5) ON CONFLICT(name) DO UPDATE SET provider=excluded.provider,session=excluded.session,endpoint=excluded.endpoint,enabled=excluded.enabled",params![agent.name, agent.provider.label(), agent.session, agent.endpoint, agent.enabled])?;
        tx.commit()?;
        Ok(())
    }

    pub fn agent(&self, name: &str) -> Result<Agent> {
        let (name, provider, session, endpoint, enabled): (String, String, String, String, bool) =
            self.conn
                .query_row(
                    "SELECT name,provider,session,endpoint,enabled FROM agents WHERE name=?",
                    [name],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()?
                .context("Unknown agent; register it first")?;
        let provider = provider.parse::<Provider>()?;
        Ok(Agent {
            name,
            provider,
            session,
            endpoint,
            enabled,
        })
    }

    pub fn agents(&self) -> Result<Vec<Agent>> {
        self.listed_agents(true)
    }

    pub fn listed_agents(&self, all: bool) -> Result<Vec<Agent>> {
        let mut s = self
            .conn
            .prepare("SELECT name FROM agents WHERE ?1 OR enabled=1 ORDER BY name")?;
        let names = s
            .query_map([all], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        names.iter().map(|n| self.agent(n)).collect()
    }

    pub fn display_name(&self, provider: &Provider, session: &str) -> Result<Option<String>> {
        if crate::clients::Adapter::for_provider(provider).is_none() {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT display_name FROM session_metadata WHERE provider=? AND session=?",
                params![provider.label(), session],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Native names are aliases for stable IDs. Duplicate names must never guess a destination.
    pub fn resolve_agent(&self, name: &str) -> Result<Agent> {
        let exact: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE name=?)",
            [name],
            |r| r.get(0),
        )?;
        if exact {
            return self.agent(name);
        }
        let (provider, title) = match name.split_once(':') {
            Some((p, title)) => (Some(p.parse::<Provider>()?), title),
            _ => (None, name),
        };
        let mut s = self.conn.prepare(
            "SELECT a.name FROM agents a JOIN session_metadata n
        ON a.session=n.session AND a.provider=n.provider
        WHERE a.name=n.provider||'-'||n.session AND a.enabled=1 AND n.display_name=?1
        AND (?2 IS NULL OR n.provider=?2) ORDER BY a.name",
        )?;
        let names = s
            .query_map(
                params![title, provider.as_ref().map(Provider::label)],
                |r| r.get::<_, String>(0),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ensure!(
            names.len() <= 1,
            "Native name is ambiguous; use a stable agent ID: {}",
            names.join(", ")
        );
        self.agent(
            names
                .first()
                .context("Unknown agent name; use agent-bridge list")?,
        )
    }

    pub fn observe_codex(
        &mut self,
        session: &str,
        name: Option<&str>,
        enabled: bool,
    ) -> Result<()> {
        let routing_name = crate::clients::lifecycle::routing_name(&Provider::Codex, session);
        self.register(
            &Agent {
                name: routing_name,
                provider: Provider::Codex,
                session: session.into(),
                endpoint: String::new(),
                enabled,
            },
            false,
        )?;
        self.observe_session_name(&Provider::Codex, session, name)?;
        Ok(())
    }

    pub fn observe_claude_name(&self, session: &str, name: &str) -> Result<()> {
        self.observe_session_name(&Provider::Claude, session, Some(name))
    }

    /// Optional display metadata never registers or enables a session.
    pub fn observe_session_name(
        &self,
        provider: &Provider,
        session: &str,
        name: Option<&str>,
    ) -> Result<()> {
        ensure!(
            crate::clients::Adapter::for_provider(provider).is_some(),
            "Inbox has no native display metadata"
        );
        crate::clients::validate_session(provider, session)?;
        self.conn.execute("INSERT INTO session_metadata(provider,session,display_name) VALUES(?,?,?) ON CONFLICT(provider,session) DO UPDATE SET display_name=excluded.display_name", params![provider.label(),session,name])?;
        Ok(())
    }

    pub fn codex_offline(&self, session: Option<&str>) -> Result<()> {
        self.conn.execute("UPDATE agents SET enabled=0 WHERE provider='codex' AND name='codex-'||session AND (?1 IS NULL OR session=?1)",[session])?;
        Ok(())
    }

    pub fn unregister(&self, name: &str, session: &str) -> Result<()> {
        ensure!(name != "user", "Cannot unregister the user inbox");
        self.conn.execute(
            "UPDATE agents SET enabled=0 WHERE name=? AND session=?",
            params![name, session],
        )?;
        Ok(())
    }

    pub fn send(
        &self,
        id: &str,
        sender: &str,
        recipient: &str,
        body: &str,
        parent: &Option<String>,
    ) -> Result<Message> {
        uuid::Uuid::parse_str(id).context("Message ID must be a UUID")?;
        ensure!(
            !body.trim().is_empty() && body.len() <= MAX_BODY,
            "Message must contain 1–65536 bytes"
        );
        let sender = self.resolve_agent(sender)?.name;
        let target = self.resolve_agent(recipient)?;
        let recipient = target.name.as_str();
        if let Some(old) = self.get_optional(id)? {
            ensure!(
                old.sender == sender
                    && old.recipient == recipient
                    && old.body == body
                    && old.parent == *parent,
                "Message ID already exists with different contents"
            );
            return Ok(old);
        }
        ensure!(sender != recipient, "Sending to yourself is disabled");
        let hops = if let Some(parent) = parent {
            let p = self.get(parent)?;
            ensure!(
                p.recipient == sender,
                "Only the recipient can reply to a message"
            );
            p.hops + 1
        } else {
            0
        };
        ensure!(
            hops <= 8,
            "Conversation reached the eight-hop limit; ask the user to start a new exchange"
        );
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM messages WHERE state='queued'",
            [],
            |r| r.get(0),
        )?;
        ensure!(count < 1000, "Queue is full (1000 pending messages)");
        let t = now();
        self.conn.execute(
            "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7,'queued',NULL,?8,?8)",
            params![id, sender, recipient, target.session, body, parent, hops, t],
        )?;
        self.get(id)
    }

    pub fn get_optional(&self, id: &str) -> Result<Option<Message>> {
        Ok(self
            .conn
            .query_row("SELECT * FROM messages WHERE id=?", [id], message_row)
            .optional()?)
    }
    pub fn get(&self, id: &str) -> Result<Message> {
        self.get_optional(id)?.context("Unknown message")
    }

    pub fn history(&self, agent: &Option<String>, limit: usize) -> Result<Vec<Message>> {
        let mut s=self.conn.prepare("SELECT * FROM messages WHERE (?1 IS NULL OR sender=?1 OR recipient=?1) ORDER BY rowid DESC LIMIT ?2")?;
        Ok(
            s.query_map(params![agent, limit.clamp(1, 100) as i64], message_row)?
                .collect::<rusqlite::Result<_>>()?,
        )
    }

    // Claim one message per recipient. SQLite access is serialized by the service mutex.
    pub fn claim(&mut self) -> Result<Option<(Message, Agent)>> {
        if self.paused()? {
            return Ok(None);
        }
        self.block_revoked_inbound()?;
        self.opencode_tick(now())?;
        let id:Option<String> = self.conn.query_row("SELECT m.id FROM messages m JOIN agents a ON a.name=m.recipient AND a.session=m.recipient_session AND a.enabled=1 WHERE (m.state='queued' OR (a.provider='opencode' AND m.state='uncertain' AND EXISTS(SELECT 1 FROM opencode_deliveries requested WHERE requested.id=m.id AND requested.phase='reconcile'))) AND NOT EXISTS(SELECT 1 FROM messages busy WHERE busy.recipient=m.recipient AND busy.state='sending') AND (a.provider<>'opencode' OR (EXISTS(SELECT 1 FROM opencode_deliveries d JOIN opencode_leases l ON l.session=d.session WHERE d.id=m.id AND d.generation=l.generation AND l.endpoint=a.endpoint AND l.expires>?1 AND (l.ready=1 OR d.phase='reconcile')) AND (EXISTS(SELECT 1 FROM opencode_deliveries current WHERE current.id=m.id AND current.phase='reconcile') OR NOT EXISTS(SELECT 1 FROM opencode_deliveries head JOIN messages h ON h.id=head.id WHERE head.session=m.recipient_session AND h.id<>m.id AND (h.state='sending' OR h.state='uncertain' OR (h.state='queued' AND (h.rowid<m.rowid OR head.phase<>'queued'))))))) ORDER BY m.rowid LIMIT 1",[now()],|r|r.get(0)).optional()?;
        let Some(id) = id else { return Ok(None) };
        self.conn.execute(
            "UPDATE messages SET state='sending',updated=? WHERE id=?",
            params![now(), id],
        )?;
        let msg = self.get(&id)?;
        let agent = self.agent(&msg.recipient)?;
        Ok(Some((msg, agent)))
    }

    pub fn finish(&self, id: &str, state: &str, detail: &str) -> Result<()> {
        // A very fast acknowledgement can arrive before the transport finishes.
        self.conn.execute(
            "UPDATE messages SET state=?,detail=?,updated=? WHERE id=? AND state='sending'",
            params![state, detail, now(), id],
        )?;
        Ok(())
    }

    pub fn ack(&self, id: &str, agent: &str) -> Result<Message> {
        let m = self.get(id)?;
        ensure!(
            m.recipient == agent,
            "Acknowledgement must come from the recipient"
        );
        ensure!(
            self.agent(agent)?.session == m.recipient_session,
            "Recipient session changed"
        );
        ensure!(
            ["sending", "submitted", "uncertain", "acknowledged"].contains(&m.state.as_str()),
            "Message has not been submitted"
        );
        self.conn.execute("UPDATE messages SET state='acknowledged',detail='Recipient acknowledged receipt',updated=? WHERE id=?",params![now(),id])?;
        self.get(id)
    }

    pub fn retry(&self, id: &str, allow_duplicate: bool) -> Result<Message> {
        if let Some(message) = self.retry_transfer(id)? {
            return Ok(message);
        }
        if let Some(message) = self.opencode_retry(id)? {
            return Ok(message);
        }
        let m = self.get(id)?;
        ensure!(
            ["failed", "blocked"].contains(&m.state.as_str())
                || (allow_duplicate && ["uncertain", "submitted"].contains(&m.state.as_str())),
            "Only failed or blocked deliveries can be retried directly; submitted/uncertain messages require --allow-duplicate after checking the native conversation"
        );
        ensure!(
            self.agent(&m.recipient)?.session == m.recipient_session,
            "Recipient session changed; create a new message explicitly"
        );
        self.conn.execute(
            "UPDATE messages SET state='queued',detail=NULL,updated=? WHERE id=?",
            params![now(), id],
        )?;
        self.get(id)
    }

    pub fn cancel(&self, id: &str) -> Result<Message> {
        if let Some(message) = self.cancel_transfer(id)? {
            return Ok(message);
        }
        self.opencode_cancel_admission(id)?;
        ensure!(
            self.get(id)?.state == "queued",
            "Only queued messages can be cancelled"
        );
        self.conn.execute(
            "UPDATE messages SET state='cancelled',updated=? WHERE id=?",
            params![now(), id],
        )?;
        self.get(id)
    }

    pub fn paused(&self) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT value FROM settings WHERE key='paused'", [], |r| {
                r.get::<_, String>(0)
            })?
            == "true")
    }
    pub fn pause(&self, paused: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE settings SET value=? WHERE key='paused'",
            [paused.to_string()],
        )?;
        Ok(())
    }
    pub fn counts(&self) -> Result<serde_json::Value> {
        let mut s = self
            .conn
            .prepare("SELECT state,count(*) FROM messages GROUP BY state")?;
        let pairs = s
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(serde_json::to_value(
            pairs
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
        )?)
    }
}
