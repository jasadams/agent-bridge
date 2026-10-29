//! Coordinates durable state with bounded transport tasks. No database lock crosses await.
use super::{
    discovery::{DiscoveredPeer, normalize_lan_address},
    identity::Identity,
    transport::{Incoming, Transport},
    wire::{
        Operation, VERSION, WireRequest, WireResponse, supported_providers, validate_providers,
    },
};
use crate::{broker::Broker, db::Db};
use anyhow::{Result, anyhow, ensure};
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use serde_json::json;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::{Duration, Instant},
};

enum Kind {
    Hello,
    Directory,
    DirectorySupported(Option<String>),
    Sync,
    Send(String),
}
struct PairingCompleted {
    id: String,
    result: Result<serde_json::Value>,
}

enum NetworkIncoming {
    Message(Incoming),
    Pairing(super::transport::IncomingPairing),
    Stopped,
}

struct Completed {
    peer: String,
    kind: Kind,
    result: Result<WireResponse>,
}
struct Schedule {
    hello: Instant,
    next: Instant,
    directory: Instant,
    sync: Instant,
    failures: u32,
    providers: Option<Vec<String>>,
    probe: Instant,
    fallback: bool,
    legacy: bool,
    send_permit: Option<String>,
}
impl Default for Schedule {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            hello: now,
            next: now,
            directory: now,
            sync: now,
            failures: 0,
            providers: None,
            probe: now,
            fallback: false,
            legacy: false,
            send_permit: None,
        }
    }
}

/// Manual routes survive discovery expiry. Untrusted advertisements cannot add trust.
fn peer_routes(
    db: &Db,
    discovered: &[DiscoveredPeer],
) -> Result<BTreeMap<String, Vec<libp2p::Multiaddr>>> {
    let mut routes: BTreeMap<String, Vec<libp2p::Multiaddr>> = BTreeMap::new();
    let relays = db.relay_nodes()?;
    for (peer, address) in db.network_peers()? {
        if address.contains("/p2p-circuit/")
            && super::relay_config::configured_circuit(&address, &peer, &relays).is_err()
        {
            continue;
        }
        routes.entry(peer).or_default().push(address.parse()?);
    }
    let trusted: HashSet<_> = db
        .peers()?
        .into_iter()
        .filter(|peer| !peer.revoked)
        .map(|peer| peer.peer_id)
        .collect();
    for entry in discovered.iter().take(256) {
        if !trusted.contains(&entry.peer_id) {
            continue;
        }
        let Ok(peer) = entry.peer_id.parse() else {
            continue;
        };
        for address in entry.addresses.iter().take(8) {
            let address = address
                .parse()
                .ok()
                .and_then(|address| normalize_lan_address(peer, &address));
            if let Some(address) = address {
                let candidates = routes.entry(entry.peer_id.clone()).or_default();
                if candidates.len() < 8 && !candidates.contains(&address) {
                    candidates.push(address);
                }
            }
        }
    }
    Ok(routes)
}

// A healthy direct connection must remain usable when its rendezvous hint expires
// or infrastructure is temporarily unavailable. These hints are derived only from
// authenticated live transport connections and never persisted for later dialing.
fn live_direct_routes(
    status: &serde_json::Value,
    trusted: &HashSet<String>,
) -> BTreeMap<String, Vec<libp2p::Multiaddr>> {
    let mut routes = BTreeMap::new();
    for path in status["paths"].as_array().into_iter().flatten().take(32) {
        let Some(peer) = path["peer_id"].as_str() else {
            continue;
        };
        if !trusted.contains(peer)
            || !status["connected"]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == peer))
        {
            continue;
        }
        let Ok(id) = peer.parse::<libp2p::PeerId>() else {
            continue;
        };
        for address in path["addresses"]
            .as_array()
            .into_iter()
            .flatten()
            .take(2)
            .filter_map(serde_json::Value::as_str)
        {
            let Ok(mut address) = address.parse::<libp2p::Multiaddr>() else {
                continue;
            };
            if address
                .iter()
                .any(|part| matches!(part, libp2p::multiaddr::Protocol::P2pCircuit))
            {
                continue;
            }
            if matches!(
                address.iter().last(),
                Some(libp2p::multiaddr::Protocol::P2p(_))
            ) && address.pop() != Some(libp2p::multiaddr::Protocol::P2p(id))
            {
                continue;
            }
            if super::store::validate_address(&address.to_string(), false).is_err() {
                continue;
            }
            let candidates: &mut Vec<libp2p::Multiaddr> =
                routes.entry(peer.to_string()).or_default();
            if !candidates.contains(&address) {
                candidates.push(address);
            }
        }
    }
    routes
}

