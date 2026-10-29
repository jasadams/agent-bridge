//! OpenCode sharing exercises real QUIC brokers and the private native socket boundary.
use agent_bridge::{
    broker::{self, Broker},
    protocol::{Agent, Provider, Request},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{task::JoinHandle, time::timeout};

const A: &str = "ses_0123456789abcdefghijklmnop";
const B: &str = "ses_0123456789abcdefghijklmnox";

// Accepted sockets can inherit O_NONBLOCK on macOS. Only accept is polled;
// each connected socket uses bounded blocking I/O for complete native frames.
fn read_native_frame(stream: &UnixStream) -> Value {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream.try_clone().unwrap())
        .read_line(&mut line)
        .unwrap();
    serde_json::from_str(&line).unwrap()
}

#[test]
fn native_fixture_reads_delayed_frames_from_nonblocking_connections() {
    let (reader, mut writer) = UnixStream::pair().unwrap();
    // Reproduce inherited flags on every platform, including Linux where
    // accept normally returns a blocking socket from a nonblocking listener.
    reader.set_nonblocking(true).unwrap();
    let worker = std::thread::spawn(move || read_native_frame(&reader));
    std::thread::sleep(Duration::from_millis(50));
    write!(writer, "{{\"op\":\"probe\",").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    writeln!(writer, "\"session\":\"{A}\"}}").unwrap();
    assert_eq!(worker.join().unwrap(), json!({"op":"probe", "session":A}));
}

struct Native {
    _root: tempfile::TempDir,
    path: PathBuf,
    session: &'static str,
    frames: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Native {
    fn start(session: &'static str, lose_receipt: bool) -> Self {
        let root = temp();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        let path = root.path().join(format!("{instance}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let frames = Arc::new(Mutex::new(Vec::new()));
        let received = frames.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("native accept: {error}"),
                };
                let frame = read_native_frame(&stream);
                assert_eq!(frame["session"], session);
                let mut receipt =
                    json!({"ok":true,"protocol":2,"session":session,"instance":instance});
                if frame["op"] == "deliver" {
                    assert!(frame.get("permissions").is_none());
                    assert!(frame.get("tools").is_none());
                    let id = frame["id"].as_str().unwrap();
                    uuid::Uuid::parse_str(id).unwrap();
                    receipt["id"] = json!(id);
                    for key in ["native_message_id", "namespace", "generation", "attempt"] {
                        receipt[key] = frame[key].clone();
                    }
                    receipt["status"] = json!("inserted");
                    received.lock().unwrap().push(frame);
                    if lose_receipt {
                        continue;
                    }
                } else {
                    assert_eq!(frame["op"], "probe");
                }
                writeln!(stream, "{receipt}").unwrap();
            }
        });
        Self {
            _root: root,
            path,
            session,
            frames,
            stop,
            worker: Some(worker),
        }
    }
    fn name(&self) -> String {
        format!("opencode-{}", self.session)
    }
    fn observe(&self, bridge: &Running, title: &str) {
        bridge.request(Request::OpenCodeObserve {
            session: self.session.into(),
            endpoint: self.path.to_str().unwrap().into(),
            title: Some(title.into()),
            ready: true,
        });
    }
    fn frames(&self) -> Vec<Value> {
        self.frames.lock().unwrap().clone()
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.worker.take().unwrap().join().unwrap();
    }
}
struct Running {
    broker: Broker,
    task: JoinHandle<anyhow::Result<()>>,
}
impl Running {
    fn start(path: &Path) -> Self {
        let bound = Broker::bind(path.into(), "/missing/codex".into()).unwrap();
        Self {
            broker: bound.broker.clone(),
            task: tokio::spawn(broker::serve(bound)),
        }
    }
    fn request(&self, request: Request) -> Value {
        if matches!(&request, Request::OpenCodeObserve { .. })
            || matches!(&request, Request::Register { agent, .. } if agent.provider == Provider::OpenCode)
        {
            // The synchronous registration API performs its live async probe on
            // a private runtime, just as the broker RPC blocking worker does.
            std::thread::scope(|scope| scope.spawn(|| self.broker.request(request)).join().unwrap())
                .unwrap()
        } else {
            self.broker.request(request).unwrap()
        }
    }
    fn identity(&self) -> String {
        self.request(Request::Identity)["peer_id"]
            .as_str()
            .unwrap()
            .into()
    }
    fn share(&self, peer: &str, name: &str, shared: bool) {
        self.request(Request::PeerShare {
            peer: peer.into(),
            agent: name.into(),
            shared,
        });
    }
    fn send(&self, id: &str, sender: &str, recipient: &str, body: &str, parent: Option<String>) {
        self.request(Request::Send {
            id: id.into(),
            sender: sender.into(),
            recipient: recipient.into(),
            body: body.into(),
            parent,
        });
    }
    fn history(&self) -> Vec<Value> {
        self.request(Request::History {
            agent: None,
            limit: 100,
        })
        .as_array()
        .unwrap()
        .clone()
    }
    async fn listening(&self) -> String {
        wait("QUIC listener", || {
            self.request(Request::Status)["network"]["listening"]
                .as_array()?
                .first()
                .cloned()
        })
        .await
        .as_str()
        .unwrap()
        .into()
    }
    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}
