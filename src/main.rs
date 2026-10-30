use agent_bridge::{
    board::Name,
    broker, hook,
    network::trust::Permission,
    protocol::{Agent, EventSelector, Provider, Request},
    service,
};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod update_install;

#[derive(Parser)]
#[command(
    version = agent_bridge::VERSION,
    about = "Durable local and peer messaging for native Claude, Codex and local OpenCode sessions"
)]
struct Cli {
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Commands,
}

async fn subscription_revisions(
    state: &std::path::Path,
    events: &[EventSelector],
) -> Result<std::collections::BTreeMap<Name, String>> {
    let mut revisions = std::collections::BTreeMap::new();
    for event in events {
        if let EventSelector::BoardChanged { board } = event
            && !board.is_main()
            && !revisions.contains_key(board)
        {
            let current = broker::rpc(state, &Request::board_read(board.clone())).await?;
            revisions.insert(
                board.clone(),
                current["revision"]
                    .as_str()
                    .context("Missing current board revision")?
                    .to_owned(),
            );
        }
    }
    Ok(revisions)
}

#[derive(Subcommand)]
enum Commands {
    /// Expose structured agent tools over a persistent local MCP stdio connection.
    Mcp,
    /// View and answer local shared Markdown board controls, or print its current Markdown.
    Board {
        /// Board name (defaults to main); named boards require explicit creation.
        #[arg(conflicts_with_all = ["list", "create", "archive", "restore"])]
        name: Option<Name>,
        /// List local boards without opening the terminal UI.
        #[arg(long, conflicts_with_all = ["raw", "create", "archive", "restore"])]
        list: bool,
        /// Explicitly create an empty board without opening the terminal UI.
        #[arg(long, value_name = "NAME", conflicts_with_all = ["raw", "archive", "restore", "archived"])]
        create: Option<Name>,
        /// Archive NAME for 30 days, after which managed archive data is permanently deleted.
        #[arg(long, value_name = "NAME", requires = "expected_revision", conflicts_with_all = ["name", "list", "create", "restore", "raw", "metadata", "archived"])]
        archive: Option<Name>,
        /// Restore an archived NAME before its retention deadline.
        #[arg(long, value_name = "NAME", requires = "expected_revision", conflicts_with_all = ["name", "list", "create", "archive", "raw", "metadata", "archived"])]
        restore: Option<Name>,
        /// Exact revision for --archive or --restore, obtained by reading or inspecting the board.
        #[arg(long)]
        expected_revision: Option<String>,
        /// List archived metadata, or explicitly inspect NAME with --raw/--metadata.
        #[arg(long, conflicts_with_all = ["create", "archive", "restore"])]
        archived: bool,
        /// Print Markdown once without opening the terminal UI.
        #[arg(long)]
        raw: bool,
        /// Print revision and UTC timestamp metadata as JSON, without Markdown.
        #[arg(long, conflicts_with_all = ["raw", "list", "create", "archive", "restore"])]
        metadata: bool,
    },
    /// Add explicit local event selectors; returns effective events and lifecycle snapshot.
    Subscribe {
        #[arg(long)]
        agent: String,
        /// JSON selector array, e.g. '[{"event":"board.changed","board":"main"}]'.
        #[arg(long)]
        events: String,
    },
    /// Remove selectors, or omit --events to remove all subscriptions.
    Unsubscribe {
        #[arg(long)]
        agent: String,
        /// JSON selector array to remove (must not be empty).
        #[arg(long)]
        events: Option<String>,
    },
    /// Append an input pinned to an exact enabled local agent registration.
    AddBoardMessageInput {
        agent: String,
        #[arg(long, default_value = "main")]
        board: Name,
        #[arg(long)]
        expected_revision: String,
        #[arg(long)]
        id: String,
        #[arg(long)]
        label: String,
    },
    /// Run the background broker (launchd runs this command).
    Serve {
        #[arg(long)]
        codex_bin: Option<PathBuf>,
        /// Disable read-only discovery of the running native Codex server.
        #[arg(long)]
        no_discovery: bool,
    },
    /// Register a running native session under a stable routing name.
    Register {
        #[arg(value_enum)]
        provider: Provider,
        name: String,
        #[arg(long, alias = "thread")]
        session: String,
        #[arg(long)]
        socket: Option<PathBuf>,
        #[arg(long)]
        replace: bool,
    },
    Unregister {
        name: String,
        #[arg(long)]
        session: String,
    },
    /// List available local sessions. Include stale registrations with --all.
    List {
        #[arg(long)]
        all: bool,
        /// Show the cached permitted directory of remote agents.
        #[arg(long, conflicts_with = "all")]
        remote: bool,
    },
    Status,
    /// Check for releases or update a managed installation after confirmation.
    Update {
        /// Refresh release availability without installing anything.
        #[arg(long, conflicts_with_all = ["disable_checks", "enable_checks"])]
        check: bool,
        /// Disable automatic daily release checks (explicit --check still works).
        #[arg(long, conflicts_with = "enable_checks")]
        disable_checks: bool,
        /// Enable automatic daily release checks.
        #[arg(long)]
        enable_checks: bool,
    },
    /// Read-only browser monitor, embedded in this broker.
    Monitor {
        #[command(subcommand)]
        action: MonitorAction,
    },
    /// Enable or disable peer networking and optional LAN discovery.
    Network {
        #[command(subcommand)]
        action: NetworkAction,
    },
    /// Operate an independent public rendezvous and encrypted circuit relay node.
    Relay {
        #[command(subcommand)]
        action: RelayAction,
    },
    /// Show this bridge's persistent public identity. Creates its key on first use.
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },
    /// Inspect or change this bridge's persisted machine display name.
    Machine {
        #[command(subcommand)]
        action: MachineAction,
    },
    /// Organize LAN or Internet pairing; owners compare and approve the verification code.
    Pairing {
        #[command(subcommand)]
        action: LanPairingAction,
    },
    /// Manage local trust policy for remote bridges. Does not open network listeners.
    Peers {
        #[command(subcommand)]
        action: PeerAction,
    },
    /// Persist a message, then return its ID. Native delivery happens asynchronously.
    Send {
        #[arg(long, default_value = "user")]
        from: String,
        #[arg(long)]
        to: String,
        /// Omit to read stdin, keeping sensitive bodies out of process listings.
        #[arg(long)]
        message: Option<String>,
        /// Reuse after an RPC timeout to avoid duplicate submissions.
        #[arg(long)]
        id: Option<String>,
        #[arg(long)]
        in_reply_to: Option<String>,
    },
    History {
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    Get {
        id: String,
    },
    /// Confirm receipt, independently of task completion.
    Ack {
        #[arg(long)]
        agent: String,
        id: String,
    },
    Retry {
        id: String,
        #[arg(long)]
        allow_duplicate: bool,
    },
    Cancel {
        id: String,
    },
    /// Pause new deliveries; in-flight submissions finish normally.
    Pause,
    Resume,
    /// Native lifecycle hook; reads hook JSON from stdin.
    Hook {
        #[arg(long, value_enum)]
        provider: Provider,
    },
    /// Explicitly install or remove the OpenCode V1 plugin and MCP configuration.
    Opencode {
        #[command(subcommand)]
        action: OpenCodeAction,
    },
    Hooks {
        #[command(subcommand)]
        action: HookAction,
    },
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Subcommand)]
enum OpenCodeAction {
    Install {
        #[arg(long)]
        config: Option<PathBuf>,
    },
    Uninstall {
        #[arg(long)]
        config: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum HookAction {
    Install,
    Uninstall,
}

#[derive(Subcommand)]
enum IdentityAction {
    Show,
}

#[derive(Subcommand)]
enum MachineAction {
    Show,
    /// Change the display name without changing identity, trust or queued messages.
    SetName {
        name: String,
    },
}

#[derive(Subcommand)]
enum MonitorAction {
    /// Enable loopback HTTP; prints URL and a separate browser access token.
    Enable {
        #[arg(long, default_value_t = 0)]
        port: u16,
    },
    /// Stop HTTP and invalidate browser access tokens and streams.
    Disable,
    /// Inspect listener state without exposing credentials.
    Status,
    /// Retrieve the current token through the private local broker socket.
    Access,
}

#[derive(Subcommand)]
enum RelayAction {
    /// Serve infrastructure only; use a state directory separate from the broker.
    Serve {
        #[arg(long)]
        listen: String,
        /// Public IP or DNS transport address advertised to clients.
        #[arg(long)]
        advertise: Option<String>,
        /// Add an encrypted TCP listener alongside the primary listener.
        #[arg(long)]
        tcp_listen: Option<String>,
        #[arg(long, requires = "tcp_listen")]
        tcp_advertise: Option<String>,
        #[arg(long)]
        max_reservations: Option<usize>,
        #[arg(long)]
        max_circuits: Option<usize>,
        #[arg(long)]
        max_circuit_bytes: Option<u64>,
        #[arg(long)]
        max_circuit_seconds: Option<u64>,
        #[arg(long)]
        reservation_seconds: Option<u64>,
        #[arg(long)]
        max_registrations: Option<usize>,
        #[arg(long)]
        registration_seconds: Option<u64>,
        #[arg(long)]
        max_connections: Option<u32>,
    },
}
#[derive(Subcommand)]
enum RelayConfigAction {
    List,
    /// Add a public relay with its complete pinned /p2p/identity address.
    Add {
        address: String,
    },
    Remove {
        peer_id: String,
    },
}

#[derive(Subcommand)]
enum NetworkAction {
    /// Configure independent relay/rendezvous operators. Does not grant trust.
    Relays {
        #[command(subcommand)]
        action: RelayConfigAction,
    },
    /// Persist opt-in networking and start an encrypted QUIC listener.
    Enable {
        /// IP/UDP/QUIC multiaddress; port 0 chooses an available port.
        #[arg(long, required_unless_present = "lan")]
        listen: Option<String>,
        /// Discover nearby bridges with mDNS. Defaults to listening on all IPv4 interfaces.
        #[arg(long)]
        lan: bool,
    },
    /// Stop listeners and transfers while preserving durable queues.
    Disable,
}

#[derive(Subcommand)]
enum LanPairingAction {
    /// Create a signed public contact link. Owners still compare and approve the code.
    Invite {
        /// Deliver owner prompts into this local agent (defaults to the current Codex session).
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 600)]
        seconds: u64,
        /// Render a locally generated QR in the terminal.
        #[arg(long)]
        qr: bool,
        /// Save a locally generated SVG without overwriting an existing file.
        #[arg(long)]
        svg: Option<PathBuf>,
    },
    /// Accept bounded pairing requests for a short time. Does not grant trust.
    Open {
        /// Select the local native agent that asks the owner Yes/No.
        #[arg(long)]
        agent: Option<String>,
        #[arg(long, default_value_t = 120)]
        seconds: u64,
    },
    /// Route pairing prompts into one local agent for this window or exchange.
    Watch {
        #[arg(long)]
        agent: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long, default_value_t = 120)]
        seconds: u64,
    },
    /// Close the window and cancel incomplete exchanges.
    Close,
    /// Inspect pending requests and local verification codes. Never auto-confirm these.
    List,
    /// Request pairing using a signed contact link, host:port, discovered name or peer ID.
    Connect {
        /// Select the local native agent that asks the owner Yes/No.
        #[arg(long)]
        agent: Option<String>,
        peer: String,
        #[arg(long)]
        address: Option<String>,
        #[arg(long, default_value_t = 120)]
        seconds: u64,
        /// Explicitly permit using the public relay operators carried by a signed link.
        #[arg(long)]
        use_invite_relays: bool,
    },
    /// Accept a pending request after the local owner authorizes it.
    Accept {
        id: String,
    },
    /// Confirm only after the owner compares the other machine's code through a trusted channel.
    Confirm {
        id: String,
        #[arg(long)]
        code: String,
    },
    Cancel {
        id: String,
    },
}

