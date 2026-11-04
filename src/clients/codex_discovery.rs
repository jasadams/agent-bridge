//! A read-only observer of the *existing* native Codex daemon. Never starts or resumes a thread.
use super::{ConnectorState, NativeState, StateError};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};
use tokio::{
    net::UnixStream,
    time::{sleep, timeout},
};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Socket = WebSocketStream<UnixStream>;
const MAX_LOADED_SESSIONS: usize = 4096;
const MAX_LOADED_PAGES: usize = 64;

/// Replaceable native observations owned by the existing, single connection.
/// No database writes, previous lifecycle inventory, or owner policy live here.
#[derive(Clone, Default)]
pub struct State(Arc<Mutex<Observation>>);

#[derive(Default)]
struct Observation {
    connected: bool,
    complete: bool,
    rows: BTreeMap<String, NativeState>,
}

impl State {
    fn snapshot(&self) -> std::result::Result<MutexGuard<'_, Observation>, StateError> {
        let observation = self
            .0
            .lock()
            .map_err(|_| StateError::Unavailable("Codex observation lock poisoned".into()))?;
        if !observation.connected {
            return Err(StateError::Unavailable(
                "Codex observer disconnected".into(),
            ));
        }
        if !observation.complete {
            return Err(StateError::Incomplete(
                "Codex loaded-session read incomplete".into(),
            ));
        }
        Ok(observation)
    }
}

impl ConnectorState for State {
    fn provider(&self) -> crate::protocol::Provider {
        crate::protocol::Provider::Codex
    }
    fn list(&self, _max_age_days: u32) -> std::result::Result<Vec<NativeState>, StateError> {
        Ok(self.snapshot()?.rows.values().cloned().collect())
    }
    fn get(&self, session: &str) -> std::result::Result<Option<NativeState>, StateError> {
        if session.len() != 36 || uuid::Uuid::parse_str(session).is_err() {
            return Err(StateError::Incomplete(
                "Codex lookup requires a full session UUID".into(),
            ));
        }
        Ok(self.snapshot()?.rows.get(session).cloned())
    }
}

struct Connection<'a>(&'a State);
impl Drop for Connection<'_> {
    fn drop(&mut self) {
        if let Ok(mut observation) = self.0.0.lock() {
            *observation = Observation::default();
        }
    }
}

/// Socket layout is isolated here because native daemon layout can change between Codex versions.
pub fn socket_paths() -> Vec<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let root = PathBuf::from(format!("/tmp/codex-daemon-{uid}"));
    let Ok(dir) = root.canonicalize() else {
        return Vec::new();
    };
    let Ok(meta) = std::fs::metadata(&dir) else {
        return Vec::new();
    };
    if meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            std::fs::symlink_metadata(p)
                .is_ok_and(|m| m.file_type().is_socket() && m.uid() == uid && m.mode() & 0o077 == 0)
        })
        .collect()
}

async fn send(socket: &mut Socket, value: Value) -> Result<()> {
    timeout(
        Duration::from_secs(5),
        socket.send(Message::Text(value.to_string().into())),
    )
    .await??;
    Ok(())
}

async fn receive(socket: &mut Socket) -> Result<Value> {
    loop {
        match socket
            .next()
            .await
            .context("Native Codex socket closed")??
        {
            Message::Text(text) => return Ok(serde_json::from_str(&text)?),
            Message::Close(_) => bail!("Native Codex disconnected"),
            _ => {}
        }
    }
}

enum Pending {
    Loaded,
    Read(String),
}

async fn request_read(
    socket: &mut Socket,
    id: &str,
    next: &mut u64,
    pending: &mut HashMap<u64, Pending>,
    state: &State,
) -> Result<()> {
    ensure!(
        id.len() == 36 && uuid::Uuid::parse_str(id).is_ok(),
        "Invalid native Codex session UUID"
    );
    if pending
        .values()
        .any(|p| matches!(p,Pending::Read(existing) if existing==id))
    {
        return Ok(());
    }
    ensure!(
        pending.len() < MAX_LOADED_SESSIONS,
        "Native discovery has too many pending sessions"
    );
    let request = *next;
    *next += 1;
    pending.insert(request, Pending::Read(id.into()));
    state.0.lock().unwrap().complete = false;
    send(
        socket,
        json!({"id":request,"method":"thread/read","params":{"threadId":id}}),
    )
    .await
}

