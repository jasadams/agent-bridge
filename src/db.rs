use crate::{
    network::trust::{Grant, Peer, Permission},
    protocol::{Agent, MAX_BODY, MAX_REPLY_HOPS, Message, Provider},
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    os::unix::fs::MetadataExt,
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

/// Reserved canonical native route: provider prefix plus a validator-accepted
/// session ID. No database row is required or created for this identity.
pub(crate) fn canonical_route(name: &str) -> Result<Option<(Provider, String)>> {
    let Some((provider, session)) = name.split_once('-') else {
        return Ok(None);
    };
    let Ok(provider) = provider.parse::<Provider>() else {
        return Ok(None);
    };
    if provider == Provider::Inbox || crate::clients::validate_session(&provider, session).is_err()
    {
        return Ok(None);
    }
    Ok(Some((provider, session.to_owned())))
}

/// SessionEnd may arrive after the socket is removed. Match only the canonical
/// parent path and never touch a replacement endpoint.
fn normalize_claude_endpoint(endpoint: &str) -> Result<String> {
    let path = Path::new(endpoint);
    ensure!(path.is_absolute(), "Socket path must be absolute");
    let parent = path
        .parent()
        .context("Missing Claude socket directory")?
        .canonicalize()?;
    let meta = std::fs::metadata(&parent)?;
    ensure!(
        meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0,
        "Socket directory must be private to your user"
    );
    let normalized = parent.join(path.file_name().context("Missing socket filename")?);
    ensure!(
        normalized.as_os_str().len() < 104,
        "Socket path is too long for macOS"
    );
    Ok(normalized.to_string_lossy().into_owned())
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
    /// Resolve historical proof using immutable destination provenance, never
    /// a mutable registration or human-readable detail. No history is rewritten.
    pub(crate) fn delivery_state(&self, message: &Message) -> Result<&'static str> {
        let (provider, phase): (Option<String>, Option<String>) = self.conn.query_row(
            "SELECT coalesce(json_extract(o.envelope,'$.destination.provider'),json_extract(c.destination,'$.provider')),d.phase FROM message_context c LEFT JOIN outbound_transfers o ON o.id=c.id LEFT JOIN opencode_deliveries d ON d.id=c.id WHERE c.id=?",
            [&message.id], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(crate::protocol::delivery_outcome(
            &message.state,
            provider.as_deref(),
            phase.as_deref(),
        ))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let mut conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_secs(3))?;
        crate::migrations::migrate(&mut conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO settings(key,value) VALUES('opencode_namespace',?)",
            [uuid::Uuid::new_v4().to_string()],
        )?;
        conn.execute("UPDATE opencode_deliveries SET namespace=(SELECT value FROM settings WHERE key='opencode_namespace') WHERE namespace='legacy'", [])?;
        {
            let tx = conn.transaction()?;
            // The time is when the broker detects interruption, not the time
            // of any native outcome. Preserve only existing dispatch identity.
            tx.execute("UPDATE message_context SET destination=json_insert(destination,'$.opencode_attempt_detail','Service stopped during delivery; native outcome unknown','$.opencode_attempt_id',(SELECT attempt FROM opencode_deliveries WHERE id=message_context.id),'$.opencode_attempt_timestamp',?1,'$.opencode_attempt_timestamp_kind','detected') WHERE json_type(destination,'$.opencode_attempt_detail') IS NULL AND json_type(destination,'$.opencode_attempt_id') IS NULL AND json_type(destination,'$.opencode_attempt_timestamp') IS NULL AND json_type(destination,'$.opencode_attempt_timestamp_kind') IS NULL AND EXISTS(SELECT 1 FROM opencode_deliveries d JOIN messages m ON m.id=d.id WHERE d.id=message_context.id AND d.phase='dispatched' AND d.attempt IS NOT NULL AND m.state='sending')", [now()])?;
            tx.execute("UPDATE messages SET state='uncertain', detail='Service stopped during delivery; inspect the native conversation before retrying.' WHERE state='sending' AND NOT EXISTS(SELECT 1 FROM outbound_transfers o WHERE o.id=messages.id)", [])?;
            // Requalify interrupted deliveries for read-only reconciliation.
            tx.execute("UPDATE opencode_deliveries SET phase='uncertain' WHERE phase='dispatched' AND id IN (SELECT id FROM messages WHERE state='uncertain')", [])?;
            tx.commit()?;
        }
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
        let agent = self.agent(agent)?;
        crate::clients::validate_remote_session(&agent.provider, &agent.session)?;
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
        Ok(self.conn.query_row("SELECT EXISTS(SELECT 1 FROM peers p JOIN peer_grants g ON p.peer_id=g.peer_id WHERE p.peer_id=?1 AND p.revoked=0 AND g.provider=?2 AND g.session=?3 AND g.permission=?4)", params![peer,provider.label(),session,permission.label()], |r|r.get(0))?)
    }

    fn binding_endpoint(&self, provider: &Provider, session: &str) -> Result<String> {
        match provider {
            Provider::Claude => Ok(self
                .conn
                .query_row(
                    "SELECT endpoint FROM claude_bindings WHERE session=?",
                    [session],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or_default()),
            _ => Ok(String::new()),
        }
    }

    fn config_agent(&self, name: &str) -> Result<Option<Agent>> {
        let inbox: Option<String> = self
            .conn
            .query_row("SELECT session FROM agents WHERE name=?", [name], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(session) = inbox {
            return Ok(Some(Agent {
                name: name.into(),
                provider: Provider::Inbox,
                session,
                endpoint: String::new(),
            }));
        }
        let alias: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT provider,session FROM route_aliases WHERE name=?",
                [name],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((provider, session)) = alias else {
            return Ok(None);
        };
        let provider = provider.parse::<Provider>()?;
        Ok(Some(Agent {
            name: name.into(),
            endpoint: self.binding_endpoint(&provider, &session)?,
            provider,
            session,
        }))
    }

    /// Explicit configuration or a reserved canonical provider ID. Native
    /// existence is verified by the connector state boundary, never here.
    pub fn agent(&self, name: &str) -> Result<Agent> {
        if let Some(agent) = self.config_agent(name)? {
            return Ok(agent);
        }
        if let Some((provider, session)) = canonical_route(name)? {
            return Ok(Agent {
                name: name.into(),
                endpoint: self.binding_endpoint(&provider, &session)?,
                provider,
                session,
            });
        }
        bail!("Unknown agent; register it first")
    }

    pub fn agents(&self) -> Result<Vec<Agent>> {
        self.listed_agents(true)
    }

    /// Local manual Inbox configuration plus explicit stable aliases. This is
    /// route configuration, not a native catalog or availability snapshot.
    pub fn listed_agents(&self, all: bool) -> Result<Vec<Agent>> {
        let mut agents = Vec::new();
        let mut inbox = self.conn.prepare(
            "SELECT name,provider,session FROM agents WHERE ?1 OR enabled=1 ORDER BY name",
        )?;
        for row in inbox.query_map([all], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })? {
            let (name, provider, session) = row?;
            agents.push(Agent {
                name,
                provider: provider.parse()?,
                session,
                endpoint: String::new(),
            });
        }
        let mut aliases = self
            .conn
            .prepare("SELECT name,provider,session FROM route_aliases ORDER BY name")?;
        for row in aliases.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })? {
            let (name, provider, session) = row?;
            let provider = provider.parse::<Provider>()?;
            agents.push(Agent {
                name,
                endpoint: self.binding_endpoint(&provider, &session)?,
                provider,
                session,
            });
        }
        agents.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(agents)
    }

    pub fn register(&mut self, agent: &Agent, replace: bool) -> Result<()> {
        ensure!(
            valid_name(&agent.name),
            "Agent names use 1–100 letters, digits, dots, underscores or hyphens"
        );
        ensure!(
            agent.name != "user" || agent.provider == Provider::Inbox,
            "The name user is reserved for your inbox"
        );
        crate::clients::validate_session(&agent.provider, &agent.session)?;
        ensure!(
            canonical_route(&agent.name)?.is_none(),
            "Canonical provider session IDs are reserved; choose an explicit alias"
        );
        let tx = self.conn.transaction()?;
        let inbox: Option<String> = tx
            .query_row(
                "SELECT session FROM agents WHERE name=?",
                [&agent.name],
                |row| row.get(0),
            )
            .optional()?;
        let alias: Option<(String, String)> = tx
            .query_row(
                "SELECT session,provider FROM route_aliases WHERE name=?",
                [&agent.name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let current = inbox.map(|session| (Provider::Inbox, session)).or_else(|| {
            alias.as_ref().and_then(|(session, provider)| {
                provider
                    .parse::<Provider>()
                    .ok()
                    .map(|provider| (provider, session.clone()))
            })
        });
        if let Some((provider, session)) = current
            && (provider != agent.provider || session != agent.session)
        {
            ensure!(
                replace,
                "Name already belongs to another session; choose another name or explicitly use --replace"
            );
            tx.execute("UPDATE messages SET state='uncertain',detail='Recipient registration changed during attempted OpenCode reconciliation',updated=? WHERE recipient=? AND state='queued' AND id IN(SELECT id FROM opencode_deliveries WHERE phase<>'queued')",params![now(),agent.name])?;
            tx.execute("UPDATE messages SET state='failed',detail='Recipient registration was replaced',updated=? WHERE recipient=? AND state='queued'", params![now(), agent.name])?;
        }
        // One name resolves to one configuration row. Canonical native routes
        // never enter either table.
        tx.execute("DELETE FROM route_aliases WHERE name=?", [&agent.name])?;
        tx.execute("DELETE FROM agents WHERE name=?", [&agent.name])?;
        if agent.provider == Provider::Inbox {
            ensure!(
                agent.endpoint.is_empty(),
                "Inbox registrations do not accept a socket endpoint"
            );
            tx.execute(
                "INSERT INTO agents(name,provider,session,endpoint,enabled) VALUES(?1,'inbox',?2,'',1)",
                params![agent.name, agent.session],
            )?;
        } else {
            tx.execute(
                "INSERT INTO route_aliases(name,provider,session) VALUES(?1,?2,?3)",
                params![agent.name, agent.provider.label(), agent.session],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn unregister(&self, name: &str, session: &str) -> Result<()> {
        ensure!(name != "user", "Cannot unregister the user inbox");
        let inbox: Option<String> = self
            .conn
            .query_row("SELECT session FROM agents WHERE name=?", [name], |row| {
                row.get(0)
            })
            .optional()?;
        if let Some(stored) = inbox {
            ensure!(
                stored == session,
                "Inbox registration session does not match"
            );
            self.conn.execute(
                "UPDATE agents SET enabled=0 WHERE name=? AND session=?",
                params![name, session],
            )?;
            return Ok(());
        }
        let alias: Option<String> = self
            .conn
            .query_row(
                "SELECT session FROM route_aliases WHERE name=?",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(stored) = alias {
            ensure!(stored == session, "Route alias session does not match");
            self.conn.execute(
                "DELETE FROM route_aliases WHERE name=? AND session=?",
                params![name, session],
            )?;
            return Ok(());
        }
        ensure!(
            canonical_route(name)?.is_none(),
            "Canonical native session routes cannot be unregistered; no native membership is stored"
        );
        bail!("Unknown local registration")
    }

    pub(crate) fn inbox_enabled(&self, name: &str, session: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM agents WHERE name=? AND session=? AND enabled=1)",
            params![name, session],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn claude_binding_sessions(&self) -> Result<Vec<String>> {
        Ok(self
            .conn
            .prepare("SELECT session FROM claude_bindings WHERE endpoint<>'' ORDER BY session")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn claude_bind(&self, session: &str, endpoint: &str) -> Result<()> {
        crate::clients::validate_session(&Provider::Claude, session)?;
        let normalized = crate::clients::claude::validate_socket(Path::new(endpoint))?
            .to_string_lossy()
            .into_owned();
        self.conn.execute(
            "INSERT INTO claude_bindings(session,endpoint) VALUES(?,?)
             ON CONFLICT(session) DO UPDATE SET endpoint=excluded.endpoint",
            params![session, normalized],
        )?;
        Ok(())
    }

    pub fn claude_unbind(&self, session: &str, endpoint: &str) -> Result<()> {
        crate::clients::validate_session(&Provider::Claude, session)?;
        let normalized = normalize_claude_endpoint(endpoint)?;
        let changed = self.conn.execute(
            "DELETE FROM claude_bindings WHERE session=? AND endpoint=?",
            params![session, normalized],
        )?;
        ensure!(
            changed == 1,
            "Claude binding was already removed or replaced"
        );
        Ok(())
    }

    /// The single explicit durable message writer.
    pub(crate) fn insert_message(
        &self,
        id: &str,
        source: &Agent,
        destination: &Agent,
        body: &str,
        parent: &Option<String>,
        hops: u32,
    ) -> Result<Message> {
        insert_message_in(&self.conn, id, source, destination, body, parent, hops)?;
        self.get(id)
    }

    /// Configuration-only test/administration helper. It resolves stable routes
    /// without native I/O and delegates to the prepared writer.
    pub fn send_names(
        &self,
        id: &str,
        sender: &str,
        recipient: &str,
        body: &str,
        parent: &Option<String>,
    ) -> Result<Message> {
        let source = self.agent(sender)?;
        let destination = self.agent(recipient)?;
        self.send(id, &source, &destination, body, parent)
    }

    /// Duplicate decode before any native lookup or probe. A current explicit
    /// alias must still name the stored immutable identity; rebound routes conflict.
    pub(crate) fn send_duplicate(
        &self,
        id: &str,
        sender: &str,
        recipient: &str,
        body: &str,
        parent: &Option<String>,
    ) -> Result<Option<Message>> {
        let Some(old) = self.get_optional(id)? else {
            return Ok(None);
        };
        if recipient.contains('/') {
            let request: Option<(String, String)> = self
                .conn
                .query_row(
                    "SELECT requested_sender,requested_recipient FROM outbound_transfers WHERE id=?",
                    [id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            ensure!(
                request.is_some_and(|(stored_sender, stored_recipient)| {
                    stored_sender == sender && stored_recipient == recipient
                }) && old.body == body
                    && old.parent == *parent,
                "Message ID already exists with different contents"
            );
            return Ok(Some(old));
        }
        ensure!(
            old.sender == sender
                && old.recipient == recipient
                && old.body == body
                && old.parent == *parent,
            "Message ID already exists with different contents"
        );
        if !recipient.contains('/')
            && let Ok(agent) = self.agent(recipient)
        {
            ensure!(
                agent.session == old.recipient_session,
                "Message ID already exists under a rebound destination route"
            );
        }
        let stored: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(destination,'$.provider') FROM message_context WHERE id=?",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if let Ok(agent) = self.agent(recipient) {
            ensure!(
                stored.is_none_or(|provider| provider == agent.provider.label()),
                "Message ID already exists under a rebound destination provider"
            );
        }
        Ok(Some(old))
    }

    /// `source` and `destination` are prepared request-local identities. This
    /// function performs no native lookup, so duplicates are decided first.
    pub fn send(
        &self,
        id: &str,
        source: &Agent,
        destination: &Agent,
        body: &str,
        parent: &Option<String>,
    ) -> Result<Message> {
        uuid::Uuid::parse_str(id).context("Message ID must be a UUID")?;
        ensure!(
            !body.trim().is_empty() && body.len() <= MAX_BODY,
            "Message must contain 1–65536 bytes"
        );
        if let Some(old) = self.get_optional(id)? {
            let stored: Option<String> = self
                .conn
                .query_row(
                    "SELECT json_extract(destination,'$.provider') FROM message_context WHERE id=?",
                    [id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            ensure!(
                old.sender == source.name
                    && old.recipient == destination.name
                    && old.recipient_session == destination.session
                    && old.body == body
                    && old.parent == *parent
                    && stored.is_none_or(|provider| provider == destination.provider.label()),
                "Message ID already exists with different contents or a rebound route"
            );
            return Ok(old);
        }
        ensure!(
            source.name != destination.name,
            "Sending to yourself is disabled"
        );
        let hops = if let Some(parent) = parent {
            let p = self.get(parent)?;
            ensure!(
                p.recipient == source.name,
                "Only the recipient can reply to a message"
            );
            p.hops + 1
        } else {
            0
        };
        self.insert_message(id, source, destination, body, parent, hops)
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
        let mut cursor = match cursor
            .filter(|value| !value.is_empty() && *value != crate::protocol::START_CURSOR)
        {
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
                .context("Message cursor anchor is missing; restart with an empty or start cursor")?,
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

    pub(crate) fn stored_destination(&self, message: &Message) -> Result<Agent> {
        let provider: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(destination,'$.provider') FROM message_context WHERE id=?",
                [&message.id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let Some(provider) = provider else {
            return self.agent(&message.recipient);
        };
        let provider = provider.parse::<Provider>()?;
        Ok(Agent {
            name: message.recipient.clone(),
            endpoint: self.binding_endpoint(&provider, &message.recipient_session)?,
            provider,
            session: message.recipient_session.clone(),
        })
    }

    // Claim one message per recipient. Native verification is the caller's
    // request-local responsibility; this selects only durable work.
    pub fn claim(&mut self) -> Result<Option<(Message, Agent)>> {
        self.claim_excluding_codex(&[], None)
    }

    pub(crate) fn claim_excluding_codex(
        &mut self,
        busy_codex: &[String],
        ready: Option<&std::collections::HashSet<(String, String)>>,
    ) -> Result<Option<(Message, Agent)>> {
        if self.paused()? {
            return Ok(None);
        }
        self.block_revoked_inbound()?;
        let candidates = {
            let mut statement = self.conn.prepare(
                "SELECT m.id,m.rowid,m.recipient,m.recipient_session,
                 json_extract(c.destination,'$.provider')
                 FROM messages m JOIN message_context c ON c.id=m.id
                 WHERE (m.state='queued'
                    OR (m.state='uncertain' AND EXISTS(SELECT 1 FROM opencode_deliveries d WHERE d.id=m.id AND d.phase='reconcile')))
                   AND NOT EXISTS(SELECT 1 FROM outbound_transfers o WHERE o.id=m.id)
                 ORDER BY m.rowid LIMIT 1000",
            )?;
            statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let time = now();
        for (id, rowid, recipient, session, provider) in candidates {
            let Some(provider) = provider.and_then(|value| value.parse::<Provider>().ok()) else {
                continue;
            };
            if provider == Provider::Codex && busy_codex.contains(&session) {
                continue;
            }
            if !matches!(provider, Provider::Inbox | Provider::OpenCode)
                && ready.is_some_and(|ready| {
                    !ready.contains(&(provider.label().to_string(), session.clone()))
                })
            {
                continue;
            }
            let alias_fence: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM route_aliases WHERE name=? AND (provider<>? OR session<>?))",
                params![recipient, provider.label(), session],
                |row| row.get(0),
            )?;
            if alias_fence {
                continue;
            }
            if let Some((canonical_provider, canonical_session)) = canonical_route(&recipient)?
                && (canonical_provider != provider || canonical_session != session)
            {
                continue;
            }
            if provider == Provider::Inbox {
                let enabled: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM agents WHERE name=? AND session=? AND enabled=1)",
                    params![recipient, session],
                    |row| row.get(0),
                )?;
                if !enabled {
                    continue;
                }
            }
            let busy: bool = self.conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages busy WHERE busy.id<>?1 AND busy.state='sending' AND NOT EXISTS(SELECT 1 FROM outbound_transfers o WHERE o.id=busy.id) AND (busy.recipient=?2 OR (?3='codex' AND busy.recipient_session=?4)))",
                params![id, recipient, provider.label(), session],
                |row| row.get(0),
            )?;
            if busy {
                continue;
            }
            let mut endpoint = String::new();
            if provider == Provider::OpenCode {
                let delivery: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM opencode_deliveries WHERE id=?)",
                    [&id],
                    |row| row.get(0),
                )?;
                if !delivery {
                    continue;
                }
                let earlier: bool = self.conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM opencode_deliveries head JOIN messages h ON h.id=head.id WHERE head.session=?1 AND h.id<>?2 AND (h.state='sending' OR (h.state='queued' AND head.phase='queued' AND h.rowid<?3)))",
                    params![session, id, rowid],
                    |row| row.get(0),
                )?;
                if earlier {
                    continue;
                }
            } else if provider == Provider::Claude {
                endpoint = self.binding_endpoint(&provider, &session)?;
            }
            self.conn.execute(
                "UPDATE messages SET state='sending',updated=? WHERE id=? AND state IN ('queued','uncertain')",
                params![time, id],
            )?;
            let msg = self.get(&id)?;
            let agent = Agent {
                name: recipient,
                provider,
                session,
                endpoint,
            };
            return Ok(Some((msg, agent)));
        }
        Ok(None)
    }

    pub fn finish(&self, id: &str, state: &str, detail: &str) -> Result<()> {
        // Only the worker that still owns an in-flight delivery can finish it.
        self.conn.execute(
            "UPDATE messages SET state=?,detail=?,updated=? WHERE id=? AND state='sending'",
            params![state, detail, now(), id],
        )?;
        Ok(())
    }

    /// A local message the claim loop can take now. Keep in step with the candidate
    /// query in `claim_excluding_codex`: a narrower predicate here would strand work.
    pub fn has_claimable(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages m WHERE (m.state='queued' OR (m.state='uncertain' AND EXISTS(SELECT 1 FROM opencode_deliveries d WHERE d.id=m.id AND d.phase='reconcile'))) AND NOT EXISTS(SELECT 1 FROM outbound_transfers o WHERE o.id=m.id))",
            [],
            |row| row.get(0),
        )?)
    }

    /// Time until the reconcile sweep next has work (same scope as
    /// `opencode_sweep_uncertain`, keep the two in step); None when nothing
    /// time-driven is pending.
    pub fn next_reconcile_in(&self) -> Result<Option<std::time::Duration>> {
        let due: Option<i64> = self.conn.query_row(
            "SELECT min(coalesce(d.reconcile_at,0)) FROM opencode_deliveries d JOIN messages m ON m.id=d.id AND m.recipient_session=d.session WHERE d.phase IN ('uncertain','dispatched') AND m.state='uncertain' AND NOT EXISTS(SELECT 1 FROM message_context c WHERE c.id=d.id AND (json_extract(c.destination,'$.control_notice')=1 OR json_extract(c.destination,'$.event_control') IS NOT NULL))",
            [],
            |row| row.get(0),
        )?;
        Ok(due.map(|due| std::time::Duration::from_millis((due - now()).max(1000) as u64)))
    }

    pub fn retry(&self, id: &str, allow_duplicate: bool) -> Result<Message> {
        if let Some(message) = self.retry_transfer(id)? {
            return Ok(message);
        }
        let topic_work: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM topic_push_work WHERE message_id=?)",
            [id],
            |row| row.get(0),
        )?;
        if topic_work {
            let phase: Option<String> = self
                .conn
                .query_row(
                    "SELECT phase FROM opencode_deliveries WHERE id=?",
                    [id],
                    |row| row.get(0),
                )
                .optional()?;
            if phase.as_deref() == Some("queued") {
                let message = self.get(id)?;
                self.topic_push_authorize(id, &self.stored_destination(&message)?)?;
            }
        }
        if let Some(message) = self.opencode_retry(id)? {
            return Ok(message);
        }
        let m = self.get(id)?;
        if topic_work {
            ensure!(
                !matches!(m.state.as_str(), "uncertain" | "submitted"),
                "Topic push has an ambiguous native outcome; it cannot be replayed"
            );
            self.topic_push_authorize(id, &self.stored_destination(&m)?)?;
        }
        ensure!(
            ["failed", "blocked"].contains(&m.state.as_str())
                || (allow_duplicate && ["uncertain", "submitted"].contains(&m.state.as_str())),
            "Only failed or blocked deliveries can be retried directly; submitted/uncertain messages require --allow-duplicate after checking the native conversation"
        );
        let destination = self.stored_destination(&m)?;
        ensure!(
            destination.session == m.recipient_session,
            "Recipient session changed; create a new message explicitly"
        );
        if let Ok(current) = self.agent(&m.recipient) {
            ensure!(
                current.provider == destination.provider && current.session == destination.session,
                "Recipient route was rebound; create a new message explicitly"
            );
        }
        self.codex_retry_admission(&m, &destination)?;
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
            "UPDATE messages SET state='cancelled',detail='Cancelled before native delivery; no automatic retry',updated=? WHERE id=?",
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
            .prepare("SELECT m.state,coalesce(json_extract(o.envelope,'$.destination.provider'),json_extract(c.destination,'$.provider')),d.phase,count(*) FROM messages m LEFT JOIN message_context c ON c.id=m.id LEFT JOIN outbound_transfers o ON o.id=m.id LEFT JOIN opencode_deliveries d ON d.id=m.id GROUP BY 1,2,3")?;
        let pairs = s
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut counts = std::collections::BTreeMap::<&str, i64>::new();
        for (state, provider, phase, count) in pairs {
            let outcome =
                crate::protocol::delivery_outcome(&state, provider.as_deref(), phase.as_deref());
            *counts.entry(outcome).or_default() += count;
        }
        Ok(serde_json::to_value(counts)?)
    }
}

