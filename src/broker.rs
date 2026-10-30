use crate::{
    db::Db,
    delivery, hook,
    network::identity::Identity,
    protocol::{MAX_FRAME, Provider, Request, Response},
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

// Native receipt acknowledgement may change durable state before its worker
// finishes. Keep the concurrency fence until the actual task is joined.
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

#[derive(Clone)]
pub(crate) struct NetworkSnapshot {
    pub generation: u64,
    pub config: crate::network::store::NetworkConfig,
    pub relays: Vec<crate::network::relay_config::RelayNode>,
    pub handle: crate::network::transport::Handle,
}

#[derive(Clone)]
pub struct Broker {
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
    pub claude_wake: Arc<Notify>,
    claude_failures: Arc<Mutex<std::collections::HashMap<String, (String, String)>>>,
    pub state_dir: PathBuf,
    codex_bin: PathBuf,
    discovery_status: Arc<Mutex<Value>>,
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
    _lock: File,
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

    fn message_sender_name(&self, db: &Db, sender: &str) -> String {
        if let Some((peer_id, name)) = sender.split_once('/') {
            let peer = db.peer(peer_id).ok();
            let qualifier = peer
                .as_ref()
                .map(|peer| peer.display_name.as_str())
                .unwrap_or(peer_id);
            return format!("{qualifier}/{name}");
        }
        // An explicit registration alias is already the agent's chosen name.
        if let Ok(agent) = db.agent(sender)
            && sender == hook::routing_name(&agent.provider, &agent.session)
            && let Ok(Some(name)) = db.display_name(&agent.provider, &agent.session)
            && !name.trim().is_empty()
        {
            return name;
        }
        sender.to_owned()
    }

    fn message_value(&self, db: &Db, message: &crate::protocol::Message) -> Result<Value> {
        let mut value = json!(message);
        value["source_machine"] = self.message_machine(db, &message.sender)?;
        value["destination_machine"] = self.message_machine(db, &message.recipient)?;
        if let Some(transfer) = db.transfer_info(&message.id)? {
            value["transfer"] = transfer;
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
        let mut event_state = crate::board_notifications::State::default();
        event_state.reconcile(
            crate::board_notifications::available(&db)?,
            "broker startup",
        );
        let listener = UnixListener::bind(&socket)?;
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))?;
        let broker = Self {
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
            claude_wake: Arc::new(Notify::new()),
            claude_failures: Arc::new(Mutex::new(std::collections::HashMap::new())),
            state_dir,
            codex_bin,
            discovery_status: Arc::new(Mutex::new(json!({"state":"disabled"}))),
        };
        Ok(BoundServer {
            listener,
            broker,
            _lock: lock,
            socket,
        })
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
    pub async fn request_async(&self, req: Request) -> Result<Value> {
        match req {
            req @ (Request::OpenCodeObserve { .. } | Request::OpenCodeHeartbeat { .. }) => {
                let broker = self.clone();
                tokio::task::spawn_blocking(move || broker.request(req)).await?
            }
            req @ Request::Register {
                agent:
                    crate::protocol::Agent {
                        provider: Provider::OpenCode,
                        ..
                    },
                ..
            } => {
                let broker = self.clone();
                tokio::task::spawn_blocking(move || broker.request(req)).await?
            }
            Request::MonitorEnable { port } => {
                self.monitor_control.lock().unwrap().enable(port);
                timeout(Duration::from_secs(5), async {
                    loop {
                        let value = self.monitor_control.lock().unwrap().credentials();
                        ensure!(
                            value["requested_port"].as_u64() == Some(port as u64),
                            "Monitor enable was superseded"
                        );
                        if value["url"].is_string() {
                            return Ok(value);
                        }
                        if let Some(error) = value["last_error"].as_str() {
                            anyhow::bail!("Monitor could not start: {error}");
                        }
                        ensure!(
                            value["enabled"] == true,
                            "Monitor was disabled while starting"
                        );
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                })
                .await
                .context("Monitor did not start in time; inspect monitor status")?
            }
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
            other => self.request(other),
        }
    }

    pub fn request(&self, req: Request) -> Result<Value> {
        // A native probe may trigger lifecycle cleanup through this same broker.
        // Complete transport I/O before holding the database administration lock.
        let candidate = match &req {
            Request::Register { agent, .. } if agent.provider == Provider::OpenCode => {
                Some(agent.clone())
            }
            Request::OpenCodeObserve {
                session, endpoint, ..
            }
            | Request::OpenCodeHeartbeat {
                session, endpoint, ..
            } => Some(crate::protocol::Agent {
                name: crate::clients::lifecycle::routing_name(&Provider::OpenCode, session),
                provider: Provider::OpenCode,
                session: session.clone(),
                endpoint: endpoint.clone(),
                enabled: true,
            }),
            _ => None,
        };
        if let Some(mut agent) = candidate {
            crate::clients::validate_registration(&mut agent)?;
            crate::clients::opencode::verify_registration(&agent)?;
        }
        let mut db = self
            .db
            .lock()
            .map_err(|_| anyhow::anyhow!("Database lock poisoned"))?;
        db.opencode_tick(crate::db::now())?;
        {
            let mut state = self.board_notifications.lock().unwrap();
            state.reconcile(
                crate::board_notifications::available(&db)?,
                "OpenCode lease expired",
            );
            state.prune(|name| db.agent(name).ok());
        }
        let registration = matches!(
            &req,
            Request::Register { .. } | Request::OpenCodeObserve { .. }
        );
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
                // Board edits share the broker's local administration mutex. No
                // network request can invoke this API or select a filesystem path.
                Request::BoardMessageSend { request } => {
                    self.board_message_submit_locked(&db, &request).body
                }
                Request::AddBoardMessageInput {
                    agent,
                    board,
                    expected_revision,
                    id,
                    label,
                } => {
                    let actor = db.agent(&agent)?;
                    ensure!(actor.enabled, "Message input owner must be enabled");
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
                Request::BoardInspect { board } => json!(crate::board_lifecycle::inspect(
                    &db,
                    &self.state_dir,
                    &board,
                    crate::db::now()
                )?),
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
                    ensure!(
                        actor.as_ref().is_none_or(|agent| agent.enabled),
                        "Board writer must be an enabled local registration"
                    );
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
                    agent,
                    events,
                    board_revisions,
                } => {
                    self.validate_board_fences_locked(&db, &events, &board_revisions)?;
                    let agent = db.agent(&agent)?;
                    let mut boards = std::collections::BTreeMap::new();
                    ensure!(
                        !events.is_empty() && events.len() <= 64,
                        "Provide between 1 and 64 event selectors"
                    );
                    for event in &events {
                        if let crate::protocol::EventSelector::BoardChanged { board } = event {
                            boards.insert(
                                board.clone(),
                                self.read_board_locked(&db, board)?.revision,
                            );
                        }
                    }
                    let agents = crate::board_notifications::available(&db)?;
                    ensure!(
                        agents.len() <= 256,
                        "Available-agent snapshot exceeds 256 identities; reduce local registrations before subscribing"
                    );
                    ensure!(
                        serde_json::to_vec(&agents)?.len() <= crate::protocol::MAX_BODY,
                        "Available-agent snapshot exceeds 65536 bytes; reduce local registrations before subscribing"
                    );
                    let recovery = db.event_recovery(Some(&agent))?;
                    let mut state = self.board_notifications.lock().unwrap();
                    state.reconcile(agents.clone(), "local registration changed");
                    state.prune(|name| db.agent(name).ok());
                    state.subscribe(agent.clone(), &events, &boards, std::time::Instant::now())?;
                    let effective = state.events(&agent.name);
                    let lifecycle = effective.iter().any(|event| {
                        !matches!(event, crate::protocol::EventSelector::BoardChanged { .. })
                    });
                    json!({"agent":agent.name,"events":effective,"agents":if lifecycle {agents} else {Vec::new()},"persistent":false,"recovery":recovery})
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
                            if let crate::protocol::EventSelector::BoardChanged { board } = event {
                                self.read_board_locked(&db, board)?;
                            }
                        }
                    }
                    let registered: bool = db.conn.query_row(
                        "SELECT EXISTS(SELECT 1 FROM agents WHERE name=?)",
                        [&agent],
                        |row| row.get(0),
                    )?;
                    let receiver = registered.then(|| db.agent(&agent)).transpose()?;
                    let recovery = db.event_recovery(receiver.as_ref())?;
                    let mut state = self.board_notifications.lock().unwrap();
                    state.unsubscribe(&agent, events.as_deref());
                    json!({"agent":agent,"events":state.events(&agent),"agents":[],"persistent":false,"recovery":recovery})
                }
                Request::Register { mut agent, replace } => {
                    crate::clients::validate_registration(&mut agent)?;
                    if agent.provider == Provider::OpenCode {
                        db.opencode_alias_admission(&agent)?;
                    }
                    db.register(&agent, replace)?;
                    if agent.provider == Provider::Claude {
                        self.claude_failures
                            .lock()
                            .unwrap()
                            .retain(|_, (session, _)| session != &agent.session);
                    }
                    json!(agent)
                }
                Request::OpenCodeObserve {
                    session,
                    endpoint,
                    title,
                    ready,
                } => {
                    ensure!(
                        title
                            .as_ref()
                            .is_none_or(|s| s.len() <= 4096 && !s.chars().any(char::is_control)),
                        "Invalid OpenCode title"
                    );
                    let mut agent = crate::protocol::Agent {
                        name: crate::clients::lifecycle::routing_name(
                            &Provider::OpenCode,
                            &session,
                        ),
                        provider: Provider::OpenCode,
                        session,
                        endpoint,
                        enabled: true,
                    };
                    crate::clients::validate_registration(&mut agent)?;
                    let observed_at = crate::db::now();
                    db.opencode_tick(observed_at)?;
                    {
                        let mut state = self.board_notifications.lock().unwrap();
                        state.reconcile(
                            crate::board_notifications::available(&db)?,
                            "OpenCode lease expired",
                        );
                        state.prune(|name| db.agent(name).ok());
                    }
                    let result = db.opencode_observe_at(&agent, ready, observed_at)?;
                    db.observe_session_name(&agent.provider, &agent.session, title.as_deref())?;
                    result
                }
                Request::OpenCodeHeartbeat {
                    session,
                    endpoint,
                    generation,
                    ready,
                } => {
                    let endpoint = crate::clients::opencode::validate_endpoint(&endpoint)?
                        .to_string_lossy()
                        .into_owned();
                    db.opencode_heartbeat(&session, &endpoint, &generation, ready)?
                }
                Request::OpenCodeClose {
                    session,
                    endpoint,
                    generation,
                } => {
                    let path = std::path::Path::new(&endpoint);
                    let endpoint = path
                        .parent()
                        .context("Missing OpenCode endpoint directory")?
                        .canonicalize()?
                        .join(
                            path.file_name()
                                .context("Missing OpenCode endpoint filename")?,
                        )
                        .to_string_lossy()
                        .into_owned();
                    db.opencode_close(&session, &endpoint, &generation)?
                }
                Request::OpenCodeAdmit {
                    id,
                    session,
                    generation,
                    instance,
                    attempt,
                } => {
                    if let Some(token) = db.event_control_token(&id)? {
                        let message = db.get(&id)?;
                        let agent = db.agent(&message.recipient)?;
                        self.authorize_event_locked(&db, &agent, &token)?;
                    }
                    db.opencode_admit(&id, &session, &generation, &instance, &attempt)?
                }
                Request::OpenCodeNotice {
                    id,
                    recipient,
                    content,
                    event_token,
                } => {
                    let agent = db.agent(&recipient)?;
                    ensure!(
                        agent.provider == Provider::OpenCode && agent.enabled,
                        "Typed OpenCode notice requires an enabled exact local session"
                    );
                    if let Some(token) = &event_token {
                        self.authorize_event_locked(&db, &agent, token)?;
                    }
                    let transaction = db.conn.unchecked_transaction()?;
                    let message = db.send(&id, "user", &agent.name, &content, &None)?;
                    db.opencode_notice_content(&id, &content)?;
                    if let Some(token) = &event_token {
                        db.event_control_record(&id, token)?;
                    }
                    transaction.commit()?;
                    json!({"queued":true,"message":message,"acknowledged":false})
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
                    json!({"queued":true,"message":message,"acknowledged":false})
                }
                Request::SessionClose {
                    name,
                    session,
                    endpoint,
                } => {
                    // Normalize the parent only: SessionEnd may run after the
                    // old socket was removed, but must never close its replacement.
                    let endpoint = if endpoint.is_empty() {
                        endpoint
                    } else {
                        let path = Path::new(&endpoint);
                        path.parent()
                            .context("Missing native endpoint directory")?
                            .canonicalize()?
                            .join(
                                path.file_name()
                                    .context("Missing native endpoint filename")?,
                            )
                            .to_string_lossy()
                            .into_owned()
                    };
                    db.close_native(&name, &session, &endpoint)?;
                    json!({"closed":name})
                }
                Request::Unregister { name, session } => {
                    // SessionEnd can race a disconnect, so exact names remain usable while disabled.
                    db.unregister(&db.resolve_agent(&name)?.name, &session)?;
                    json!({"unregistered":name})
                }
                Request::List { all } => {
                    let mut agents = Vec::new();
                    let machine = self.local_machine(&db)?;
                    for agent in db.listed_agents(all)? {
                        let stale_reason = if !agent.enabled {
                            Some("disabled".to_owned())
                        } else if agent.provider == Provider::Claude {
                            crate::clients::claude::stale_reason(Path::new(&agent.endpoint))
                        } else {
                            None
                        };
                        if !all && stale_reason.is_some() {
                            continue;
                        }
                        let display = db.display_name(&agent.provider, &agent.session)?;
                        let mut value = json!(agent);
                        if let Some(reason) = stale_reason {
                            value["stale_reason"] = json!(reason);
                        }
                        value["machine_name"] = machine["machine_name"].clone();
                        value["peer_id"] = machine["peer_id"].clone();
                        value["machine_display_name"] = machine["display_name"].clone();
                        if let Some(display) = display {
                            value["display_name"] = json!(display);
                            value["qualified_name"] =
                                json!(format!("{}:{display}", agent.provider.label()));
                        }
                        agents.push(value);
                    }
                    json!(agents)
                }
                Request::Send {
                    id,
                    sender,
                    recipient,
                    body,
                    parent,
                } => {
                    if recipient.contains('/') {
                        let local = Identity::load_or_create(&self.state_dir)?.info().peer_id;
                        self.message_value(
                            &db,
                            &db.send_remote(&local, &id, &sender, &recipient, &body, &parent)?,
                        )?
                    } else {
                        self.message_value(
                            &db,
                            &db.send(&id, &sender, &recipient, &body, &parent)?,
                        )?
                    }
                }
                Request::History { agent, limit } => {
                    let agent = agent
                        .map(|name| db.resolve_agent(&name).map(|a| a.name))
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
                        .map(|name| db.resolve_agent(&name).map(|a| a.name))
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
                Request::Ack { id, agent } => {
                    self.message_value(&db, &db.ack(&id, &db.resolve_agent(&agent)?.name)?)?
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
                    let mut agents = match &req {
                        Request::RemoteListPeer { peer } => db.remote_agents_for(peer)?,
                        _ => db.remote_agents()?,
                    };
                    let status = self.network_status.lock().unwrap().clone();
                    if let Some(agents) = agents.as_array_mut() {
                        for agent in agents {
                            agent["online"] =
                                json!(status["connected"].as_array().is_some_and(|ids| {
                                    ids.iter().any(|id| id == &agent["peer_id"])
                                }));
                        }
                    }
                    agents
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
                        agent.enabled
                            && crate::clients::supports(
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
            state.reconcile(
                crate::board_notifications::available(&db)?,
                "local registration changed",
            );
            state.prune(|name| db.agent(name).ok());
        }
        self.revoke_event_controls_locked(&db)?;
        drop(db);
        self.wake.notify_one();
        self.network_wake.notify_one();
        let result = result?;
        if registration {
            self.claude_wake.notify_one();
        }
        Ok(result)
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
            current.enabled
                && current.provider == agent.provider
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
        if agent.provider == Provider::Claude
            && let Some(reason) = crate::clients::claude::stale_reason(Path::new(&agent.endpoint))
        {
            return Reply::error(503, "unavailable", reason);
        }
        // The native admission checks and OpenCode durable lease/queue triggers in
        // Db::send remain authoritative. No native conversation is started here.
        let result = (|| -> Result<Value> {
            let tx = db.conn.unchecked_transaction()?;
            let message = db.send(&request.id, "user", &owner.name, &request.text, &None)?;
            let receipt = json!({"accepted":true,"id":request.id,"message":self.message_value(db, &message)?,"state":message.state,"duplicate":false});
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
                } else if message.contains("lease")
                    || message.contains("admission")
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
            .filter_map(|event| match event {
                crate::protocol::EventSelector::BoardChanged { board } => Some(board.clone()),
                _ => None,
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

    /// Retire missed Claude SessionEnd registrations only after two consecutive
    /// sweeps fail. A successful probe or a fresh registration resets suspicion.
    pub fn reap_claude_sessions(&self) -> Result<()> {
        let db = self.db.lock().unwrap();
        let mut previous = self.claude_failures.lock().unwrap();
        let mut failed = std::collections::HashMap::new();
        let agents = db.listed_agents(false)?;
        let mut live = std::collections::HashSet::new();
        let mut stale = Vec::new();
        for agent in agents
            .into_iter()
            .filter(|agent| agent.provider == Provider::Claude)
        {
            if crate::clients::claude::stale_reason(Path::new(&agent.endpoint)).is_none() {
                live.insert(agent.session.clone());
            } else {
                stale.push(agent);
            }
        }
        for agent in stale {
            // An old alias socket cannot retire a native session that remains
            // reachable through another exact registration of that identity.
            if live.contains(&agent.session) {
                continue;
            }
            let identity = (agent.session.clone(), agent.endpoint.clone());
            if previous.get(&agent.name) == Some(&identity) {
                db.close_native(&agent.name, &agent.session, &agent.endpoint)?;
            } else {
                failed.insert(agent.name, identity);
            }
        }
        *previous = failed;
        {
            let mut state = self.board_notifications.lock().unwrap();
            state.reconcile(
                crate::board_notifications::available(&db)?,
                "local registration changed",
            );
            state.prune(|name| db.agent(name).ok());
        }
        self.revoke_event_controls_locked(&db)?;
        drop(db);
        self.network_wake.notify_one();
        Ok(())
    }

    async fn reap_claude_loop(&self) -> Result<()> {
        let period = Duration::from_secs(60);
        let mut interval = tokio::time::interval_at(tokio::time::Instant::now() + period, period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            self.reap_claude_sessions()?;
        }
    }

    pub fn claude_sessions(&self) -> Result<Vec<String>> {
        Ok(self
            .db
            .lock()
            .unwrap()
            .agents()?
            .into_iter()
            .filter(|a| a.provider == Provider::Claude && a.enabled)
            .map(|a| a.session)
            .collect())
    }

    pub fn observe_claude_name(&self, session: &str, name: &str) -> Result<()> {
        self.db.lock().unwrap().observe_claude_name(session, name)
    }

    pub fn observe_codex(&self, thread: &Value) -> Result<()> {
        let session = thread["id"]
            .as_str()
            .context("Native thread is missing its ID")?;
        let enabled = thread["status"]["type"] != "notLoaded";
        let mut db = self.db.lock().unwrap();
        db.observe_codex(session, thread["name"].as_str(), enabled)?;
        if !enabled {
            db.codex_offline(Some(session))?;
        }
        {
            let mut state = self.board_notifications.lock().unwrap();
            state.reconcile(
                crate::board_notifications::available(&db)?,
                "local registration changed",
            );
            state.prune(|name| db.agent(name).ok());
        }
        self.revoke_event_controls_locked(&db)?;
        drop(db);
        self.wake.notify_one();
        self.network_wake.notify_one();
        Ok(())
    }

    /// Reconcile the complete initial loaded scan without closing unchanged
    /// sessions merely because the read-only observer connected after startup.
    pub fn reconcile_codex_loaded(&self, loaded: &std::collections::HashSet<String>) -> Result<()> {
        let db = self.db.lock().unwrap();
        for agent in db.listed_agents(false)? {
            if agent.provider == Provider::Codex && !loaded.contains(&agent.session) {
                db.codex_offline(Some(&agent.session))?;
            }
        }
        {
            let mut state = self.board_notifications.lock().unwrap();
            state.reconcile(
                crate::board_notifications::available(&db)?,
                "Codex session absent from complete loaded scan",
            );
            state.prune(|name| db.agent(name).ok());
        }
        self.revoke_event_controls_locked(&db)?;
        Ok(())
    }

    pub fn codex_offline(&self, session: Option<&str>) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.codex_offline(session)?;
        {
            let mut state = self.board_notifications.lock().unwrap();
            state.reconcile(
                crate::board_notifications::available(&db)?,
                "local registration changed",
            );
            state.prune(|name| db.agent(name).ok());
        }
        self.revoke_event_controls_locked(&db)?;
        Ok(())
    }

    pub fn discovery_status(&self, status: Value) {
        *self.discovery_status.lock().unwrap() = status;
    }

    async fn dispatch(&self) -> Result<()> {
        let mut workers = JoinSet::new();
        let mut codex_workers = CodexWorkers::default();
        loop {
            {
                let db = self.db.lock().unwrap();
                db.opencode_tick(crate::db::now())?;
                let mut state = self.board_notifications.lock().unwrap();
                state.reconcile(
                    crate::board_notifications::available(&db)?,
                    "OpenCode lease expired",
                );
                state.prune(|name| db.agent(name).ok());
            }
            // Construct and enable before querying so arrivals cannot be lost between query and wait.
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            while workers.len() < 16 {
                let next = {
                    let mut db = self.db.lock().unwrap();
                    self.revoke_event_controls_locked(&db)?;
                    db.claim_excluding_codex(&codex_workers.sessions())?.map(|(msg, agent)| {
                        let machines = json!({"source_machine":self.message_machine(&db, &msg.sender)?,"destination_machine":self.message_machine(&db, &msg.recipient)?,"sender_name":self.message_sender_name(&db, &msg.sender)});
                        let dispatch = if agent.provider==Provider::OpenCode {
                            let content = crate::clients::Adapter::OpenCode.message_content(&msg,&self.state_dir,&machines);
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
                    }).transpose()?.flatten()
                };
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
                        crate::clients::opencode::deliver_dispatch(&agent, dispatch).await
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
                        outcome.detail =
                            "Native accepted typed control; no acknowledgement is required".into();
                    }
                    if outcome.state != "submitted"
                        && let Some(token) = &event_token
                    {
                        this.board_notifications.lock().unwrap().lost(token, &agent);
                    }
                    let db = this.db.lock().unwrap();
                    let result = if let Some(dispatch) = &dispatch {
                        db.opencode_finish(&msg.id, dispatch, outcome.state, &outcome.detail)
                    } else {
                        db.finish(&msg.id, outcome.state, &outcome.detail)
                    };
                    drop(db);
                    if let Err(e) = result {
                        eprintln!("Could not persist delivery outcome {}: {e:#}", msg.id);
                    }
                    this.wake.notify_one();
                    this.network_wake.notify_one();
                });
                codex_workers.track(worker.id(), &worker_agent);
            }
            tokio::select! {
                _=&mut notified=>{},
                _=tokio::time::sleep(Duration::from_secs(1))=>{},
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
                self.request_async(serde_json::from_slice(&frame).context("Invalid request")?)
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
    let claude_reaper = broker.reap_claude_loop();
    tokio::pin!(claude_reaper);
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
    let observer = crate::clients::observe(bound.broker.clone(), codex_home);
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
            res=bound.listener.accept()=>{
                let (stream,_)=res?;
                if let Ok(permit)=permits.clone().try_acquire_owned(){
                    let broker=bound.broker.clone();
                    connections.spawn(async move {let _permit=permit;if let Err(e)=broker.connection(stream).await{eprintln!("Connection: {e:#}");}});
                }
            },
            Some(_)=connections.join_next(),if !connections.is_empty()=>{},
            res=&mut dispatcher=>break res,
            res=&mut claude_reaper=>break res,
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
            anyhow::bail!("{}", response.error.unwrap_or_default())
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
mod machine_tests {
    use super::*;

    #[tokio::test]
    async fn message_sender_labels_prefer_native_names_but_preserve_manual_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let bound = Broker::bind(directory.path().join("state"), "/missing/codex".into()).unwrap();
        let broker = &bound.broker;
        let mut db = broker.db.lock().unwrap();
        db.observe_codex(
            "97e8d81e-073d-4cde-a017-4a8255b7b3b8",
            Some("researcher"),
            true,
        )
        .unwrap();
        db.register(
            &crate::protocol::Agent {
                name: "reviewer".into(),
                provider: Provider::Codex,
                session: "97e8d81e-073d-4cde-a017-4a8255b7b3b8".into(),
                endpoint: String::new(),
                enabled: true,
            },
            false,
        )
        .unwrap();
        assert_eq!(
            broker.message_sender_name(&db, "codex-97e8d81e-073d-4cde-a017-4a8255b7b3b8"),
            "researcher"
        );
        assert_eq!(broker.message_sender_name(&db, "reviewer"), "reviewer");
        assert_eq!(broker.message_sender_name(&db, "missing"), "missing");
        assert_eq!(
            broker.message_sender_name(&db, "unknown-peer/reviewer"),
            "unknown-peer/reviewer"
        );
        db.observe_codex(
            "97e8d81e-073d-4cde-a017-4a8255b7b3b8",
            Some("renamed"),
            true,
        )
        .unwrap();
        assert_eq!(
            broker.message_sender_name(&db, "codex-97e8d81e-073d-4cde-a017-4a8255b7b3b8"),
            "renamed"
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
            name: "opencode-owner-test".into(),
            provider: Provider::OpenCode,
            session: "ses_0123456789abcdefghijklmnop".into(),
            endpoint: format!("/tmp/{}.sock", uuid::Uuid::new_v4()),
            enabled: true,
        };
        // A verified-live registration fixture isolates the capability selector;
        // native endpoint/probe checks have separate real-socket coverage.
        broker
            .db
            .lock()
            .unwrap()
            .opencode_observe(&agent, true)
            .unwrap();
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
                name: "native".into(),
                provider: Provider::OpenCode,
                session: "ses_0123456789abcdefghijklmnop".into(),
                endpoint: format!("/tmp/{}.sock", uuid::Uuid::new_v4()),
                enabled: true,
            };
            let (message, dispatch) = {
                let mut db = broker.db.lock().unwrap();
                // Inject a verified-live database fixture; native probes have
                // separate real-socket coverage. No dispatcher runs in this test.
                db.opencode_observe(&agent, true).unwrap();
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
                db.accept_remote(&local, &origin, &envelope).unwrap();
                let (msg, target) = db.claim().unwrap().unwrap();
                let frame = db
                    .opencode_dispatch(&msg, &target, "durable peer content")
                    .unwrap();
                (msg, frame)
            };
            // This is the deterministic gate after receiving the delivery frame,
            // before the plugin invokes the real privileged admission operation.
            assert_eq!(dispatch.frame["op"], "deliver");
            let instance =
                crate::clients::opencode::endpoint_instance(Path::new(&agent.endpoint)).unwrap();
            let admission = || Request::OpenCodeAdmit {
                id: message.id.clone(),
                session: agent.session.clone(),
                generation: dispatch.generation.clone(),
                instance: instance.clone(),
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
            enabled: true,
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
            db.send(
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
        db.ack(&first.id, &one.name).unwrap();
        assert!(
            db.claim_excluding_codex(&busy.sessions())
                .unwrap()
                .is_none()
        );
        let unrelated = send(&db, &other);
        assert_eq!(
            db.claim_excluding_codex(&busy.sessions())
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
            db.claim_excluding_codex(&busy.sessions())
                .unwrap()
                .unwrap()
                .0
                .id,
            second.id
        );
        db.finish(&second.id, "submitted", "accepted").unwrap();
        assert_eq!(db.get(&first.id).unwrap().state, "acknowledged");
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
                db.claim_excluding_codex(&busy.sessions())
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
                db.claim_excluding_codex(&busy.sessions())
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
mod board_timestamp_integration_tests {
    use super::*;

    #[tokio::test]
    async fn timestamp_persistence_error_still_prunes_lifecycle_subscriptions() {
        let root = tempfile::tempdir().unwrap();
        let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
        let broker = &bound.broker;
        let agent = crate::protocol::Agent {
            name: "timestamp-watcher".into(),
            provider: Provider::Codex,
            session: uuid::Uuid::new_v4().to_string(),
            endpoint: String::new(),
            enabled: true,
        };
        broker
            .request(Request::Register {
                agent: agent.clone(),
                replace: false,
            })
            .unwrap();
        broker
            .request(Request::Subscribe {
                board_revisions: Default::default(),
                agent: agent.name.clone(),
                events: vec![
                    crate::protocol::EventSelector::AgentOpened {},
                    crate::protocol::EventSelector::BoardChanged {
                        board: Default::default(),
                    },
                ],
            })
            .unwrap();
        let before = broker.request(Request::BoardRead).unwrap();
        assert_eq!(
            broker
                .board_notifications
                .lock()
                .unwrap()
                .events(&agent.name)
                .len(),
            2
        );
        // SQLite FAIL retains earlier trigger statements, modeling a failed
        // metadata write that changes registration availability before returning.
        broker
            .db
            .lock()
            .unwrap()
            .conn
            .execute_batch(
                "CREATE TEMP TRIGGER fail_board_timestamp BEFORE INSERT ON settings
             WHEN NEW.key='board_timestamps' BEGIN
               UPDATE agents SET enabled=0 WHERE name='timestamp-watcher';
               SELECT RAISE(FAIL, 'timestamp persistence fixture failure');
             END;",
            )
            .unwrap();
        let error = broker
            .request(Request::board_update(
                Default::default(),
                before["revision"].as_str().unwrap().into(),
                vec![crate::board::Edit::Set {
                    markdown: "applied before metadata failure".into(),
                }],
                None,
            ))
            .unwrap_err();
        assert!(format!("{error:#}").contains("timestamp persistence fixture failure"));
        assert!(
            !broker
                .db
                .lock()
                .unwrap()
                .agent(&agent.name)
                .unwrap()
                .enabled
        );
        assert!(
            broker
                .board_notifications
                .lock()
                .unwrap()
                .events(&agent.name)
                .is_empty()
        );
        // This is the documented post-rename ambiguity, not an unapplied write.
        assert_eq!(
            crate::board::read(&broker.state_dir).unwrap().markdown,
            "applied before metadata failure"
        );
        broker
            .db
            .lock()
            .unwrap()
            .conn
            .execute_batch("DROP TRIGGER fail_board_timestamp")
            .unwrap();
        let after = broker.request(Request::BoardRead).unwrap();
        assert_ne!(after["revision"], before["revision"]);
        assert!(after["last_updated"].is_i64());
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