#[derive(Subcommand)]
enum PeerAction {
    List,
    /// Show trust, pending pairing, network connectivity and session sharing.
    Connections,
    /// List shared and partially permitted local sessions, including stale grants.
    Shared,
    /// Share exactly one local session for discovery and two-way messaging.
    Share {
        peer: String,
        #[arg(long)]
        agent: String,
    },
    /// Remove all three permissions for this exact local session.
    Unshare {
        peer: String,
        #[arg(long, required_unless_present = "session", conflicts_with_all = ["provider", "session"])]
        agent: Option<String>,
        /// Remove stale grants by exact provider/session instead of a current routing name.
        #[arg(long, value_enum, requires = "session")]
        provider: Option<Provider>,
        #[arg(long, requires = "provider")]
        session: Option<String>,
    },
    /// List unverified LAN connection hints. Discovery does not grant trust.
    Discover,
    /// Start a single-use mutual pairing exchange; no session access is granted.
    Offer {
        #[arg(long, default_value_t = 120)]
        seconds: u64,
    },
    /// Verify an offer fingerprint and return a signed response. Omit token to read stdin.
    Join {
        token: Option<String>,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        alias: Option<String>,
    },
    /// Verify the responder fingerprint and complete local pairing. Omit token for stdin.
    Finish {
        token: Option<String>,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        alias: Option<String>,
    },
    /// Invalidate an outstanding local pairing offer.
    CancelOffer {
        nonce: String,
    },
    /// Create a signed invitation with a bounded lifetime and no agent grants.
    Invite {
        #[arg(long, default_value_t = 120)]
        seconds: u64,
    },
    /// Verify and consume an invitation once on this bridge after comparing its fingerprint.
    Accept {
        token: String,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        alias: Option<String>,
    },
    /// Set the explicit IP/UDP/QUIC address of an already paired peer.
    Add {
        peer: String,
        #[arg(long)]
        address: String,
    },
    /// Pair after comparing the full public fingerprint through a trusted channel.
    Pair {
        peer_id: String,
        #[arg(long)]
        fingerprint: String,
        #[arg(long)]
        alias: Option<String>,
    },
    /// Grant a peer discovery, sending or receiving permission for one local session.
    Allow {
        peer: String,
        #[arg(long)]
        agent: String,
        #[arg(long, value_enum)]
        permission: Permission,
    },
    /// Remove one grant immediately.
    Deny {
        peer: String,
        #[arg(long)]
        agent: String,
        #[arg(long, value_enum)]
        permission: Permission,
    },
    /// Revoke a peer and remove all its grants. Re-pairing requires fresh grants.
    Revoke {
        peer: String,
    },
}
#[derive(Subcommand)]
enum ServiceAction {
    Install {
        #[arg(long)]
        codex_bin: Option<PathBuf>,
    },
    Uninstall,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let invite_output = match &cli.command {
        Commands::Pairing {
            action: LanPairingAction::Invite { qr, svg, .. },
        } => Some((*qr, svg.clone())),
        _ => None,
    };
    let notify_agent = match &cli.command {
        Commands::Pairing {
            action:
                LanPairingAction::Open { agent, seconds }
                | LanPairingAction::Invite { agent, seconds, .. }
                | LanPairingAction::Connect { agent, seconds, .. },
        } => {
            let current = std::env::var("CODEX_THREAD_ID")
                .ok()
                .filter(|id| uuid::Uuid::parse_str(id).is_ok())
                .map(|id| format!("codex-{id}"));
            agent.clone().or(current).map(|agent| (agent, *seconds))
        }
        _ => None,
    };
    let state_dir = match cli.state_dir {
        Some(path) => path,
        None if matches!(&cli.command, Commands::Relay { .. }) => {
            service::default_state_dir()?.with_file_name("AgentBridgeRelay")
        }
        None => service::default_state_dir()?,
    };
    let req = match cli.command {
        Commands::Update {
            check,
            disable_checks,
            enable_checks,
        } => {
            return update_install::run(&state_dir, check, disable_checks, enable_checks);
        }
        Commands::Mcp => return agent_bridge::mcp::run(state_dir).await,
        Commands::Opencode { action } => {
            let (install, config) = match action {
                OpenCodeAction::Install { config } => (true, config),
                OpenCodeAction::Uninstall { config } => (false, config),
            };
            return agent_bridge::opencode_install::configure(
                &state_dir,
                config.as_deref(),
                install,
            );
        }

        Commands::AddBoardMessageInput {
            agent,
            board,
            expected_revision,
            id,
            label,
        } => {
            let value = broker::rpc(
                &state_dir,
                &Request::AddBoardMessageInput {
                    agent,
                    board,
                    expected_revision,
                    id,
                    label,
                },
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            return Ok(());
        }
        Commands::Board {
            name,
            raw,
            list,
            create,
            metadata,
            archive,
            restore,
            expected_revision,
            archived,
        } => {
            anyhow::ensure!(
                expected_revision.is_none() || archive.is_some() || restore.is_some(),
                "--expected-revision requires --archive or --restore"
            );
            if archive.is_some() || restore.is_some() {
                let expected_revision =
                    expected_revision.context("An exact --expected-revision is required")?;
                let request = if let Some(board) = archive {
                    Request::BoardArchive {
                        board,
                        expected_revision,
                    }
                } else {
                    Request::BoardRestore {
                        board: restore.context("Missing restore board")?,
                        expected_revision,
                    }
                };
                let mut value = broker::rpc(&state_dir, &request).await?;
                if let Some(object) = value.as_object_mut() {
                    object.remove("markdown");
                    object.remove("path");
                }
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(());
            }
            if list || (archived && name.is_none() && !raw && !metadata) {
                let request = if archived {
                    Request::BoardArchivedList
                } else {
                    Request::BoardList
                };
                let value = broker::rpc(&state_dir, &request).await?;
                if archived {
                    println!("{}", serde_json::to_string_pretty(&value)?);
                    return Ok(());
                }
                let boards: Vec<agent_bridge::board::Summary> = serde_json::from_value(value)?;
                for board in boards {
                    println!(
                        "{}\t{}\t{}",
                        board.name,
                        if board.exists { "exists" } else { "absent" },
                        board.revision
                    );
                }
                return Ok(());
            }
            if let Some(board) = create {
                broker::rpc(
                    &state_dir,
                    &Request::BoardCreate {
                        board: board.clone(),
                    },
                )
                .await?;
                println!("Created board {board}");
                return Ok(());
            }
            let name = name.unwrap_or_default();
            if metadata {
                let request = if archived {
                    Request::BoardInspect { board: name }
                } else {
                    Request::board_read(name)
                };
                let mut board = broker::rpc(&state_dir, &request).await?;
                if let Some(object) = board.as_object_mut() {
                    object.remove("markdown");
                    object.remove("path");
                }
                println!("{}", serde_json::to_string_pretty(&board)?);
                return Ok(());
            }
            if raw {
                let request = if archived {
                    Request::BoardInspect {
                        board: name.clone(),
                    }
                } else {
                    Request::board_read(name.clone())
                };
                let value = broker::rpc(&state_dir, &request).await?;
                let board: agent_bridge::board::Snapshot = serde_json::from_value(value)?;
                use std::io::Write;
                std::io::stdout().write_all(board.markdown.as_bytes())?;
                return Ok(());
            }
            return agent_bridge::board_tui::run_named(state_dir, name).await;
        }
        Commands::Serve {
            codex_bin,
            no_discovery,
        } => {
            unsafe {
                libc::umask(0o077);
            }
            let bin = codex_bin.unwrap_or_else(|| {
                service::find_binary("codex").unwrap_or_else(|| PathBuf::from("codex"))
            });
            let codex_home = if no_discovery {
                None
            } else {
                Some(
                    std::env::var_os("CODEX_HOME")
                        .map(PathBuf::from)
                        .unwrap_or(service::home()?.join(".codex")),
                )
            };
            return broker::serve_with_discovery(broker::Broker::bind(state_dir, bin)?, codex_home)
                .await;
        }
        Commands::Register {
            provider,
            name,
            session,
            socket,
            replace,
        } => Request::Register {
            agent: Agent {
                name,
                provider,
                session,
                endpoint: socket
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                enabled: true,
            },
            replace,
        },
        Commands::Unregister { name, session } => Request::Unregister { name, session },
        Commands::List { all, remote } => {
            if remote {
                Request::RemoteList
            } else {
                Request::List { all }
            }
        }
        Commands::Subscribe { agent, events } => {
            let events = serde_json::from_str::<Vec<EventSelector>>(&events)
                .context("Invalid event selector JSON")?;
            let board_revisions = subscription_revisions(&state_dir, &events).await?;
            Request::Subscribe {
                agent,
                events,
                board_revisions,
            }
        }
        Commands::Unsubscribe { agent, events } => {
            let events = events
                .map(|value| serde_json::from_str::<Vec<EventSelector>>(&value))
                .transpose()
                .context("Invalid event selector JSON")?;
            let board_revisions =
                subscription_revisions(&state_dir, events.as_deref().unwrap_or_default()).await?;
            Request::Unsubscribe {
                agent,
                events,
                board_revisions,
            }
        }
        Commands::Status => Request::Status,
        Commands::Monitor { action } => match action {
            MonitorAction::Enable { port } => Request::MonitorEnable { port },
            MonitorAction::Disable => Request::MonitorDisable,
            MonitorAction::Status => Request::MonitorStatus,
            MonitorAction::Access => Request::MonitorAccess,
        },
        Commands::Relay { action } => match action {
            RelayAction::Serve {
                listen,
                advertise,
                tcp_listen,
                tcp_advertise,
                max_reservations,
                max_circuits,
                max_circuit_bytes,
                max_circuit_seconds,
                reservation_seconds,
                max_registrations,
                registration_seconds,
                max_connections,
            } => {
                let mut limits = agent_bridge::network::relay_node::Limits::default();
                if let Some(value) = max_reservations {
                    limits.max_reservations = value;
                }
                if let Some(value) = max_circuits {
                    limits.max_circuits = value;
                }
                if let Some(value) = max_circuit_bytes {
                    limits.max_circuit_bytes = value;
                }
                if let Some(value) = max_circuit_seconds {
                    limits.max_circuit_seconds = value;
                }
                if let Some(value) = reservation_seconds {
                    limits.reservation_seconds = value;
                }
                if let Some(value) = max_registrations {
                    limits.max_registrations = value;
                }
                if let Some(value) = registration_seconds {
                    limits.registration_seconds = value;
                }
                if let Some(value) = max_connections {
                    limits.max_connections = value;
                }
                let mut listeners = vec![(listen, advertise)];
                if let Some(listen) = tcp_listen {
                    anyhow::ensure!(
                        agent_bridge::network::relay_config::carrier(&listen)? == "tcp",
                        "--tcp-listen requires a TCP address"
                    );
                    listeners.push((listen, tcp_advertise));
                }
                return agent_bridge::network::relay_node::serve_with_listeners(
                    &state_dir, listeners, limits,
                )
                .await;
            }
        },
        Commands::Network { action } => match action {
            NetworkAction::Relays { action } => match action {
                RelayConfigAction::List => Request::RelayList,
                RelayConfigAction::Add { address } => Request::RelayAdd { address },
                RelayConfigAction::Remove { peer_id } => Request::RelayRemove { peer_id },
            },
            NetworkAction::Enable { listen, lan } => {
                if lan {
                    Request::NetworkLan { listen }
                } else {
                    Request::NetworkConfigure {
                        enabled: true,
                        listen,
                    }
                }
            }
            NetworkAction::Disable => Request::NetworkConfigure {
                enabled: false,
                listen: None,
            },
        },
        Commands::Identity {
            action: IdentityAction::Show,
        } => Request::Identity,
        Commands::Machine { action } => match action {
            MachineAction::Show => Request::MachineShow,
            MachineAction::SetName { name } => Request::MachineSetName { name },
        },
        Commands::Pairing { action } => match action {
            LanPairingAction::Invite { seconds, .. } => Request::ContactCreate { seconds },
            LanPairingAction::Open { seconds, .. } => Request::LanPairingOpen { seconds },
            LanPairingAction::Watch { agent, id, seconds } => {
                Request::LanPairingWatch { agent, id, seconds }
            }
            LanPairingAction::Close => Request::LanPairingClose,
            LanPairingAction::List => Request::LanPairingList,
            LanPairingAction::Connect {
                peer,
                address,
                seconds,
                use_invite_relays,
                ..
            } => {
                if peer.starts_with(agent_bridge::network::contact::PREFIX) {
                    anyhow::ensure!(
                        address.is_none(),
                        "Contact links carry their own routing hints; omit --address"
                    );
                    Request::ContactConnect {
                        link: peer,
                        use_invite_relays,
                        seconds,
                    }
                } else {
                    anyhow::ensure!(
                        !use_invite_relays,
                        "--use-invite-relays requires a signed contact link"
                    );
                    if address.is_none() && (peer.starts_with('/') || peer.contains(':')) {
                        Request::ContactLocation {
                            location: peer,
                            seconds,
                        }
                    } else {
                        Request::LanPairingConnect {
                            peer,
                            address,
                            seconds,
                        }
                    }
                }
            }
            LanPairingAction::Accept { id } => Request::LanPairingAccept { id },
            LanPairingAction::Confirm { id, code } => Request::LanPairingConfirm { id, code },
            LanPairingAction::Cancel { id } => Request::LanPairingCancel { id },
        },
        Commands::Peers { action } => match action {
            PeerAction::List => Request::PeersList,
            PeerAction::Connections => Request::PeerConnections,
            PeerAction::Shared => Request::SharedSessions,
            PeerAction::Share { peer, agent } => Request::PeerShare {
                peer,
                agent,
                shared: true,
            },
            PeerAction::Unshare {
                peer,
                agent,
                provider,
                session,
            } => match (agent, provider, session) {
                (Some(agent), None, None) => Request::PeerShare {
                    peer,
                    agent,
                    shared: false,
                },
                (None, Some(provider), Some(session)) => Request::PeerUnshareSession {
                    peer,
                    provider,
                    session,
                },
                _ => anyhow::bail!("Choose --agent or --provider with --session"),
            },
            PeerAction::Discover => Request::PeersDiscover,
            PeerAction::Offer { seconds } => Request::PairingOffer { seconds },
            PeerAction::Join {
                token,
                fingerprint,
                alias,
            } => Request::PairingJoin {
                token: pairing_token(token)?,
                fingerprint,
                alias,
            },
            PeerAction::Finish {
                token,
                fingerprint,
                alias,
            } => Request::PairingFinish {
                token: pairing_token(token)?,
                fingerprint,
                alias,
            },
            PeerAction::CancelOffer { nonce } => Request::PairingCancel { nonce },
            PeerAction::Invite { seconds } => Request::InvitationCreate { seconds },
            PeerAction::Accept {
                token,
                fingerprint,
                alias,
            } => Request::InvitationAccept {
                token,
                fingerprint,
                alias,
            },
            PeerAction::Add { peer, address } => Request::PeerAddress { peer, address },
            PeerAction::Pair {
                peer_id,
                fingerprint,
                alias,
            } => Request::PeerPair {
                peer_id,
                fingerprint,
                alias,
            },
            PeerAction::Allow {
                peer,
                agent,
                permission,
            } => Request::PeerGrant {
                peer,
                agent,
                permission,
                allow: true,
            },
            PeerAction::Deny {
                peer,
                agent,
                permission,
            } => Request::PeerGrant {
                peer,
                agent,
                permission,
                allow: false,
            },
            PeerAction::Revoke { peer } => Request::PeerRevoke { peer },
        },
        Commands::Send {
            from,
            to,
            message,
            id,
            in_reply_to,
        } => {
            let body = match message {
                Some(s) => s,
                None => {
                    use std::io::Read;
                    let mut s = String::new();
                    std::io::stdin()
                        .take((agent_bridge::protocol::MAX_BODY + 1) as u64)
                        .read_to_string(&mut s)?;
                    s
                }
            };
            let id = id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            eprintln!("Message ID: {id}");
            Request::Send {
                id,
                sender: from,
                recipient: to,
                body,
                parent: in_reply_to,
            }
        }
        Commands::History { agent, limit } => Request::History { agent, limit },
        Commands::Get { id } => Request::Get { id },
        Commands::Ack { id, agent } => Request::Ack { id, agent },
        Commands::Retry {
            id,
            allow_duplicate,
        } => Request::Retry {
            id,
            allow_duplicate,
        },
        Commands::Cancel { id } => Request::Cancel { id },
        Commands::Pause => Request::Pause { paused: true },
        Commands::Resume => Request::Pause { paused: false },
        Commands::Hook { provider } => return hook::run(&state_dir, provider).await,
        Commands::Hooks { action } => {
            hook::configure(&state_dir, matches!(action, HookAction::Install))
                .context("Could not update native hook configuration")?;
            return Ok(());
        }
        Commands::Service { action } => {
            match action {
                ServiceAction::Install { codex_bin } => service::install(&state_dir, codex_bin)?,
                ServiceAction::Uninstall => service::uninstall()?,
            }
            return Ok(());
        }
    };
    // Validate the selected local destination before opening admission or
    // creating an exchange. Failed selection must not leave a hidden window.
    if let Some((agent, _)) = &notify_agent {
        let agents = broker::rpc(&state_dir, &Request::List { all: false }).await?;
        anyhow::ensure!(
            agents
                .as_array()
                .is_some_and(|agents| agents.iter().any(|entry| entry["name"] == *agent
                    && entry["enabled"] == true
                    && entry["provider"]
                        .as_str()
                        .and_then(|provider| provider.parse::<Provider>().ok())
                        .is_some_and(|provider| agent_bridge::clients::supports(
                            &provider,
                            agent_bridge::clients::Capability::PairingPrompts
                        )))),
            "Pairing owner input is unavailable for this session; omit --agent and use the explicit pairing CLI confirmation path"
        );
    }
    let mut result = broker::rpc(&state_dir, &req).await?;
    if let Some((agent, seconds)) = notify_agent {
        let id = result["id"].as_str().map(str::to_owned);
        let subscription =
            broker::rpc(&state_dir, &Request::LanPairingWatch { agent, id, seconds }).await?;
        result["notifications"] = subscription;
    }
    if let Some((qr, svg)) = invite_output {
        if let Some(path) = svg {
            use std::{io::Write, os::unix::fs::OpenOptionsExt};
            let contents = result["qr_svg"]
                .as_str()
                .context("Service omitted contact SVG")?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("Could not create contact SVG {}", path.display()))?;
            file.write_all(contents.as_bytes())?;
        }
        if qr {
            println!(
                "{}",
                result["qr_terminal"]
                    .as_str()
                    .context("Service omitted contact QR")?
            );
        }
        let object = result.as_object_mut().context("Invalid contact response")?;
        object.remove("qr_svg");
        object.remove("qr_terminal");
    }
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

// Bound stdin before allocating a pairing token; copying via stdin avoids shell history.
fn pairing_token(token: Option<String>) -> Result<String> {
    use std::io::Read;
    let token = match token {
        Some(token) => token,
        None => {
            let mut token = String::new();
            std::io::stdin().take(32769).read_to_string(&mut token)?;
            token
        }
    };
    let token = token.trim();
    anyhow::ensure!(
        !token.is_empty() && token.len() <= 32768,
        "Invalid pairing token size"
    );
    Ok(token.to_owned())
}

#[cfg(test)]
mod update_cli_tests {
    use super::*;

