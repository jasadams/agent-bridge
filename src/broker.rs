use crate::{
    clients::{ConnectorState, NativeState},
    db::Db,
    delivery,
    network::identity::Identity,
    protocol::{Agent, MAX_FRAME, Provider, Request, Response},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Notify, Semaphore},
    task::JoinSet,
    time::timeout,
};

// Keep the exact-session concurrency fence until the native worker is joined,
// even when its durable outcome has already been stored.
#[derive(Default)]
struct CodexWorkers(std::collections::HashMap<tokio::task::Id, String>);
impl CodexWorkers {
    fn sessions(&self) -> Vec<String> {
        self.0.values().cloned().collect()
    }
    fn track(&mut self, id: tokio::task::Id, agent: &crate::protocol::Agent) {
        if agent.provider == Provider::Codex {
            self.0.insert(id, agent.session.clone());
        }
    }
    fn completed<T>(
        &mut self,
        result: &std::result::Result<(tokio::task::Id, T), tokio::task::JoinError>,
    ) {
        let id = match result {
            Ok((id, _)) => *id,
            Err(error) => error.id(),
        };
        self.0.remove(&id);
    }
}

enum PreparedSend {
    Duplicate(crate::protocol::Message),
    Local(Agent, Agent),
    Remote(Agent, Option<crate::network::wire::RemoteAgent>, String),
}

#[derive(Clone)]
pub(crate) struct NetworkSnapshot {
    pub generation: u64,
    pub config: crate::network::store::NetworkConfig,
    pub relays: Vec<crate::network::relay_config::RelayNode>,
    pub handle: crate::network::transport::Handle,
}

#[derive(Clone)]
pub struct Broker {
    service_lock: std::sync::Weak<File>,
    pub(crate) db: Arc<Mutex<Db>>,
    pub(crate) monitor_control: Arc<Mutex<crate::monitor::Control>>,
    pub(crate) wake: Arc<Notify>,
    pub(crate) network_wake: Arc<Notify>,
    pub(crate) pairing_notifications: Arc<Mutex<crate::pairing_notifications::State>>,
    pub(crate) board_notifications: Arc<Mutex<crate::board_notifications::State>>,
    pub(crate) lan_pairing: Arc<Mutex<Option<crate::network::lan_pairing::Manager>>>,
    pub(crate) pairing_routes:
        Arc<Mutex<std::collections::BTreeMap<String, Vec<libp2p::Multiaddr>>>>,
    pub(crate) network_status: Arc<Mutex<Value>>,
    pub(crate) network_handle: Arc<Mutex<Option<NetworkSnapshot>>>,
    pub(crate) network_generation: Arc<AtomicU64>,
    pub(crate) pairing_generation: Arc<AtomicU64>,
    pub(crate) contact_routes: Arc<Mutex<std::collections::BTreeMap<String, (String, String)>>>,
    pub state_dir: PathBuf,
    codex_bin: PathBuf,
    discovery_status: Arc<Mutex<Value>>,
    claude_state: Arc<dyn ConnectorState>,
    codex_state: Arc<crate::clients::codex_discovery::State>,
}

pub fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    let meta = std::fs::symlink_metadata(path)?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "State directory must be a real directory"
    );
    ensure!(
        meta.uid() == unsafe { libc::geteuid() },
        "State directory belongs to another user"
    );
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}

pub struct BoundServer {
    pub listener: UnixListener,
    pub broker: Broker,
    _lock: Arc<File>,
    socket: PathBuf,
}
impl Drop for BoundServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

impl Broker {
    fn pairing_addresses(&self) -> Vec<String> {
        self.network_status.lock().unwrap()["listening"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|a| crate::network::store::validate_address(a, false).is_ok())
                    .take(4)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn local_machine(&self, db: &Db) -> Result<Value> {
        let machine_name = db.machine_name()?;
        // Keep local-only use independent of identity creation or a damaged network key.
        let peer_id = if self.state_dir.join("identity.key").exists() {
            Identity::load_or_create(&self.state_dir)
                .ok()
                .map(|identity| identity.info().peer_id)
        } else {
            None
        };
        Ok(
            json!({"peer_id":peer_id,"machine_name":machine_name,"peer_alias":null,"display_name":machine_name}),
        )
    }

    fn message_machine(&self, db: &Db, address: &str) -> Result<Value> {
        if let Some((peer_id, _)) = address.split_once('/') {
            let peer = db.peer(peer_id)?;
            Ok(
                json!({"peer_id":peer.peer_id,"machine_name":peer.machine_name,"peer_alias":peer.alias,"display_name":peer.display_name}),
            )
        } else {
            self.local_machine(db)
        }
    }

    /// The one agent resolver: route ID -> {id, name, provider, machine}, read on demand from the native
    /// client. Nothing is cached. `remote_label` is the untrusted name a remote sender attached. Never call
    /// this with the DB lock held; native reads block.
    pub(crate) fn resolve_agent(
        &self,
        id: &str,
        remote_label: Option<&str>,
    ) -> crate::naming::AgentRef {
        use crate::naming::{AgentRef, fallback_label, sanitize_label};
        if let Some((peer_id, name)) = id.split_once('/') {
            let machine = self
                .db
                .lock()
                .unwrap()
                .peer(peer_id)
                .ok()
                .and_then(|p| sanitize_label(&p.display_name))
                .unwrap_or_else(|| peer_id.to_owned());
            let route = crate::db::canonical_route(name).ok().flatten();
            let label = remote_label
                .and_then(sanitize_label)
                .or_else(|| match &route {
                    Some((provider, session)) => Some(fallback_label(provider, session)),
                    None => sanitize_label(name),
                });
            return AgentRef {
                id: id.into(),
                name: label.unwrap_or_else(|| name.into()),
                provider: route.map(|(provider, _)| provider),
                machine,
                local: false,
            };
        }
        let (agent, machine) = {
            let db = self.db.lock().unwrap();
            (db.agent(id).ok(), db.machine_name().unwrap_or_default())
        };
        let name = agent.as_ref().and_then(|agent| {
            if id != crate::clients::lifecycle::routing_name(&agent.provider, &agent.session) {
                // An explicit registration alias is already the agent's chosen name.
                return sanitize_label(id);
            }
            self.native_get(&agent.provider, &agent.session)
                .ok()
                .flatten()
                .and_then(|state| state.display_name)
                .and_then(|name| sanitize_label(&name))
                .or_else(|| Some(fallback_label(&agent.provider, &agent.session)))
        });
        AgentRef {
            id: id.into(),
            name: name.unwrap_or_else(|| id.into()),
            provider: agent.map(|agent| agent.provider),
            machine,
            local: true,
        }
    }

    fn message_value(&self, db: &Db, message: &crate::protocol::Message) -> Result<Value> {
        let mut value = json!(message);
        value["state"] = json!(db.delivery_state(message)?);
        if message.state == "blocked" {
            value["detail"] = json!(format!(
                "{}; policy-blocked, restore exact-session permissions and explicitly retry this ID; it will not resume automatically",
                message.detail.as_deref().unwrap_or("Delivery is blocked")
            ));
        }
        value["source_machine"] = self.message_machine(db, &message.sender)?;
        value["destination_machine"] = self.message_machine(db, &message.recipient)?;
        if let Some(transfer) = db.transfer_info(&message.id)? {
            value["transfer"] = transfer;
        }
        if let Some(diagnostic) = db.opencode_attempt_diagnostic(&message.id)? {
            value["opencode_attempt"] = diagnostic;
        }
        Ok(value)
    }

    pub fn bind(state_dir: PathBuf, codex_bin: PathBuf) -> Result<BoundServer> {
        private_dir(&state_dir)?;
        let state_dir = state_dir.canonicalize()?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(state_dir.join("service.lock"))?;
        lock.try_lock_exclusive()
            .context("Another agent-bridge service owns this state directory")?;
        let lock = Arc::new(lock);
        let socket = state_dir.join("bridge.sock");
        ensure!(
            socket.as_os_str().len() < 104,
            "State directory is too long for a macOS socket path; select a shorter --state-dir"
        );
        if let Ok(meta) = std::fs::symlink_metadata(&socket) {
            use std::os::unix::fs::FileTypeExt;
            ensure!(
                meta.file_type().is_socket() && meta.uid() == unsafe { libc::geteuid() },
                "Refusing to replace a non-socket or foreign socket"
            );
            // Hold our lock, but also avoid replacing a live listener created by another program.
            ensure!(
                std::os::unix::net::UnixStream::connect(&socket).is_err(),
                "Socket already has a live listener"
            );
            std::fs::remove_file(&socket)?;
        }
        let dbpath = state_dir.join("messages.sqlite3");
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&dbpath)?;
        std::fs::set_permissions(&dbpath, std::fs::Permissions::from_mode(0o600))?;
        let db = Db::open(&dbpath)?;
        crate::board_lifecycle::initialize(&db)?;
        for error in crate::board_lifecycle::cleanup(&db, &state_dir, crate::db::now())? {
            eprintln!("Board archive recovery: {error}");
        }
        let event_state = crate::board_notifications::State::default();
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let broker = Self {
            service_lock: Arc::downgrade(&lock),
            db: Arc::new(Mutex::new(db)),
            monitor_control: Arc::new(Mutex::new(crate::monitor::Control::default())),
            wake: Arc::new(Notify::new()),
            network_wake: Arc::new(Notify::new()),
            pairing_notifications: Arc::new(Mutex::new(
                crate::pairing_notifications::State::default(),
            )),
            board_notifications: Arc::new(Mutex::new(event_state)),
            lan_pairing: Arc::new(Mutex::new(None)),
            pairing_routes: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            network_status: Arc::new(Mutex::new(json!({"enabled":false}))),
            network_handle: Arc::new(Mutex::new(None)),
            network_generation: Arc::new(AtomicU64::new(0)),
            pairing_generation: Arc::new(AtomicU64::new(0)),
            contact_routes: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
            state_dir,
            codex_bin,
            discovery_status: Arc::new(Mutex::new(json!({"state":"disabled"}))),
            claude_state: Arc::new(crate::clients::claude_discovery::State::new("claude")),
            codex_state: Arc::new(crate::clients::codex_discovery::State::default()),
        };
        Ok(BoundServer {
            listener,
            broker,
            _lock: lock,
            socket,
        })
    }

    /// One connector implementation per provider for this request. OpenCode's
    /// sockets are discovered in the state directory; no DB lock is held while
    /// the native read runs.
    fn connector(&self, provider: &Provider) -> Result<Option<Arc<dyn ConnectorState>>> {
        Ok(match provider {
            Provider::Claude => Some(self.claude_state.clone()),
            Provider::Codex => Some(self.codex_state.clone()),
            Provider::OpenCode => Some(Arc::new(crate::clients::opencode::State::new(
                &self.state_dir,
            ))),
            Provider::Inbox => None,
        })
    }

    pub(crate) fn native_get(
        &self,
        provider: &Provider,
        session: &str,
    ) -> Result<Option<NativeState>> {
        let Some(connector) = self.connector(provider)? else {
            return Ok(None);
        };
        connector
            .get(session)
            .map_err(|error| anyhow::anyhow!("{error}"))
    }

    fn native_list(
        &self,
        provider: &Provider,
        max_age_days: u32,
    ) -> std::result::Result<Vec<NativeState>, crate::clients::StateError> {
        match self.connector(provider) {
            Ok(Some(connector)) => connector.list(max_age_days),
            Ok(None) => Ok(Vec::new()),
            Err(error) => Err(crate::clients::StateError::Unavailable(error.to_string())),
        }
    }

    /// Resolve configured routing identity and enrich transport details from the
    /// connector boundary when available. Native errors never become a false
    /// empty directory; durable work still keeps its immutable identity.
    fn prepare_agent(&self, name: &str, require_present: bool) -> Result<Agent> {
        let agent = {
            let db = self.db.lock().unwrap();
            db.agent(name)?
        };
        if agent.provider == Provider::Inbox {
            if require_present {
                let enabled = self
                    .db
                    .lock()
                    .unwrap()
                    .inbox_enabled(&agent.name, &agent.session)?;
                ensure!(
                    enabled,
                    "Inbox registration is disabled; enable it explicitly before new work"
                );
            }
            return Ok(agent);
        }
        // OpenCode sockets are resolved per delivery, never pinned to the agent.
        if require_present
            && agent.provider != Provider::OpenCode
            && let Ok(Some(state)) = self.native_get(&agent.provider, &agent.session)
        {
            // Enrich only this request from the connector observation. Absence
            // never fabricates a catalog row; durable work remains queued for
            // the immutable identity and is delivered only when it is present.
            return Ok(Agent {
                name: agent.name,
                endpoint: state
                    .transport_endpoint
                    .unwrap_or_else(|| agent.endpoint.clone()),
                provider: agent.provider,
                session: agent.session,
            });
        }
        Ok(agent)
    }

    /// Duplicate readback first, then request-local native verification for a
    /// new local send. The final locked arm repeats the duplicate and route
    /// fences immediately before the durable insert.
    fn prepare_send(
        &self,
        id: &str,
        sender: &str,
        recipient: &str,
        body: &str,
        parent: &Option<String>,
    ) -> Result<PreparedSend> {
        if let Some(message) = self
            .db
            .lock()
            .unwrap()
            .send_duplicate(id, sender, recipient, body, parent)?
        {
            return Ok(PreparedSend::Duplicate(message));
        }
        if recipient.contains('/') {
            let source = self.prepare_agent(sender, true)?;
            let (peer, name) = recipient
                .split_once('/')
                .context("Remote destination must be peer/agent")?;
            let destination = {
                let db = self.db.lock().unwrap();
                db.agent(name)
                    .ok()
                    .map(|agent| crate::network::wire::RemoteAgent {
                        name: name.into(),
                        provider: agent.provider,
                        session: agent.session,
                    })
            };
            let destination = match destination {
                Some(destination) => Some(destination),
                None => self
                    .sync_remote_directory(peer)?
                    .into_iter()
                    .find(|row| row["name"] == name)
                    .and_then(|row| {
                        Some(crate::network::wire::RemoteAgent {
                            name: name.into(),
                            provider: row["provider"].as_str()?.parse().ok()?,
                            session: row["session"].as_str()?.into(),
                        })
                    }),
            };
            let label = self.resolve_agent(&source.name, None).name;
            return Ok(PreparedSend::Remote(source, destination, label));
        }
        let source = self.prepare_agent(sender, true)?;
        let destination = self.prepare_agent(recipient, true)?;
        for (side, agent) in [("Sender", &source), ("Recipient", &destination)] {
            if agent.provider == Provider::OpenCode
                && !matches!(
                    self.native_get(&agent.provider, &agent.session),
                    Ok(Some(_))
                )
            {
                return Err(crate::protocol::SendNotPersisted(format!(
                    "{side} {} is not reachable: no OpenCode plugin answered for its session",
                    agent.name
                ))
                .into());
            }
        }
        Ok(PreparedSend::Local(source, destination))
    }