fn respond(broker: &Broker, local_peer: &str, incoming: Incoming) {
    let peer = incoming.peer.to_string();
    let response = (|| -> Result<WireResponse> {
        ensure!(
            incoming.request.version == VERSION,
            "Unsupported protocol version"
        );
        let db = broker.db.lock().unwrap();
        if !db.network_config()?.enabled {
            return Ok(WireResponse::Rejected {
                reason: "Networking is disabled".into(),
                retryable: true,
            });
        }
        ensure!(!db.peer(&peer)?.revoked, "Peer is revoked");
        Ok(match incoming.request.operation {
            Operation::Hello { machine_name } => {
                db.remember_peer_name(&peer, &machine_name)?;
                WireResponse::Hello {
                    machine_name: db.machine_name()?,
                }
            }
            Operation::Directory => WireResponse::Directory {
                agents: db.export_agents(&peer)?,
            },
            Operation::DirectorySupported { providers } => {
                validate_providers(&providers)?;
                WireResponse::SupportedDirectory {
                    providers: supported_providers(),
                    agents: db.export_agents_supported(&peer, &providers)?,
                }
            }
            Operation::Send { envelope } => {
                db.accept_remote(local_peer, &peer, &envelope)?;
                broker.wake.notify_one();
                WireResponse::Accepted { id: envelope.id }
            }
            Operation::Sync { after } => WireResponse::Status {
                events: db.sync_events(&peer, after)?,
            },
        })
    })();
    let response = response.unwrap_or_else(|error| WireResponse::Rejected {
        retryable: super::store::retryable(&error),
        reason: error.to_string(),
    });
    let _ = incoming.response.send(response);
}

fn apply(broker: &Broker, done: Completed) -> Result<()> {
    let response = done.result?;
    let db = broker.db.lock().unwrap();
    ensure!(!db.peer(&done.peer)?.revoked, "Peer is revoked");
    match (done.kind, response) {
        (Kind::Hello, WireResponse::Hello { machine_name }) => {
            db.remember_peer_name(&done.peer, &machine_name)
        }
        (Kind::Directory, WireResponse::Directory { agents }) => {
            ensure!(
                agents
                    .iter()
                    .all(|agent| agent.provider != crate::protocol::Provider::OpenCode),
                "Legacy directory contains an unsupported provider"
            );
            db.replace_remote_agents(&done.peer, &agents)
        }
        (Kind::DirectorySupported(_), WireResponse::SupportedDirectory { providers, agents }) => {
            validate_providers(&providers)?;
            ensure!(
                agents
                    .iter()
                    .all(|agent| providers.iter().any(|p| p == agent.provider.label())),
                "Directory contains an unadvertised provider"
            );
            db.replace_remote_agents(&done.peer, &agents)
        }
        (Kind::Sync, WireResponse::Status { events }) => db.apply_events(&done.peer, &events),
        (Kind::Send(id), response) => {
            db.transfer_result(&done.peer, &id, &response)?;
            if let WireResponse::Rejected {
                retryable: true,
                reason,
            } = response
            {
                return Err(anyhow!(
                    "Remote capacity unavailable: {}",
                    reason.chars().take(200).collect::<String>()
                ));
            }
            Ok(())
        }
        (_, WireResponse::Rejected { reason, .. }) => Err(anyhow!(
            "Remote rejected request: {}",
            reason.chars().take(200).collect::<String>()
        )),
        _ => Err(anyhow!("Unexpected remote response")),
    }
}

fn complete_request(broker: &Broker, done: Completed, schedule: &mut Schedule) -> Result<()> {
    let legacy_fallback = matches!(done.kind, Kind::Directory) && schedule.fallback;
    apply(broker, done)?;
    if legacy_fallback {
        schedule.legacy = true;
        schedule.fallback = false;
        schedule.probe = Instant::now() + Duration::from_secs(60);
        schedule.directory = Instant::now() + Duration::from_secs(5);
    }
    Ok(())
}