    #[test]
    fn board_modes_validate_names_and_conflicting_options() {
        for args in [
            vec!["agent-bridge", "board"],
            vec!["agent-bridge", "board", "project", "--raw"],
            vec!["agent-bridge", "board", "project", "--metadata"],
            vec!["agent-bridge", "board", "--list"],
            vec!["agent-bridge", "board", "--create", "project"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        for args in [
            vec!["agent-bridge", "board", "../project"],
            vec!["agent-bridge", "board", "--create", "UPPER"],
            vec!["agent-bridge", "board", "--list", "--raw"],
            vec!["agent-bridge", "board", "--raw", "--metadata"],
            vec!["agent-bridge", "board", "--list", "--metadata"],
            vec!["agent-bridge", "board", "--create", "project", "--metadata"],
            vec!["agent-bridge", "board", "project", "--list"],
            vec!["agent-bridge", "board", "project", "--create", "other"],
            vec!["agent-bridge", "board", "--create", "project", "--raw"],
            vec!["agent-bridge", "board", "--create", "project", "--list"],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn update_modes_are_explicit_and_mutually_exclusive() {
        for option in ["--check", "--disable-checks", "--enable-checks"] {
            assert!(Cli::try_parse_from(["agent-bridge", "update", option]).is_ok());
        }
        for options in [
            ["--check", "--disable-checks"],
            ["--check", "--enable-checks"],
            ["--disable-checks", "--enable-checks"],
        ] {
            assert!(
                Cli::try_parse_from(["agent-bridge", "update", options[0], options[1]]).is_err()
            );
        }
        assert!(Cli::try_parse_from(["agent-bridge", "update", "--yes"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["agent-bridge", "update"])
                .unwrap()
                .command,
            Commands::Update {
                check: false,
                disable_checks: false,
                enable_checks: false,
            }
        ));
    }
}