fn temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ab-ocnet-")
        .tempdir_in("/tmp")
        .unwrap()
}
async fn wait(label: &str, mut inspect: impl FnMut() -> Option<Value>) -> Value {
    timeout(Duration::from_secs(40), async {
        loop {
            if let Some(value) = inspect() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
}
fn pair(a: &Running, b: &Running) {
    for (bridge, peer, alias) in [(a, b, "bob"), (b, a, "alice")] {
        let id = peer.identity();
        bridge.request(Request::PeerPair {
            peer_id: id.clone(),
            fingerprint: id,
            alias: Some(alias.into()),
        });
    }
}
async fn connect(a: &Running, b: &Running) {
    for bridge in [a, b] {
        bridge.request(Request::NetworkConfigure {
            enabled: true,
            listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
        });
    }
    a.request(Request::PeerAddress {
        peer: "bob".into(),
        address: b.listening().await,
    });
    b.request(Request::PeerAddress {
        peer: "alice".into(),
        address: a.listening().await,
    });
}
async fn directory(bridge: &Running, session: &str) -> Value {
    wait("OpenCode directory", || {
        bridge
            .request(Request::RemoteList)
            .as_array()?
            .iter()
            .find(|row| row["session"] == session)
            .cloned()
    })
    .await
}
async fn received(bridge: &Running, body: &str) -> Value {
    wait("received envelope", || {
        bridge.history().into_iter().find(|row| row["body"] == body)
    })
    .await
}
async fn state(bridge: &Running, id: &str, desired: &str) -> Value {
    wait(desired, || {
        let row = bridge.request(Request::Get { id: id.into() });
        (row["state"] == desired).then_some(row)
    })
    .await
}
fn setup(a: &Running, b: &Running, na: &Native, nb: &Native) {
    na.observe(a, "Origin");
    nb.observe(b, "Destination");
    pair(a, b);
    assert_eq!(a.request(Request::SharedSessions), json!([]));
    assert_eq!(b.request(Request::SharedSessions), json!([]));
    a.share("bob", &na.name(), true);
    b.share("alice", &nb.name(), true);
}

#[tokio::test]
async fn shared_opencode_sessions_submit_once_and_exchange_exact_acknowledged_reply_chains() {
    let root = temp();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let na = Native::start(A, false);
    let nb = Native::start(B, false);
    na.observe(&a, "Origin");
    nb.observe(&b, "Private");
    a.request(Request::MachineSetName {
        name: "machine-a".into(),
    });
    b.request(Request::MachineSetName {
        name: "machine-b".into(),
    });
    pair(&a, &b);
    connect(&a, &b).await;
    // A connection and a live native registration convey no sharing authority.
    wait("connected", || {
        a.request(Request::PeerConnections)
            .as_array()?
            .iter()
            .find(|row| row["state"] == "connected")
            .cloned()
    })
    .await;
    assert_eq!(a.request(Request::RemoteList), json!([]));
    assert_eq!(a.request(Request::SharedSessions), json!([]));
    a.share("bob", &na.name(), true);
    b.share("alice", &nb.name(), true);
    wait("destination machine metadata", || {
        a.request(Request::RemoteList)
            .as_array()?
            .iter()
            .find(|row| row["session"] == B && row["machine_name"] == "machine-b")
            .cloned()
    })
    .await;
    wait("source machine metadata", || {
        b.request(Request::RemoteList)
            .as_array()?
            .iter()
            .find(|row| row["session"] == A && row["machine_name"] == "machine-a")
            .cloned()
    })
    .await;
    nb.observe(&b, "Renamed title");
    let id = uuid::Uuid::new_v4().to_string();
    let body = "Exact native destination\nUnicode 🦀";
    for _ in 0..2 {
        a.send(&id, &na.name(), &format!("bob/{}", nb.name()), body, None);
    }
    let incoming = received(&b, body).await;
    let local = incoming["id"].as_str().unwrap();
    state(&b, local, "submitted").await;
    state(&a, &id, "submitted").await;
    assert_eq!(incoming["recipient_session"], B);
    assert_eq!(incoming["source_machine"]["peer_id"], a.identity());
    assert_eq!(incoming["source_machine"]["machine_name"], "machine-a");
    assert_eq!(incoming["destination_machine"]["peer_id"], b.identity());
    assert_eq!(incoming["destination_machine"]["machine_name"], "machine-b");
    let frames = nb.frames();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0]["id"], local);
    let content = frames[0]["content"].as_str().unwrap();
    assert_eq!(
        content,
        format!(
            "alice/{}:\n{body}\n\n[peer id={local} from={}/{} to={}]",
            na.name(),
            a.identity(),
            na.name(),
            nb.name()
        )
    );
    assert!(!content.contains("Prefer the configured"));
    assert!(!content.contains("--in-reply-to"));
    b.request(Request::Ack {
        id: local.into(),
        agent: nb.name(),
    });
    state(&a, &id, "acknowledged").await;
    let reply_id = uuid::Uuid::new_v4().to_string();
    b.send(
        &reply_id,
        &nb.name(),
        incoming["sender"].as_str().unwrap(),
        "Correlated reply",
        Some(local.into()),
    );
    let reply = received(&a, "Correlated reply").await;
    let reply_local = reply["id"].as_str().unwrap();
    state(&a, reply_local, "submitted").await;
    assert_eq!(reply["parent"], id);
    assert_eq!(reply["hops"], 1);
    assert_eq!(reply["recipient_session"], A);
    assert_eq!(reply["source_machine"]["peer_id"], b.identity());
    assert_eq!(reply["destination_machine"]["peer_id"], a.identity());
    a.request(Request::Ack {
        id: reply_local.into(),
        agent: na.name(),
    });
    state(&b, &reply_id, "acknowledged").await;
    a.share("bob", &na.name(), false);
    assert!(
        a.broker
            .request(Request::Send {
                id: uuid::Uuid::new_v4().to_string(),
                sender: na.name(),
                recipient: format!("bob/{}", nb.name()),
                body: "Source no longer shared".into(),
                parent: None,
            })
            .is_err()
    );
    assert_eq!(na.frames().len(), 1);
    assert_eq!(nb.frames().len(), 1);
    assert_eq!(
        b.history().iter().filter(|row| row["body"] == body).count(),
        1
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn unsharing_and_revocation_block_queued_opencode_before_native_submission() {
    for policy in ["unshare", "revoke", "disable"] {
        let root = temp();
        let a = Running::start(&root.path().join("a"));
        let b = Running::start(&root.path().join("b"));
        let na = Native::start(A, false);
        let nb = Native::start(B, false);
        setup(&a, &b, &na, &nb);
        b.request(Request::Pause { paused: true });
        connect(&a, &b).await;
        directory(&a, B).await;
        let id = uuid::Uuid::new_v4().to_string();
        a.send(
            &id,
            &na.name(),
            &format!("bob/{}", nb.name()),
            "Queued policy change",
            None,
        );
        let queued = received(&b, "Queued policy change").await;
        assert_eq!(queued["state"], "queued");
        match policy {
            "revoke" => {
                b.request(Request::PeerRevoke {
                    peer: "alice".into(),
                });
            }
            "disable" => {
                b.request(Request::Register {
                    agent: Agent {
                        name: nb.name(),
                        provider: Provider::OpenCode,
                        session: B.into(),
                        endpoint: nb.path.to_str().unwrap().into(),
                        enabled: false,
                    },
                    replace: false,
                });
            }
            _ => b.share("alice", &nb.name(), false),
        }
        b.request(Request::Pause { paused: false });
        state(&b, queued["id"].as_str().unwrap(), "blocked").await;
        assert!(nb.frames().is_empty());
        a.stop().await;
        b.stop().await;
    }
}

#[tokio::test]
async fn replacing_the_shared_route_does_not_transfer_native_provider_or_session_grants() {
    for provider in [Provider::OpenCode, Provider::Inbox] {
        let root = temp();
        let a = Running::start(&root.path().join("a"));
        let b = Running::start(&root.path().join("b"));
        let na = Native::start(A, false);
        let nb = Native::start(B, false);
        let replacement = Native::start(A, false);
        setup(&a, &b, &na, &nb);
        connect(&a, &b).await;
        directory(&a, B).await;
        if provider == Provider::OpenCode {
            replacement.observe(&b, "Explicitly opened replacement session");
        }
        b.request(Request::Register {
            agent: Agent {
                name: nb.name(),
                provider: provider.clone(),
                session: if provider == Provider::OpenCode {
                    A.into()
                } else {
                    B.into()
                },
                endpoint: if provider == Provider::OpenCode {
                    replacement.path.to_str().unwrap().into()
                } else {
                    String::new()
                },
                enabled: true,
            },
            replace: true,
        });
        let shared = b.request(Request::SharedSessions);
        assert_eq!(shared[0]["provider"], "opencode");
        assert_eq!(shared[0]["session"], B);
        assert_eq!(shared[0]["stale"], true);
        let id = uuid::Uuid::new_v4().to_string();
        a.send(
            &id,
            &na.name(),
            &format!("bob/{}", nb.name()),
            "No inherited grant",
            None,
        );
        state(&a, &id, "failed").await;
        assert!(
            b.history()
                .iter()
                .all(|row| row["body"] != "No inherited grant")
        );
        assert!(nb.frames().is_empty());
        assert!(replacement.frames().is_empty());
        a.stop().await;
        b.stop().await;
    }
}

#[tokio::test]
async fn queued_opencode_survives_origin_restart_and_destination_offline_recovery() {
    let root = temp();
    let ap = root.path().join("a");
    let bp = root.path().join("b");
    let a = Running::start(&ap);
    let b = Running::start(&bp);
    let na = Native::start(A, false);
    let nb = Native::start(B, false);
    setup(&a, &b, &na, &nb);
    connect(&a, &b).await;
    directory(&a, B).await;
    let address = b.listening().await;
    b.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some(address.clone()),
    });
    a.request(Request::NetworkConfigure {
        enabled: false,
        listen: None,
    });
    b.stop().await;
    let id = uuid::Uuid::new_v4().to_string();
    a.send(
        &id,
        &na.name(),
        &format!("bob/{}", nb.name()),
        "Durable OpenCode offline",
        None,
    );
    assert_eq!(
        a.request(Request::Get { id: id.clone() })["transfer"]["state"],
        "pending"
    );
    a.stop().await;
    let a = Running::start(&ap);
    na.observe(&a, "Origin live after broker restart");
    assert_eq!(
        a.request(Request::Get { id: id.clone() })["transfer"]["state"],
        "pending"
    );
    a.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
    });
    let b = Running::start(&bp);
    nb.observe(&b, "Destination live after broker restart");
    assert_eq!(b.listening().await, address);
    b.request(Request::PeerAddress {
        peer: "alice".into(),
        address: a.listening().await,
    });
    let incoming = received(&b, "Durable OpenCode offline").await;
    let local = incoming["id"].as_str().unwrap();
    state(&b, local, "submitted").await;
    state(&a, &id, "submitted").await;
    b.request(Request::Ack {
        id: local.into(),
        agent: nb.name(),
    });
    state(&a, &id, "acknowledged").await;
    assert_eq!(nb.frames().len(), 1);
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn lost_native_receipt_is_uncertain_and_duplicate_remote_send_never_resubmits() {
    let root = temp();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let na = Native::start(A, false);
    let nb = Native::start(B, true);
    setup(&a, &b, &na, &nb);
    connect(&a, &b).await;
    directory(&a, B).await;
    let id = uuid::Uuid::new_v4().to_string();
    let recipient = format!("bob/{}", nb.name());
    a.send(&id, &na.name(), &recipient, "Lost insertion receipt", None);
    let incoming = received(&b, "Lost insertion receipt").await;
    let local = incoming["id"].as_str().unwrap();
    state(&b, local, "uncertain").await;
    state(&a, &id, "uncertain").await;
    a.send(&id, &na.name(), &recipient, "Lost insertion receipt", None);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(nb.frames().len(), 1);
    assert_eq!(
        b.history()
            .iter()
            .filter(|row| row["body"] == "Lost insertion receipt")
            .count(),
        1
    );
    assert_eq!(
        b.request(Request::Get { id: local.into() })["state"],
        "uncertain"
    );
    a.stop().await;
    b.stop().await;
}

