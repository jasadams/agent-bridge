use crate::{
    network::trust::{Grant, Peer, Permission},
    protocol::{Agent, MAX_BODY, Message, Provider},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub struct Db {
    pub(crate) conn: Connection,
}

pub struct HistoryPage {
    pub messages: Vec<Message>,
    pub cursor: String,
    pub has_more: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HistoryCursor {
    version: u8,
    agent: Option<String>,
    after: Option<String>,
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
        // A broker restart cannot have an in-flight native attempt; requalify
        // those deliveries for the automatic read-only reconciliation sweep.
        conn.execute("UPDATE opencode_deliveries SET phase='uncertain' WHERE phase='dispatched' AND id IN (SELECT id FROM messages WHERE state='uncertain')", [])?;
        conn.execute("UPDATE messages SET state='failed', detail='Event subscription expired on broker restart; resubscribe for a fresh snapshot' WHERE state='queued' AND id IN (SELECT id FROM message_context WHERE json_extract(destination,'$.event_control') IS NOT NULL) AND NOT EXISTS(SELECT 1 FROM opencode_deliveries d WHERE d.id=messages.id AND d.phase<>'queued')", [])?;
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
        self.conn.execute(
            "UPDATE agents SET enabled=0 WHERE provider='codex' AND (?1 IS NULL OR session=?1)",
            [session],
        )?;
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

    /// Confirmed native lifecycle closure retires canonical aliases only after
    /// verifying that the reporting exact route still names that session.
    pub(crate) fn close_native(&self, name: &str, session: &str, endpoint: &str) -> Result<()> {
        let agent = self.agent(name)?;
        ensure!(
            agent.provider != Provider::Inbox
                && agent.session == session
                && agent.endpoint == endpoint,
            "Native close requires the unchanged exact native registration"
        );
        self.conn.execute(
            "UPDATE agents SET enabled=0 WHERE provider=? AND session=?",
            params![agent.provider.label(), session],
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
        crate::codex_admission::check(&self.conn, &target.provider, &target.session, None)?;
        let t = now();
        self.conn.execute(
            "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7,'queued',NULL,?8,?8)",
            params![id, sender, recipient, target.session, body, parent, hops, t],
        )?;
        self.get(id)
    }

    pub(crate) fn board_message_receipt(
        &self,
        request: &crate::board_messages::Submission,
    ) -> Result<Option<serde_json::Value>> {
        let old: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT submission,receipt FROM board_message_receipts WHERE id=?",
                [&request.id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if let Some((submission, receipt)) = old {
            if submission != serde_json::to_string(request)? {
                return Err(crate::board_messages::IdConflict(
                    "Submission ID already belongs to a different board message",
                )
                .into());
            }
            return Ok(Some(serde_json::from_str(&receipt)?));
        }
        if self.get_optional(&request.id)?.is_some() {
            return Err(crate::board_messages::IdConflict(
                "Submission ID already belongs to another message",
            )
            .into());
        }
        Ok(None)
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

    /// An opaque broker/subscription capability, never peer-controlled message text.
    /// Existing immutable destination metadata keeps this durable without a schema change.
    pub(crate) fn event_control_record(&self, id: &str, token: &str) -> Result<()> {
        ensure!(
            !token.is_empty() && token.len() <= 4096,
            "Invalid event control fence"
        );
        let changed = self.conn.execute("UPDATE message_context SET destination=json_set(destination,'$.event_control',?1) WHERE id=?2 AND (json_extract(destination,'$.event_control') IS NULL OR json_extract(destination,'$.event_control')=?1)",params![token,id])?;
        ensure!(
            changed == 1,
            "Event control fence conflicts with existing delivery"
        );
        Ok(())
    }

    pub(crate) fn event_control_token(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT json_extract(destination,'$.event_control') FROM message_context WHERE id=?", [id], |row| row.get(0)).optional()?.flatten())
    }

    pub(crate) fn event_controls(&self) -> Result<Vec<(String, String)>> {
        let mut query = self.conn.prepare("SELECT m.id,json_extract(c.destination,'$.event_control') FROM messages m JOIN message_context c ON c.id=m.id WHERE m.state='queued' AND json_extract(c.destination,'$.event_control') IS NOT NULL")?;
        Ok(query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Only proof of no attempt permits revocation. Already admitted native work
    /// retains the existing submitted/uncertain receipt and reconciliation rules.
    pub(crate) fn event_control_revoke(&self, id: &str) -> Result<()> {
        self.conn.execute("UPDATE messages SET state='failed',detail='Event subscription or exact recipient expired before native admission',updated=?1 WHERE id=?2 AND state='queued' AND NOT EXISTS(SELECT 1 FROM opencode_deliveries d WHERE d.id=messages.id AND d.phase<>'queued')", params![now(),id])?;
        Ok(())
    }

    pub fn history(&self, agent: &Option<String>, limit: usize) -> Result<Vec<Message>> {
        let mut s=self.conn.prepare("SELECT * FROM messages WHERE NOT EXISTS(SELECT 1 FROM message_context c WHERE c.id=messages.id AND (json_extract(c.destination,'$.control_notice')=1 OR json_extract(c.destination,'$.event_control') IS NOT NULL)) AND NOT EXISTS(SELECT 1 FROM codex_notices n WHERE n.id=messages.id) AND NOT EXISTS(SELECT 1 FROM opencode_deliveries d WHERE d.id=messages.id AND d.content=messages.body) AND (?1 IS NULL OR sender=?1 OR recipient=?1) ORDER BY rowid DESC LIMIT ?2")?;
        Ok(
            s.query_map(params![agent, limit.clamp(1, 100) as i64], message_row)?
                .collect::<rusqlite::Result<_>>()?,
        )
    }

    /// New messages only: state updates are read through `get`, never replayed.
    /// Anchor by message ID rather than rowid so cursors survive SQLite VACUUM.
    pub fn history_since(
        &self,
        agent: &Option<String>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<HistoryPage> {
        ensure!((1..=100).contains(&limit), "Message limit must be 1–100");
        let mut cursor = match cursor.filter(|value| !value.is_empty()) {
            Some(value) => {
                ensure!(value.len() <= 1024, "Invalid message cursor");
                let bytes = URL_SAFE_NO_PAD
                    .decode(value)
                    .context("Invalid message cursor")?;
                let decoded: HistoryCursor =
                    serde_json::from_slice(&bytes).context("Invalid message cursor")?;
                ensure!(decoded.version == 1, "Unsupported message cursor version");
                ensure!(
                    decoded.agent == *agent,
                    "Message cursor belongs to another agent scope"
                );
                decoded
            }
            None => HistoryCursor {
                version: 1,
                agent: agent.clone(),
                after: None,
            },
        };
        let after: i64 = match &cursor.after {
            Some(id) => self
                .conn
                .query_row(
                    "SELECT rowid FROM messages WHERE id=?1 AND (?2 IS NULL OR sender=?2 OR recipient=?2)",
                    params![id, agent],
                    |row| row.get(0),
                )
                .optional()?
                .context("Message cursor anchor is missing; restart with an empty cursor")?,
            None => 0,
        };
        let mut statement = self.conn.prepare(
            "SELECT * FROM messages WHERE NOT EXISTS(SELECT 1 FROM message_context c WHERE c.id=messages.id AND (json_extract(c.destination,'$.control_notice')=1 OR json_extract(c.destination,'$.event_control') IS NOT NULL)) AND NOT EXISTS(SELECT 1 FROM codex_notices n WHERE n.id=messages.id) AND NOT EXISTS(SELECT 1 FROM opencode_deliveries d WHERE d.id=messages.id AND d.content=messages.body) AND rowid>?1 AND (?2 IS NULL OR sender=?2 OR recipient=?2) ORDER BY rowid ASC LIMIT ?3",
        )?;
        let mut messages: Vec<Message> = statement
            .query_map(params![after, agent, limit as i64 + 1], message_row)?
            .collect::<rusqlite::Result<_>>()?;
        let has_more = messages.len() > limit;
        messages.truncate(limit);
        if let Some(last) = messages.last() {
            cursor.after = Some(last.id.clone());
        }
        Ok(HistoryPage {
            messages,
            cursor: URL_SAFE_NO_PAD.encode(serde_json::to_vec(&cursor)?),
            has_more,
        })
    }

    // Claim one message per recipient. SQLite access is serialized by the service mutex.
    pub fn claim(&mut self) -> Result<Option<(Message, Agent)>> {
        self.claim_excluding_codex(&[])
    }

    pub(crate) fn claim_excluding_codex(
        &mut self,
        busy_codex: &[String],
    ) -> Result<Option<(Message, Agent)>> {
        if self.paused()? {
            return Ok(None);
        }
        self.block_revoked_inbound()?;
        self.opencode_tick(now())?;
        let id:Option<String> = self.conn.query_row("SELECT m.id FROM messages m JOIN agents a ON a.name=m.recipient AND a.session=m.recipient_session AND a.enabled=1 WHERE (a.provider<>'codex' OR NOT EXISTS(SELECT 1 FROM json_each(?2) running WHERE running.value=m.recipient_session)) AND (m.state='queued' OR (a.provider='opencode' AND m.state='uncertain' AND EXISTS(SELECT 1 FROM opencode_deliveries requested WHERE requested.id=m.id AND requested.phase='reconcile'))) AND EXISTS(SELECT 1 FROM message_context identity WHERE identity.id=m.id AND (json_extract(identity.destination,'$.provider')=a.provider OR (a.provider<>'codex' AND json_extract(identity.destination,'$.provider') IS NULL))) AND NOT EXISTS(SELECT 1 FROM messages busy JOIN message_context bc ON bc.id=busy.id WHERE busy.state='sending' AND NOT EXISTS(SELECT 1 FROM outbound_transfers o WHERE o.id=busy.id) AND (busy.recipient=m.recipient OR (a.provider='codex' AND busy.recipient_session=m.recipient_session AND (json_extract(bc.destination,'$.provider')='codex' OR json_extract(bc.destination,'$.provider') IS NULL)))) AND (a.provider<>'opencode' OR (EXISTS(SELECT 1 FROM opencode_deliveries d JOIN opencode_leases l ON l.session=d.session WHERE d.id=m.id AND d.generation=l.generation AND l.endpoint=a.endpoint AND l.expires>?1 AND (l.ready=1 OR d.phase='reconcile')) AND (EXISTS(SELECT 1 FROM opencode_deliveries current WHERE current.id=m.id AND current.phase='reconcile') OR NOT EXISTS(SELECT 1 FROM opencode_deliveries head JOIN messages h ON h.id=head.id WHERE head.session=m.recipient_session AND h.id<>m.id AND (h.state='sending' OR h.state='uncertain' OR (h.state='queued' AND (h.rowid<m.rowid OR head.phase<>'queued'))))))) ORDER BY m.rowid LIMIT 1",params![now(),serde_json::to_string(busy_codex)?],|r|r.get(0)).optional()?;
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
        self.codex_retry_admission(&m, &self.agent(&m.recipient)?)?;
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