    fn prepare_list(&self, all: bool, max_age_days: u32) -> Result<Value> {
        ensure!(max_age_days >= 1, "max_age_days must be at least 1");
        let (configured, machine) = {
            let db = self.db.lock().unwrap();
            (db.listed_agents(all)?, self.local_machine(&db)?)
        };
        let mut providers = Vec::new();
        let mut rows: std::collections::BTreeMap<(String, String, String), Value> =
            std::collections::BTreeMap::new();
        let mut native_sessions = std::collections::BTreeSet::new();
        for provider in [
            Provider::Claude,
            Provider::Codex,
            Provider::OpenCode,
            Provider::Inbox,
        ] {
            if provider == Provider::Inbox {
                providers.push(json!({"provider":"inbox","state":"complete"}));
                continue;
            }
            match self.native_list(&provider, max_age_days) {
                Ok(states) => {
                    providers.push(json!({"provider":provider,"state":"complete"}));
                    for state in states {
                        native_sessions
                            .insert((provider.label().to_string(), state.session.clone()));
                        let name =
                            crate::clients::lifecycle::routing_name(&provider, &state.session);
                        rows.entry((name.clone(), provider.label().into(), state.session.clone()))
                            .or_insert_with(|| {
                                json!({
                                    "name":name,
                                    "provider":provider,
                                    "session":state.session,
                                    "display_name":state.display_name,
                                    "status":state.status,
                                    "directory":state.directory,
                                    "updated_ms":state.updated_ms,
                                    "machine_name":machine["machine_name"],
                                    "peer_id":machine["peer_id"],
                                    "machine_display_name":machine["display_name"],
                                })
                            });
                    }
                }
                Err(error) => {
                    let (state, detail) = match error {
                        crate::clients::StateError::Unavailable(detail) => ("unavailable", detail),
                        crate::clients::StateError::Incomplete(detail) => ("incomplete", detail),
                    };
                    providers.push(json!({
                        "provider":provider,
                        "state":state,
                        "detail":detail.chars().take(512).collect::<String>(),
                    }));
                }
            }
        }
        // Explicit aliases appear only when their exact target is present in
        // this request. Manual Inbox rows remain visible configuration.
        for agent in configured {
            if agent.provider == Provider::Inbox {
                let enabled = self
                    .db
                    .lock()
                    .unwrap()
                    .inbox_enabled(&agent.name, &agent.session)?;
                let mut row = json!({
                    "name":agent.name,
                    "provider":"inbox",
                    "session":agent.session,
                    "display_name":Value::Null,
                    "status":Value::Null,
                    "directory":Value::Null,
                    "updated_ms":Value::Null,
                    "machine_name":machine["machine_name"],
                    "peer_id":machine["peer_id"],
                    "machine_display_name":machine["display_name"],
                    "enabled":enabled,
                });
                if !enabled {
                    row["stale_reason"] = json!("disabled");
                }
                rows.insert(
                    (agent.name.clone(), "inbox".into(), agent.session.clone()),
                    row,
                );
                continue;
            }
            if native_sessions.contains(&(agent.provider.label().into(), agent.session.clone())) {
                rows.entry((
                    agent.name.clone(),
                    agent.provider.label().into(),
                    agent.session.clone(),
                ))
                .or_insert_with(|| {
                    json!({
                        "name":agent.name,
                        "provider":agent.provider,
                        "session":agent.session,
                        "display_name":Value::Null,
                        "status":Value::Null,
                        "directory":Value::Null,
                        "updated_ms":Value::Null,
                        "machine_name":machine["machine_name"],
                        "peer_id":machine["peer_id"],
                        "machine_display_name":machine["display_name"],
                    })
                });
            }
        }
        Ok(json!({
            "agents": rows.into_values().collect::<Vec<_>>(),
            "providers": providers,
        }))
    }

    /// Request-local native presence snapshot for claiming durable work. Native
    /// absence leaves queued work untouched; it never becomes a false delivery.
    fn native_readiness(&self) -> std::collections::HashSet<(String, String)> {
        // Bindings are delivery coordinates. Claiming does not probe the socket
        // (that would create payload-free native connections); a dead socket
        // fails the delivery attempt with its existing diagnostic.
        let mut ready: std::collections::HashSet<_> = self
            .db
            .lock()
            .unwrap()
            .claude_binding_sessions()
            .unwrap_or_default()
            .into_iter()
            .map(|session| ("claude".to_string(), session))
            .collect();
        if let Ok(rows) = self.codex_state.list(7) {
            for row in rows {
                ready.insert(("codex".into(), row.session));
            }
        }
        ready
    }

    pub(crate) fn prepare_directory(&self) -> Result<Vec<Agent>> {
        let configured = self.db.lock().unwrap().listed_agents(true)?;
        let mut directory = Vec::new();
        for provider in [Provider::Claude, Provider::Codex, Provider::OpenCode] {
            let Ok(rows) = self.native_list(&provider, 7) else {
                continue;
            };
            for row in rows {
                directory.push(Agent {
                    name: crate::clients::lifecycle::routing_name(&provider, &row.session),
                    provider: provider.clone(),
                    session: row.session,
                    endpoint: String::new(),
                });
            }
        }
        for agent in configured {
            if agent.provider == Provider::Inbox {
                let enabled = self
                    .db
                    .lock()
                    .unwrap()
                    .inbox_enabled(&agent.name, &agent.session)?;
                if enabled {
                    directory.push(agent);
                }
                continue;
            }
            // Explicit aliases are owner configuration. Connector-present
            // canonical Claude/Codex rows were added above; Claude bindings are
            // delivery coordinates, not directory membership.
            directory.push(agent);
        }
        directory.sort_by(|left, right| left.name.cmp(&right.name));
        directory.dedup_by(|left, right| left.name == right.name);
        Ok(directory)
    }