type Status = std::sync::Arc<std::sync::Mutex<serde_json::Value>>;

fn report(status: &Status, value: serde_json::Value) {
    *status.lock().unwrap() = value;
}

pub async fn observe_connection(
    path: &Path,
    codex_home: &Path,
    state: &State,
    status: &Status,
) -> Result<()> {
    *state.0.lock().unwrap() = Observation::default();
    let _connection = Connection(state); // Also clears observations on cancellation.
    let stream = timeout(Duration::from_secs(3), UnixStream::connect(path)).await??;
    ensure!(
        stream.peer_cred()?.uid() == unsafe { libc::geteuid() },
        "Native daemon is owned by another user"
    );
    let (mut socket, _) = timeout(
        Duration::from_secs(5),
        tokio_tungstenite::client_async("ws://localhost/", stream),
    )
    .await??;
    send(&mut socket,json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"agent_bridge","title":"Agent Bridge","version":crate::VERSION}}})).await?;
    loop {
        let value = timeout(Duration::from_secs(5), receive(&mut socket)).await??;
        if value["id"] != 1 {
            continue;
        }
        ensure!(
            value.get("error").is_none(),
            "Native initialize rejected: {}",
            value["error"]
        );
        let home = value["result"]["codexHome"]
            .as_str()
            .context("Native daemon did not identify its Codex home")?;
        ensure!(
            Path::new(home).canonicalize()? == codex_home.canonicalize()?,
            "Native daemon belongs to another Codex home"
        );
        break;
    }
    send(&mut socket, json!({"method":"initialized"})).await?;
    state.0.lock().unwrap().connected = true;
    let mut loaded = HashSet::new();
    let mut closed_during_scan = HashSet::new();
    let mut cursors = HashSet::new();
    let mut initial_scan = true;
    let mut pending = HashMap::from([(2, Pending::Loaded)]);
    let mut next = 3;
    let mut failed_reads = HashSet::new();
    send(
        &mut socket,
        json!({"id":2,"method":"thread/loaded/list","params":{}}),
    )
    .await?;
    report(
        status,
        json!({"state":"connected","socket":path,"observed_sessions":0}),
    );
    loop {
        let value = receive(&mut socket).await?;
        if value.get("method").is_none()
            && let Some(id) = value["id"].as_u64()
        {
            let Some(request) = pending.remove(&id) else {
                continue;
            };
            if value.get("error").is_some() {
                match request {
                    Pending::Loaded => bail!("Native discovery rejected: {}", value["error"]),
                    Pending::Read(session) => {
                        state.0.lock().unwrap().rows.remove(&session);
                        state.0.lock().unwrap().complete = false;
                        failed_reads.insert(session);
                        continue;
                    }
                }
            }
            match request {
                Pending::Loaded => {
                    let data = value["result"]["data"]
                        .as_array()
                        .context("Native loaded-session page is missing its data array")?;
                    for id in data {
                        let id = id
                            .as_str()
                            .context("Native loaded-session page contains an invalid identity")?;
                        uuid::Uuid::parse_str(id)
                            .context("Native loaded-session ID is not a UUID")?;
                        if !closed_during_scan.contains(id) {
                            loaded.insert(id.to_owned());
                            ensure!(
                                loaded.len() <= MAX_LOADED_SESSIONS,
                                "Native loaded-session scan exceeds 4096 identities"
                            );
                            request_read(&mut socket, id, &mut next, &mut pending, state).await?;
                        }
                    }
                    let cursor = &value["result"]["nextCursor"];
                    ensure!(
                        cursor.is_null() || cursor.is_string(),
                        "Native loaded-session page has an invalid cursor"
                    );
                    if let Some(cursor) = cursor.as_str() {
                        ensure!(
                            cursors.len() < MAX_LOADED_PAGES && cursors.insert(cursor.to_owned()),
                            "Native loaded-session pagination exceeded its bound or repeated a cursor"
                        );
                        let id = next;
                        next += 1;
                        pending.insert(id, Pending::Loaded);
                        send(&mut socket,json!({"id":id,"method":"thread/loaded/list","params":{"cursor":cursor}})).await?;
                    } else {
                        initial_scan = false;
                        closed_during_scan.clear();
                    }
                }
                Pending::Read(session) => {
                    let thread = &value["result"]["thread"];
                    ensure!(
                        thread["id"] == session && thread["status"]["type"].is_string(),
                        "Native thread/read returned a mismatched or incomplete identity"
                    );
                    if initial_scan && thread["status"]["type"] != "notLoaded" {
                        loaded.insert(session.clone());
                        ensure!(
                            loaded.len() <= MAX_LOADED_SESSIONS,
                            "Native loaded-session scan exceeds 4096 identities"
                        );
                    }
                    failed_reads.remove(&session);
                    let mut observation = state.0.lock().unwrap();
                    if thread["status"]["type"] == "notLoaded" {
                        observation.rows.remove(&session);
                    } else {
                        observation.rows.insert(
                            session.clone(),
                            NativeState {
                                session,
                                display_name: thread["name"].as_str().map(str::to_owned),
                                directory: thread["cwd"].as_str().map(str::to_owned),
                                status: thread["status"]["type"].as_str().map(str::to_owned),
                                updated_ms: None,
                                transport_endpoint: None,
                            },
                        );
                        ensure!(
                            observation.rows.len() <= MAX_LOADED_SESSIONS,
                            "Native Codex observation exceeds 4096 sessions"
                        );
                    }
                    report(
                        status,
                        json!({"state":"connected","socket":path,"observed_sessions":observation.rows.len()}),
                    );
                }
            }
            state.0.lock().unwrap().complete =
                !initial_scan && pending.is_empty() && failed_reads.is_empty();
            continue;
        }
        let method = value["method"].as_str().unwrap_or("");
        let params = &value["params"];
        match method {
            "thread/started" => {
                if let Some(id) = params["thread"]["id"].as_str() {
                    if initial_scan {
                        loaded.insert(id.to_owned());
                        closed_during_scan.remove(id);
                        ensure!(
                            loaded.len() <= MAX_LOADED_SESSIONS,
                            "Native loaded-session scan exceeds 4096 identities"
                        );
                    }
                    request_read(&mut socket, id, &mut next, &mut pending, state).await?;
                }
            }
            "thread/name/updated" | "thread/status/changed" | "thread/unarchived" => {
                if let Some(id) = params["threadId"].as_str() {
                    if params["status"]["type"] == "notLoaded" {
                        pending.retain(
                            |_, request| !matches!(request,Pending::Read(session) if session==id),
                        );
                        if initial_scan {
                            loaded.remove(id);
                            closed_during_scan.insert(id.to_owned());
                        }
                        state.0.lock().unwrap().rows.remove(id);
                        failed_reads.remove(id);
                    } else {
                        if initial_scan {
                            loaded.insert(id.to_owned());
                            closed_during_scan.remove(id);
                            ensure!(
                                loaded.len() <= MAX_LOADED_SESSIONS,
                                "Native loaded-session scan exceeds 4096 identities"
                            );
                        }
                        request_read(&mut socket, id, &mut next, &mut pending, state).await?;
                    }
                }
            }
            "thread/closed" | "thread/archived" | "thread/deleted" => {
                if let Some(id) = params["threadId"].as_str() {
                    pending.retain(
                        |_, request| !matches!(request,Pending::Read(session) if session==id),
                    );
                    if initial_scan {
                        loaded.remove(id);
                        closed_during_scan.insert(id.to_owned());
                    }
                    state.0.lock().unwrap().rows.remove(id);
                    failed_reads.remove(id);
                }
            }
            _ => {} // Do not collect transcript contents, tools, model deltas, or approval requests.
        }
        state.0.lock().unwrap().complete =
            !initial_scan && pending.is_empty() && failed_reads.is_empty();
    }
}

pub async fn run(state: State, codex_home: PathBuf, status: Status) {
    let mut delay = 1;
    loop {
        let mut last = "No native Codex daemon socket found".to_string();
        for socket in socket_paths() {
            if let Err(error) = observe_connection(&socket, &codex_home, &state, &status).await {
                last = format!("{error:#}");
            }
        }
        report(
            &status,
            json!({"state":"disconnected","detail":last,"retry_seconds":delay}),
        );
        // Reconnect backoff only. Connected discovery waits on socket notifications, not a timer.
        sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(15);
    }
}