pub(crate) fn insert_message_in(
    conn: &Connection,
    id: &str,
    source: &Agent,
    destination: &Agent,
    body: &str,
    parent: &Option<String>,
    hops: u32,
) -> Result<()> {
    ensure!(
        hops <= MAX_REPLY_HOPS,
        "Conversation reached the {MAX_REPLY_HOPS}-hop limit; ask the user to start a new exchange"
    );
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM messages WHERE state='queued'",
        [],
        |r| r.get(0),
    )?;
    ensure!(count < 1000, "Queue is full (1000 pending messages)");
    crate::codex_admission::check(conn, &destination.provider, &destination.session, None)?;
    let local_opencode =
        destination.provider == Provider::OpenCode && !destination.name.contains('/');
    if local_opencode {
        let pending: i64 = conn.query_row(
                "SELECT count(*) FROM messages m JOIN opencode_deliveries d ON d.id=m.id WHERE d.session=? AND m.state IN ('queued','sending','uncertain')",
                [&destination.session],
                |row| row.get(0),
            )?;
        ensure!(
            pending < 64,
            "OpenCode session queue is full (64 pending messages)"
        );
    }
    let machine: Option<String> = conn
        .query_row(
            "SELECT value FROM settings WHERE key='machine_name'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let snapshot = |agent: &Agent| {
        json!({
            "peer_id": Value::Null,
            "machine_name": machine,
            "agent": agent.name,
            "provider": agent.provider.label(),
            "session": agent.session,
        })
    };
    let t = now();
    // Caller transactions/savepoints stay owned by the caller.
    let own_transaction = conn.is_autocommit();
    let tx = if own_transaction {
        Some(conn.unchecked_transaction()?)
    } else {
        None
    };
    let target = tx.as_ref();
    let exec = |sql: &str, values: &[&dyn rusqlite::ToSql]| -> Result<usize> {
        Ok(match target {
            Some(tx) => tx.execute(sql, values)?,
            None => conn.execute(sql, values)?,
        })
    };
    exec(
        "INSERT INTO messages VALUES(?1,?2,?3,?4,?5,?6,?7,'queued',NULL,?8,?8)",
        &[
            &id,
            &source.name,
            &destination.name,
            &destination.session,
            &body,
            parent,
            &hops,
            &t,
        ],
    )?;
    let source_json = serde_json::to_string(&snapshot(source))?;
    let destination_json = serde_json::to_string(&snapshot(destination))?;
    exec(
        "INSERT INTO message_context(id,source,destination) VALUES(?1,?2,?3)",
        &[&id, &source_json, &destination_json],
    )?;
    if local_opencode {
        let namespace: String = match target {
            Some(tx) => tx.query_row(
                "SELECT value FROM settings WHERE key='opencode_namespace'",
                [],
                |row| row.get(0),
            )?,
            None => conn.query_row(
                "SELECT value FROM settings WHERE key='opencode_namespace'",
                [],
                |row| row.get(0),
            )?,
        };
        exec(
            "INSERT INTO opencode_deliveries(id,session,namespace,native_id,content,attempt,phase,deadline) VALUES(?1,?2,?3,'pending',NULL,NULL,'queued',?4)",
            &[&id, &destination.session, &namespace, &(t + 120_000)],
        )?;
    }
    if let Some(tx) = tx {
        tx.commit()?;
    }
    Ok(())
}
