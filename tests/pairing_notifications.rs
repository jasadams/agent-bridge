//! Exercise owner prompts through real loopback QUIC and the private native
//! Claude socket transport. No model process or existing session is contacted.
mod support;
use agent_bridge::{
    broker::{self, Broker},
    protocol::{Agent, Provider, Request},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixListener,
    sync::mpsc,
    time::timeout,
};

struct Running {
    broker: Broker,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}
impl Running {
    fn start(state: &Path) -> Self {
        let server = support::bind(state.into(), "/missing/codex".into()).unwrap();
        Self {
            broker: server.broker.clone(),
            task: tokio::spawn(broker::serve(server)),
        }
    }
    fn request(&self, request: Request) -> Value {
        self.broker.request(request).unwrap()
    }
    fn peers(&self) -> Value {
        self.request(Request::PeersList)
    }
    fn pairing(&self) -> Value {
        self.request(Request::LanPairingList)
    }
    fn identity(&self) -> String {
        self.request(Request::Identity)["peer_id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn address(&self) -> String {
        self.request(Request::NetworkConfigure {
            enabled: true,
            listen: Some("/ip4/127.0.0.1/udp/0/quic-v1".into()),
        });
        timeout(Duration::from_secs(5), async {
            loop {
                if let Some(address) = self.request(Request::Status)["network"]["listening"]
                    .as_array()
                    .and_then(|addresses| addresses.first())
                    .and_then(Value::as_str)
                {
                    break address.to_owned();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }
    async fn wait_state(&self, id: &str, state: &str) -> Value {
        timeout(Duration::from_secs(12), async {
            loop {
                if let Some(view) = self.pairing()["requests"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|view| view["id"] == id && view["state"] == state)
                {
                    break view.clone();
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("waiting for {state}: {}", self.pairing()))
    }
    fn watch(&self, agent: &Agent, id: Option<&str>) {
        self.request(Request::LanPairingWatch {
            agent: agent.name.clone(),
            id: id.map(str::to_owned),
            seconds: 120,
        });
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Native {
    agent: Agent,
    frames: mpsc::UnboundedReceiver<Value>,
    task: tokio::task::JoinHandle<()>,
}
impl Native {
    fn start(broker: &Running, directory: &Path, name: &str) -> Self {
        std::fs::create_dir_all(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = directory.join(format!("{name}.sock"));
        let listener = UnixListener::bind(&socket).unwrap();
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
        let agent = Agent {
            name: name.into(),
            provider: Provider::Claude,
            session: uuid::Uuid::new_v4().to_string(),
            endpoint: socket.to_string_lossy().into(),
            enabled: true,
        };
        broker.request(Request::Register {
            agent: agent.clone(),
            replace: false,
        });
        let (sender, frames) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).await.unwrap_or(0) != 0 {
                    let frame = serde_json::from_str(&line).unwrap();
                    if sender.send(frame).is_err() {
                        return;
                    }
                }
            }
        });
        Self {
            agent,
            frames,
            task,
        }
    }
    async fn phase(&mut self, id: &str, phase: &str) -> Value {
        timeout(Duration::from_secs(12), async {
            let frame = self.frames.recv().await.expect("native listener stopped");
            assert_eq!(frame["session_id"], self.agent.session);
            let content = frame["message"]["content"].as_str().unwrap();
            assert!(
                content.starts_with("[Agent Bridge: local pairing status, not user approval]\n")
            );
            let event: Value = serde_json::from_str(content.lines().nth(1).unwrap()).unwrap();
            assert_eq!(event["request_id"], id);
            assert_eq!(event["phase"], phase, "unexpected notification: {event}");
            frame
        })
        .await
        .unwrap_or_else(|_| panic!("no {phase} notification for {id}"))
    }
    async fn quiet(&mut self) {
        assert!(
            timeout(Duration::from_millis(600), self.frames.recv())
                .await
                .is_err(),
            "unexpected native notification"
        );
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn connect(a: &Running, b: &Running, address: String) -> String {
    a.request(Request::LanPairingConnect {
        peer: b.identity(),
        address: Some(address),
        seconds: 120,
    })["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn root() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ab-prompt-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[tokio::test]
async fn selected_agents_receive_one_prompt_and_completion_without_automatic_trust_or_sharing() {
    let root = root();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let mut an = Native::start(&a, &root.path().join("native-a"), "owner-a");
    let mut bn = Native::start(&b, &root.path().join("native-b"), "owner-b");
    let mut unrelated = Native::start(&b, &root.path().join("other"), "unrelated");
    let _ = a.address().await;
    let address = b.address().await;
    b.request(Request::LanPairingOpen { seconds: 120 });
    b.watch(&bn.agent, None);
    let id = connect(&a, &b, address).await;
    a.watch(&an.agent, Some(&id));
    let av = a.wait_state(&id, "compare_code").await;
    let bv = b.wait_state(&id, "compare_code").await;
    assert_eq!(av["verification_code"], bv["verification_code"]);
    assert_eq!(av["local_confirmed"], false);
    assert_eq!(bv["local_confirmed"], false);
    let code = av["verification_code"].as_str().unwrap().to_owned();
    for native in [&mut an, &mut bn] {
        let frame = native.phase(&id, "compare_code").await;
        let prompt = frame["message"]["content"].as_str().unwrap();
        assert!(prompt.contains(&code));
        assert!(prompt.contains("Yes or no"));
        native.quiet().await;
    }
    assert_eq!(a.peers(), json!([]));
    assert_eq!(b.peers(), json!([]));
    a.request(Request::LanPairingConfirm {
        id: id.clone(),
        code: code.clone(),
    });
    an.phase(&id, "waiting_peer_confirmation").await;
    assert_eq!(a.peers(), json!([]));
    assert_eq!(b.peers(), json!([]));
    b.request(Request::LanPairingConfirm {
        id: id.clone(),
        code,
    });
    a.wait_state(&id, "paired").await;
    b.wait_state(&id, "paired").await;
    an.phase(&id, "paired").await;
    // A responder may briefly wait for the authenticated remote confirmation
    // even when the initiating owner already approved it.
    timeout(Duration::from_secs(12), async {
        loop {
            let frame = bn.frames.recv().await.unwrap();
            let content = frame["message"]["content"].as_str().unwrap();
            let event: Value = serde_json::from_str(content.lines().nth(1).unwrap()).unwrap();
            assert_eq!(event["request_id"], id);
            if event["phase"] == "paired" {
                break;
            }
            assert_eq!(event["phase"], "waiting_peer_confirmation");
        }
    })
    .await
    .unwrap();
    for broker in [&a, &b] {
        assert_eq!(broker.peers()[0]["grants"], json!([]));
        assert_eq!(broker.request(Request::RemoteList), json!([]));
    }
    unrelated.quiet().await;
    an.quiet().await;
    bn.quiet().await;
}

#[tokio::test]
async fn unselected_requests_do_not_wake_agents_and_decline_never_creates_trust() {
    let root = root();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let mut native = Native::start(&b, &root.path().join("native"), "owner");
    let _ = a.address().await;
    let address = b.address().await;
    b.request(Request::LanPairingOpen { seconds: 120 });
    let id = connect(&a, &b, address).await;
    b.wait_state(&id, "awaiting_accept").await;
    native.quiet().await;
    assert_eq!(
        b.wait_state(&id, "awaiting_accept").await["local_confirmed"],
        false
    );
    b.watch(&native.agent, None);
    let view = b.wait_state(&id, "compare_code").await;
    native.phase(&id, "compare_code").await;
    b.request(Request::LanPairingCancel { id: id.clone() });
    native.phase(&id, "cancelled").await;
    assert!(
        b.broker
            .request(Request::LanPairingConfirm {
                id,
                code: view["verification_code"].as_str().unwrap().into(),
            })
            .is_err()
    );
    assert_eq!(a.peers(), json!([]));
    assert_eq!(b.peers(), json!([]));
}

#[tokio::test]
async fn notification_watch_does_not_follow_replaced_or_disabled_sessions() {
    for replace in [false, true] {
        let root = root();
        let a = Running::start(&root.path().join("a"));
        let b = Running::start(&root.path().join("b"));
        let mut native = Native::start(&b, &root.path().join("native"), "owner");
        let _ = a.address().await;
        let address = b.address().await;
        b.request(Request::LanPairingOpen { seconds: 120 });
        b.watch(&native.agent, None);
        let mut changed = native.agent.clone();
        if replace {
            changed.session = uuid::Uuid::new_v4().to_string();
        } else {
            changed.enabled = false;
        }
        b.request(Request::Register {
            agent: changed,
            replace,
        });
        let id = connect(&a, &b, address).await;
        b.wait_state(&id, "awaiting_accept").await;
        native.quiet().await;
        assert_eq!(
            b.wait_state(&id, "awaiting_accept").await["local_confirmed"],
            false
        );
        assert_eq!(a.peers(), json!([]));
        assert_eq!(b.peers(), json!([]));
    }
}

#[tokio::test]
async fn watch_requires_an_enabled_native_session_and_a_live_local_pairing_scope() {
    let root = root();
    let broker = Running::start(&root.path().join("state"));
    let _ = broker.address().await;
    assert!(
        broker
            .broker
            .request(Request::LanPairingWatch {
                agent: "missing".into(),
                id: None,
                seconds: 120,
            })
            .is_err()
    );
    let mut native = Native::start(&broker, &root.path().join("native"), "owner");
    assert!(
        broker
            .broker
            .request(Request::LanPairingWatch {
                agent: native.agent.name.clone(),
                id: None,
                seconds: 120,
            })
            .is_err()
    );
    broker.request(Request::LanPairingOpen { seconds: 120 });
    assert!(
        broker
            .broker
            .request(Request::LanPairingWatch {
                agent: native.agent.name.clone(),
                id: Some(uuid::Uuid::new_v4().to_string()),
                seconds: 120,
            })
            .is_err()
    );
    broker.request(Request::Register {
        agent: Agent {
            name: "inbox-only".into(),
            provider: Provider::Inbox,
            session: "inbox".into(),
            endpoint: String::new(),
            enabled: true,
        },
        replace: false,
    });
    assert!(
        broker
            .broker
            .request(Request::LanPairingWatch {
                agent: "inbox-only".into(),
                id: None,
                seconds: 120,
            })
            .is_err()
    );
    native.agent.enabled = false;
    broker.request(Request::Register {
        agent: native.agent.clone(),
        replace: false,
    });
    assert!(
        broker
            .broker
            .request(Request::LanPairingWatch {
                agent: native.agent.name.clone(),
                id: None,
                seconds: 120,
            })
            .is_err()
    );
    native.quiet().await;
}

#[tokio::test]
async fn cli_selects_current_codex_or_explicit_agent_and_rejects_invalid_agent_before_opening() {
    let root = root();
    let state = root.path().join("state");
    let broker = Running::start(&state);
    let _ = broker.address().await;
    let codex_session = uuid::Uuid::new_v4().to_string();
    let codex_agent = Agent {
        name: format!("codex-{codex_session}"),
        provider: Provider::Codex,
        session: codex_session.clone(),
        endpoint: String::new(),
        enabled: true,
    };
    broker.request(Request::Register {
        agent: codex_agent.clone(),
        replace: false,
    });
    let native = Native::start(&broker, &root.path().join("native"), "explicit-owner");
    support::claude_directory::set_rows(
        &state.join("native-cli"),
        &json!([{
            "sessionId":native.agent.session,"name":native.agent.name,
            "cwd":"/fixture","kind":"interactive","startedAt":0,"status":"idle"
        }]),
    );
    let executable = env!("CARGO_BIN_EXE_agent-bridge");
    let output = tokio::process::Command::new(executable)
        .arg("--state-dir")
        .arg(&state)
        .args(["pairing", "open", "--seconds", "120"])
        .env("CODEX_THREAD_ID", &codex_session)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["notifications"]["notification_agent"],
        codex_agent.name
    );
    assert_eq!(result["notifications"]["provider"], "codex");
    assert_eq!(result["notifications"]["session"], codex_session);
    broker.request(Request::LanPairingClose);

    let output = tokio::process::Command::new(executable)
        .arg("--state-dir")
        .arg(&state)
        .args(["pairing", "open", "--agent", &native.agent.name])
        .env("CODEX_THREAD_ID", uuid::Uuid::new_v4().to_string())
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        result["notifications"]["notification_agent"],
        native.agent.name
    );
    assert_eq!(result["notifications"]["provider"], "claude");
    assert_eq!(result["notifications"]["session"], native.agent.session);
    broker.request(Request::LanPairingClose);

    let output = tokio::process::Command::new(executable)
        .arg("--state-dir")
        .arg(&state)
        .args(["pairing", "open", "--agent", "missing"])
        .env_remove("CODEX_THREAD_ID")
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert_eq!(broker.pairing()["open"], false);
    assert!(broker.pairing()["requests"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn paused_broker_defers_pairing_prompts_and_crypto_acceptance_until_resume() {
    let root = root();
    let a = Running::start(&root.path().join("a"));
    let b = Running::start(&root.path().join("b"));
    let mut native = Native::start(&b, &root.path().join("native"), "owner");
    let _ = a.address().await;
    let address = b.address().await;
    b.request(Request::LanPairingOpen { seconds: 120 });
    b.watch(&native.agent, None);
    b.request(Request::Pause { paused: true });
    let id = connect(&a, &b, address).await;
    b.wait_state(&id, "awaiting_accept").await;
    native.quiet().await;
    assert_eq!(
        b.wait_state(&id, "awaiting_accept").await["local_confirmed"],
        false
    );
    assert_eq!(a.peers(), json!([]));
    assert_eq!(b.peers(), json!([]));
    b.request(Request::Pause { paused: false });
    b.wait_state(&id, "compare_code").await;
    native.phase(&id, "compare_code").await;
    native.quiet().await;
    assert_eq!(a.peers(), json!([]));
    assert_eq!(b.peers(), json!([]));
}