    /// On-demand peer directory. Nothing is persisted; a failed lookup is not a
    /// false empty directory.
    async fn remote_directory(&self, only: Option<&str>) -> Result<Value> {
        let peers = {
            let db = self.db.lock().unwrap();
            ensure!(db.network_config()?.enabled, "Networking is disabled");
            let mut peers = db.peers()?;
            if let Some(only) = only {
                let peer = db.peer(only)?;
                ensure!(!peer.revoked, "Peer is revoked");
                peers.retain(|candidate| candidate.peer_id == peer.peer_id);
            }
            let peers = peers
                .into_iter()
                .filter(|peer| !peer.revoked)
                .collect::<Vec<_>>();
            ensure!(
                !peers.is_empty(),
                "No active paired peers are available for a remote directory"
            );
            peers
        };
        let handle = self
            .network_handle
            .lock()
            .unwrap()
            .clone()
            .context("Network transport is not ready; retry after status shows a connection")?
            .handle;
        let status = self.network_status.lock().unwrap().clone();
        let stored = {
            let db = self.db.lock().unwrap();
            db.network_peers()?
        };
        let mut rows = Vec::new();
        for peer in peers {
            let peer_id: libp2p::PeerId = peer.peer_id.parse()?;
            let mut addresses: Vec<libp2p::Multiaddr> = Vec::new();
            for (id, address) in &stored {
                if id == &peer.peer_id
                    && let Ok(address) = address.parse()
                {
                    addresses.push(address);
                }
            }
            for path in status["paths"].as_array().into_iter().flatten() {
                if path["peer_id"] != peer.peer_id {
                    continue;
                }
                for address in path["addresses"].as_array().into_iter().flatten().take(8) {
                    if let Some(address) = address
                        .as_str()
                        .and_then(|value| value.parse::<libp2p::Multiaddr>().ok())
                    {
                        addresses.push(address);
                    }
                }
            }
            for discovered in status["discovered"].as_array().into_iter().flatten() {
                if discovered["peer_id"] != peer.peer_id {
                    continue;
                }
                for address in discovered["addresses"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .take(8)
                {
                    if let Some(address) = address
                        .as_str()
                        .and_then(|value| value.parse::<libp2p::Multiaddr>().ok())
                    {
                        addresses.push(address);
                    }
                }
            }
            addresses.truncate(8);
            for address in &mut addresses {
                if !address
                    .iter()
                    .any(|part| matches!(part, libp2p::multiaddr::Protocol::P2p(_)))
                {
                    address.push(libp2p::multiaddr::Protocol::P2p(peer_id));
                }
            }
            ensure!(
                !addresses.is_empty(),
                "No authenticated route is available for peer {}; reconnect before listing its directory",
                peer.display_name
            );
            let response = match handle
                .request_addresses(
                    peer_id,
                    addresses.clone(),
                    crate::network::wire::WireRequest::new(
                        crate::network::wire::Operation::DirectorySupported {
                            providers: crate::network::wire::supported_providers(),
                        },
                    ),
                )
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    return Err(error);
                }
            };
            let crate::network::wire::WireResponse::SupportedDirectory { providers, agents } =
                response
            else {
                anyhow::bail!("Peer did not return a supported directory");
            };
            crate::network::wire::validate_providers(&providers)?;
            ensure!(
                agents
                    .iter()
                    .all(|agent| providers.iter().any(|p| p == agent.provider.label())),
                "Directory contains an unadvertised provider"
            );
            for agent in agents {
                rows.push(json!({
                    "peer_id":peer.peer_id,
                    "peer_alias":peer.alias,
                    "machine_name":peer.machine_name,
                    "machine_display_name":peer.display_name,
                    "name":agent.name,
                    "provider":agent.provider,
                    "session":agent.session,
                    "qualified_name":format!("{}/{}",peer.alias.as_deref().unwrap_or(&peer.peer_id),agent.name),
                    "online":true,
                }));
            }
        }
        Ok(json!(rows))
    }

    /// Blocking request paths may ask the active transport for one fresh peer
    /// directory. This is request-local and never persisted.
    fn sync_remote_directory(&self, peer: &str) -> Result<Vec<Value>> {
        let broker = self.clone();
        let peer = peer.to_owned();
        let run = move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(broker.remote_directory(Some(&peer)))
        };
        let value = if tokio::runtime::Handle::try_current().is_ok() {
            std::thread::spawn(run)
                .join()
                .map_err(|_| anyhow::anyhow!("Remote directory worker stopped"))??
        } else {
            run()?
        };
        value
            .as_array()
            .cloned()
            .context("Peer directory response is not an array")
    }

    async fn ready_network(&self, cancel: u64) -> Result<NetworkSnapshot> {
        let expected = {
            let db = self.db.lock().unwrap();
            let config = db.network_config()?;
            ensure!(config.enabled, "Networking is disabled");
            (config, db.relay_nodes()?)
        };
        self.network_wake.notify_one();
        timeout(Duration::from_secs(5), async {
            loop {
                ensure!(
                    self.pairing_generation.load(Ordering::SeqCst) == cancel,
                    "Pairing was cancelled while preparing contact"
                );
                let current = self.network_handle.lock().unwrap().clone();
                if let Some(snapshot) = current
                    && (snapshot.config.clone(), snapshot.relays.clone()) == expected
                {
                    return Ok(snapshot);
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .context("Networking is not ready; inspect status and retry")?
    }
    fn start_contact(
        &self,
        peer: &str,
        addresses: Vec<libp2p::Multiaddr>,
        seconds: u64,
        cancel: u64,
        snapshot: &NetworkSnapshot,
        persist_direct: bool,
    ) -> Result<Value> {
        ensure!(
            !persist_direct || addresses.len() == 1,
            "Direct contact requires one location"
        );
        let db = self.db.lock().unwrap();
        ensure!(
            db.network_config()? == snapshot.config
                && snapshot.config.enabled
                && db.relay_nodes()? == snapshot.relays
                && self.network_generation.load(Ordering::SeqCst) == snapshot.generation,
            "Network configuration changed during contact lookup"
        );
        ensure!(
            self.pairing_generation.load(Ordering::SeqCst) == cancel,
            "Pairing was cancelled during contact lookup"
        );
        let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
        let mut guard = self.lan_pairing.lock().unwrap();
        let manager = guard.get_or_insert(crate::network::lan_pairing::Manager::new(&local)?);
        let view = manager.start(peer, seconds, crate::db::now())?;
        let mut routes = self.pairing_routes.lock().unwrap();
        let active = manager
            .list(crate::db::now())
            .into_iter()
            .map(|v| v.peer)
            .collect::<std::collections::HashSet<_>>();
        routes.retain(|peer, _| active.contains(peer));
        let mut contacts = self.contact_routes.lock().unwrap();
        contacts.retain(|peer, _| active.contains(peer));
        contacts.remove(peer);
        if persist_direct {
            contacts.insert(peer.into(), (view.id.clone(), addresses[0].to_string()));
        }
        drop(contacts);
        routes.insert(peer.into(), addresses);
        let mut result = json!(view);
        result["identity_unverified"] = json!(true);
        drop(guard);
        drop(db);
        self.network_wake.notify_one();
        Ok(result)
    }
    // A started blocking task is not cancelled when its waiter is dropped.
    // Retain exclusive broker ownership until that task has actually finished.
    async fn blocking<T: Send + 'static>(
        &self,
        work: impl FnOnce(Broker) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let ownership = self
            .service_lock
            .upgrade()
            .context("Broker ownership ended")?;
        let broker = self.clone();
        tokio::task::spawn_blocking(move || {
            let _ownership = ownership;
            work(broker)
        })
        .await?
    }

    pub async fn request_async(&self, req: Request) -> Result<Value> {
        match req {
            req @ Request::Register {
                provider: Provider::OpenCode,
                ..
            } => self.blocking(move |broker| broker.request(req)).await,
            Request::MonitorEnable { port } => {
                self.monitor_control.lock().unwrap().enable(port);
                timeout(Duration::from_secs(5), async {
                    loop {
                        let value = self.monitor_control.lock().unwrap().credentials();
                        ensure!(
                            value["requested_port"].as_u64() == Some(port as u64),
                            "Web enable was superseded"
                        );
                        if value["url"].is_string() {
                            return Ok(value);
                        }
                        if let Some(error) = value["last_error"].as_str() {
                            anyhow::bail!("Web interface could not start: {error}");
                        }
                        ensure!(
                            value["enabled"] == true,
                            "Web interface was disabled while starting"
                        );
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .context("Web interface did not start in time; run `agent-bridge web status`")?
            }
            Request::RemoteList => self.remote_directory(None).await,
            Request::RemoteListPeer { peer } => self.remote_directory(Some(&peer)).await,
            Request::ContactLocation { location, seconds } => {
                ensure!(
                    (30..=600).contains(&seconds),
                    "Pairing lifetime must be 30–600 seconds"
                );
                let address = crate::network::relay_config::contact_location(&location)?;
                let cancel = self.pairing_generation.load(Ordering::SeqCst);
                let snapshot = self.ready_network(cancel).await?;
                let peer = snapshot
                    .handle
                    .resolve_contact(address.clone())
                    .await?
                    .to_string();
                self.start_contact(&peer, vec![address], seconds, cancel, &snapshot, true)
            }
            Request::ContactConnect {
                link,
                use_invite_relays,
                seconds,
            } => {
                ensure!(
                    (30..=600).contains(&seconds),
                    "Pairing lifetime must be 30–600 seconds"
                );
                let contact = crate::network::contact::verify(&link, crate::db::now())?;
                let cancel = self.pairing_generation.load(Ordering::SeqCst);
                {
                    let mut db = self.db.lock().unwrap();
                    ensure!(
                        db.network_config()?.enabled,
                        "Enable networking before connecting a contact"
                    );
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    ensure!(
                        contact.peer_id != local,
                        "Cannot pair this bridge with itself"
                    );
                    let configured = db.relay_nodes()?;
                    let unfamiliar = contact
                        .relays
                        .iter()
                        .any(|address| !configured.iter().any(|node| &node.address == address));
                    ensure!(
                        !unfamiliar || use_invite_relays,
                        "This contact introduces new or changed relay addresses; use --use-invite-relays to choose them explicitly"
                    );
                    ensure!(
                        !contact.relays.iter().any(|address| {
                            crate::network::relay_config::validate_relay(address)
                                .is_ok_and(|r| r.peer_id == local)
                        }),
                        "This bridge cannot be its own relay"
                    );
                    if unfamiliar {
                        db.add_relays(&contact.relays)?;
                    }
                }
                let snapshot = self.ready_network(cancel).await?;
                // Expiry is checked again after waiting for infrastructure reconfiguration.
                let contact = crate::network::contact::verify(&link, crate::db::now())?;
                let routes = contact
                    .relays
                    .iter()
                    .map(|relay| format!("{relay}/p2p-circuit/p2p/{}", contact.peer_id).parse())
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                self.start_contact(&contact.peer_id, routes, seconds, cancel, &snapshot, false)
            }
            other => self.blocking(move |broker| broker.request(other)).await,
        }
    }

    pub fn request(&self, req: Request) -> Result<Value> {
        // Complete all native/provider I/O before taking the DB administration
        // lock. These snapshots are request-local and discarded on return.
        let prepared_send = match &req {
            Request::Send {
                id,
                sender,
                recipient,
                body,
                parent,
            } => Some(self.prepare_send(id, sender, recipient, body, parent)?),
            _ => None,
        };
        let prepared_list = match &req {
            Request::List { all, max_age_days } => Some(self.prepare_list(*all, *max_age_days)?),
            _ => None,
        };
        let board_preflight = match &req {
            Request::BoardMessageSend { request } => Some(self.board_send_preflight(request)?),
            _ => None,
        };
        let prepared_subscribe = match &req {
            Request::Subscribe { agent, .. } => Some(self.prepare_agent(agent, true)?),
            _ => None,
        };
        let mut db = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("Database lock poisoned"))?;
        {
            let mut state = self.board_notifications.lock().unwrap();
            state.prune(|name| db.agent(name).ok());
        }
        let req = match req {
            Request::BoardRead => Request::NamedBoardRead {
                board: Default::default(),
            },
            Request::BoardUpdate {
                expected_revision,
                edits,
                actor,
            } => Request::NamedBoardUpdate {
                board: Default::default(),
                expected_revision,
                edits,
                actor,
            },
            request => request,
        };
        let result = (|| -> Result<Value> {
            Ok(match req {
                Request::TopicPushGet { topic, agent } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    let mut value = json!(db.topic_push_get(&local, &topic, &agent)?);
                    value["delivery"] = json!(db.topic_push_diagnostics(&local, &topic, &agent)?);
                    value
                }
                Request::TopicPushSet { topic, agent, mode } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.topic_push_set(&local, &topic, &agent, &mode)?)
                }
                Request::TopicCreate { name } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.topic_create(&local, &name, &Default::default())?)
                }
                Request::TopicList { include_archived } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!({"topics": db.topic_list(&local, include_archived)?})
                }
                Request::TopicInspect { topic } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.topic_inspect(&local, &topic)?)
                }
                Request::TopicRename { topic, name } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.topic_rename(&local, &topic, &name)?)
                }
                Request::TopicArchive { topic } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.topic_archive(&local, &topic, true)?)
                }
                Request::TopicRestore { topic } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.topic_archive(&local, &topic, false)?)
                }
                Request::TopicPublish {
                    topic,
                    id,
                    author,
                    body,
                    parent,
                    mentions,
                } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.topic_publish(
                        &local,
                        &crate::topics::TopicPublication {
                            topic,
                            id,
                            author,
                            body,
                            parent,
                            mentions,
                        },
                        &Default::default(),
                    )?)
                }
                Request::TopicRead {
                    topic,
                    cursor,
                    before,
                    limit,
                } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.topic_read(
                        &local,
                        &topic,
                        cursor.as_deref(),
                        before.as_deref(),
                        limit
                    )?)
                }
                // Board edits share the broker's local administration mutex. No
                // network request can invoke this API or select a filesystem path.
                Request::BoardMessageSend { request } => {
                    if let Some(detail) = board_preflight.flatten() {
                        crate::board_messages::Reply::error(503, "unavailable", detail).body
                    } else {
                        self.board_message_submit_locked(&db, &request).body
                    }
                }
                Request::AddBoardMessageInput {
                    agent,
                    board,
                    expected_revision,
                    id,
                    label,
                } => {
                    let actor = db.agent(&agent)?;
                    if actor.provider == Provider::Inbox {
                        ensure!(
                            db.inbox_enabled(&actor.name, &actor.session)?,
                            "Message input owner must be enabled"
                        );
                    }
                    let marker = crate::board_messages::Marker {
                        id,
                        instance: uuid::Uuid::new_v4().to_string(),
                        label,
                        owner: crate::board_messages::Owner {
                            name: actor.name.clone(),
                            provider: actor.provider.clone(),
                            session: actor.session.clone(),
                            endpoint: actor.endpoint.clone(),
                        },
                    };
                    ensure!(
                        marker.valid(),
                        "Invalid message input identity, ID or label"
                    );
                    let current = self.read_board_locked(&db, &board)?;
                    let text = format!(
                        "{}<!-- board-message: {} -->\n",
                        if current.markdown.ends_with('\n') || current.markdown.is_empty() {
                            ""
                        } else {
                            "\n"
                        },
                        serde_json::to_string(&marker)?
                    );
                    let candidate = format!("{}{text}", current.markdown);
                    ensure!(crate::board_interaction::controls(&candidate).iter().any(|control| matches!(&control.kind, crate::board_interaction::Kind::Message { id, instance, owner } if id == &marker.id && instance == &marker.instance && owner == &marker.owner) && control.label == marker.label), "Message input ID is duplicated or board controls are malformed");
                    match self.update_board_locked(
                        &db,
                        &board,
                        &expected_revision,
                        &[crate::board::Edit::Append { text }],
                        Some(&actor),
                    ) {
                        Ok(board) => json!({"applied":true,"board":board}),
                        Err(error) => {
                            if let Some(conflict) = error.downcast_ref::<crate::board::Conflict>() {
                                json!({"applied":false,"conflict":true,"board":conflict.current})
                            } else {
                                return Err(error);
                            }
                        }
                    }
                }
                Request::BoardList => json!(self.list_boards_locked(&db)?),
                Request::BoardArchivedList => json!(crate::board_lifecycle::archived(
                    &db,
                    &self.state_dir,
                    crate::db::now()
                )?),
                Request::BoardInspect { board } => {
                    let snapshot = crate::board_lifecycle::inspect(
                        &db,
                        &self.state_dir,
                        &board,
                        crate::db::now(),
                    )?;
                    let labels = crate::board_messages::display_labels(&db, &snapshot)?;
                    let mut value = json!(snapshot);
                    value["message_labels"] = json!(labels);
                    value
                }
                Request::BoardArchive {
                    board,
                    expected_revision,
                } => {
                    json!(self.archive_board_locked(
                        &db,
                        &board,
                        &expected_revision,
                        crate::db::now()
                    )?)
                }
                Request::BoardRestore {
                    board,
                    expected_revision,
                } => {
                    json!(crate::board_lifecycle::restore(
                        &db,
                        &self.state_dir,
                        &board,
                        &expected_revision,
                        crate::db::now()
                    )?)
                }
                Request::BoardCreate { board } => {
                    json!(crate::board_lifecycle::create(
                        &db,
                        &self.state_dir,
                        &board
                    )?)
                }
                Request::NamedBoardRead { board } => {
                    json!(self.read_board_locked(&db, &board)?)
                }
                Request::BoardRead | Request::BoardUpdate { .. } => {
                    unreachable!("Legacy board requests normalized above")
                }
                Request::NamedBoardUpdate {
                    board: name,
                    expected_revision,
                    edits,
                    actor,
                } => {
                    let actor = actor.map(|name| db.agent(&name)).transpose()?;
                    if let Some(agent) = &actor
                        && agent.provider == Provider::Inbox
                    {
                        ensure!(
                            db.inbox_enabled(&agent.name, &agent.session)?,
                            "Board writer must be an enabled local registration"
                        );
                    }
                    match self.update_board_locked(
                        &db,
                        &name,
                        &expected_revision,
                        &edits,
                        actor.as_ref(),
                    ) {
                        Ok(board) => {
                            json!({"applied":true,"board":board})
                        }
                        Err(error) => {
                            if let Some(conflict) = error.downcast_ref::<crate::board::Conflict>() {
                                json!({"applied":false,"conflict":true,"board":conflict.current,"message":"The board changed. Review this current snapshot and retry your edits with its revision."})
                            } else {
                                return Err(error);
                            }
                        }
                    }
                }
                Request::Subscribe {
                    agent: _,
                    events,
                    board_revisions,
                } => {
                    self.validate_board_fences_locked(&db, &events, &board_revisions)?;
                    let prepared =
                        prepared_subscribe.context("Subscribe preparation is missing")?;
                    let mut boards = std::collections::BTreeMap::new();
                    ensure!(
                        !events.is_empty() && events.len() <= 64,
                        "Provide between 1 and 64 event selectors"
                    );
                    for event in &events {
                        let crate::protocol::EventSelector::BoardChanged { board } = event;
                        boards.insert(board.clone(), self.read_board_locked(&db, board)?.revision);
                    }
                    let recovery = db.event_recovery(Some(&prepared))?;
                    let mut state = self.board_notifications.lock().unwrap();
                    state.prune(|name| db.agent(name).ok());
                    state.subscribe(
                        prepared.clone(),
                        &events,
                        &boards,
                        std::time::Instant::now(),
                    )?;
                    let effective = state.events(&prepared.name);
                    json!({"agent":prepared.name,"events":effective,"persistent":false,"recovery":recovery})
                }
                Request::Unsubscribe {
                    agent,
                    events,
                    board_revisions,
                } => {
                    self.validate_board_fences_locked(
                        &db,
                        events.as_deref().unwrap_or_default(),
                        &board_revisions,
                    )?;
                    if let Some(events) = &events {
                        ensure!(
                            !events.is_empty() && events.len() <= 64,
                            "Provide between 1 and 64 event selectors"
                        );
                        for event in events {
                            let crate::protocol::EventSelector::BoardChanged { board } = event;
                            self.read_board_locked(&db, board)?;
                        }
                    }
                    let receiver = db.agent(&agent).ok();
                    let recovery = db.event_recovery(receiver.as_ref())?;
                    let mut state = self.board_notifications.lock().unwrap();
                    state.unsubscribe(&agent, events.as_deref());
                    json!({"agent":agent,"events":state.events(&agent),"agents":[],"persistent":false,"recovery":recovery})
                }
                Request::Register {
                    name,
                    provider,
                    session,
                    replace,
                } => {
                    let mut agent = Agent {
                        name,
                        provider,
                        session,
                        endpoint: String::new(),
                    };
                    if agent.provider == Provider::Inbox {
                        crate::clients::validate_registration(&mut agent)?;
                    } else {
                        crate::clients::validate_session(&agent.provider, &agent.session)?;
                    }
                    db.register(&agent, replace)?;
                    json!(agent)
                }
                Request::ClaudeBind { session, endpoint } => {
                    db.claude_bind(&session, &endpoint)?;
                    json!({"bound":true,"session":session})
                }
                Request::ClaudeUnbind { session, endpoint } => {
                    db.claude_unbind(&session, &endpoint)?;
                    json!({"unbound":true,"session":session})
                }
                Request::OpenCodeAdmit {
                    id,
                    session,
                    attempt,
                } => {
                    let message = db.get(&id)?;
                    let agent = db.stored_destination(&message)?;
                    db.topic_push_authorize(&id, &agent)?;
                    if let Some(token) = db.event_control_token(&id)? {
                        self.authorize_event_locked(&db, &agent, &token)?;
                    }
                    db.opencode_admit(&id, &session, &attempt)?
                }
                Request::OpenCodeNotice {
                    id,
                    recipient,
                    content,
                    event_token,
                } => {
                    let agent = db.agent(&recipient)?;
                    ensure!(
                        agent.provider == Provider::OpenCode,
                        "Typed OpenCode notice requires an exact local session"
                    );
                    if let Some(token) = &event_token {
                        self.authorize_event_locked(&db, &agent, token)?;
                    }
                    let transaction = db.conn.unchecked_transaction()?;
                    let source = Agent {
                        name: "user".into(),
                        provider: Provider::Inbox,
                        session: "user".into(),
                        endpoint: String::new(),
                    };
                    let message = db.send(&id, &source, &agent, &content, &None)?;
                    db.opencode_notice_content(&id, &content)?;
                    if let Some(token) = &event_token {
                        db.event_control_record(&id, token)?;
                    }
                    transaction.commit()?;
                    json!({"queued":true,"message":self.message_value(&db, &message)?})
                }
                Request::CodexNotice {
                    id,
                    recipient,
                    session,
                    content,
                    event_token,
                } => {
                    let agent = db.agent(&recipient)?;
                    if let Some(token) = &event_token {
                        self.authorize_event_locked(&db, &agent, token)?;
                    }
                    let message = db.codex_event_notice(
                        &id,
                        &recipient,
                        &session,
                        &content,
                        event_token.as_deref(),
                    )?;
                    json!({"queued":true,"message":self.message_value(&db, &message)?})
                }
                Request::Unregister { name, session } => {
                    db.unregister(&name, &session)?;
                    json!({"unregistered":name})
                }
                Request::List { .. } => prepared_list.context("List preparation is missing")?,
                Request::Resolve { name } => json!(db.agent(&name)?),
                Request::Send {
                    id,
                    sender,
                    recipient,
                    body,
                    parent,
                } => {
                    let sent = match prepared_send.context("Send preparation is missing")? {
                        PreparedSend::Duplicate(message) => Ok(message),
                        PreparedSend::Remote(source, destination, label) => {
                            let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                            db.send_remote(
                                &local,
                                &id,
                                &source,
                                &recipient,
                                &body,
                                &parent,
                                destination.as_ref(),
                                Some(label),
                            )
                        }
                        PreparedSend::Local(source, destination) => {
                            // Repeat the duplicate readback and route fences under
                            // the final administration lock.
                            if let Some(message) =
                                db.send_duplicate(&id, &sender, &recipient, &body, &parent)?
                            {
                                Ok(message)
                            } else {
                                let current_source = db.agent(&sender)?;
                                let current_target = db.agent(&recipient)?;
                                ensure!(
                                    current_source.provider == source.provider
                                        && current_source.session == source.session
                                        && current_target.provider == destination.provider
                                        && current_target.session == destination.session,
                                    "Route changed while preparing the send; retry with the current identity"
                                );
                                db.send(&id, &source, &destination, &body, &parent)
                            }
                        }
                    };
                    let message = match sent {
                        Ok(message) => message,
                        Err(error) => {
                            // This runs under the broker's database lock. Only
                            // claim a definite rejection when readback proves no
                            // message was persisted; otherwise preserve ambiguity.
                            if matches!(db.get_optional(&id), Ok(None)) {
                                return Err(crate::protocol::SendNotPersisted(format!(
                                    "{error:#}"
                                ))
                                .into());
                            }
                            return Err(error);
                        }
                    };
                    self.message_value(&db, &message)?
                }
                Request::History { agent, limit } => {
                    let agent = agent
                        .map(|name| db.agent(&name).map(|a| a.name))
                        .transpose()?;
                    json!(
                        db.history(&agent, limit)?
                            .iter()
                            .map(|message| self.message_value(&db, message))
                            .collect::<Result<Vec<_>>>()?
                    )
                }
                Request::Get { id } => self.message_value(&db, &db.get(&id)?)?,
                Request::HistorySince {
                    agent,
                    cursor,
                    limit,
                } => {
                    let agent = agent
                        .map(|name| db.agent(&name).map(|a| a.name))
                        .transpose()?;
                    let page = db.history_since(&agent, cursor.as_deref(), limit)?;
                    json!({
                        "messages": page.messages.iter()
                            .map(|message| self.message_value(&db, message))
                            .collect::<Result<Vec<_>>>()?,
                        "cursor": page.cursor,
                        "has_more": page.has_more,
                    })
                }
                Request::Retry {
                    id,
                    allow_duplicate,
                } => self.message_value(&db, &db.retry(&id, allow_duplicate)?)?,
                Request::Cancel { id } => self.message_value(&db, &db.cancel(&id)?)?,
                Request::Pause { paused } => {
                    db.pause(paused)?;
                    json!({"paused":paused})
                }
                Request::MonitorEnable { .. } => anyhow::bail!("Use the asynchronous monitor RPC"),
                Request::MonitorDisable => self.monitor_control.lock().unwrap().disable(),
                Request::MonitorStatus => self.monitor_control.lock().unwrap().status(),
                Request::MonitorAccess => self.monitor_control.lock().unwrap().credentials(),
                Request::Status => {
                    let mut network = self.network_status.lock().unwrap().clone();
                    network["configured"] = json!(db.network_config()?);
                    let updates = match crate::update::cached_status(&self.state_dir) {
                        Ok(status) => json!(status),
                        Err(error) => json!({"error": format!("{error:#}")}),
                    };
                    json!({"version":crate::VERSION,"build_commit":crate::BUILD_COMMIT,"pid":std::process::id(),"state_dir":self.state_dir,"paused":db.paused()?,"messages":db.counts()?,"codex_discovery":self.discovery_status.lock().unwrap().clone(),"network":network,"monitor":self.monitor_control.lock().unwrap().status(),"machine":self.local_machine(&db)?,"updates":updates,"board_archives":crate::board_lifecycle::diagnostics(&db)?})
                }
                Request::RemoteList | Request::RemoteListPeer { .. } => {
                    anyhow::bail!("Use the asynchronous on-demand peer directory RPC")
                }
                Request::RelayList => json!(db.relay_nodes()?),
                Request::RelayAdd { address } => {
                    let configured = crate::network::relay_config::validate_relay(&address)?;
                    if self.state_dir.join("identity.key").exists() {
                        ensure!(
                            Identity::load_or_create(&self.state_dir)?.info().peer_id
                                != configured.peer_id,
                            "This bridge cannot be its own relay"
                        );
                    }
                    json!(db.add_relay(&address)?)
                }
                Request::RelayRemove { peer_id } => json!(db.remove_relay(&peer_id)?),
                Request::NetworkConfigure { enabled, listen } => {
                    if !enabled {
                        self.pairing_notifications.lock().unwrap().reset();
                        self.pairing_generation.fetch_add(1, Ordering::SeqCst);
                        self.contact_routes.lock().unwrap().clear();
                        if let Some(manager) = self.lan_pairing.lock().unwrap().as_mut() {
                            manager.close();
                        }
                        self.pairing_routes.lock().unwrap().clear();
                    }
                    if enabled {
                        Identity::load_or_create(&self.state_dir)?;
                    }
                    db.set_network_mode(enabled, listen.as_deref(), enabled.then_some(false))?;
                    json!(db.network_config()?)
                }
                Request::NetworkLan { listen } => {
                    Identity::load_or_create(&self.state_dir)?;
                    db.set_network_mode(
                        true,
                        Some(listen.as_deref().unwrap_or("/ip4/0.0.0.0/udp/0/quic-v1")),
                        Some(true),
                    )?;
                    json!(db.network_config()?)
                }
                Request::PeersDiscover => {
                    let config = db.network_config()?;
                    let mut discovered = if config.enabled && config.lan {
                        self.network_status.lock().unwrap()["discovered"]
                            .as_array()
                            .cloned()
                            .unwrap_or_default()
                    } else {
                        Vec::new()
                    };
                    if config.enabled {
                        let status = self.network_status.lock().unwrap().clone();
                        for (peer_id, addresses) in crate::network::relay_config::rendezvous_routes(
                            &status["rendezvous"],
                            &db.relay_nodes()?,
                            &Identity::load_or_create(&self.state_dir)?.info().peer_id,
                        ) {
                            if let Some(existing) = discovered
                                .iter_mut()
                                .find(|entry| entry["peer_id"] == peer_id)
                            {
                                let hints = existing["addresses"]
                                    .as_array_mut()
                                    .context("Invalid discovery hints")?;
                                for address in addresses {
                                    if hints.len() < 8
                                        && !hints.iter().any(|a| a == &address.to_string())
                                    {
                                        hints.push(json!(address.to_string()));
                                    }
                                }
                                existing["via_rendezvous"] = json!(true);
                            } else {
                                discovered.push(json!({"peer_id":peer_id,"addresses":addresses.into_iter().map(|a|a.to_string()).collect::<Vec<_>>(),"via_rendezvous":true}));
                            }
                        }
                    }
                    let peers = db.peers()?;
                    let mut labels: std::collections::BTreeMap<
                        String,
                        std::collections::BTreeSet<String>,
                    > = std::collections::BTreeMap::new();
                    for entry in &discovered {
                        if let (Some(name), Some(id)) =
                            (entry["machine_name"].as_str(), entry["peer_id"].as_str())
                        {
                            labels.entry(name.into()).or_default().insert(id.into());
                        }
                    }
                    for peer in &peers {
                        if let Some(name) = peer.alias.as_ref().or(peer.machine_name.as_ref()) {
                            labels
                                .entry(name.clone())
                                .or_default()
                                .insert(peer.peer_id.clone());
                        }
                    }
                    let all_ids: std::collections::BTreeSet<String> = discovered
                        .iter()
                        .filter_map(|entry| entry["peer_id"].as_str().map(str::to_string))
                        .chain(peers.iter().map(|peer| peer.peer_id.clone()))
                        .collect();
                    for entry in &mut discovered {
                        let id = entry["peer_id"].clone();
                        let peer = peers.iter().find(|peer| id == peer.peer_id);
                        let claimed = entry["machine_name"].as_str();
                        let name = peer
                            .and_then(|peer| peer.machine_name.as_deref())
                            .or(claimed);
                        let peer_id = id.as_str().unwrap_or_default();
                        let suffix = (8.min(peer_id.len())..=peer_id.len())
                            .map(|length| &peer_id[peer_id.len() - length..])
                            .find(|suffix| {
                                !all_ids
                                    .iter()
                                    .any(|other| other != peer_id && other.ends_with(suffix))
                            })
                            .unwrap_or(peer_id);
                        let display =
                            if let Some(alias) = peer.and_then(|peer| peer.alias.as_deref()) {
                                alias.to_string()
                            } else if let Some(name) = name {
                                if labels.get(name).is_some_and(|ids| ids.len() > 1) {
                                    format!("{name} [{suffix}]")
                                } else {
                                    name.into()
                                }
                            } else {
                                format!("peer [{suffix}]")
                            };
                        entry["machine_display_name"] = json!(display);
                        entry["authenticated_machine_name"] =
                            json!(peer.and_then(|peer| peer.machine_name.as_ref()));
                        entry["fingerprint"] = id;
                        entry["trusted"] = json!(peer.is_some_and(|peer| !peer.revoked));
                        entry["revoked"] = json!(peer.is_some_and(|peer| peer.revoked));
                        entry["verified_advertisement"] = json!(false);
                    }
                    json!(discovered)
                }
                Request::ContactCreate { seconds } => {
                    ensure!(
                        db.network_config()?.enabled,
                        "Enable networking before creating a contact"
                    );
                    let identity = Identity::load_or_create(&self.state_dir)?;
                    let relays = db
                        .relay_nodes()?
                        .into_iter()
                        .map(|r| r.address)
                        .collect::<Vec<_>>();
                    let contact = crate::network::contact::create(
                        &identity,
                        &db.machine_name()?,
                        &relays,
                        seconds,
                    )?;
                    let mut guard = self.lan_pairing.lock().unwrap();
                    guard
                        .get_or_insert(crate::network::lan_pairing::Manager::new(
                            &identity.info().peer_id,
                        )?)
                        .open(seconds, crate::db::now())?;
                    json!(contact)
                }
                Request::ContactConnect { .. } | Request::ContactLocation { .. } => {
                    anyhow::bail!("Use the asynchronous contact RPC")
                }
                Request::LanPairingWatch { agent, id, seconds } => {
                    let agent = db.agent(&agent)?;
                    ensure!(
                        crate::clients::supports(
                            &agent.provider,
                            crate::clients::Capability::PairingPrompts
                        ),
                        "Pairing prompts are unavailable for this native client; use the explicit CLI pairing workflow"
                    );
                    let now = crate::db::now();
                    let mut pairing = self.lan_pairing.lock().unwrap();
                    let manager = pairing.as_mut().context("No pairing window or exchange")?;
                    if let Some(id) = id.as_deref() {
                        ensure!(
                            manager.list(now).iter().any(|view| view.id == id),
                            "Unknown or expired pairing exchange"
                        );
                    } else {
                        ensure!(
                            manager.window_open(now),
                            "Open a pairing window before selecting its agent"
                        );
                    }
                    self.pairing_notifications.lock().unwrap().watch(
                        agent.clone(),
                        id.clone(),
                        seconds,
                        now,
                    )?;
                    json!({"notification_agent":agent.name,"provider":agent.provider,"session":agent.session,"id":id,"valid_for_seconds":seconds})
                }
                Request::LanPairingClose => {
                    self.pairing_notifications.lock().unwrap().reset();
                    self.pairing_generation.fetch_add(1, Ordering::SeqCst);
                    self.contact_routes.lock().unwrap().clear();
                    if let Some(manager) = self.lan_pairing.lock().unwrap().as_mut() {
                        manager.close();
                    }
                    self.pairing_routes.lock().unwrap().clear();
                    json!({"open":false})
                }
                Request::LanPairingList => {
                    let mut guard = self.lan_pairing.lock().unwrap();
                    let requests = guard
                        .as_mut()
                        .map(|manager| manager.list(crate::db::now()))
                        .unwrap_or_default();
                    let discovered = self.network_status.lock().unwrap().clone();
                    let requests: Vec<_> = requests
                        .into_iter()
                        .map(|view| {
                            let mut value = json!(view);
                            let known = db.peer(&view.peer).ok();
                            let claim = discovered["discovered"]
                                .as_array()
                                .and_then(|entries| {
                                    entries.iter().find(|e| e["peer_id"] == view.peer)
                                })
                                .and_then(|entry| entry["machine_name"].as_str());
                            value["display_name"] = json!(
                                known
                                    .as_ref()
                                    .map(|p| p.display_name.as_str())
                                    .or(claim)
                                    .unwrap_or(&view.peer)
                            );
                            value["identity_verified"] =
                                json!(known.as_ref().is_some_and(|p| !p.revoked));
                            value
                        })
                        .collect();
                    json!({"open":guard.as_mut().is_some_and(|manager| manager.window_open(crate::db::now())), "requests":requests})
                }
                Request::LanPairingOpen { seconds } => {
                    ensure!(
                        db.network_config()?.enabled,
                        "Enable networking before opening pairing"
                    );
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    let mut guard = self.lan_pairing.lock().unwrap();
                    let manager =
                        guard.get_or_insert(crate::network::lan_pairing::Manager::new(&local)?);
                    manager.open(seconds, crate::db::now())?;
                    json!({"open":true,"valid_for_seconds":seconds})
                }
                Request::LanPairingConnect {
                    peer,
                    address,
                    seconds,
                } => {
                    ensure!(
                        db.network_config()?.enabled,
                        "Enable networking before pairing"
                    );
                    let status = self.network_status.lock().unwrap().clone();
                    let mut entries = status["discovered"].as_array().cloned().unwrap_or_default();
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    for (id, addresses) in crate::network::relay_config::rendezvous_routes(
                        &status["rendezvous"],
                        &db.relay_nodes()?,
                        &local,
                    ) {
                        if let Some(entry) = entries.iter_mut().find(|entry| entry["peer_id"] == id)
                        {
                            let hints = entry["addresses"]
                                .as_array_mut()
                                .context("Invalid discovery hints")?;
                            for address in addresses {
                                if hints.len() < 8 {
                                    hints.push(json!(address.to_string()));
                                }
                            }
                        } else {
                            entries.push(json!({"peer_id":id,"addresses":addresses.into_iter().map(|a|a.to_string()).collect::<Vec<_>>()}));
                        }
                    }
                    let matches: Vec<_> = entries
                        .iter()
                        .filter(|entry| entry["peer_id"] == peer || entry["machine_name"] == peer)
                        .collect();
                    let (peer_id, addresses): (String, Vec<libp2p::Multiaddr>) = if let Some(
                        address,
                    ) = address
                    {
                        crate::network::store::validate_address(&address, false)?;
                        if address.contains("/p2p-circuit/") {
                            crate::network::relay_config::configured_circuit(
                                &address,
                                &peer,
                                &db.relay_nodes()?,
                            )?;
                        }
                        let id: libp2p::PeerId = peer
                            .parse()
                            .context("An explicit address requires a canonical peer ID")?;
                        ensure!(id.to_string() == peer, "Peer ID must be canonical");
                        (peer, vec![address.parse()?])
                    } else {
                        ensure!(
                            matches.len() == 1,
                            "Target is missing or ambiguous; use peers discover and a canonical peer ID"
                        );
                        let entry = matches[0];
                        let id = entry["peer_id"]
                            .as_str()
                            .context("Invalid discovery identity")?
                            .to_string();
                        let addresses = entry["addresses"]
                            .as_array()
                            .context("No discovered addresses")?
                            .iter()
                            .take(4)
                            .filter_map(|value| value.as_str()?.parse().ok())
                            .collect();
                        (id, addresses)
                    };
                    ensure!(!addresses.is_empty(), "No reachable target hints");
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    let mut guard = self.lan_pairing.lock().unwrap();
                    let manager =
                        guard.get_or_insert(crate::network::lan_pairing::Manager::new(&local)?);
                    let view = manager.start(&peer_id, seconds, crate::db::now())?;
                    let mut routes = self.pairing_routes.lock().unwrap();
                    // No more routes than active pairing transactions; bounded even after cancellation.
                    if routes.len() >= 8 && !routes.contains_key(&peer_id) {
                        routes.clear();
                    }
                    routes.insert(peer_id, addresses);
                    json!(view)
                }
                Request::LanPairingAccept { id } => {
                    ensure!(db.network_config()?.enabled, "Networking is disabled");
                    let mut guard = self.lan_pairing.lock().unwrap();
                    json!(
                        guard
                            .as_mut()
                            .context("No pairing window")?
                            .accept(&id, crate::db::now())?
                    )
                }
                Request::LanPairingConfirm { id, code } => {
                    ensure!(db.network_config()?.enabled, "Networking is disabled");
                    let mut guard = self.lan_pairing.lock().unwrap();
                    json!(guard.as_mut().context("No pairing window")?.confirm(
                        &id,
                        &code,
                        crate::db::now()
                    )?)
                }
                Request::LanPairingCancel { id } => {
                    self.pairing_generation.fetch_add(1, Ordering::SeqCst);
                    self.contact_routes
                        .lock()
                        .unwrap()
                        .retain(|_, (pending, _)| pending != &id);
                    self.lan_pairing
                        .lock()
                        .unwrap()
                        .as_mut()
                        .context("No pairing window")?
                        .cancel(&id, crate::db::now())?;
                    json!({"cancelled":true,"id":id})
                }
                Request::PairingOffer { seconds } => {
                    let identity = Identity::load_or_create(&self.state_dir)?;
                    json!(db.pairing_offer(&identity, &self.pairing_addresses(), seconds)?)
                }
                Request::PairingJoin {
                    token,
                    fingerprint,
                    alias,
                } => {
                    let identity = Identity::load_or_create(&self.state_dir)?;
                    let mut result = serde_json::to_value(db.pairing_join(
                        &identity,
                        &token,
                        &fingerprint,
                        alias.as_deref(),
                        &self.pairing_addresses(),
                    )?)?;
                    result["mutual_pairing_complete"] = json!(false);
                    result
                }
                Request::PairingFinish {
                    token,
                    fingerprint,
                    alias,
                } => {
                    let identity = Identity::load_or_create(&self.state_dir)?;
                    json!({"peer":db.pairing_finish(&identity, &token, &fingerprint, alias.as_deref())?, "local_pairing_complete":true})
                }
                Request::PairingCancel { nonce } => {
                    db.pairing_cancel(&nonce)?;
                    json!({"cancelled":true,"nonce":nonce})
                }
                Request::InvitationCreate { seconds } => {
                    let identity = Identity::load_or_create(&self.state_dir)?;
                    let addresses: Vec<String> = self.network_status.lock().unwrap()["listening"]
                        .as_array()
                        .map(|values| {
                            values
                                .iter()
                                .filter_map(|value| value.as_str())
                                .filter(|address| {
                                    crate::network::store::validate_address(address, false).is_ok()
                                })
                                .take(4)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({"invitation":crate::network::invitation::create(&identity, &addresses, seconds)?,"fingerprint":identity.info().fingerprint,"valid_for_seconds":seconds})
                }
                Request::InvitationAccept {
                    token,
                    fingerprint,
                    alias,
                } => {
                    let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                    json!(db.accept_invitation(&token, &fingerprint, alias.as_deref(), &local)?)
                }
                Request::PeerAddress { peer, address } => {
                    db.set_peer_address(&peer, &address)?;
                    json!({"peer":peer,"address":address})
                }
                Request::Identity => json!(Identity::load_or_create(&self.state_dir)?.info()),
                Request::MachineShow => self.local_machine(&db)?,
                Request::MachineSetName { name } => {
                    db.set_machine_name(&name)?;
                    self.local_machine(&db)?
                }
                Request::PeersList => json!(db.peers()?),
                Request::SharedSessions => db.shared_sessions()?,
                Request::PeerUnshareSession {
                    peer,
                    provider,
                    session,
                } => json!(db.unshare_peer_session(&peer, &provider, &session)?),
                Request::PeerShare {
                    peer,
                    agent,
                    shared,
                } => json!(db.share_peer_session(&peer, &agent, shared)?),
                Request::PeerConnections => {
                    let status = self.network_status.lock().unwrap().clone();
                    let enabled = db.network_config()?.enabled;
                    let peers = db.peers()?;
                    let sharing = db.shared_sessions()?;
                    let mut guard = self.lan_pairing.lock().unwrap();
                    let requests = guard
                        .as_mut()
                        .map(|manager| manager.list(crate::db::now()))
                        .unwrap_or_default();
                    let mut rows = std::collections::BTreeMap::<String, Value>::new();
                    for peer in peers {
                        let connected = enabled
                            && !peer.revoked
                            && status["connected"]
                                .as_array()
                                .is_some_and(|ids| ids.iter().any(|id| id == &peer.peer_id));
                        let state = if peer.revoked {
                            "revoked"
                        } else if connected {
                            "connected"
                        } else if !enabled {
                            "network_disabled"
                        } else {
                            "offline"
                        };
                        let sessions: Vec<_> = sharing
                            .as_array()
                            .unwrap()
                            .iter()
                            .filter(|s| s["peer_id"] == peer.peer_id)
                            .cloned()
                            .collect();
                        rows.insert(peer.peer_id.clone(), json!({"peer_id":peer.peer_id,"display_name":peer.display_name,"alias":peer.alias,"machine_name":peer.machine_name,"trusted":!peer.revoked,"revoked":peer.revoked,"connected":connected,"state":state,"pairing":[],"shared_sessions":sessions}));
                    }
                    // Only report authenticated live endpoint paths. Infrastructure
                    // observations and stale snapshots never imply peer authorization.
                    for (id, row) in &mut rows {
                        let transport = if row["connected"] == true {
                            status["paths"]
                                .as_array()
                                .and_then(|paths| {
                                    paths.iter().find(|path| path["peer_id"] == id.as_str())
                                })
                                .cloned()
                        } else {
                            None
                        };
                        row["path"] = transport
                            .as_ref()
                            .and_then(|path| path.get("path"))
                            .cloned()
                            .unwrap_or(Value::Null);
                        row["transport"] = transport.unwrap_or(Value::Null);
                    }
                    if enabled {
                        for discovered in status["discovered"].as_array().into_iter().flatten() {
                            let Some(id) = discovered["peer_id"].as_str() else {
                                continue;
                            };
                            rows.entry(id.to_string()).or_insert_with(|| json!({"peer_id":id,"display_name":discovered["machine_name"],"machine_name":discovered["machine_name"],"trusted":false,"revoked":false,"connected":false,"state":"unpaired","pairing":[],"shared_sessions":[]}));
                        }
                        let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                        for (id, _) in crate::network::relay_config::rendezvous_routes(
                            &status["rendezvous"],
                            &db.relay_nodes()?,
                            &local,
                        ) {
                            rows.entry(id.clone()).or_insert_with(|| json!({"peer_id":id,"display_name":id,"trusted":false,"revoked":false,"connected":false,"state":"unpaired","via_rendezvous":true,"pairing":[],"shared_sessions":[]}));
                        }
                    }
                    for request in requests {
                        let row = rows.entry(request.peer.clone()).or_insert_with(|| json!({"peer_id":request.peer,"display_name":request.peer,"trusted":false,"revoked":false,"connected":false,"state":"unpaired","pairing":[],"shared_sessions":[]}));
                        if !matches!(request.state, "paired" | "cancelled")
                            && row["trusted"] == false
                            && row["revoked"] != true
                        {
                            row["state"] = json!("pairing_pending");
                        }
                        // Connection summaries need state, not the locally displayed secret comparison code.
                        row["pairing"].as_array_mut().unwrap().push(json!({"id":request.id,"state":request.state,"role":request.role,"expires_at":request.expires_at,"local_confirmed":request.local_confirmed,"remote_confirmed":request.remote_confirmed}));
                    }
                    for row in rows.values_mut() {
                        row.as_object_mut()
                            .unwrap()
                            .entry("path")
                            .or_insert(Value::Null);
                        row.as_object_mut()
                            .unwrap()
                            .entry("transport")
                            .or_insert(Value::Null);
                    }
                    json!(rows.into_values().collect::<Vec<_>>())
                }
                Request::PeerPair {
                    peer_id,
                    fingerprint,
                    alias,
                } => {
                    ensure!(
                        peer_id != Identity::load_or_create(&self.state_dir)?.info().peer_id,
                        "Cannot pair this bridge with itself"
                    );
                    let suggested = if alias.is_none() && db.peer(&peer_id).is_err() {
                        self.network_status.lock().unwrap()["discovered"]
                            .as_array()
                            .and_then(|entries| {
                                entries.iter().find(|entry| entry["peer_id"] == peer_id)
                            })
                            .and_then(|entry| entry["machine_name"].as_str())
                            .map(str::to_string)
                            .map(|name| db.suggest_peer_alias(&peer_id, &name))
                            .transpose()?
                    } else {
                        None
                    };
                    json!(db.pair_peer(
                        &peer_id,
                        &fingerprint,
                        alias.as_deref().or(suggested.as_deref())
                    )?)
                }
                Request::PeerGrant {
                    peer,
                    agent,
                    permission,
                    allow,
                } => {
                    json!(db.grant_peer(&peer, &agent, permission, allow)?)
                }
                Request::PeerRevoke { peer } => {
                    self.pairing_notifications.lock().unwrap().reset();
                    self.pairing_generation.fetch_add(1, Ordering::SeqCst);
                    self.contact_routes.lock().unwrap().clear();
                    // Invalidate all incomplete approvals before revocation can race a delayed MAC.
                    if let Some(manager) = self.lan_pairing.lock().unwrap().as_mut() {
                        manager.close();
                    }
                    self.pairing_routes.lock().unwrap().clear();
                    db.revoke_peer(&peer)?;
                    json!({"revoked":peer})
                }
            })
        })();
        {
            let mut state = self.board_notifications.lock().unwrap();
            state.prune(|name| db.agent(name).ok());
        }
        self.revoke_event_controls_locked(&db)?;
        drop(db);
        self.wake.notify_one();
        self.network_wake.notify_one();
        result
    }

    pub(crate) fn event_guard(
        &self,
        agent: &crate::protocol::Agent,
        token: &str,
    ) -> crate::clients::NoticeGuard {
        let broker = self.clone();
        let agent = agent.clone();
        let token = token.to_owned();
        Arc::new(move || {
            let db = broker.db.lock().unwrap();
            ensure!(!db.paused()?, "Event delivery paused");
            broker.authorize_event_locked(&db, &agent, &token)
        })
    }
    fn authorize_event_locked(
        &self,
        db: &Db,
        agent: &crate::protocol::Agent,
        token: &str,
    ) -> Result<()> {
        ensure!(token.len() <= 1024, "Event authorization exceeds limit");
        let current = db.agent(&agent.name)?;
        ensure!(
            current.provider == agent.provider
                && current.session == agent.session
                && current.endpoint == agent.endpoint,
            "Event recipient registration changed"
        );
        ensure!(
            self.board_notifications
                .lock()
                .unwrap()
                .authorizes(token, &current),
            "Event subscription was removed or replaced"
        );
        Ok(())
    }
    fn revoke_event_controls_locked(&self, db: &Db) -> Result<()> {
        for (id, token) in db.event_controls()? {
            let message = db.get(&id)?;
            let valid = db.agent(&message.recipient).ok().is_some_and(|agent| {
                agent.session == message.recipient_session
                    && self.authorize_event_locked(db, &agent, &token).is_ok()
            });
            if !valid {
                db.event_control_revoke(&id)?;
            }
        }
        Ok(())
    }

    /// Expensive native checks for a board submission happen before the DB
    /// administration lock. Receipt duplicates and identity conflicts skip
    /// all native I/O. Returning Some(detail) is a definite 503.
    fn board_send_preflight(
        &self,
        request: &crate::board_messages::Submission,
    ) -> Result<Option<String>> {
        let owner = {
            let db = self.db.lock().unwrap();
            match db.board_message_receipt(request) {
                Ok(Some(_)) | Err(_) => return Ok(None),
                Ok(None) => {}
            }
            if !request.valid() {
                return Ok(None);
            }
            let snapshot = match self.read_board_locked(&db, &request.board) {
                Ok(snapshot) => snapshot,
                Err(_) => return Ok(None),
            };
            if snapshot.revision != request.expected_revision {
                return Ok(None);
            }
            crate::board_interaction::controls(&snapshot.markdown)
                .iter()
                .find(|control| {
                    crate::board_interaction::control_id(&snapshot, control) == request.control_id
                })
                .and_then(|control| match &control.kind {
                    crate::board_interaction::Kind::Message { owner, .. } => Some(owner.clone()),
                    _ => None,
                })
        };
        let Some(owner) = owner else {
            return Ok(None);
        };
        if owner.provider == Provider::Claude {
            return Ok(crate::clients::claude::stale_reason(Path::new(
                &owner.endpoint,
            )));
        }
        Ok(None)
    }

    /// Shared explicit user submission path. The caller holds the DB mutex.
    pub fn board_message_submit_locked(
        &self,
        db: &Db,
        request: &crate::board_messages::Submission,
    ) -> crate::board_messages::Reply {
        use crate::board_messages::Reply;
        if !request.valid() {
            return Reply::error(400, "invalid", "Invalid board message submission");
        }
        match db.board_message_receipt(request) {
            Ok(Some(mut receipt)) => {
                receipt["duplicate"] = json!(true);
                // Old board receipts are immutable idempotency records. Normalize
                // their presentation without rewriting them or enqueueing work.
                if let Some(state) = receipt["state"].as_str().map(str::to_owned) {
                    receipt["state"] = json!(crate::protocol::delivery_state(&state));
                }
                if let Some(state) = receipt["message"]["state"].as_str().map(str::to_owned) {
                    receipt["message"]["state"] = json!(crate::protocol::delivery_state(&state));
                }
                return Reply {
                    status: 200,
                    body: receipt,
                };
            }
            Err(error) => {
                return if error
                    .downcast_ref::<crate::board_messages::IdConflict>()
                    .is_some()
                {
                    Reply::error(409, "conflict", error)
                } else {
                    Reply::error(500, "uncertain", error)
                };
            }
            Ok(None) => (),
        }
        let snapshot = match self.read_board_locked(db, &request.board) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                if let Ok(board) = crate::board_lifecycle::inspect(
                    db,
                    &self.state_dir,
                    &request.board,
                    crate::db::now(),
                ) && !board.writable()
                {
                    return Reply {
                        status: 409,
                        body: json!({"code":"archived","error":"This board is not active; no message was sent","board":board}),
                    };
                }
                return Reply::error(503, "unavailable", error);
            }
        };
        let conflict = |error: &str| {
            let mut reply = Reply::error(409, "conflict", error);
            reply.body["board"] = json!(snapshot);
            reply
        };
        if snapshot.revision != request.expected_revision {
            return conflict("Board changed; review the current control before sending");
        }
        let controls = crate::board_interaction::controls(&snapshot.markdown);
        let Some(control) = controls.iter().find(|control| {
            crate::board_interaction::control_id(&snapshot, control) == request.control_id
        }) else {
            return conflict("Message input is missing, duplicated or malformed");
        };
        let crate::board_interaction::Kind::Message { owner, .. } = &control.kind else {
            return conflict("Selected control is not a message input");
        };
        let agent = match db.agent(&owner.name) {
            Ok(agent) if owner.matches(&agent) => agent,
            _ => {
                return Reply::error(
                    503,
                    "unavailable",
                    "Exact message input owner is closed, disabled or replaced",
                );
            }
        };
        if agent.provider == Provider::Inbox
            && !db
                .inbox_enabled(&agent.name, &agent.session)
                .unwrap_or(false)
        {
            return Reply::error(503, "unavailable", "Message input inbox is disabled");
        }
        // The native admission checks and OpenCode durable queue guards in
        // Db::insert_message remain authoritative. No native conversation starts here.
        let result = (|| -> Result<Value> {
            let tx = db.conn.unchecked_transaction()?;
            let source = Agent {
                name: "user".into(),
                provider: Provider::Inbox,
                session: "user".into(),
                endpoint: String::new(),
            };
            let message = db.send(&request.id, &source, &agent, &request.text, &None)?;
            let receipt = json!({"accepted":true,"id":request.id,"message":self.message_value(db, &message)?,"state":db.delivery_state(&message)?,"duplicate":false});
            tx.execute("INSERT INTO board_message_receipts(id,submission,owner,receipt) VALUES(?1,?2,?3,?4)", rusqlite::params![request.id, serde_json::to_string(request)?, serde_json::to_string(owner)?, serde_json::to_string(&receipt)?])?;
            tx.commit()?;
            Ok(receipt)
        })();
        match result {
            Ok(body) => {
                self.wake.notify_one();
                Reply { status: 200, body }
            }
            Err(error) => {
                let message = error.to_string();
                if error
                    .downcast_ref::<crate::codex_admission::Capacity>()
                    .is_some()
                    || message.contains("Queue is full")
                    || message.contains("queue is full")
                {
                    Reply::error(429, "rejected", error)
                } else if message.contains("admission")
                    || message.contains("session")
                    || message.contains("disabled")
                {
                    Reply::error(503, "unavailable", error)
                } else {
                    Reply::error(500, "uncertain", error)
                }
            }
        }
    }

    /// Caller already holds the local database administration mutex.
    pub(crate) fn read_board_locked(
        &self,
        db: &Db,
        name: &crate::board::Name,
    ) -> Result<crate::board::Snapshot> {
        crate::board_lifecycle::read_active(db, &self.state_dir, name)
    }

    /// Enrich the exact snapshots read by the bounded board listing.
    pub(crate) fn list_boards_locked(&self, db: &Db) -> Result<Vec<crate::board::Summary>> {
        crate::board_lifecycle::list_active(db, &self.state_dir)
    }

    fn validate_board_fences_locked(
        &self,
        db: &Db,
        events: &[crate::protocol::EventSelector],
        revisions: &std::collections::BTreeMap<crate::board::Name, String>,
    ) -> Result<()> {
        let names = events
            .iter()
            .map(|event| {
                let crate::protocol::EventSelector::BoardChanged { board } = event;
                board.clone()
            })
            .collect::<std::collections::BTreeSet<_>>();
        ensure!(
            revisions.len() <= 64 && revisions.keys().all(|name| names.contains(name)),
            "Board revision fences must match the requested event selectors"
        );
        for name in names {
            if !name.is_main() || revisions.contains_key(&name) {
                let current = self.read_board_locked(db, &name)?;
                ensure!(
                    revisions.get(&name) == Some(&current.revision),
                    "Named-board subscription revision is missing or stale; read the board and provide board_revisions before retrying"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn archive_board_locked(
        &self,
        db: &Db,
        name: &crate::board::Name,
        expected: &str,
        now: i64,
    ) -> Result<crate::board::Snapshot> {
        let board = crate::board_lifecycle::archive(db, &self.state_dir, name, expected, now)?;
        self.board_notifications
            .lock()
            .unwrap()
            .invalidate_board(name);
        self.revoke_event_controls_locked(db)?;
        Ok(board)
    }

    pub(crate) async fn board_cleanup_loop(&self, period: std::time::Duration) {
        let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let result = {
                let db = self.db.lock().unwrap();
                crate::board_lifecycle::cleanup(&db, &self.state_dir, crate::db::now())
            };
            match result {
                Ok(errors) => {
                    for error in errors {
                        eprintln!("Board archive cleanup: {error}");
                    }
                }
                Err(error) => {
                    eprintln!("Board archive cleanup failed; retrying on the next pass: {error:#}")
                }
            }
        }
    }

    /// Caller holds the local DB administration mutex. HTTP additionally retains
    /// its monitor generation guard through this update and both observations.
    pub(crate) fn update_board_locked(
        &self,
        db: &Db,
        name: &crate::board::Name,
        expected: &str,
        edits: &[crate::board::Edit],
        actor: Option<&crate::protocol::Agent>,
    ) -> Result<crate::board::Snapshot> {
        let before = self.read_board_locked(db, name)?;
        if before.revision != expected {
            return Err(crate::board::Conflict { current: before }.into());
        }
        let raw_expected = crate::board::snapshot(
            &self.state_dir,
            name,
            before.markdown.clone(),
            before.exists,
        )
        .revision;
        self.board_notifications.lock().unwrap().observe_named(
            name,
            &before.revision,
            None,
            std::time::Instant::now(),
        );
        let board = match crate::board::update_named(&self.state_dir, name, &raw_expected, edits) {
            Ok(board) => crate::board_lifecycle::enrich_written(db, &self.state_dir, board)?,
            Err(error) => {
                if let Some(conflict) = error.downcast_ref::<crate::board::Conflict>() {
                    return Err(crate::board::Conflict {
                        current: crate::board_lifecycle::enrich_written(
                            db,
                            &self.state_dir,
                            conflict.current.clone(),
                        )?,
                    }
                    .into());
                }
                return Err(error);
            }
        };
        self.board_notifications.lock().unwrap().observe_named(
            name,
            &board.revision,
            actor,
            std::time::Instant::now(),
        );
        Ok(board)
    }

    pub fn discovery_status(&self, status: Value) {
        *self.discovery_status.lock().unwrap() = status;
    }

    async fn dispatch(&self) -> Result<()> {
        let mut workers = JoinSet::new();
        let mut codex_workers = CodexWorkers::default();
        loop {
            self.blocking(move |broker| -> Result<()> {
                let db = broker.db.lock().unwrap();
                let now = crate::db::now();
                // Unresolved receipts block their session queue; reconcile them
                // read-only with bounded backoff instead of waiting for a manual retry.
                if let Err(error) = db.opencode_sweep_uncertain(now) {
                    eprintln!("OpenCode reconciliation sweep failed: {error:#}");
                }
                let mut state = broker.board_notifications.lock().unwrap();
                state.prune(|name| db.agent(name).ok());
                Ok(())
            })
            .await?;
            // Construct and enable before querying so arrivals cannot be lost between query and wait.
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            while workers.len() < 16 {
                let busy_codex = codex_workers.sessions();
                let next = self.blocking(move |broker| -> Result<_> {
                    // Readiness and housekeeping only matter for a claimable message.
                    if !broker.db.lock().unwrap().has_claimable()? {
                        return Ok(None);
                    }
                    let readiness = broker.native_readiness();
                    let claimed = {
                        let mut db = broker.db.lock().unwrap();
                        broker.revoke_event_controls_locked(&db)?;
                        db.topic_push_revoke_obsolete()?;
                        db.claim_excluding_codex(&busy_codex, Some(&readiness))?
                    };
                    Ok(claimed.map(|(msg, agent)| {
                        // Resolve the sender once for this delivery, outside the DB lock.
                        let label = if msg.sender.contains('/') { broker.db.lock().unwrap().inbound_source_label(&msg.id) } else { None };
                        let sender = broker.resolve_agent(&msg.sender, label.as_deref());
                        let db = broker.db.lock().unwrap();
                        let machines = json!({"source_machine":broker.message_machine(&db, &msg.sender)?,"destination_machine":broker.message_machine(&db, &msg.recipient)?,"sender_name":sender.label()});
                        let dispatch = if agent.provider==Provider::OpenCode {
                            let content = crate::clients::Adapter::OpenCode.message_content(&msg,&broker.state_dir,&machines);
                            match db.opencode_dispatch(&msg,&agent,&content) {
                                Ok(dispatch) => Some(dispatch),
                                Err(error) => {
                                    db.opencode_dispatch_failed(&msg.id,&error.to_string())?;
                                    return Ok(None);
                                }
                            }
                        } else { None };
                        let raw_codex_notice = agent.provider == Provider::Codex && db.codex_is_notice(&msg.id)?;
                        let event_token = db.event_control_token(&msg.id)?;
                        Ok::<_, anyhow::Error>(Some((msg, agent, machines, dispatch, raw_codex_notice, event_token)))
                    }).transpose()?.flatten())
                }).await?;
                let Some((msg, agent, machines, dispatch, raw_codex_notice, event_token)) = next
                else {
                    break;
                };
                let this = self.clone();
                let worker_agent = agent.clone();
                let worker = workers.spawn(async move {
                    let mut outcome = if raw_codex_notice {
                        if let Some(token) = &event_token {
                            crate::clients::codex::deliver_guarded(
                                &msg,
                                &msg.body,
                                Some(this.event_guard(&agent, token)),
                            )
                            .await
                        } else {
                            crate::clients::deliver_content(&agent, &msg, &msg.body).await
                        }
                    } else if let Some(dispatch) = &dispatch {
                        crate::clients::opencode::deliver_dispatch(&this.state_dir, &agent, dispatch).await
                    } else {
                        delivery::deliver_with_machines(
                            &agent,
                            &msg,
                            &this.codex_bin,
                            &this.state_dir,
                            &machines,
                        )
                        .await
                    };
                    if raw_codex_notice && outcome.state == "submitted" {
                        outcome.detail = "Native accepted typed control".into();
                    }
                    if outcome.state == "failed" {
                        eprintln!("Control {} failed: {}", msg.id, outcome.detail);
                    } else if outcome.state == "uncertain" {
                        eprintln!(
                            "Control {} has Unknown delivery after an ambiguous native attempt; inspect the native conversation, without resubmission: {}",
                            msg.id, outcome.detail
                        );
                    }
                    let finish_id = msg.id.clone();
                    let result = this
                        .blocking(move |finish_broker| {
                            let db = finish_broker.db.lock().unwrap();
                            if let Some(dispatch) = &dispatch {
                                db.opencode_finish(
                                    &msg.id,
                                    dispatch,
                                    outcome.state,
                                    &outcome.detail,
                                )
                            } else {
                                db.finish(&msg.id, outcome.state, &outcome.detail)
                            }
                        })
                        .await;
                    if let Err(e) = result {
                        eprintln!("Could not persist delivery outcome {}: {e:#}", finish_id);
                    }
                    this.wake.notify_one();
                    this.network_wake.notify_one();
                });
                codex_workers.track(worker.id(), &worker_agent);
            }
            // Every new message wakes this loop. A claimable message may only be
            // waiting on native readiness (poll); otherwise sleep until the next
            // reconcile is due, and not at all when nothing is pending.
            let wait = self
                .blocking(|broker| {
                    let db = broker.db.lock().unwrap();
                    Ok(if db.has_claimable()? {
                        Some(Duration::from_secs(1))
                    } else {
                        db.next_reconcile_in()?
                    })
                })
                .await?;
            tokio::select! {
                _=&mut notified=>{},
                _=async {
                    match wait {
                        Some(delay) => tokio::time::sleep(delay).await,
                        None => std::future::pending().await,
                    }
                }=>{},
                Some(result)=workers.join_next_with_id(),if !workers.is_empty()=>{
                    codex_workers.completed(&result);
                    result.context("Delivery task panicked")?;
                },
            }
        }
    }

    async fn connection(&self, stream: UnixStream) -> Result<()> {
        ensure!(
            stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
            "Foreign user connection refused"
        );
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read).take(MAX_FRAME as u64 + 1);
        let mut frame = Vec::new();
        timeout(Duration::from_secs(5), reader.read_until(b'\n', &mut frame)).await??;
        let response = Response::from_result(
            async {
                ensure!(
                    frame.len() <= MAX_FRAME && frame.last() == Some(&b'\n'),
                    "Request must be one newline-terminated JSON frame, at most 256 KiB"
                );
                self.request_async(serde_json::from_slice(&frame).map_err(|error| {
                    if ["opencode_observe", "opencode_heartbeat", "opencode_close"]
                        .iter()
                        .any(|op| frame.windows(op.len()).any(|w| w == op.as_bytes()))
                    {
                        anyhow::anyhow!("OpenCode plugin is outdated; restart OpenCode")
                    } else {
                        anyhow::Error::new(error).context("Invalid request")
                    }
                })?)
                .await
            }
            .await,
        );
        let mut bytes = serde_json::to_vec(&response)?;
        bytes.push(b'\n');
        timeout(Duration::from_secs(5), write.write_all(&bytes)).await??;
        Ok(())
    }
}

pub async fn serve(bound: BoundServer) -> Result<()> {
    serve_with_discovery(bound, None).await
}

pub async fn serve_with_discovery(bound: BoundServer, codex_home: Option<PathBuf>) -> Result<()> {
    let permits = Arc::new(Semaphore::new(64));
    let mut connections = JoinSet::new();
    let broker = bound.broker.clone();
    let dispatcher = broker.dispatch();
    tokio::pin!(dispatcher);
    let monitoring = crate::monitor::run(broker.clone());
    tokio::pin!(monitoring);
    let pairing_notifications = crate::pairing_notifications::run(broker.clone());
    tokio::pin!(pairing_notifications);
    let board_notifications = crate::board_notifications::run(broker.clone());
    tokio::pin!(board_notifications);
    let board_cleanup = broker.board_cleanup_loop(std::time::Duration::from_secs(300));
    tokio::pin!(board_cleanup);
    let networking = crate::network::worker::run(broker.clone());
    tokio::pin!(networking);
    let observer = crate::clients::observe(
        bound.broker.codex_state.clone(),
        codex_home,
        bound.broker.discovery_status.clone(),
    );
    tokio::pin!(observer);
    let update_checks = crate::update::run_checks(broker.state_dir.clone());
    tokio::pin!(update_checks);
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    eprintln!(
        "agent-bridge {} listening at {}",
        crate::VERSION,
        bound.socket.display()
    );
    let result = loop {
        tokio::select! {
            res=bound.listener.accept(), if permits.available_permits() > 0=>{
                let (stream,_)=res?;
                // Only this loop acquires permits, so the guarded accept cannot
                // race another acquirer. Leave excess peers in the bounded OS
                // listen backlog rather than dropping an unread request.
                let permit=permits.clone().try_acquire_owned().expect("accept requires a connection permit");
                let broker=bound.broker.clone();
                connections.spawn(async move {let _permit=permit;if let Err(e)=broker.connection(stream).await{eprintln!("Connection: {e:#}");}});
            },
            Some(_)=connections.join_next(),if !connections.is_empty()=>{},
            res=&mut dispatcher=>break res,
            _=&mut monitoring=>unreachable!("Monitor worker runs until shutdown"),
            res=&mut pairing_notifications=>break res,
            res=&mut board_notifications=>break res,
            _=&mut board_cleanup=>unreachable!("Board archive cleanup runs until shutdown"),
            _=&mut networking=>unreachable!("Network worker runs until shutdown"),
            _=&mut observer=>unreachable!("Discovery observer runs until shutdown"),
            _=&mut update_checks=>unreachable!("Update checker runs until shutdown"),
            _=tokio::signal::ctrl_c()=>break Ok(()),
            _=terminate.recv()=>break Ok(()),
        }
    };
    broker.monitor_control.lock().unwrap().disable();
    connections.abort_all();
    result
}

/// Project only documented board snapshot locations from older brokers. Keep
/// unrelated records and authored Markdown byte-exact.
fn logical_board_rpc_result(request: &Request, mut value: Value) -> Value {
    fn snapshot(value: &mut Value) {
        if let Some(object) = value.as_object_mut() {
            object.remove("path");
        }
    }
    match request {
        Request::BoardRead
        | Request::NamedBoardRead { .. }
        | Request::BoardCreate { .. }
        | Request::BoardInspect { .. }
        | Request::BoardArchive { .. }
        | Request::BoardRestore { .. } => snapshot(&mut value),
        Request::BoardUpdate { .. }
        | Request::NamedBoardUpdate { .. }
        | Request::AddBoardMessageInput { .. }
        | Request::BoardMessageSend { .. } => {
            if let Some(board) = value.get_mut("board") {
                snapshot(board);
            }
        }
        Request::BoardList | Request::BoardArchivedList => {
            if let Some(boards) = value.as_array_mut() {
                for board in boards {
                    snapshot(board);
                }
            }
        }
        _ => (),
    }
    value
}

pub async fn rpc(state_dir: &Path, req: &Request) -> Result<Value> {
    let path = state_dir.join("bridge.sock");
    let operation = async {
        let stream = UnixStream::connect(&path).await.with_context(|| {
            format!(
                "Cannot connect to {}; check service status and this command's permission to access the Unix socket",
                path.display()
            )
        })?;
        ensure!(
            stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
            "Service is owned by another user"
        );
        let (read, mut write) = stream.into_split();
        let mut bytes = serde_json::to_vec(req)?;
        bytes.push(b'\n');
        ensure!(bytes.len() <= MAX_FRAME, "Request exceeds 256 KiB");
        write.write_all(&bytes).await?;
        let mut reader = BufReader::new(read).take(40 * 1024 * 1024);
        let mut frame = Vec::new();
        reader.read_until(b'\n', &mut frame).await?;
        ensure!(
            frame.last() == Some(&b'\n'),
            "Service closed without a complete response; use the same --id to retry a send safely"
        );
        let response: Response = serde_json::from_slice(&frame)?;
        if !response.ok {
            let error = response.error.unwrap_or_default();
            if response.send_persisted == Some(false) {
                return Err(crate::protocol::SendNotPersisted(error).into());
            }
            anyhow::bail!("{error}")
        }
        Ok(logical_board_rpc_result(
            req,
            response.result.unwrap_or(Value::Null),
        ))
    };
    let seconds = if matches!(
        req,
        Request::ContactConnect { .. } | Request::ContactLocation { .. }
    ) {
        25
    } else {
        10
    };
    timeout(Duration::from_secs(seconds), operation)
        .await
        .context("Service request timed out; retry sends with the same --id")?
}

#[cfg(test)]
mod request_scheduling_tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_blocking_request_retains_service_ownership_until_finished() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state");
        let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        let held = broker.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let _guard = held.db.lock().unwrap();
            ready.wait();
            let _ = wait.recv_timeout(Duration::from_secs(2));
        });
        barrier.wait();
        let request = tokio::spawn(async move {
            broker
                .request_async(Request::List {
                    all: false,
                    max_age_days: 7,
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        request.abort();
        let _ = request.await;
        drop(bound);
        let replacement_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .open(state.join("service.lock"))
            .unwrap();
        let acquired = replacement_lock.try_lock_exclusive().is_ok();
        if acquired {
            FileExt::unlock(&replacement_lock).unwrap();
        }
        release.send(()).unwrap();
        owner.join().unwrap();
        assert!(
            !acquired,
            "replacement broker acquired ownership while an old blocking request could still mutate state"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_contention_does_not_block_async_request_timers() {
        let directory = tempfile::tempdir().unwrap();
        let bound = Broker::bind(directory.path().join("state"), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        let held = broker.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let owner = std::thread::spawn(move || {
            let _guard = held.db.lock().unwrap();
            ready.wait();
            std::thread::sleep(Duration::from_millis(400));
        });
        barrier.wait();
        let started = std::time::Instant::now();
        let request = tokio::spawn(async move {
            broker
                .request_async(Request::List {
                    all: false,
                    max_age_days: 7,
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let elapsed = started.elapsed();
        request.await.unwrap().unwrap();
        owner.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(200),
            "database contention blocked the async runtime for {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn claude_topic_notification_uses_the_direct_message_frame() {
        use crate::topics::{AgentRef, TopicLimits, TopicPublication};
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::AsyncReadExt;
        let root = tempfile::Builder::new()
            .prefix("ab-topic-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = root.path().join("native.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let agent = crate::protocol::Agent {
            name: "claude-topic".into(),
            provider: Provider::Claude,
            session: uuid::Uuid::new_v4().to_string(),
            endpoint: socket.to_string_lossy().into_owned(),
        };
        let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        {
            let mut db = broker.db.lock().unwrap();
            db.claude_bind(&agent.session, &agent.endpoint).unwrap();
            db.register(&agent, false).unwrap();
            let local = libp2p_identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id()
                .to_string();
            let topic = db
                .topic_create(&local, "Parity", &TopicLimits::default())
                .unwrap()
                .reference;
            let identity = AgentRef {
                bridge_id: None,
                agent_id: format!("claude-{}", agent.session),
            };
            db.topic_push_set(&local, &topic, &identity, "all").unwrap();
            db.topic_publish(
                &local,
                &TopicPublication {
                    topic,
                    id: uuid::Uuid::new_v4().to_string(),
                    author: AgentRef {
                        bridge_id: None,
                        agent_id: "inbox-user".into(),
                    },
                    body: "hello topic".into(),
                    parent: None,
                    mentions: vec![],
                },
                &TopicLimits::default(),
            )
            .unwrap();
        }
        let dispatcher = tokio::spawn(async move { broker.dispatch().await });
        let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).await.unwrap();
        dispatcher.abort();
        let frame: Value = serde_json::from_slice(&bytes).unwrap();
        let content = frame["message"]["content"].as_str().unwrap();
        assert!(
            content.starts_with("<cross-session-message from-name=\"@topic\">"),
            "topic notification must use the DM renderer, got: {content}"
        );
        assert!(content.contains("Agent Bridge topic notification"));
        assert!(content.contains("Reply to: @topic"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn database_contention_in_dispatch_does_not_block_async_timers() {
        let directory = tempfile::tempdir().unwrap();
        let bound = Broker::bind(directory.path().join("state"), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        let held = broker.clone();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let ready = barrier.clone();
        let owner = std::thread::spawn(move || {
            let _guard = held.db.lock().unwrap();
            ready.wait();
            std::thread::sleep(Duration::from_millis(400));
        });
        barrier.wait();
        let started = std::time::Instant::now();
        let dispatcher = tokio::spawn(async move { broker.dispatch().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let elapsed = started.elapsed();
        dispatcher.abort();
        let _ = dispatcher.await;
        owner.join().unwrap();
        assert!(
            elapsed < Duration::from_millis(200),
            "dispatch blocked the async runtime for {elapsed:?}"
        );
    }
}

#[cfg(test)]
mod machine_tests {
    use super::*;

    #[tokio::test]
    async fn resolver_names_agents_from_the_client_once_per_call() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let mut bound =
            Broker::bind(directory.path().join("state"), "/missing/codex".into()).unwrap();
        let session = "97e8d81e-073d-4cde-a017-4a8255b7b3b8";
        let other = "11111111-2222-4333-8444-555555555555";
        let calls = directory.path().join("calls");
        let script = directory.path().join("claude");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho x >> {}\nprintf '[{{\"sessionId\":\"{session}\",\"name\":\"Plan\\\\u202ener\"}},{{\"sessionId\":\"{other}\"}}]'\n",
                calls.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        bound.broker.claude_state = Arc::new(crate::clients::claude_discovery::State::new(script));
        let broker = &bound.broker;
        let route = format!("claude-{session}");
        let named = broker.resolve_agent(&route, None);
        assert_eq!(named.name, "Planner");
        assert_eq!(
            (named.id.as_str(), named.provider.clone(), named.local),
            (route.as_str(), Some(Provider::Claude), true)
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
        // No client name: the single fallback, not the raw route.
        assert_eq!(
            broker.resolve_agent(&format!("claude-{other}"), None).name,
            "Claude 5555"
        );
        // Explicit aliases are already the agent's chosen name; unknown routes stay as given.
        broker
            .db
            .lock()
            .unwrap()
            .register(
                &crate::protocol::Agent {
                    name: "reviewer".into(),
                    provider: Provider::Codex,
                    session: session.into(),
                    endpoint: String::new(),
                },
                false,
            )
            .unwrap();
        assert_eq!(broker.resolve_agent("reviewer", None).name, "reviewer");
        assert_eq!(broker.resolve_agent("missing", None).name, "missing");
        // Remote senders: the attached label is untrusted and sanitized; without it, the fallback.
        let remote = broker.resolve_agent("unknown-peer/reviewer", Some("Dev\u{202e}\nAgent"));
        assert_eq!(
            (remote.name.as_str(), remote.local, remote.label()),
            ("DevAgent", false, "unknown-peer/DevAgent".to_string())
        );
        assert_eq!(
            broker
                .resolve_agent(&format!("unknown-peer/claude-{session}"), None)
                .name,
            "Claude b3b8"
        );
    }

    #[tokio::test]
    async fn discovery_names_are_cosmetic_and_pairing_keeps_owner_aliases() {
        let directory = tempfile::Builder::new()
            .prefix("ab-names-")
            .tempdir_in("/tmp")
            .unwrap();
        let state = directory.path().join("state");
        let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
        let broker = &bound.broker;
        let local = broker.request(Request::MachineShow).unwrap();
        assert!(local["peer_id"].is_null());
        assert!(!state.join("identity.key").exists());
        let first = libp2p::PeerId::random().to_string();
        let second = libp2p::PeerId::random().to_string();
        broker
            .request(Request::NetworkLan { listen: None })
            .unwrap();
        *broker.network_status.lock().unwrap() = json!({"discovered":[
            {"peer_id":first,"machine_name":"machine-b","addresses":["/ip4/192.168.1.10/udp/44001/quic-v1"]},
            {"peer_id":second,"machine_name":"machine-b","addresses":["/ip4/192.168.1.11/udp/44001/quic-v1"]}
        ]});
        let discovered = broker.request(Request::PeersDiscover).unwrap();
        assert!(
            discovered[0]["machine_display_name"]
                .as_str()
                .unwrap()
                .starts_with("machine-b [")
        );
        assert_ne!(
            discovered[0]["machine_display_name"],
            discovered[1]["machine_display_name"]
        );
        assert_eq!(discovered[0]["verified_advertisement"], false);
        assert!(discovered[0]["authenticated_machine_name"].is_null());
        assert!(broker.db.lock().unwrap().peers().unwrap().is_empty());
        let paired = broker
            .request(Request::PeerPair {
                peer_id: first.clone(),
                fingerprint: first.clone(),
                alias: None,
            })
            .unwrap();
        assert_eq!(paired["alias"], "machine-b");
        assert!(paired["machine_name"].is_null());
        assert!(paired["grants"].as_array().unwrap().is_empty());
        let other = broker
            .request(Request::PeerPair {
                peer_id: second.clone(),
                fingerprint: second.clone(),
                alias: None,
            })
            .unwrap();
        assert_ne!(other["alias"], paired["alias"]);
        broker
            .db
            .lock()
            .unwrap()
            .remember_peer_name(&first, "renamed")
            .unwrap();
        let paired = broker
            .request(Request::PeerPair {
                peer_id: first.clone(),
                fingerprint: first,
                alias: None,
            })
            .unwrap();
        assert_eq!(paired["alias"], "machine-b");
        assert_eq!(paired["machine_name"], "renamed");
    }
}

#[cfg(test)]
mod opencode_admission_tests {
    use super::*;
    use crate::{
        network::wire::{Envelope, RemoteAgent},
        protocol::Agent,
    };
    #[tokio::test]
    async fn opencode_board_opt_in_does_not_enable_owner_pairing_prompts() {
        let root = tempfile::tempdir().unwrap();
        let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
        let broker = &bound.broker;
        let agent = Agent {
            name: "opencode-ses_0123456789abcdefghijklmnop".into(),
            provider: Provider::OpenCode,
            session: "ses_0123456789abcdefghijklmnop".into(),
            endpoint: format!("/tmp/{}.sock", uuid::Uuid::new_v4()),
        };
        assert!(
            broker
                .request(Request::Subscribe {
                    board_revisions: Default::default(),
                    agent: agent.name.clone(),
                    events: vec![crate::protocol::EventSelector::BoardChanged {
                        board: Default::default()
                    }],
                })
                .unwrap()["events"]
                .as_array()
                .unwrap()
                .len()
                == 1
        );
        let error = broker
            .request(Request::LanPairingWatch {
                agent: agent.name.clone(),
                id: None,
                seconds: 120,
            })
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Pairing prompts are unavailable")
        );
        broker
            .request(Request::Unsubscribe {
                board_revisions: Default::default(),
                agent: agent.name,
                events: None,
            })
            .unwrap();
    }
    #[tokio::test]
    async fn remote_policy_is_rechecked_through_actual_admission_request_after_claim() {
        for revoke in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
            let broker = &bound.broker;
            let origin = libp2p_identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id()
                .to_string();
            let local = broker.request(Request::Identity).unwrap()["peer_id"]
                .as_str()
                .unwrap()
                .to_owned();
            let agent = Agent {
                name: "opencode-ses_0123456789abcdefghijklmnop".into(),
                provider: Provider::OpenCode,
                session: "ses_0123456789abcdefghijklmnop".into(),
                endpoint: format!("/tmp/{}.sock", uuid::Uuid::new_v4()),
            };
            let (message, dispatch) = {
                let mut db = broker.db.lock().unwrap();
                // Inject a verified-live database fixture; native probes have
                // separate real-socket coverage. No dispatcher runs in this test.
                db.pair_peer(&origin, &origin, Some("origin")).unwrap();
                db.share_peer_session(&origin, &agent.name, true).unwrap();
                let envelope = Envelope {
                    origin_peer: origin.clone(),
                    id: uuid::Uuid::new_v4().to_string(),
                    source: RemoteAgent {
                        name: "source".into(),
                        provider: Provider::Inbox,
                        session: "source".into(),
                    },
                    source_label: Some("Dev".into()),
                    destination_peer: local.clone(),
                    destination: RemoteAgent {
                        name: agent.name.clone(),
                        provider: agent.provider.clone(),
                        session: agent.session.clone(),
                    },
                    body: "Gate after native delivery claim".into(),
                    parent: None,
                    hops: 0,
                };
                db.accept_remote_names(&local, &origin, &envelope).unwrap();
                let (msg, target) = db.claim().unwrap().unwrap();
                assert_eq!(db.inbound_source_label(&msg.id).as_deref(), Some("Dev"));
                let frame = db
                    .opencode_dispatch(&msg, &target, "durable peer content")
                    .unwrap();
                (msg, frame)
            };
            // This is the deterministic gate after receiving the delivery frame,
            // before the plugin invokes the real privileged admission operation.
            assert_eq!(dispatch.frame["op"], "deliver");
            let admission = || Request::OpenCodeAdmit {
                id: message.id.clone(),
                session: agent.session.clone(),
                attempt: dispatch.attempt.clone(),
            };
            broker.request(admission()).expect("Positive control: the current remote Receive grant admits this valid claimed frame");
            if revoke {
                broker
                    .request(Request::PeerRevoke {
                        peer: origin.clone(),
                    })
                    .unwrap();
            } else {
                broker
                    .request(Request::PeerShare {
                        peer: origin.clone(),
                        agent: agent.name.clone(),
                        shared: false,
                    })
                    .unwrap();
            }
            let denied = broker.request(admission());
            assert!(
                denied.is_err(),
                "Revoked/unshared receiving policy must deny an already claimed frame"
            );
            let db = broker.db.lock().unwrap();
            let phase: String = db
                .conn
                .query_row(
                    "SELECT phase FROM opencode_deliveries WHERE id=?",
                    [&message.id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(phase, "dispatched"); // No insertion receipt or admission authority.
            db.opencode_finish(
                &message.id,
                &dispatch,
                "queued",
                "plugin deferred denied admission before native invocation",
            )
            .unwrap();
            assert_eq!(db.get(&message.id).unwrap().state, "queued");
        }
    }
}

#[cfg(test)]
mod codex_worker_tests {
    use super::*;
    use crate::protocol::Agent;
    #[tokio::test]
    async fn codex_fast_ack_cannot_release_worker_fence_and_all_completions_remove_it() {
        let root = tempfile::tempdir().unwrap();
        let mut db = Db::open(&root.path().join("db")).unwrap();
        let one = Agent {
            name: "one".into(),
            provider: Provider::Codex,
            session: "123e4567-e89b-12d3-a456-426614174000".into(),
            endpoint: String::new(),
        };
        let mut alias = one.clone();
        alias.name = "alias".into();
        let mut other = one.clone();
        other.name = "other".into();
        other.session = "123e4567-e89b-12d3-a456-426614174001".into();
        for agent in [&one, &alias, &other] {
            db.register(agent, false).unwrap();
        }
        let send = |db: &Db, agent: &Agent| {
            db.send_names(
                &uuid::Uuid::new_v4().to_string(),
                "user",
                &agent.name,
                "peer input",
                &None,
            )
            .unwrap()
        };
        let first = send(&db, &one);
        let second = send(&db, &alias);
        assert_eq!(db.claim().unwrap().unwrap().0.id, first.id);
        let mut workers = JoinSet::new();
        let mut busy = CodexWorkers::default();
        let (release, held) = tokio::sync::oneshot::channel();
        let worker = workers.spawn(async move {
            held.await.unwrap();
        });
        busy.track(worker.id(), &one);
        db.finish(&first.id, "submitted", "native accepted")
            .unwrap();
        assert!(
            db.claim_excluding_codex(&busy.sessions(), None)
                .unwrap()
                .is_none()
        );
        let unrelated = send(&db, &other);
        assert_eq!(
            db.claim_excluding_codex(&busy.sessions(), None)
                .unwrap()
                .unwrap()
                .0
                .id,
            unrelated.id
        );
        db.finish(&unrelated.id, "submitted", "accepted").unwrap();
        release.send(()).unwrap();
        let result = workers.join_next_with_id().await.unwrap();
        busy.completed(&result);
        result.unwrap();
        assert_eq!(
            db.claim_excluding_codex(&busy.sessions(), None)
                .unwrap()
                .unwrap()
                .0
                .id,
            second.id
        );
        db.finish(&second.id, "submitted", "accepted").unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "submitted");
        for cancel in [false, true] {
            let queued = send(&db, &alias);
            let worker = workers.spawn(async move {
                if !cancel {
                    panic!("injected worker panic");
                }
                std::future::pending::<()>().await;
            });
            busy.track(worker.id(), &one);
            assert!(
                db.claim_excluding_codex(&busy.sessions(), None)
                    .unwrap()
                    .is_none()
            );
            if cancel {
                worker.abort();
            }
            let result = workers.join_next_with_id().await.unwrap();
            assert!(result.is_err());
            busy.completed(&result);
            assert!(busy.sessions().is_empty());
            assert_eq!(
                db.claim_excluding_codex(&busy.sessions(), None)
                    .unwrap()
                    .unwrap()
                    .0
                    .id,
                queued.id
            );
            db.finish(&queued.id, "failed", "test worker cleanup")
                .unwrap();
        }
    }
}

#[cfg(test)]
mod board_rpc_projection_tests {
    use super::*;

    #[test]
    fn legacy_projection_is_limited_to_known_request_snapshot_positions() {
        let snapshot = json!({"name":"logical","path":"/private/boards/logical.md","markdown":"Authored /work/report.md and {\"path\":\"example\"}","revision":"current","exists":true});
        for operation in [
            "board_read",
            "named_board_read",
            "board_create",
            "board_inspect",
            "board_archive",
            "board_restore",
        ] {
            let mut request = json!({"op":operation});
            if operation != "board_read" {
                request["board"] = json!("logical");
            }
            if matches!(operation, "board_archive" | "board_restore") {
                request["expected_revision"] = json!("current");
            }
            let request: Request = serde_json::from_value(request).unwrap();
            let result = logical_board_rpc_result(&request, snapshot.clone());
            assert!(result.get("path").is_none(), "{operation}");
            assert_eq!(result["markdown"], snapshot["markdown"]);
        }
        for operation in [
            "board_update",
            "named_board_update",
            "add_board_message_input",
            "board_message_send",
        ] {
            let request = match operation {
                "board_update" => json!({"op":operation,"expected_revision":"current","edits":[]}),
                "named_board_update" => {
                    json!({"op":operation,"board":"logical","expected_revision":"current","edits":[]})
                }
                "add_board_message_input" => {
                    json!({"op":operation,"board":"logical","expected_revision":"current","agent":"owner","id":"compose","label":"Message owner"})
                }
                _ => {
                    json!({"op":operation,"request":{"board":"logical","expected_revision":"current","control_id":"control","id":"id","text":"Authored path text"}})
                }
            };
            let request: Request = serde_json::from_value(request).unwrap();
            let envelope = json!({"board":snapshot,"message":{"path":"/unrelated/endpoint","body":"Authored /work/report.md"}});
            let result = logical_board_rpc_result(&request, envelope.clone());
            assert!(result["board"].get("path").is_none(), "{operation}");
            assert_eq!(result["board"]["markdown"], snapshot["markdown"]);
            assert_eq!(result["message"], envelope["message"]);
        }
        for request in [Request::BoardList, Request::BoardArchivedList] {
            let result = logical_board_rpc_result(&request, json!([snapshot]));
            assert!(result[0].get("path").is_none());
        }
        let unrelated = json!({"path":"/unrelated/connection","state_dir":"/private/state","endpoint":"/private/socket","board":snapshot});
        assert_eq!(
            logical_board_rpc_result(&Request::Status, unrelated.clone()),
            unrelated
        );
    }
}