fn next_transfer(
    db: &Db,
    peer: &str,
    schedule: &mut Schedule,
) -> Result<Option<(Kind, Operation)>> {
    for envelope in db.pending_transfers(peer, 100)? {
        let needs_extension = envelope.source.provider == crate::protocol::Provider::OpenCode
            || envelope.destination.provider == crate::protocol::Provider::OpenCode;
        if needs_extension {
            if schedule.legacy
                || schedule
                    .providers
                    .as_ref()
                    .is_some_and(|providers| !providers.iter().any(|p| p == "opencode"))
            {
                db.transfer_result(
                    peer,
                    &envelope.id,
                    &WireResponse::Rejected {
                        reason: "Peer does not support OpenCode sessions".into(),
                        retryable: false,
                    },
                )?;
                continue;
            }
            if schedule.send_permit.as_deref() != Some(&envelope.id) {
                return Ok(Some((
                    Kind::DirectorySupported(Some(envelope.id)),
                    Operation::DirectorySupported {
                        providers: supported_providers(),
                    },
                )));
            }
            schedule.send_permit = None;
        }
        db.mark_transfer_attempt(&envelope.id)?;
        return Ok(Some((
            Kind::Send(envelope.id.clone()),
            Operation::Send {
                envelope: Box::new(envelope),
            },
        )));
    }
    Ok(None)
}