#[tokio::test]
async fn legacy_peer_refuses_opencode_source_explicitly_and_preserves_supported_traffic() {
    use agent_bridge::network::{
        transport::Transport,
        wire::{Operation, RemoteAgent, WireResponse},
    };
    let root = temp();
    let source = Running::start(&root.path().join("source"));
    let native = Native::start(A, false);
    native.observe(&source, "Private origin");
    let key = libp2p::identity::Keypair::generate_ed25519();
    let legacy_id = key.public().to_peer_id().to_string();
    let mut legacy = Transport::start(
        key,
        "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        vec![source.identity().parse().unwrap()],
    )
    .unwrap();
    let address = wait("legacy listener", || {
        legacy.status.borrow()["listening"]
            .as_array()?
            .first()
            .cloned()
    })
    .await
    .as_str()
    .unwrap()
    .to_owned();
    let claude_session = uuid::Uuid::new_v4().to_string();
    let codex_session = uuid::Uuid::new_v4().to_string();
    for (name, provider, session, endpoint) in [
        (
            "claude",
            Provider::Claude,
            claude_session.clone(),
            native.path.to_str().unwrap().to_owned(),
        ),
        (
            "codex",
            Provider::Codex,
            codex_session.clone(),
            String::new(),
        ),
    ] {
        source.request(Request::Register {
            agent: Agent {
                name: name.into(),
                provider,
                session,
                endpoint,
                enabled: true,
            },
            replace: false,
        });
    }
    let (sent, mut received) = tokio::sync::mpsc::channel(4);
    let server = tokio::spawn(async move {
        while let Some(incoming) = legacy.incoming.recv().await {
            let response = match incoming.request.operation {
                Operation::Hello { .. } | Operation::DirectorySupported { .. } => {
                    WireResponse::Rejected {
                        reason: "unknown operation in legacy protocol".into(),
                        retryable: false,
                    }
                }
                Operation::Directory => WireResponse::Directory {
                    agents: vec![
                        RemoteAgent {
                            name: "user".into(),
                            provider: Provider::Inbox,
                            session: "user".into(),
                        },
                        RemoteAgent {
                            name: "claude".into(),
                            provider: Provider::Claude,
                            session: claude_session.clone(),
                        },
                        RemoteAgent {
                            name: "codex".into(),
                            provider: Provider::Codex,
                            session: codex_session.clone(),
                        },
                    ],
                },
                Operation::Sync { .. } => WireResponse::Status { events: vec![] },
                Operation::Send { envelope } => {
                    assert_ne!(
                        envelope.source.provider,
                        Provider::OpenCode,
                        "unsupported provider reached legacy decoder"
                    );
                    assert_ne!(envelope.destination.provider, Provider::OpenCode);
                    let id = envelope.id.clone();
                    sent.send(*envelope).await.unwrap();
                    WireResponse::Accepted { id }
                }
            };
            let _ = incoming.response.send(response);
        }
    });
    source.request(Request::PeerPair {
        peer_id: legacy_id.clone(),
        fingerprint: legacy_id,
        alias: Some("legacy".into()),
    });
    source.share("legacy", &native.name(), true);
    source.share("legacy", "user", true);
    source.share("legacy", "claude", true);
    source.share("legacy", "codex", true);
    source.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
    });
    source.request(Request::PeerAddress {
        peer: "legacy".into(),
        address,
    });
    directory(&source, "user").await;
    let unsupported = uuid::Uuid::new_v4().to_string();
    source.send(
        &unsupported,
        &native.name(),
        "legacy/user",
        "Explicitly unsupported origin",
        None,
    );
    let rejected = state(&source, &unsupported, "failed").await;
    assert_eq!(rejected["transfer"]["state"], "rejected");
    assert!(
        rejected["detail"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("opencode")
    );
    let supported = uuid::Uuid::new_v4().to_string();
    source.send(
        &supported,
        "user",
        "legacy/user",
        "Legacy traffic still works",
        None,
    );
    let envelope = timeout(Duration::from_secs(20), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(envelope.id, supported);
    assert_eq!(envelope.body, "Legacy traffic still works");
    wait("accepted legacy transfer", || {
        let row = source.request(Request::Get {
            id: supported.clone(),
        });
        (row["transfer"]["state"] == "accepted").then_some(row)
    })
    .await;
    for (name, provider) in [("claude", Provider::Claude), ("codex", Provider::Codex)] {
        let id = uuid::Uuid::new_v4().to_string();
        source.send(
            &id,
            name,
            &format!("legacy/{name}"),
            "Existing native traffic",
            None,
        );
        let envelope = timeout(Duration::from_secs(20), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(envelope.id, id);
        assert_eq!(envelope.source.provider, provider);
        assert_eq!(envelope.destination.provider, provider);
        wait("accepted native legacy transfer", || {
            let row = source.request(Request::Get { id: id.clone() });
            (row["transfer"]["state"] == "accepted").then_some(row)
        })
        .await;
    }
    assert!(
        timeout(Duration::from_millis(200), received.recv())
            .await
            .is_err()
    );
    source.stop().await;
    server.abort();
    let _ = server.await;
}
