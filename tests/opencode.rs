use agent_bridge::{
    broker::{self, Broker},
    clients, delivery,
    protocol::{Agent, Message, Provider, Request},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::PathBuf,
};

const SESSION: &str = "ses_0123456789abcdefghijklmnop";
const OTHER: &str = "ses_0123456789abcdefghijklmnox";
const ID: &str = "123e4567-e89b-12d3-a456-426614174000";

struct PluginSocket {
    root: tempfile::TempDir,
    path: PathBuf,
    worker: std::thread::JoinHandle<()>,
}
impl PluginSocket {
    fn start(count: usize, response: impl Fn(Value, &str) -> Value + Send + 'static) -> Self {
        Self::start_optional(count, move |frame, instance| {
            Some(response(frame, instance))
        })
    }
    fn start_optional(
        count: usize,
        response: impl Fn(Value, &str) -> Option<Value> + Send + 'static,
    ) -> Self {
        let root = tempfile::Builder::new()
            .prefix("ab-oc-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        let path = root.path().join(format!("{instance}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let worker = std::thread::spawn(move || {
            for _ in 0..count {
                let (mut stream, _) = listener.accept().unwrap();
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let frame = serde_json::from_str(&line).unwrap();
                let Some(response) = response(frame, &instance) else {
                    continue;
                };
                let bytes = format!("{response}\n");
                if let Err(error) = stream.write_all(bytes.as_bytes()) {
                    // Oversized-receipt tests intentionally make the client close
                    // at its frame bound, before the fixture finishes writing.
                    assert!(
                        bytes.len() > agent_bridge::protocol::MAX_FRAME
                            && matches!(
                                error.kind(),
                                std::io::ErrorKind::BrokenPipe
                                    | std::io::ErrorKind::ConnectionReset
                            ),
                        "Unexpected fixture write failure: {error}"
                    );
                }
            }
        });
        Self { root, path, worker }
    }
    fn agent(&self) -> Agent {
        Agent {
            name: format!("opencode-{SESSION}"),
            provider: Provider::OpenCode,
            session: SESSION.into(),
            endpoint: self.path.to_str().unwrap().into(),
            enabled: true,
        }
    }
    fn finish(self) {
        self.worker.join().unwrap();
    }
}
fn receipt(frame: Value, instance: &str) -> Value {
    if frame["session"] != SESSION {
        return json!({"ok":false,"error":"unknown session"});
    }
    let mut value = json!({"ok":true,"protocol":2,"session":SESSION,"instance":instance});
    if frame["op"] == "deliver" || frame["op"] == "reconcile" {
        assert!(!frame["content"].as_str().unwrap().is_empty());
        assert!(frame.get("permissions").is_none());
        assert!(frame.get("tools").is_none());
        value["id"] = frame["id"].clone();
        for key in ["native_message_id", "namespace", "generation", "attempt"] {
            value[key] = frame[key].clone();
        }
        value["status"] = json!("inserted");
    }
    value
}
fn message() -> Message {
    Message {
        id: ID.into(),
        sender: "user".into(),
        recipient: format!("opencode-{SESSION}"),
        recipient_session: SESSION.into(),
        body: "test".into(),
        parent: None,
        hops: 0,
        state: "queued".into(),
        detail: None,
        created: 0,
        updated: 0,
    }
}

async fn broker_delivery(plugin: &PluginSocket) -> Message {
    let state = plugin.root.path().join("delivery-state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    let agent = plugin.agent();
    broker::rpc(
        &state,
        &Request::OpenCodeObserve {
            session: SESSION.into(),
            endpoint: agent.endpoint,
            title: Some("Delivery".into()),
            ready: true,
        },
    )
    .await
    .unwrap();
    broker::rpc(
        &state,
        &Request::Send {
            id: ID.into(),
            sender: "user".into(),
            recipient: agent.name,
            body: "test".into(),
            parent: None,
        },
    )
    .await
    .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let value = broker::rpc(&state, &Request::Get { id: ID.into() })
                .await
                .unwrap();
            let msg: Message = serde_json::from_value(value).unwrap();
            if !["queued", "sending"].contains(&msg.state.as_str()) {
                break msg;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    server.abort();
    let _ = server.await;
    result
}

#[tokio::test]
async fn exact_session_receipts_bind_native_and_broker_ids() {
    let plugin = PluginSocket::start(2, receipt);
    let outcome = broker_delivery(&plugin).await;
    assert_eq!(outcome.state, "submitted", "{:?}", outcome.detail);
    assert!(
        outcome
            .detail
            .unwrap()
            .contains("acknowledgement and completion remain unconfirmed")
    );
    plugin.finish();
}

#[tokio::test]
async fn wrong_instance_session_and_message_receipts_never_submit() {
    for field in ["instance", "session", "id", "native_message_id"] {
        let plugin = PluginSocket::start(2, move |frame, instance| {
            if frame["op"] == "probe" {
                return receipt(frame, instance);
            }
            let mut value = receipt(frame, instance);
            value[field] = json!("wrong");
            value
        });
        let outcome = broker_delivery(&plugin).await;
        assert_eq!(outcome.state, "uncertain");
        plugin.finish();
    }
    let plugin = PluginSocket::start(0, receipt);
    let mut msg = message();
    msg.recipient_session = OTHER.into();
    assert_eq!(
        delivery::deliver(
            &plugin.agent(),
            &msg,
            std::path::Path::new("unused"),
            plugin.root.path()
        )
        .await
        .state,
        "failed"
    );
    plugin.finish();
}

#[tokio::test]
async fn live_registration_titles_disposal_and_endpoint_conflicts_use_shared_registry() {
    let plugin = PluginSocket::start(4, receipt);
    let agent = plugin.agent();
    let state = plugin.root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    for title in ["First", "Renamed"] {
        broker::rpc(
            &state,
            &Request::OpenCodeObserve {
                session: SESSION.into(),
                endpoint: agent.endpoint.clone(),
                title: Some(title.into()),
                ready: true,
            },
        )
        .await
        .unwrap();
        let listed = broker::rpc(&state, &Request::List { all: false })
            .await
            .unwrap();
        assert!(
            listed
                .as_array()
                .unwrap()
                .iter()
                .any(|a| a["session"] == SESSION && a["display_name"] == title)
        );
    }
    assert!(
        broker::rpc(
            &state,
            &Request::OpenCodeObserve {
                session: OTHER.into(),
                endpoint: agent.endpoint.clone(),
                title: None,
                ready: true,
            }
        )
        .await
        .is_err()
    );
    let other = PluginSocket::start(1, receipt);
    assert!(
        broker::rpc(
            &state,
            &Request::OpenCodeObserve {
                session: SESSION.into(),
                endpoint: other.agent().endpoint,
                title: None,
                ready: true,
            }
        )
        .await
        .is_err()
    );
    other.finish();
    broker::rpc(
        &state,
        &Request::Unregister {
            name: agent.name.clone(),
            session: SESSION.into(),
        },
    )
    .await
    .unwrap();
    let listed = broker::rpc(&state, &Request::List { all: false })
        .await
        .unwrap();
    assert!(
        !listed
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["session"] == SESSION)
    );
    // Re-registration still requires a live exact-session probe.
    broker::rpc(
        &state,
        &Request::Register {
            agent,
            replace: false,
        },
    )
    .await
    .unwrap();
    server.abort();
    plugin.finish();
}

#[test]
fn native_identity_and_remote_boundary_are_provider_specific() {
    assert_eq!("opencode".parse::<Provider>().unwrap(), Provider::OpenCode);
    assert_eq!(
        serde_json::to_value(Provider::OpenCode).unwrap(),
        "opencode"
    );
    clients::validate_session(&Provider::OpenCode, SESSION).unwrap();
    for value in ["", ID, "ses_../escape", "ses_0123456789abcdefghijklmno\n"] {
        assert!(clients::validate_session(&Provider::OpenCode, value).is_err());
    }
    clients::validate_remote_session(&Provider::OpenCode, SESSION).unwrap();
    for value in ["", ID, "ses_../escape", "ses_0123456789abcdefghijklmno\n"] {
        assert!(clients::validate_remote_session(&Provider::OpenCode, value).is_err());
    }
}

#[tokio::test]
async fn rejected_oversized_and_extra_field_receipts_fail_closed() {
    for variant in 0..3 {
        let plugin = PluginSocket::start(2, move |frame, instance| {
            if frame["op"] == "probe" {
                return receipt(frame, instance);
            }
            match variant {
                0 => {
                    json!({"ok":false,"instance":instance,"session":SESSION,"id":ID,"uncertain":false,"error":"unknown or stale"})
                }
                1 => json!({"ok":false,"error":"x".repeat(agent_bridge::protocol::MAX_FRAME + 1)}),
                _ => {
                    let mut value = receipt(frame, instance);
                    value["unexpected"] = json!(true);
                    value
                }
            }
        });
        let outcome = broker_delivery(&plugin).await;
        assert_eq!(outcome.state, "uncertain");
        plugin.finish();
    }
}

#[test]
fn opencode_endpoint_requires_private_socket_and_canonical_instance() {
    let plugin = PluginSocket::start(0, receipt);
    clients::opencode::validate_endpoint(plugin.path.to_str().unwrap()).unwrap();
    std::fs::set_permissions(&plugin.path, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(clients::opencode::validate_endpoint(plugin.path.to_str().unwrap()).is_err());
    std::fs::set_permissions(&plugin.path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let link = plugin
        .root
        .path()
        .join(format!("{}.sock", uuid::Uuid::new_v4()));
    std::os::unix::fs::symlink(&plugin.path, &link).unwrap();
    assert!(clients::opencode::validate_endpoint(link.to_str().unwrap()).is_err());
    std::fs::set_permissions(plugin.root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(clients::opencode::validate_endpoint(plugin.path.to_str().unwrap()).is_err());
    std::fs::set_permissions(plugin.root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    plugin.finish();
}

#[tokio::test]
async fn paused_broker_expires_crashed_leases_and_queued_deadlines_without_native_events() {
    let plugin = PluginSocket::start(1, receipt);
    let state = plugin.root.path().join("expiry-state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    let agent = plugin.agent();
    broker::rpc(
        &state,
        &Request::OpenCodeObserve {
            session: SESSION.into(),
            endpoint: agent.endpoint.clone(),
            title: None,
            ready: true,
        },
    )
    .await
    .unwrap();
    broker::rpc(&state, &Request::Pause { paused: true })
        .await
        .unwrap();
    broker::rpc(
        &state,
        &Request::Send {
            id: ID.into(),
            sender: "user".into(),
            recipient: agent.name.clone(),
            body: "still queued".into(),
            parent: None,
        },
    )
    .await
    .unwrap();
    let conn = rusqlite::Connection::open(state.join("messages.sqlite3")).unwrap();
    conn.execute("UPDATE opencode_leases SET expires=0", [])
        .unwrap();
    conn.execute("UPDATE opencode_deliveries SET deadline=0", [])
        .unwrap();
    // No broker requests or native events can supply the expiry observation.
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let enabled: bool = conn
                .query_row(
                    "SELECT enabled FROM agents WHERE name=?",
                    [&agent.name],
                    |r| r.get(0),
                )
                .unwrap();
            let status: String = conn
                .query_row("SELECT state FROM messages WHERE id=?", [ID], |r| r.get(0))
                .unwrap();
            if !enabled && status == "failed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    server.abort();
    let _ = server.await;
    plugin.finish();
}

async fn wait_message_state(broker: &Broker, id: &str, state: &str) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let message = broker.request(Request::Get { id: id.into() }).unwrap();
            if message["state"] == state {
                break message;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "Expected message {id} state {state}; current {:?}",
            broker.request(Request::Get { id: id.into() })
        )
    })
}

#[derive(Default)]
struct EventNativeStorage {
    frames: Vec<Value>,
    inserted: Vec<Value>,
    lost_event: Option<Value>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_uncertain_event_recovery_is_explicit_readonly_and_preserves_restart_barriers() {
    use agent_bridge::protocol::EventSelector;
    use std::sync::{Arc, Mutex};
    for proof_available in [true, false] {
        let root = tempfile::Builder::new()
            .prefix("ab-event-recovery-")
            .tempdir_in("/tmp")
            .unwrap();
        let state = root.path().join("state");
        let native = Arc::new(Mutex::new(EventNativeStorage::default()));
        let storage = native.clone();
        let broker_state = state.clone();
        // Two verified probes (before/after broker restart), one lost event,
        // explicit reconciliation, and (with proof) ordinary + lifecycle tails.
        let plugin = PluginSocket::start_optional(
            if proof_available { 6 } else { 4 },
            move |frame, instance| {
                if frame["op"] == "probe" {
                    return Some(receipt(frame, instance));
                }
                assert!(frame["op"] == "deliver" || frame["op"] == "reconcile");
                if frame["op"] == "deliver" {
                    // Exercise the same privileged broker admission RPC as the real
                    // plugin before native insertion. Reconciliation never calls it.
                    let admitted = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap()
                        .block_on(broker::rpc(
                            &broker_state,
                            &Request::OpenCodeAdmit {
                                id: frame["id"].as_str().unwrap().into(),
                                session: SESSION.into(),
                                generation: frame["generation"].as_str().unwrap().into(),
                                instance: instance.into(),
                                attempt: frame["attempt"].as_str().unwrap().into(),
                            },
                        ))
                        .unwrap();
                    assert_eq!(admitted["admitted"], true);
                    let mut native = storage.lock().unwrap();
                    assert!(
                        !native
                            .inserted
                            .iter()
                            .any(|old| old["native_message_id"] == frame["native_message_id"]),
                        "No native input may be inserted twice"
                    );
                    native.inserted.push(frame.clone());
                    native.frames.push(frame.clone());
                    if native.lost_event.is_none() {
                        assert!(frame["content"].as_str().unwrap().contains("agent.opened"));
                        native.lost_event = Some(frame);
                        // The native insert happened, but the connection closes
                        // without its receipt: the real adapter must mark uncertain.
                        return None;
                    }
                    return Some(receipt(frame, instance));
                }
                let mut native = storage.lock().unwrap();
                let original = native.lost_event.as_ref().unwrap();
                for key in ["id", "native_message_id", "session", "content", "namespace"] {
                    assert_eq!(
                        frame[key], original[key],
                        "Reconciliation must retain {key}"
                    );
                }
                native.frames.push(frame.clone());
                if proof_available
                    && native.inserted.iter().any(|old| {
                        ["native_message_id", "session", "content", "namespace"]
                            .into_iter()
                            .all(|key| old[key] == frame[key])
                    })
                {
                    Some(receipt(frame, instance))
                } else {
                    let mut missing = receipt(frame, instance);
                    missing.as_object_mut().unwrap().remove("status");
                    missing["ok"] = json!(false);
                    missing["uncertain"] = json!(true);
                    missing["error"] = json!("native proof is unavailable");
                    Some(missing)
                }
            },
        );
        let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
        let mut broker = bound.broker.clone();
        let mut server = tokio::spawn(broker::serve(bound));
        let observe = || Request::OpenCodeObserve {
            session: SESSION.into(),
            endpoint: plugin.path.to_str().unwrap().into(),
            title: Some("event recipient".into()),
            ready: true,
        };
        broker.request_async(observe()).await.unwrap();
        let route = format!("opencode-{SESSION}");
        let subscribe = || Request::Subscribe {
            agent: route.clone(),
            events: vec![EventSelector::AgentOpened {}],
        };
        broker.request(subscribe()).unwrap();
        broker
            .request(Request::Register {
                agent: Agent {
                    name: "first-worker".into(),
                    provider: Provider::Inbox,
                    session: "first-worker".into(),
                    endpoint: String::new(),
                    enabled: true,
                },
                replace: false,
            })
            .unwrap();
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Some(frame) = native.lock().unwrap().lost_event.clone() {
                    break frame;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let event_id = event["id"].as_str().unwrap().to_owned();
        wait_message_state(&broker, &event_id, "uncertain").await;
        let tail_id = uuid::Uuid::new_v4().to_string();
        broker
            .request(Request::Send {
                id: tail_id.clone(),
                sender: "user".into(),
                recipient: route.clone(),
                body: "ordinary tail after uncertain control".into(),
                parent: None,
            })
            .unwrap();
        // Resubscribe exposes an ID but cannot implicitly reconcile or free FIFO.
        let recovery = broker.request(subscribe()).unwrap()["recovery"].clone();
        assert_eq!(recovery["total"], 1);
        assert_eq!(recovery["items"][0]["id"], event_id);
        assert_eq!(recovery["items"][0]["state"], "uncertain");
        assert_eq!(recovery["items"][0]["reconciliation_pending"], false);
        assert_eq!(recovery["items"][0]["lease_available"], true);
        assert!(
            recovery["instruction"]
                .as_str()
                .unwrap()
                .contains("allow_duplicate=false")
        );
        assert_eq!(
            broker
                .request(Request::History {
                    agent: None,
                    limit: 100
                })
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        tokio::time::sleep(std::time::Duration::from_millis(350)).await;
        assert_eq!(
            native.lock().unwrap().frames.len(),
            1,
            "Snapshot must not run reconciliation"
        );
        assert_eq!(
            broker
                .request(Request::Get {
                    id: tail_id.clone()
                })
                .unwrap()["state"],
            "queued"
        );
        broker.request(Request::Pause { paused: true }).unwrap();
        broker
            .request(Request::Retry {
                id: event_id.clone(),
                allow_duplicate: false,
            })
            .unwrap();
        assert!(
            broker
                .request(Request::Cancel {
                    id: event_id.clone()
                })
                .is_err(),
            "Attempted reconciliation cannot escape uncertainty through cancel"
        );
        let removed = broker
            .request(Request::Unsubscribe {
                agent: route.clone(),
                events: None,
            })
            .unwrap();
        assert_eq!(
            removed["recovery"]["items"][0]["reconciliation_pending"],
            true
        );
        assert_eq!(
            broker
                .request(Request::Get {
                    id: event_id.clone()
                })
                .unwrap()["state"],
            "queued"
        );
        // Restart while explicitly requested read-only work is queued. Old
        // subscription revocation cannot erase its attempted provenance/barrier.
        server.abort();
        let _ = server.await;
        drop(broker);
        // The plugin may return after the old lease expired; stored queued
        // reconciliation must stay read-only while an explicit retry rebinds it.
        rusqlite::Connection::open(state.join("messages.sqlite3"))
            .unwrap()
            .execute(
                "UPDATE opencode_leases SET expires=0 WHERE session=?",
                [SESSION],
            )
            .unwrap();
        let rebound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
        broker = rebound.broker.clone();
        server = tokio::spawn(broker::serve(rebound));
        assert_eq!(
            broker
                .request(Request::Get {
                    id: event_id.clone()
                })
                .unwrap()["state"],
            "queued"
        );
        broker.request_async(observe()).await.unwrap();
        assert_eq!(
            broker.request(subscribe()).unwrap()["recovery"]["items"][0]["reconciliation_pending"],
            true
        );
        broker
            .request(Request::Retry {
                id: event_id.clone(),
                allow_duplicate: false,
            })
            .unwrap();
        // Existing lease rules correctly retire proven-unattempted ordinary
        // work on expiry; explicitly retry that tail against its same identity.
        assert_eq!(
            broker
                .request(Request::Get {
                    id: tail_id.clone()
                })
                .unwrap()["state"],
            "failed"
        );
        broker
            .request(Request::Retry {
                id: tail_id.clone(),
                allow_duplicate: false,
            })
            .unwrap();
        broker
            .request(Request::Register {
                agent: Agent {
                    name: "fresh-worker".into(),
                    provider: Provider::Inbox,
                    session: "fresh-worker".into(),
                    endpoint: String::new(),
                    enabled: true,
                },
                replace: false,
            })
            .unwrap();
        broker.request(Request::Pause { paused: false }).unwrap();
        if proof_available {
            wait_message_state(&broker, &event_id, "submitted").await;
            wait_message_state(&broker, &tail_id, "submitted").await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if native.lock().unwrap().inserted.len() == 3 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            let native = native.lock().unwrap();
            assert_eq!(
                native
                    .frames
                    .iter()
                    .filter(|frame| frame["op"] == "reconcile")
                    .count(),
                1
            );
            assert!(
                native.inserted[1]["content"]
                    .as_str()
                    .unwrap()
                    .contains("ordinary tail after uncertain control")
            );
            assert!(
                native.inserted[2]["content"]
                    .as_str()
                    .unwrap()
                    .contains("fresh-worker")
            );
        } else {
            wait_message_state(&broker, &event_id, "uncertain").await;
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
            assert_eq!(
                native.lock().unwrap().inserted.len(),
                1,
                "Missing proof never inserts a replacement or any tail"
            );
            assert_eq!(
                broker
                    .request(Request::Get {
                        id: tail_id.clone()
                    })
                    .unwrap()["state"],
                "queued"
            );
        }
        server.abort();
        let _ = server.await;
        drop(broker);
        plugin.finish();
    }
}
