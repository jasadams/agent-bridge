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
            board_notifications: Arc::new(Mutex::new(crate::board_notifications::State::default())),
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
            Request::BoardSubscribe { agent } => Request::NamedBoardSubscribe {
                board: Default::default(),
                agent,
            },
            Request::BoardUnsubscribe { agent } => Request::NamedBoardUnsubscribe {
                board: Default::default(),
                agent,
            },
            request => request,
        };
        let result = match req {
            // Board edits share the broker's local administration mutex. No
            // network request can invoke this API or select a filesystem path.
            Request::BoardList => json!(crate::board::list(&self.state_dir)?),
            Request::BoardCreate { board } => json!(crate::board::create(&self.state_dir, &board)?),
            Request::NamedBoardRead { board } => {
                json!(crate::board::read_named(&self.state_dir, &board)?)
            }
            Request::BoardRead
            | Request::BoardUpdate { .. }
            | Request::BoardSubscribe { .. }
            | Request::BoardUnsubscribe { .. } => {
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
                match self.update_board_locked(&name, &expected_revision, &edits, actor.as_ref()) {
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
            Request::NamedBoardSubscribe { board: name, agent } => {
                let agent = db.agent(&agent)?;
                let board = crate::board::read_named(&self.state_dir, &name)?;
                let mut state = self.board_notifications.lock().unwrap();
                state.prune(|name| db.agent(name).ok());
                state.subscribe_named(
                    &name,
                    agent.clone(),
                    &board.revision,
                    std::time::Instant::now(),
                )?;
                json!({"subscribed":true,"agent":agent.name,"board":name,"revision":board.revision,"debounce_seconds":3,"max_delay_seconds":10,"persistent":false})
            }
            Request::NamedBoardUnsubscribe { board, agent } => {
                self.board_notifications
                    .lock()
                    .unwrap()
                    .unsubscribe_named(&board, &agent);
                json!({"subscribed":false,"agent":agent,"board":board})
            }
            Request::Register { mut agent, replace } => {
                crate::clients::validate_registration(&mut agent)?;
                if agent.provider == Provider::OpenCode {
                    db.opencode_alias_admission(&agent)?;
                }
                db.register(&agent, replace)?;
                self.claude_failures.lock().unwrap().remove(&agent.name);
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
                    name: crate::clients::lifecycle::routing_name(&Provider::OpenCode, &session),
                    provider: Provider::OpenCode,
                    session,
                    endpoint,
                    enabled: true,
                };
                crate::clients::validate_registration(&mut agent)?;
                let result = db.opencode_observe(&agent, ready)?;
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
            } => db.opencode_admit(&id, &session, &generation, &instance, &attempt)?,
            Request::OpenCodeNotice {
                id,
                recipient,
                content,
            } => {
                let agent = db.resolve_agent(&recipient)?;
                ensure!(
                    agent.provider == Provider::OpenCode && agent.enabled,
                    "Typed OpenCode notice requires an enabled exact local session"
                );
                let message = db.send(&id, "user", &agent.name, &content, &None)?;
                db.opencode_notice_content(&id, &content)?;
                json!({"queued":true,"message":message,"acknowledged":false})
            }
            Request::CodexNotice {
                id,
                recipient,
                session,
                content,
            } => {
                let message = db.codex_notice(&id, &recipient, &session, &content)?;
                json!({"queued":true,"message":message,"acknowledged":false})
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
                    self.message_value(&db, &db.send(&id, &sender, &recipient, &body, &parent)?)?
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
                json!({"version":crate::VERSION,"build_commit":crate::BUILD_COMMIT,"pid":std::process::id(),"state_dir":self.state_dir,"paused":db.paused()?,"messages":db.counts()?,"codex_discovery":self.discovery_status.lock().unwrap().clone(),"network":network,"monitor":self.monitor_control.lock().unwrap().status(),"machine":self.local_machine(&db)?,"updates":updates})
            }
            Request::RemoteList | Request::RemoteListPeer { .. } => {
                let mut agents = match &req {
                    Request::RemoteListPeer { peer } => db.remote_agents_for(peer)?,
                    _ => db.remote_agents()?,
                };
                let status = self.network_status.lock().unwrap().clone();
                if let Some(agents) = agents.as_array_mut() {
                    for agent in agents {
                        agent["online"] = json!(
                            status["connected"]
                                .as_array()
                                .is_some_and(|ids| ids.iter().any(|id| id == &agent["peer_id"]))
                        );
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
                    let display = if let Some(alias) = peer.and_then(|peer| peer.alias.as_deref()) {
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
                            .and_then(|entries| entries.iter().find(|e| e["peer_id"] == view.peer))
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
                    if let Some(entry) = entries.iter_mut().find(|entry| entry["peer_id"] == id) {
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
                let (peer_id, addresses): (String, Vec<libp2p::Multiaddr>) = if let Some(address) =
                    address
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
        };
        self.board_notifications
            .lock()
            .unwrap()
            .prune(|name| db.agent(name).ok());
        drop(db);
        self.wake.notify_one();
        self.network_wake.notify_one();
        if registration {
            self.claude_wake.notify_one();
        }
        Ok(result)
    }

    /// Caller holds the local DB administration mutex. HTTP additionally retains
    /// its monitor generation guard through this update and both observations.
    pub(crate) fn update_board_locked(
        &self,
        name: &crate::board::Name,
        expected: &str,
        edits: &[crate::board::Edit],
        actor: Option<&crate::protocol::Agent>,
    ) -> Result<crate::board::Snapshot> {
        let before = crate::board::read_named(&self.state_dir, name)?;
        self.board_notifications.lock().unwrap().observe_named(
            name,
            &before.revision,
            None,
            std::time::Instant::now(),
        );
        let board = crate::board::update_named(&self.state_dir, name, expected, edits)?;
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
        for agent in db.listed_agents(false)? {
            if agent.provider != Provider::Claude
                || crate::clients::claude::stale_reason(Path::new(&agent.endpoint)).is_none()
            {
                continue;
            }
            let identity = (agent.session.clone(), agent.endpoint.clone());
            if previous.get(&agent.name) == Some(&identity) {
                db.unregister(&agent.name, &agent.session)?;
            } else {
                failed.insert(agent.name, identity);
            }
        }
        *previous = failed;
        self.board_notifications
            .lock()
            .unwrap()
            .prune(|name| db.agent(name).ok());
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
        self.board_notifications
            .lock()
            .unwrap()
            .prune(|name| db.agent(name).ok());
        drop(db);
        self.wake.notify_one();
        self.network_wake.notify_one();
        Ok(())
    }

    pub fn codex_offline(&self, session: Option<&str>) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.codex_offline(session)?;
        self.board_notifications
            .lock()
            .unwrap()
            .prune(|name| db.agent(name).ok());
        Ok(())
    }

    pub fn discovery_status(&self, status: Value) {
        *self.discovery_status.lock().unwrap() = status;
    }

    async fn dispatch(&self) -> Result<()> {
        let mut workers = JoinSet::new();
        let mut codex_workers = CodexWorkers::default();
        loop {
            self.db.lock().unwrap().opencode_tick(crate::db::now())?;
            // Construct and enable before querying so arrivals cannot be lost between query and wait.
            let notified = self.wake.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            while workers.len() < 16 {
                let next = {
                    let mut db = self.db.lock().unwrap();
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
                        Ok::<_, anyhow::Error>(Some((msg, agent, machines, dispatch, raw_codex_notice)))
                    }).transpose()?.flatten()
                };
                let Some((msg, agent, machines, dispatch, raw_codex_notice)) = next else {
                    break;
                };
                let this = self.clone();
                let worker_agent = agent.clone();
                let worker = workers.spawn(async move {
                    let mut outcome = if raw_codex_notice {
                        crate::clients::deliver_content(&agent, &msg, &msg.body).await
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
        Ok(response.result.unwrap_or(Value::Null))
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
                .request(Request::BoardSubscribe {
                    agent: agent.name.clone()
                })
                .unwrap()["subscribed"]
                .as_bool()
                == Some(true)
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
            .request(Request::BoardUnsubscribe { agent: agent.name })
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