pub async fn run(broker: Broker) {
    let mut active: Option<Transport> = None;
    let mut pairing_enabled = false;
    let mut pairing_tasks: FuturesUnordered<BoxFuture<'static, PairingCompleted>> =
        FuturesUnordered::new();
    let mut pairing_busy = HashSet::new();
    let mut pairing_next: HashMap<String, Instant> = HashMap::new();
    let mut configured = None;
    let mut configured_name = None;
    let mut configured_relays = Vec::new();
    let mut local_peer = String::new();
    let mut allowed_cache = Vec::new();
    let mut tasks: FuturesUnordered<BoxFuture<'static, Completed>> = FuturesUnordered::new();
    let mut busy = HashSet::new();
    let mut schedules: HashMap<String, Schedule> = HashMap::new();
    let mut last_routes = BTreeMap::new();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut start_after = Instant::now();
    let mut last_error: Option<String> = None;
    loop {
        let notified = broker.network_wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let config_result = broker.db.lock().unwrap().network_config();
        let config = match config_result {
            Ok(config) => config,
            Err(error) => {
                *broker.network_status.lock().unwrap() =
                    json!({"enabled":false,"last_error":error.to_string()});
                tick.tick().await;
                continue;
            }
        };
        let machine_name = broker
            .db
            .lock()
            .unwrap()
            .machine_name()
            .unwrap_or_else(|_| "agent-bridge".into());
        let relays_result = broker.db.lock().unwrap().relay_nodes();
        let relays = match relays_result {
            Ok(relays) => relays,
            Err(error) => {
                *broker.network_status.lock().unwrap() =
                    json!({"enabled":false,"last_error":error.to_string()});
                tick.tick().await;
                continue;
            }
        };
        if configured.as_ref() != Some(&config)
            || configured_name.as_ref() != Some(&machine_name)
            || configured_relays != relays
        {
            active = None;
            *broker.network_handle.lock().unwrap() = None;
            broker
                .network_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            broker.contact_routes.lock().unwrap().clear();
            pairing_enabled = false;
            pairing_tasks.clear();
            pairing_busy.clear();
            pairing_next.clear();
            if configured.is_some() {
                broker
                    .pairing_generation
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                broker.pairing_notifications.lock().unwrap().reset();
                if let Some(manager) = broker.lan_pairing.lock().unwrap().as_mut() {
                    manager.reset();
                }
                broker.pairing_routes.lock().unwrap().clear();
            }
            tasks.clear();
            busy.clear();
            schedules.clear();
            last_routes.clear();
            allowed_cache.clear();
            configured = Some(config.clone());
            configured_name = Some(machine_name.clone());
            configured_relays = relays.clone();
            start_after = Instant::now();
            last_error = None;
            *broker.network_status.lock().unwrap() = json!({"enabled":false});
        }
        let allowed: Vec<_> = broker
            .db
            .lock()
            .unwrap()
            .peers()
            .unwrap_or_default()
            .into_iter()
            .filter(|peer| !peer.revoked)
            .filter_map(|peer| peer.peer_id.parse::<libp2p::PeerId>().ok())
            .collect();
        if config.enabled && active.is_none() && Instant::now() >= start_after {
            let started = (|| -> Result<Transport> {
                let identity = Identity::load_or_create(&broker.state_dir)?;
                local_peer = identity.info().peer_id;
                Transport::start_relayed(
                    identity.keypair(),
                    config.listen.parse()?,
                    allowed.clone(),
                    config.lan,
                    &machine_name,
                    relays
                        .iter()
                        .map(|relay| relay.address.parse())
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                )
            })();
            match started {
                Ok(transport) => {
                    *broker.network_handle.lock().unwrap() = Some(crate::broker::NetworkSnapshot {
                        generation: broker
                            .network_generation
                            .load(std::sync::atomic::Ordering::SeqCst),
                        config: config.clone(),
                        relays: relays.clone(),
                        handle: transport.handle.clone(),
                    });
                    active = Some(transport);
                    allowed_cache = allowed.clone();
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                    start_after = Instant::now() + Duration::from_secs(2);
                }
            }
        }
        // Same lock order as local administration: DB, then verification state.
        // Cancellation/revocation cannot race an already-drained trust action.
        {
            let mut db = broker.db.lock().unwrap();
            let mut guard = broker.lan_pairing.lock().unwrap();
            if let Some(manager) = guard.as_mut() {
                if config.enabled {
                    for trusted in manager.take_trusted(crate::db::now()) {
                        let route = broker.contact_routes.lock().unwrap().remove(&trusted.peer);
                        let paired = match route {
                            Some((id, address)) if id == trusted.id => {
                                db.pair_peer_contact(&trusted.peer, &address)
                            }
                            _ => db.pair_peer(&trusted.peer, &trusted.peer, None),
                        };
                        match paired {
                            Ok(_) => {
                                let _ = manager.mark_paired(&trusted.id);
                            }
                            Err(error) => {
                                let _ = manager.cancel(&trusted.id, crate::db::now());
                                last_error = Some(error.to_string());
                            }
                        }
                    }
                } else {
                    manager.close();
                }
            }
        }
        if let Some(transport) = active.as_mut() {
            if allowed_cache != allowed {
                match transport.handle.set_allowed(allowed.clone()).await {
                    Ok(()) => allowed_cache = allowed,
                    Err(error) => last_error = Some(error.to_string()),
                }
            }
            let open = broker
                .lan_pairing
                .lock()
                .unwrap()
                .as_mut()
                .is_some_and(|manager| manager.window_open(crate::db::now()));
            if open != pairing_enabled {
                match transport.handle.set_pairing_enabled(open).await {
                    Ok(()) => pairing_enabled = open,
                    Err(error) => last_error = Some(error.to_string()),
                }
                if !open {
                    pairing_tasks.clear();
                    pairing_busy.clear();
                    pairing_next.clear();
                }
            }
            let mut status = transport.status.borrow().clone();
            let discovered: Vec<DiscoveredPeer> = if config.lan {
                serde_json::from_value(status["discovered"].clone()).unwrap_or_default()
            } else {
                Vec::new()
            };
            if let Some(error) = &last_error {
                status["last_error"] = json!(error);
            }
            let public_routes =
                super::relay_config::rendezvous_routes(&status["rendezvous"], &relays, &local_peer);
            *broker.network_status.lock().unwrap() = status.clone();
            let outgoing = broker
                .lan_pairing
                .lock()
                .unwrap()
                .as_mut()
                .map(|manager| manager.outgoing(crate::db::now()))
                .unwrap_or_default();
            let active_ids: HashSet<_> = outgoing.iter().map(|out| out.id.clone()).collect();
            pairing_next.retain(|id, _| active_ids.contains(id));
            for out in outgoing {
                if pairing_tasks.len() >= 8
                    || pairing_busy.contains(&out.id)
                    || pairing_next
                        .get(&out.id)
                        .is_some_and(|next| *next > Instant::now())
                {
                    continue;
                }
                let Ok(peer_id) = out.peer.parse::<libp2p::PeerId>() else {
                    continue;
                };
                let mut addresses = broker
                    .pairing_routes
                    .lock()
                    .unwrap()
                    .get(&out.peer)
                    .cloned()
                    .unwrap_or_default();
                if addresses.is_empty() {
                    addresses = discovered
                        .iter()
                        .find(|entry| entry.peer_id == out.peer)
                        .map(|entry| {
                            entry
                                .addresses
                                .iter()
                                .take(8)
                                .filter_map(|a| a.parse().ok())
                                .collect()
                        })
                        .unwrap_or_default();
                }
                if let Some(routes) = public_routes.get(&out.peer) {
                    for route in routes {
                        if addresses.len() < 8 && !addresses.contains(route) {
                            addresses.push(route.clone());
                        }
                    }
                }
                let handle = transport.handle.clone();
                pairing_busy.insert(out.id.clone());
                pairing_next.insert(out.id.clone(), Instant::now() + Duration::from_secs(1));
                pairing_tasks.push(Box::pin(async move {
                    let result = handle
                        .pairing_request(peer_id, addresses, out.packet.clone())
                        .await;
                    PairingCompleted { id: out.id, result }
                }));
            }
            let mut peers =
                peer_routes(&broker.db.lock().unwrap(), &discovered).unwrap_or_default();
            // Infrastructure hints never create endpoint trust or session permissions.
            let trusted: HashSet<_> = allowed_cache.iter().map(ToString::to_string).collect();
            for (peer, routes) in &public_routes {
                if !trusted.contains(peer) {
                    continue;
                }
                let candidates = peers.entry(peer.clone()).or_default();
                for route in routes {
                    if candidates.len() < 8 && !candidates.contains(route) {
                        candidates.push(route.clone());
                    }
                }
            }
            for (peer, routes) in live_direct_routes(&status, &trusted) {
                let candidates = peers.entry(peer).or_default();
                for route in routes {
                    if candidates.len() < 8 && !candidates.contains(&route) {
                        candidates.insert(0, route);
                    }
                }
            }
            for (peer, addresses) in &peers {
                if last_routes.get(peer) != Some(addresses) {
                    // A changed LAN port must not inherit a stale connection's backoff.
                    let schedule = schedules.entry(peer.clone()).or_default();
                    schedule.next = Instant::now();
                    schedule.directory = Instant::now();
                }
            }
            last_routes = peers.clone();
            for (peer, addresses) in peers {
                if tasks.len() >= 16 {
                    break;
                }
                if busy.contains(&peer) {
                    continue;
                }
                let schedule = schedules.entry(peer.clone()).or_default();
                let now = Instant::now();
                if now < schedule.next {
                    continue;
                }
                let work = (|| -> Result<Option<(Kind, Operation)>> {
                    let db = broker.db.lock().unwrap();
                    if !db.network_config()?.enabled {
                        return Ok(None);
                    }
                    if now >= schedule.hello {
                        schedule.hello = now + Duration::from_secs(300);
                        return Ok(Some((
                            Kind::Hello,
                            Operation::Hello {
                                machine_name: db.machine_name()?,
                            },
                        )));
                    }
                    if schedule.fallback {
                        return Ok(Some((Kind::Directory, Operation::Directory)));
                    }
                    if now >= schedule.probe {
                        schedule.probe = now + Duration::from_secs(60);
                        return Ok(Some((
                            Kind::DirectorySupported(None),
                            Operation::DirectorySupported {
                                providers: supported_providers(),
                            },
                        )));
                    }
                    if now >= schedule.directory {
                        schedule.directory = now + Duration::from_secs(5);
                        return Ok(Some(if schedule.providers.is_some() {
                            (
                                Kind::DirectorySupported(None),
                                Operation::DirectorySupported {
                                    providers: supported_providers(),
                                },
                            )
                        } else {
                            (Kind::Directory, Operation::Directory)
                        }));
                    }
                    if now >= schedule.sync {
                        schedule.sync = now + Duration::from_secs(1);
                        return Ok(Some((
                            Kind::Sync,
                            Operation::Sync {
                                after: db.sync_cursor(&peer)?,
                            },
                        )));
                    }
                    next_transfer(&db, &peer, schedule)
                })();
                let (kind, operation) = match work {
                    Ok(Some(work)) => work,
                    Ok(None) => continue,
                    Err(error) => {
                        last_error = Some(error.to_string());
                        schedule.next = now + Duration::from_secs(2);
                        continue;
                    }
                };
                let Ok(peer_id) = peer.parse() else {
                    continue;
                };
                let handle = transport.handle.clone();
                busy.insert(peer.clone());
                tasks.push(Box::pin(async move {
                    let result = handle
                        .request_addresses(peer_id, addresses, WireRequest::new(operation))
                        .await;
                    Completed { peer, kind, result }
                }));
            }
        } else {
            *broker.network_status.lock().unwrap() =
                json!({"enabled":false,"last_error":last_error});
        }
        tokio::select! {
            _ = &mut notified => {},
            _ = tick.tick(), if config.enabled => {},
            incoming = async {
                match active.as_mut() {
                    Some(transport) => tokio::select! {
                        incoming = transport.incoming.recv() => incoming.map(NetworkIncoming::Message).unwrap_or(NetworkIncoming::Stopped),
                        incoming = transport.incoming_pairing.recv() => incoming.map(NetworkIncoming::Pairing).unwrap_or(NetworkIncoming::Stopped),
                    },
                    None => std::future::pending().await,
                }
            } => match incoming {
                NetworkIncoming::Message(incoming) => respond(&broker, &local_peer, incoming),
                NetworkIncoming::Pairing(incoming) => {
                    let result = (|| -> Result<serde_json::Value> {
                        let db = broker.db.lock().unwrap();
                        ensure!(db.network_config()?.enabled, "Networking is disabled");
                        let mut guard = broker.lan_pairing.lock().unwrap();
                        guard.as_mut().ok_or_else(|| anyhow!("Pairing window is closed"))?
                            .receive(&incoming.peer.to_string(), incoming.request, crate::db::now())
                    })();
                    let response = result.unwrap_or_else(|_| json!({"ok":false,"error":"pairing request rejected"}));
                    let _ = incoming.response.send(response);
                }
                NetworkIncoming::Stopped => { schedules.clear(); active = None; *broker.network_handle.lock().unwrap() = None; broker.network_generation.fetch_add(1,std::sync::atomic::Ordering::SeqCst); start_after = Instant::now() + Duration::from_secs(2); }
            },
            Some(done) = pairing_tasks.next(), if !pairing_tasks.is_empty() => {
                pairing_busy.remove(&done.id);
                // Protocol progress is driven by authenticated packets, never by
                // transport ACKs. Pace even successful retries to bound remote work.
                if done.result.is_err() {
                    pairing_next.insert(done.id, Instant::now() + Duration::from_secs(2));
                }
            },
            Some(done) = tasks.next(), if !tasks.is_empty() => {
                busy.remove(&done.peer);
                let schedule = schedules.entry(done.peer.clone()).or_default();
                if let Kind::DirectorySupported(ref send_id) = done.kind {
                    let send_id = send_id.clone();
                    let providers = match &done.result {
                        Ok(WireResponse::SupportedDirectory { providers, .. }) => Some(providers.clone()),
                        _ => None,
                    };
                    match apply(&broker, done) {
                        Ok(()) => {
                            schedule.providers = providers;
                            schedule.legacy = false;
                            schedule.fallback = false;
                            schedule.send_permit = send_id;
                            schedule.directory = Instant::now() + Duration::from_secs(5);
                        }
                        Err(error) => {
                            // An unknown extension or codec failure must not back off
                            // supported traffic. Only a successful legacy request proves
                            // this peer is reachable without the extension.
                            schedule.providers = None;
                            schedule.send_permit = None;
                            schedule.fallback = true;
                            last_error = Some(error.to_string());
                        }
                    }
                    schedule.next = Instant::now();
                    continue;
                }
                if matches!(done.kind, Kind::Hello) {
                    // Metadata is optional: old peers can reject this operation.
                    // Keep the unchanged v1 message protocol usable in either case.
                    schedule.hello = Instant::now() + Duration::from_secs(if apply(&broker, done).is_ok() { 5 } else { 300 });
                    schedule.next = Instant::now();
                    continue;
                }
                match complete_request(&broker, done, schedule) {
                    Ok(()) => {
                        schedule.failures = 0; schedule.next = Instant::now();
                    }
                    Err(error) => {
                        schedule.failures = (schedule.failures + 1).min(6);
                        let jitter = u16::from_be_bytes(uuid::Uuid::new_v4().as_bytes()[..2].try_into().unwrap()) as u64 % 250;
                        schedule.next = Instant::now() + Duration::from_millis((500 * (1u64 << schedule.failures)).min(30_000) + jitter);
                        last_error = Some(error.to_string());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::network::{
        trust::Permission,
        wire::{Envelope, RemoteAgent},
    };
    use crate::protocol::Provider;

    #[tokio::test]
    async fn only_successful_legacy_fallback_establishes_unsupported_capability() {
        let dir = tempfile::tempdir().unwrap();
        let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
        let peer = libp2p::PeerId::random().to_string();
        bound
            .broker
            .db
            .lock()
            .unwrap()
            .pair_peer(&peer, &peer, None)
            .unwrap();
        let mut schedule = Schedule {
            fallback: true,
            ..Schedule::default()
        };
        for result in [
            Err(anyhow!("transport timeout")),
            Ok(WireResponse::Rejected {
                reason: "offline".into(),
                retryable: true,
            }),
            Ok(WireResponse::SupportedDirectory {
                providers: supported_providers(),
                agents: vec![],
            }),
        ] {
            assert!(
                complete_request(
                    &bound.broker,
                    Completed {
                        peer: peer.clone(),
                        kind: Kind::Directory,
                        result
                    },
                    &mut schedule
                )
                .is_err()
            );
            assert!(!schedule.legacy);
            assert!(schedule.fallback);
        }
        complete_request(
            &bound.broker,
            Completed {
                peer,
                kind: Kind::Directory,
                result: Ok(WireResponse::Directory { agents: vec![] }),
            },
            &mut schedule,
        )
        .unwrap();
        assert!(schedule.legacy);
        assert!(!schedule.fallback);
        assert!(schedule.probe > Instant::now());
    }

    #[test]
    fn opencode_dispatch_requires_a_new_exact_message_permit_and_legacy_does_not_starve() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("messages.sqlite3")).unwrap();
        let local = libp2p::PeerId::random().to_string();
        let peer = libp2p::PeerId::random().to_string();
        db.pair_peer(&peer, &peer, None).unwrap();
        db.grant_peer(&peer, "user", Permission::Send, true)
            .unwrap();
        let native = RemoteAgent {
            name: "native".into(),
            provider: Provider::OpenCode,
            session: "ses_0123456789abcdefghijklmnop".into(),
        };
        let inbox = RemoteAgent {
            name: "user".into(),
            provider: Provider::Inbox,
            session: "user".into(),
        };
        db.replace_remote_agents(&peer, &[native, inbox]).unwrap();
        let native_id = uuid::Uuid::new_v4().to_string();
        db.send_remote(
            &local,
            &native_id,
            "user",
            &format!("{peer}/native"),
            "native",
            &None,
        )
        .unwrap();
        let inbox_id = uuid::Uuid::new_v4().to_string();
        db.send_remote(
            &local,
            &inbox_id,
            "user",
            &format!("{peer}/user"),
            "inbox",
            &None,
        )
        .unwrap();
        let mut schedule = Schedule {
            providers: Some(supported_providers()),
            ..Schedule::default()
        };
        assert!(
            matches!(next_transfer(&db, &peer, &mut schedule).unwrap(), Some((Kind::DirectorySupported(Some(id)), _)) if id == native_id)
        );
        assert_eq!(
            db.transfer_info(&native_id).unwrap().unwrap()["state"],
            "pending"
        );
        schedule.send_permit = Some(inbox_id.clone());
        assert!(
            matches!(next_transfer(&db, &peer, &mut schedule).unwrap(), Some((Kind::DirectorySupported(Some(id)), _)) if id == native_id)
        );
        schedule.send_permit = Some(native_id.clone());
        assert!(
            matches!(next_transfer(&db, &peer, &mut schedule).unwrap(), Some((Kind::Send(id), _)) if id == native_id)
        );
        assert!(schedule.send_permit.is_none());
        assert!(
            matches!(next_transfer(&db, &peer, &mut schedule).unwrap(), Some((Kind::DirectorySupported(Some(id)), _)) if id == native_id)
        );
        // Restart loses all proof. A confirmed legacy peer explicitly rejects
        // native work, then dispatches supported work in the same scheduling pass.
        let mut restarted = Schedule::default();
        assert!(
            matches!(next_transfer(&db, &peer, &mut restarted).unwrap(), Some((Kind::DirectorySupported(Some(id)), _)) if id == native_id)
        );
        restarted.legacy = true;
        assert!(
            matches!(next_transfer(&db, &peer, &mut restarted).unwrap(), Some((Kind::Send(id), _)) if id == inbox_id)
        );
        let info = db.transfer_info(&native_id).unwrap().unwrap();
        assert_eq!(info["state"], "rejected");
        assert!(
            info["detail"]
                .as_str()
                .unwrap()
                .contains("does not support OpenCode")
        );
    }

    #[test]
    fn authenticated_live_direct_routes_outlast_discovery_but_never_trust_stale_or_mismatched_paths()
     {
        let peer = libp2p::PeerId::random().to_string();
        let wrong = libp2p::PeerId::random().to_string();
        let address = "/ip4/198.18.0.3/udp/5000/quic-v1";
        let mut status = json!({"connected":[peer],"paths":[{"peer_id":peer,"addresses":[format!("{address}/p2p/{peer}"),format!("{address}/p2p/{wrong}")]}],"rendezvous":[]});
        let trusted = HashSet::from([peer.clone()]);
        assert_eq!(
            live_direct_routes(&status, &trusted)[&peer],
            vec![address.parse::<libp2p::Multiaddr>().unwrap()]
        );
        assert!(live_direct_routes(&status, &HashSet::new()).is_empty());
        status["connected"] = json!([]);
        assert!(live_direct_routes(&status, &trusted).is_empty());
        status["connected"] = json!([peer]);
        status["paths"][0]["addresses"] = json!([format!("{address}/p2p/{wrong}")]);
        assert!(live_direct_routes(&status, &trusted).is_empty());
    }

    #[test]
    fn discovery_routes_require_trust_preserve_manual_hints_and_expire() {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Db::open(&dir.path().join("messages.sqlite3")).unwrap();
        let trusted = libp2p::PeerId::random().to_string();
        let untrusted = libp2p::PeerId::random().to_string();
        db.pair_peer(&trusted, &trusted, Some("workstation"))
            .unwrap();
        db.set_peer_address(&trusted, "/ip4/127.0.0.1/udp/44001/quic-v1")
            .unwrap();
        let discoveries = vec![
            DiscoveredPeer {
                peer_id: trusted.clone(),
                machine_name: None,
                addresses: vec![
                    "/ip4/192.168.1.21/udp/44002/quic-v1".into(),
                    "/ip4/8.8.8.8/udp/44002/quic-v1".into(),
                ],
            },
            DiscoveredPeer {
                peer_id: untrusted,
                machine_name: None,
                addresses: vec!["/ip4/192.168.1.22/udp/44002/quic-v1".into()],
            },
        ];
        let routes = peer_routes(&db, &discoveries).unwrap();
        assert_eq!(routes.len(), 1);
        let addresses = &routes[&trusted];
        assert_eq!(addresses.len(), 2);
        assert_eq!(addresses[0].to_string(), "/ip4/127.0.0.1/udp/44001/quic-v1");
        assert_eq!(
            addresses[1].to_string(),
            "/ip4/192.168.1.21/udp/44002/quic-v1"
        );
        assert_eq!(peer_routes(&db, &[]).unwrap()[&trusted].len(), 1);
        assert!(
            !db.peer_allowed(&trusted, &Provider::Inbox, "user", Permission::Receive)
                .unwrap()
        );
        db.revoke_peer(&trusted).unwrap();
        assert!(peer_routes(&db, &discoveries).unwrap().is_empty());
    }

    #[test]
    fn lan_configuration_is_persistent_and_explicit_mode_can_disable_discovery() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("messages.sqlite3");
        let db = Db::open(&path).unwrap();
        assert!(!db.network_config().unwrap().lan);
        db.set_network_mode(true, Some("/ip4/0.0.0.0/udp/0/quic-v1"), Some(true))
            .unwrap();
        db.set_network_config(false, None).unwrap();
        drop(db);
        let db = Db::open(&path).unwrap();
        let config = db.network_config().unwrap();
        assert!(config.lan);
        assert!(!config.enabled);
        db.set_network_mode(true, Some("/ip4/127.0.0.1/udp/0/quic-v1"), Some(false))
            .unwrap();
        assert!(!db.network_config().unwrap().lan);
    }

    #[tokio::test]
    async fn queued_ingress_rechecks_disable_before_accepting() {
        let dir = tempfile::Builder::new()
            .prefix("ab-disable-")
            .tempdir_in("/tmp")
            .unwrap();
        let bound = Broker::bind(dir.path().join("state"), "/missing/codex".into()).unwrap();
        let broker = &bound.broker;
        let local = Identity::load_or_create(&broker.state_dir)
            .unwrap()
            .info()
            .peer_id;
        let peer = libp2p::identity::Keypair::generate_ed25519()
            .public()
            .to_peer_id();
        {
            let mut db = broker.db.lock().unwrap();
            let id = peer.to_string();
            db.pair_peer(&id, &id, None).unwrap();
            db.grant_peer(&id, "user", Permission::Receive, true)
                .unwrap();
            db.set_network_config(true, None).unwrap();
        }
        let agent = RemoteAgent {
            name: "user".into(),
            provider: Provider::Inbox,
            session: "user".into(),
        };
        let request = WireRequest::new(Operation::Send {
            envelope: Box::new(Envelope {
                origin_peer: peer.to_string(),
                id: uuid::Uuid::new_v4().to_string(),
                source: agent.clone(),
                destination_peer: local.clone(),
                destination: agent,
                body: "already queued by transport".into(),
                parent: None,
                hops: 0,
            }),
        });
        broker
            .db
            .lock()
            .unwrap()
            .set_network_config(false, None)
            .unwrap();
        let (response, received) = tokio::sync::oneshot::channel();
        respond(
            broker,
            &local,
            Incoming {
                peer,
                request,
                response,
            },
        );
        assert!(matches!(
            received.await.unwrap(),
            WireResponse::Rejected {
                retryable: true,
                ..
            }
        ));
        assert!(
            broker
                .db
                .lock()
                .unwrap()
                .history(&None, 100)
                .unwrap()
                .is_empty()
        );
    }
}
