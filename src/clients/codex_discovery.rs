//! A read-only observer of the *existing* native Codex daemon. Never starts or resumes a thread.
use crate::broker::Broker;
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
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
) -> Result<()> {
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
    send(
        socket,
        json!({"id":request,"method":"thread/read","params":{"threadId":id}}),
    )
    .await
}

pub async fn observe_connection(broker: &Broker, path: &Path, codex_home: &Path) -> Result<()> {
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
    let mut loaded = HashSet::new();
    let mut closed_during_scan = HashSet::new();
    let mut cursors = HashSet::new();
    let mut initial_scan = true;
    let mut pending = HashMap::from([(2, Pending::Loaded)]);
    let mut next = 3;
    let mut observed = HashSet::new();
    send(
        &mut socket,
        json!({"id":2,"method":"thread/loaded/list","params":{}}),
    )
    .await?;
    broker.discovery_status(json!({"state":"connected","socket":path,"observed_sessions":0}));
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
                        broker.codex_offline(Some(&session))?;
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
                            request_read(&mut socket, id, &mut next, &mut pending).await?;
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
                        broker.reconcile_codex_loaded(&loaded)?;
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
                    broker.observe_codex(thread)?;
                    observed.insert(session);
                    broker.discovery_status(json!({"state":"connected","socket":path,"observed_sessions":observed.len()}));
                }
            }
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
                    request_read(&mut socket, id, &mut next, &mut pending).await?;
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
                        broker.codex_offline(Some(id))?;
                    } else {
                        if initial_scan {
                            loaded.insert(id.to_owned());
                            closed_during_scan.remove(id);
                            ensure!(
                                loaded.len() <= MAX_LOADED_SESSIONS,
                                "Native loaded-session scan exceeds 4096 identities"
                            );
                        }
                        request_read(&mut socket, id, &mut next, &mut pending).await?;
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
                    broker.codex_offline(Some(id))?;
                    observed.remove(id);
                }
            }
            _ => {} // Do not collect transcript contents, tools, model deltas, or approval requests.
        }
    }
}

pub async fn run(broker: Broker, codex_home: PathBuf) {
    let mut delay = 1;
    loop {
        let mut last = "No native Codex daemon socket found".to_string();
        for socket in socket_paths() {
            if let Err(error) = observe_connection(&broker, &socket, &codex_home).await {
                last = format!("{error:#}");
            }
        }
        if let Err(e) = broker.codex_offline(None) {
            eprintln!("Codex offline state: {e:#}");
        }
        broker
            .discovery_status(json!({"state":"disconnected","detail":last,"retry_seconds":delay}));
        // Reconnect backoff only. Connected discovery waits on socket notifications, not a timer.
        sleep(Duration::from_secs(delay)).await;
        delay = (delay * 2).min(15);
    }
}
