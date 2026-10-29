use agent_bridge::{
    broker::{self, Broker},
    network::{
        transport::Transport,
        trust::Permission,
        wire::{Operation, RemoteAgent, WireResponse},
    },
    protocol::{Agent, Provider, Request},
};
use serde_json::Value;
use std::{
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixListener,
    task::JoinHandle,
    time::timeout,
};

struct Running {
    broker: Broker,
    task: JoinHandle<anyhow::Result<()>>,
}

impl Running {
    fn start(state: &Path) -> Self {
        let bound = Broker::bind(state.into(), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        Self {
            broker,
            task: tokio::spawn(broker::serve(bound)),
        }
    }

    fn request(&self, request: Request) -> Value {
        self.broker.request(request).unwrap()
    }

    fn identity(&self) -> String {
        self.request(Request::Identity)["peer_id"]
            .as_str()
            .unwrap()
            .to_owned()
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

    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

async fn wait(mut inspect: impl FnMut() -> Option<Value>) -> Value {
    timeout(Duration::from_secs(40), async {
        loop {
            if let Some(value) = inspect() {
                return value;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("machine metadata condition timed out")
}

fn grant(bridge: &Running, peer: &str, agent: &str) {
    for permission in [Permission::Discover, Permission::Receive, Permission::Send] {
        bridge.request(Request::PeerGrant {
            peer: peer.into(),
            agent: agent.into(),
            permission,
            allow: true,
        });
    }
}

fn fixed_address() -> String {
    let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    format!(
        "/ip4/127.0.0.1/udp/{}/quic-v1",
        socket.local_addr().unwrap().port()
    )
}

fn pair(bridge: &Running, peer: &str, alias: &str) {
    bridge.request(Request::PeerPair {
        peer_id: peer.into(),
        fingerprint: peer.into(),
        alias: Some(alias.into()),
    });
}

fn directory_machine(bridge: &Running, agent: &str, machine: &str) -> Option<Value> {
    bridge
        .request(Request::RemoteList)
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["name"] == agent && entry["machine_name"] == machine)
        .cloned()
}

#[tokio::test]
async fn authenticated_machine_names_survive_renaming_with_pinned_queued_delivery() {
    let root = tempfile::Builder::new()
        .prefix("ab-machine-")
        .tempdir_in("/tmp")
        .unwrap();
    let alice = Running::start(&root.path().join("a"));
    let bob = Running::start(&root.path().join("b"));
    alice.request(Request::MachineSetName {
        name: "machine-a".into(),
    });
    bob.request(Request::MachineSetName {
        name: "machine-b".into(),
    });
    let alice_id = alice.identity();
    let bob_id = bob.identity();
    let socket_dir = root.path().join("receiver");
    std::fs::create_dir(&socket_dir).unwrap();
    std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket_path = socket_dir.join("native.sock");
    let socket = UnixListener::bind(&socket_path).unwrap();
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    bob.request(Request::Register {
        agent: Agent {
            name: "reviewer".into(),
            provider: Provider::Claude,
            session: session.clone(),
            endpoint: socket_path.to_string_lossy().into_owned(),
            enabled: true,
        },
        replace: false,
    });
    pair(&alice, &bob_id, "destination");
    pair(&bob, &alice_id, "origin");
    grant(&alice, "destination", "user");
    grant(&bob, "origin", "reviewer");
    let alice_address = fixed_address();
    let bob_address = fixed_address();
    for (bridge, address) in [(&alice, &alice_address), (&bob, &bob_address)] {
        bridge.request(Request::NetworkConfigure {
            enabled: true,
            listen: Some(address.clone()),
        });
    }
    alice.request(Request::PeerAddress {
        peer: "destination".into(),
        address: bob_address,
    });
    bob.request(Request::PeerAddress {
        peer: "origin".into(),
        address: alice_address,
    });
    let directory = wait(|| directory_machine(&alice, "reviewer", "machine-b")).await;
    assert_eq!(directory["peer_id"], bob_id);
    assert_eq!(directory["machine_display_name"], "destination");
    let local = alice.request(Request::List { all: false });
    assert_eq!(local[0]["machine_name"], "machine-a");
    assert_eq!(local[0]["peer_id"], alice_id);
    bob.request(Request::Pause { paused: true });
    let id = uuid::Uuid::new_v4().to_string();
    alice.request(Request::Send {
        id: id.clone(),
        sender: "user".into(),
        recipient: "destination/reviewer".into(),
        body: "queued through machine rename".into(),
        parent: None,
    });
    let queued = wait(|| {
        let message = alice.request(Request::Get { id: id.clone() });
        (message["transfer"]["state"] == "accepted").then_some(message)
    })
    .await;
    assert_eq!(queued["recipient_session"], session);
    let canonical_destination = queued["recipient"].clone();
    bob.request(Request::MachineSetName {
        name: "renamed-machine-b".into(),
    });
    let renamed = wait(|| directory_machine(&alice, "reviewer", "renamed-machine-b")).await;
    assert_eq!(renamed["machine_display_name"], "destination");
    assert_eq!(bob.identity(), bob_id);
    let peer = bob.request(Request::PeersList);
    assert_eq!(peer[0]["alias"], "origin");
    assert_eq!(peer[0]["grants"].as_array().unwrap().len(), 3);
    let still_queued = alice.request(Request::Get { id: id.clone() });
    assert_eq!(still_queued["recipient"], canonical_destination);
    assert_eq!(still_queued["recipient_session"], session);
    assert_eq!(still_queued["id"], id);
    bob.request(Request::Pause { paused: false });
    let (stream, _) = timeout(Duration::from_secs(5), socket.accept())
        .await
        .unwrap()
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    let frame: Value = serde_json::from_str(&line).unwrap();
    let text = frame["message"]["content"].as_str().unwrap();
    let local_id = frame["msg_id"].as_str().unwrap().to_owned();
    assert!(text.starts_with("<cross-session-message from-name=\"origin/user\">"));
    assert!(text.contains(&format!("Reply to: {alice_id}/user")));
    let envelope = bob.request(Request::Get {
        id: local_id.clone(),
    });
    assert_eq!(envelope["source_machine"]["machine_name"], "machine-a");
    assert_eq!(envelope["source_machine"]["peer_id"], alice_id);
    assert_eq!(
        envelope["destination_machine"]["machine_name"],
        "renamed-machine-b"
    );
    assert_eq!(envelope["destination_machine"]["peer_id"], bob_id);
    wait(|| {
        let message = bob.request(Request::Get {
            id: local_id.clone(),
        });
        (message["state"] == "submitted").then_some(message)
    })
    .await;
    bob.request(Request::Ack {
        id: local_id.clone(),
        agent: "reviewer".into(),
    });
    wait(|| {
        let message = alice.request(Request::Get { id: id.clone() });
        (message["state"] == "acknowledged").then_some(message)
    })
    .await;
    let reply = uuid::Uuid::new_v4().to_string();
    bob.request(Request::Send {
        id: reply,
        sender: "reviewer".into(),
        recipient: envelope["sender"].as_str().unwrap().into(),
        body: "reply after rename".into(),
        parent: Some(local_id),
    });
    let incoming = wait(|| {
        alice
            .history()
            .into_iter()
            .find(|message| message["body"] == "reply after rename")
    })
    .await;
    assert_eq!(incoming["parent"], id);
    assert_eq!(
        incoming["source_machine"]["machine_name"],
        "renamed-machine-b"
    );
    assert_eq!(incoming["source_machine"]["peer_id"], bob_id);
    assert_eq!(incoming["destination_machine"]["machine_name"], "machine-a");
    assert!(
        timeout(Duration::from_millis(150), socket.accept())
            .await
            .is_err()
    );
    alice.stop().await;
    bob.stop().await;
}

async fn cli(state: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .output()
        .await
        .unwrap()
}

async fn cli_json(state: &Path, args: &[&str]) -> Value {
    let output = cli(state, args).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn cli_sets_persistent_machine_name_without_changing_identity() {
    let root = tempfile::Builder::new()
        .prefix("ab-machine-cli-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = root.path().join("state");
    let bridge = Running::start(&state);
    let identity = bridge.identity();
    let initial = cli_json(&state, &["machine", "show"]).await;
    assert!(!initial["machine_name"].as_str().unwrap().is_empty());
    let updated = cli_json(&state, &["machine", "set-name", "workstation"]).await;
    assert_eq!(updated["machine_name"], "workstation");
    assert_eq!(updated["peer_id"], identity);
    assert!(
        !cli(&state, &["machine", "set-name", "other/machine"])
            .await
            .status
            .success()
    );
    bridge.stop().await;
    let bridge = Running::start(&state);
    let restored = cli_json(&state, &["machine", "show"]).await;
    assert_eq!(restored["machine_name"], "workstation");
    assert_eq!(bridge.identity(), identity);
    bridge.stop().await;
}

#[tokio::test]
async fn peer_without_machine_metadata_still_accepts_v1_message_transfers() {
    let root = tempfile::Builder::new()
        .prefix("ab-legacy-name-")
        .tempdir_in("/tmp")
        .unwrap();
    let source = Running::start(&root.path().join("source"));
    source.request(Request::MachineSetName {
        name: "machine-a".into(),
    });
    let source_id = source.identity();
    let key = libp2p::identity::Keypair::generate_ed25519();
    let legacy_id = key.public().to_peer_id();
    let mut legacy = Transport::start(
        key,
        "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        vec![source_id.parse().unwrap()],
    )
    .unwrap();
    let address = wait(|| {
        legacy.status.borrow()["listening"]
            .as_array()
            .and_then(|addresses| addresses.first())
            .cloned()
    })
    .await
    .as_str()
    .unwrap()
    .to_owned();
    let hello_count = Arc::new(AtomicUsize::new(0));
    let seen_hello = hello_count.clone();
    let (sent, mut received) = tokio::sync::mpsc::channel(4);
    let server = tokio::spawn(async move {
        while let Some(incoming) = legacy.incoming.recv().await {
            let response = match incoming.request.operation {
                Operation::Hello { .. } => {
                    seen_hello.fetch_add(1, Ordering::SeqCst);
                    WireResponse::Rejected {
                        reason: "unsupported optional metadata".into(),
                        retryable: false,
                    }
                }
                Operation::DirectorySupported { .. } => WireResponse::Rejected {
                    reason: "unsupported provider negotiation".into(),
                    retryable: false,
                },
                Operation::Directory => WireResponse::Directory {
                    agents: vec![RemoteAgent {
                        name: "user".into(),
                        provider: Provider::Inbox,
                        session: "user".into(),
                    }],
                },
                Operation::Sync { .. } => WireResponse::Status { events: vec![] },
                Operation::Send { envelope } => {
                    let id = envelope.id.clone();
                    sent.send(*envelope).await.unwrap();
                    WireResponse::Accepted { id }
                }
            };
            let _ = incoming.response.send(response);
        }
    });
    pair(&source, &legacy_id.to_string(), "legacy");
    grant(&source, "legacy", "user");
    source.request(Request::NetworkConfigure {
        enabled: true,
        listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
    });
    source.request(Request::PeerAddress {
        peer: "legacy".into(),
        address,
    });
    let directory = wait(|| {
        source
            .request(Request::RemoteList)
            .as_array()
            .unwrap()
            .iter()
            .find(|agent| agent["name"] == "user")
            .cloned()
    })
    .await;
    assert!(directory["machine_name"].is_null());
    let id = uuid::Uuid::new_v4().to_string();
    source.request(Request::Send {
        id: id.clone(),
        sender: "user".into(),
        recipient: "legacy/user".into(),
        body: "v1 envelope unchanged".into(),
        parent: None,
    });
    let envelope = timeout(Duration::from_secs(10), received.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(envelope.id, id);
    assert_eq!(envelope.origin_peer, source_id);
    assert_eq!(envelope.destination_peer, legacy_id.to_string());
    wait(|| {
        let message = source.request(Request::Get { id: id.clone() });
        (message["transfer"]["state"] == "accepted").then_some(message)
    })
    .await;
    assert_eq!(hello_count.load(Ordering::SeqCst), 1);
    assert!(source.request(Request::PeersList)[0]["machine_name"].is_null());
    source.stop().await;
    server.abort();
    let _ = server.await;
}
