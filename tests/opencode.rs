mod support;

use agent_bridge::{
    broker::{self, Broker},
    clients::{self, ConnectorState, StateError},
    delivery,
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
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    worker: std::thread::JoinHandle<()>,
}
impl PluginSocket {
    fn start(response: impl Fn(Value, &str) -> Value + Send + 'static) -> Self {
        Self::start_optional(move |frame, instance| Some(response(frame, instance)))
    }
    fn start_optional(response: impl Fn(Value, &str) -> Option<Value> + Send + 'static) -> Self {
        let root = tempfile::Builder::new()
            .prefix("ab-oc-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        let path = root.path().join(format!("{instance}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        listener.set_nonblocking(true).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let worker = std::thread::spawn(move || {
            while !stopping.load(std::sync::atomic::Ordering::SeqCst) {
                let mut stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(_) => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                };
                stream.set_nonblocking(false).unwrap();
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
                    // The client may legitimately be gone: oversized-receipt tests
                    // make it close at its frame bound, and a test that has seen
                    // its outcome aborts the broker while a follow-up reconcile
                    // attempt is still in flight. Anything else is a real failure.
                    assert!(
                        matches!(
                            error.kind(),
                            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                        ),
                        "Unexpected fixture write failure: {error}"
                    );
                }
            }
        });
        Self {
            root,
            path,
            stop,
            worker,
        }
    }
    /// Publish this socket in a broker's fixed plugin socket directory.
    fn publish(&self, state: &std::path::Path) {
        let dir = clients::opencode::socket_dir(state);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::hard_link(&self.path, dir.join(self.path.file_name().unwrap()));
    }
    fn agent(&self) -> Agent {
        Agent {
            name: format!("opencode-{SESSION}"),
            provider: Provider::OpenCode,
            session: SESSION.into(),
            endpoint: self.path.to_str().unwrap().into(),
        }
    }
    fn finish(self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        self.worker.join().unwrap();
    }
}
fn receipt(frame: Value, instance: &str) -> Value {
    if frame["op"] == "state_get" || frame["op"] == "state_list" {
        let row = state_row(SESSION, Value::Null);
        return state_reply(
            &frame,
            instance,
            if frame["op"] == "state_get" {
                row
            } else {
                json!([row])
            },
        );
    }
    if frame["session"] != SESSION {
        return json!({"ok":false,"error":"unknown session"});
    }
    let mut value = json!({"ok":true,"protocol":2,"session":SESSION,"instance":instance});
    if frame["op"] == "deliver" || frame["op"] == "reconcile" {
        assert!(!frame["content"].as_str().unwrap().is_empty());
        assert!(frame.get("permissions").is_none());
        assert!(frame.get("tools").is_none());
        value["id"] = frame["id"].clone();
        for key in ["native_message_id", "namespace", "attempt"] {
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
    plugin.publish(&state);
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
            if msg.state != "pending" {
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
    let plugin = PluginSocket::start(receipt);
    let outcome = broker_delivery(&plugin).await;
    assert_eq!(outcome.state, "delivered", "{:?}", outcome.detail);
    let detail = outcome.detail.unwrap();
    assert!(detail.contains("exact native client acceptance verified"));
    assert!(
        detail.contains("does not prove transcript consumption, model reading or task completion")
    );
    plugin.finish();
}

#[tokio::test]
async fn wrong_instance_session_and_message_receipts_never_submit() {
    for field in ["instance", "session", "id", "native_message_id"] {
        let plugin = PluginSocket::start(move |frame, instance| {
            if frame["op"] == "state_get" || frame["op"] == "state_list" {
                return receipt(frame, instance);
            }
            let mut value = receipt(frame, instance);
            value[field] = json!("wrong");
            value
        });
        let outcome = broker_delivery(&plugin).await;
        assert_eq!(outcome.state, "unknown");
        plugin.finish();
    }
    let plugin = PluginSocket::start(receipt);
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

#[test]
fn native_state_pages_use_only_updated_age_and_exact_lookup_ignores_age() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let plugin = PluginSocket::start(move |frame, instance| {
        let old = state_row(SESSION, json!(now - 15 * 86_400_000));
        if frame["op"] == "state_get" {
            state_reply(&frame, instance, old)
        } else if frame["cursor"].is_null() {
            let mut reply = state_reply(&frame, instance, json!([old]));
            reply["next_cursor"] = json!("second");
            reply
        } else {
            assert_eq!(frame["cursor"], "second");
            state_reply(&frame, instance, json!([state_row(OTHER, Value::Null)]))
        }
    });
    let (_dir, state) = state_for(&plugin);
    assert_eq!(state.provider(), Provider::OpenCode);
    let recent = state.list(7).unwrap();
    assert_eq!(recent.len(), 1);
    assert_eq!(recent[0].session, OTHER);
    assert_eq!(state.list(30).unwrap().len(), 2);
    let exact = state.get(SESSION).unwrap().unwrap();
    assert_eq!(exact.session, SESSION);
    assert!(
        exact
            .transport_endpoint
            .unwrap()
            .ends_with(plugin.path.file_name().unwrap().to_str().unwrap())
    );
    plugin.finish();
}
/// A scratch state directory whose socket directory holds this plugin socket.
fn state_for(plugin: &PluginSocket) -> (tempfile::TempDir, clients::opencode::State) {
    let dir = tempfile::tempdir().unwrap();
    plugin.publish(dir.path());
    let state = clients::opencode::State::new(dir.path());
    (dir, state)
}
fn state_row(session: &str, updated: Value) -> Value {
    json!({"session":session,"display_name":"same title","status":null,"directory":"/cold/location","updated_ms":updated})
}
fn state_reply(frame: &Value, instance: &str, rows: Value) -> Value {
    let mut reply = json!({"ok":true,"protocol":2,"instance":instance,"op":frame["op"]});
    if frame["op"] == "state_get" {
        reply["session"] = frame["session"].clone();
        reply["state"] = rows;
    } else {
        reply["sessions"] = rows;
        reply["next_cursor"] = Value::Null;
    }
    reply
}

#[test]
fn native_state_strict_identity_and_required_metadata_reject_receipts_and_cross_scope_rows() {
    for variant in 0..9 {
        let plugin = PluginSocket::start(move |frame, instance| {
            let mut value = state_reply(&frame, instance, state_row(SESSION, Value::Null));
            match variant {
                0 => value["instance"] = json!(uuid::Uuid::new_v4().to_string()),
                1 => value["protocol"] = json!(1),
                2 => value["op"] = json!("state_list"),
                3 => value["session"] = json!(OTHER),
                4 => value["state"]["session"] = json!(OTHER),
                5 => {
                    value["state"]
                        .as_object_mut()
                        .unwrap()
                        .remove("display_name");
                }
                6 => value["state"]["transport_endpoint"] = json!("/forged.sock"),
                7 => value["status"] = json!("inserted"),
                _ => return json!({"ok":true,"protocol":2,"session":SESSION,"instance":instance}),
            }
            value
        });
        let (_dir, state) = state_for(&plugin);
        let got = state.get(SESSION);
        assert!(
            matches!(got, Err(StateError::Incomplete(_))),
            "variant {variant}: {got:?}"
        );
        plugin.finish();
    }
}

#[test]
fn native_state_partial_pages_cycles_overflow_and_native_errors_are_not_empty_success() {
    for variant in 0..4 {
        let plugin = PluginSocket::start(move |frame, instance| {
            if variant == 3 {
                return json!({"ok":false,"protocol":2,"instance":instance,"op":frame["op"],"error":"unavailable"});
            }
            let mut reply = state_reply(&frame, instance, json!([]));
            if variant == 2 {
                reply["sessions"] = json!([state_row(SESSION, Value::Null)]);
                reply["sessions"][0]["display_name"] =
                    json!("x".repeat(agent_bridge::protocol::MAX_FRAME));
            } else if frame["cursor"].is_null() || variant == 0 {
                reply["next_cursor"] = json!("same");
            } else {
                reply = json!({"ok":false,"protocol":2,"instance":instance,"op":frame["op"],"error":"unavailable"});
            }
            reply
        });
        let (_dir, state) = state_for(&plugin);
        let result = state.list(7);
        assert!(if variant == 3 {
            matches!(result, Err(StateError::Unavailable(_)))
        } else {
            matches!(result, Err(StateError::Incomplete(_)))
        });
        plugin.finish();
    }
}

#[test]
fn native_state_session_and_page_limits_never_return_partial_directories() {
    for count in [4096, 4097] {
        let plugin = PluginSocket::start(move |frame, instance| {
            let start: usize = frame["cursor"]
                .as_str()
                .map(|s| s.parse().unwrap())
                .unwrap_or(0);
            let end = (start + 100).min(count);
            let rows: Vec<_> = (start..end)
                .map(|n| state_row(&format!("ses_{n:026}"), Value::Null))
                .collect();
            let mut reply = state_reply(&frame, instance, json!(rows));
            if end < count {
                reply["next_cursor"] = json!(end.to_string());
            }
            reply
        });
        let result = {
            let (_dir, state) = state_for(&plugin);
            state.list(7)
        };
        if count == 4096 {
            assert_eq!(result.unwrap().len(), count);
        } else {
            assert!(matches!(result, Err(StateError::Incomplete(_))));
        }
        plugin.finish();
    }
    let plugin = PluginSocket::start(|frame, instance| {
        let next = frame["cursor"]
            .as_str()
            .map(|s| s.parse::<usize>().unwrap())
            .unwrap_or(0)
            + 1;
        let mut reply = state_reply(&frame, instance, json!([]));
        reply["next_cursor"] = json!(next.to_string());
        reply
    });
    assert!(matches!(
        {
            let (_dir, state) = state_for(&plugin);
            state.list(7)
        },
        Err(StateError::Incomplete(_))
    ));
    plugin.finish();
}

#[test]
fn native_state_total_deadline_bounds_a_stalled_native_reply() {
    let plugin = PluginSocket::start_optional(|_, _| {
        std::thread::sleep(std::time::Duration::from_millis(5100));
        None
    });
    let start = std::time::Instant::now();
    assert!(matches!(
        {
            let (_dir, state) = state_for(&plugin);
            state.list(7)
        },
        Err(StateError::Unavailable(_))
    ));
    assert!(start.elapsed() < std::time::Duration::from_secs(6));
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
        let plugin = PluginSocket::start(move |frame, instance| {
            if frame["op"] == "state_get" || frame["op"] == "state_list" {
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
        assert_eq!(outcome.state, "unknown");
        plugin.finish();
    }
}

#[test]
fn opencode_endpoint_requires_private_socket_and_canonical_instance() {
    let plugin = PluginSocket::start(receipt);
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_push_opencode_admission_fences_old_epoch_but_not_accepted_work() {
    use agent_bridge::topics::{AgentRef, TopicRef};
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    // In the first run the fixture pauses before the actual broker admission
    // RPC. In the second it pauses after admission, before native acceptance.
    for admit_first in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        let broker_state = state.clone();
        let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
        let reached_tx = Mutex::new(Some(reached_tx));
        let (resume_tx, resume_rx) = mpsc::channel();
        let inserted = Arc::new(Mutex::new(Vec::<String>::new()));
        let native = inserted.clone();
        let plugin = PluginSocket::start(move |frame, instance| {
            if frame["op"] == "state_get" || frame["op"] == "state_list" {
                return receipt(frame, instance);
            }
            assert_eq!(frame["op"], "deliver");
            let admit = || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(broker::rpc(
                        &broker_state,
                        &Request::OpenCodeAdmit {
                            id: frame["id"].as_str().unwrap().into(),
                            session: SESSION.into(),
                            attempt: frame["attempt"].as_str().unwrap().into(),
                        },
                    ))
            };
            let early = admit_first.then(&admit);
            reached_tx
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .send(frame.clone())
                .unwrap();
            resume_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            let admission = early.unwrap_or_else(admit);
            if admit_first {
                assert_eq!(admission.unwrap()["admitted"], true);
                native
                    .lock()
                    .unwrap()
                    .push(frame["id"].as_str().unwrap().into());
                receipt(frame, instance)
            } else {
                assert!(
                    admission.is_err(),
                    "old epoch must be denied after resubscription"
                );
                let mut deferred = receipt(frame, instance);
                deferred["ok"] = json!(false);
                deferred["status"] = json!("deferred");
                deferred["error"] =
                    json!("Broker admission was unavailable; no native attempt was made");
                deferred
            }
        });
        let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        let server = tokio::spawn(broker::serve(bound));
        plugin.publish(&state);
        let topic: TopicRef = serde_json::from_value(
            broker
                .request(Request::TopicCreate {
                    name: "Admission race".into(),
                })
                .unwrap()["reference"]
                .clone(),
        )
        .unwrap();
        let agent = AgentRef {
            bridge_id: None,
            agent_id: format!("opencode-{SESSION}"),
        };
        let set = |mode: &str| Request::TopicPushSet {
            topic: topic.clone(),
            agent: agent.clone(),
            mode: mode.into(),
        };
        broker.request(set("all")).unwrap();
        broker
            .request(Request::TopicPublish {
                topic: topic.clone(),
                id: uuid::Uuid::new_v4().to_string(),
                author: AgentRef {
                    bridge_id: None,
                    agent_id: "inbox-user".into(),
                },
                body: "gated topic push".into(),
                parent: None,
                mentions: vec![],
            })
            .unwrap();
        let frame = tokio::time::timeout(Duration::from_secs(5), reached_rx)
            .await
            .unwrap()
            .unwrap();
        let id = frame["id"].as_str().unwrap().to_owned();
        broker.request(set("manual")).unwrap();
        broker.request(set("all")).unwrap();
        resume_tx.send(()).unwrap();
        wait_message_state(
            &broker,
            &id,
            if admit_first { "delivered" } else { "failed" },
        )
        .await;
        assert_eq!(inserted.lock().unwrap().len(), usize::from(admit_first));
        server.abort();
        let _ = server.await;
        plugin.finish();
    }
}

#[derive(Default)]
struct EventNativeStorage {
    frames: Vec<Value>,
    inserted: Vec<Value>,
    lost_event: Option<Value>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hidden_uncertain_event_recovery_is_explicit_readonly_and_allows_later_delivery() {
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
        // Two probes, the lost event, an ordinary tail, explicit readback and
        // a new board event. Missing proof must not block the latter two.
        let plugin = PluginSocket::start_optional(move |frame, instance| {
            if frame["op"] == "state_get" || frame["op"] == "state_list" {
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
                    assert!(frame["content"].as_str().unwrap().contains("board.changed"));
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
        });
        let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
        let mut broker = bound.broker.clone();
        let mut server = tokio::spawn(broker::serve(bound));
        plugin.publish(&state);
        let route = format!("opencode-{SESSION}");
        let subscribe = || Request::Subscribe {
            board_revisions: Default::default(),
            agent: route.clone(),
            events: vec![EventSelector::BoardChanged {
                board: Default::default(),
            }],
        };
        broker.request(subscribe()).unwrap();
        let board = broker.request(Request::BoardRead).unwrap();
        broker
            .request(Request::board_update(
                Default::default(),
                board["revision"].as_str().unwrap().into(),
                vec![agent_bridge::board::Edit::Append {
                    text: "\nboard event\n".into(),
                }],
                None,
            ))
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
        wait_message_state(&broker, &event_id, "unknown").await;
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
        // Later ordinary input may proceed, but resubscribe must not reconcile
        // or replay the original uncertain control.
        wait_message_state(&broker, &tail_id, "delivered").await;
        let recovery = broker.request(subscribe()).unwrap()["recovery"].clone();
        assert_eq!(recovery["total"], 1);
        assert_eq!(recovery["items"][0]["id"], event_id);
        assert_eq!(recovery["items"][0]["state"], "unknown");
        assert_eq!(recovery["items"][0]["reconciliation_pending"], false);
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
        let frames = native.lock().unwrap().frames.clone();
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["op"] == "reconcile")
                .count(),
            0,
            "Snapshot must not run reconciliation"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["id"] == event_id && frame["op"] == "deliver")
                .count(),
            1,
            "Unknown control must not be replayed"
        );
        assert_eq!(
            frames
                .iter()
                .filter(|frame| frame["id"] == tail_id && frame["op"] == "deliver")
                .count(),
            1,
            "Ordinary tail must arrive once"
        );
        assert_eq!(
            broker
                .request(Request::Get {
                    id: tail_id.clone()
                })
                .unwrap()["state"],
            "delivered"
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
                board_revisions: Default::default(),
                agent: route.clone(),
                events: None,
            })
            .unwrap();
        assert_eq!(
            removed["recovery"]["items"][0]["reconciliation_pending"],
            true
        );
        assert_eq!(removed["recovery"]["items"][0]["state"], "unknown");
        assert_eq!(
            broker
                .request(Request::Get {
                    id: event_id.clone()
                })
                .unwrap()["state"],
            "unknown"
        );
        // Restart while explicitly requested read-only work is queued. Old
        // subscription revocation cannot erase its attempted provenance.
        server.abort();
        let _ = server.await;
        drop(broker);
        support::wait_for_shutdown(&state).await;
        let rebound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
        broker = rebound.broker.clone();
        server = tokio::spawn(broker::serve(rebound));
        assert_eq!(
            broker
                .request(Request::Get {
                    id: event_id.clone()
                })
                .unwrap()["state"],
            "unknown"
        );
        let resumed = broker.request(subscribe()).unwrap();
        assert_eq!(
            resumed["recovery"]["items"][0]["reconciliation_pending"],
            true
        );
        assert_eq!(resumed["recovery"]["items"][0]["state"], "unknown");
        broker
            .request(Request::Retry {
                id: event_id.clone(),
                allow_duplicate: false,
            })
            .unwrap();
        // The ordinary tail already reached the native session before restart.
        assert_eq!(
            broker
                .request(Request::Get {
                    id: tail_id.clone()
                })
                .unwrap()["state"],
            "delivered"
        );
        broker.request(Request::Pause { paused: false }).unwrap();
        if proof_available {
            wait_message_state(&broker, &event_id, "delivered").await;
            wait_message_state(&broker, &tail_id, "delivered").await;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if native.lock().unwrap().inserted.len() == 2 {
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
        } else {
            // The public Unknown state also covers queued/in-flight readback.
            // Wait for this readback's own diagnostic before checking its
            // final outcome, rather than accepting the earlier Unknown state.
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let event = broker
                        .request(Request::Get {
                            id: event_id.clone(),
                        })
                        .unwrap();
                    if event["detail"]
                        .as_str()
                        .is_some_and(|detail| detail.contains("unconfirmed plugin receipt"))
                    {
                        assert_eq!(event["state"], "unknown");
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if native.lock().unwrap().inserted.len() == 2 {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                native.lock().unwrap().inserted.len(),
                2,
                "Missing proof does not replay the original and later input proceeds"
            );
            assert_eq!(
                broker
                    .request(Request::Get {
                        id: tail_id.clone()
                    })
                    .unwrap()["state"],
                "delivered"
            );
        }
        {
            let native = native.lock().unwrap();
            assert_eq!(
                native
                    .frames
                    .iter()
                    .filter(|frame| frame["op"] == "reconcile" && frame["id"] == event_id)
                    .count(),
                1
            );
            assert_eq!(
                native
                    .frames
                    .iter()
                    .filter(|frame| frame["op"] == "deliver" && frame["id"] == event_id)
                    .count(),
                1
            );
            assert_eq!(
                native
                    .frames
                    .iter()
                    .filter(|frame| frame["op"] == "deliver" && frame["id"] == tail_id)
                    .count(),
                1
            );
        }
        server.abort();
        let _ = server.await;
        drop(broker);
        plugin.finish();
    }
}

async fn list_opencode(state: &std::path::Path) -> Value {
    broker::rpc(
        state,
        &Request::List {
            all: false,
            max_age_days: 7,
        },
    )
    .await
    .unwrap()
}

fn row_status(frame: Value, instance: &str, status: Value) -> Value {
    let mut row = state_row(SESSION, Value::Null);
    row["status"] = status;
    state_reply(
        &frame,
        instance,
        if frame["op"] == "state_get" {
            row
        } else {
            json!([row])
        },
    )
}

#[tokio::test]
async fn plugin_without_any_registration_is_listed_with_its_status_and_deliverable() {
    let plugin = PluginSocket::start(|frame, instance| {
        if frame["op"] == "state_get" || frame["op"] == "state_list" {
            row_status(frame, instance, json!("busy"))
        } else {
            receipt(frame, instance)
        }
    });
    let state = plugin.root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    plugin.publish(&state);
    let listed = list_opencode(&state).await;
    let row = listed["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["session"] == SESSION)
        .unwrap_or_else(|| panic!("{listed}"));
    assert_eq!(row["status"], "busy");
    broker::rpc(
        &state,
        &Request::Send {
            id: ID.into(),
            sender: "user".into(),
            recipient: format!("opencode-{SESSION}"),
            body: "test".into(),
            parent: None,
        },
    )
    .await
    .unwrap();
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let message: Message = serde_json::from_value(
                broker::rpc(&state, &Request::Get { id: ID.into() })
                    .await
                    .unwrap(),
            )
            .unwrap();
            if message.state == "delivered" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        delivered.is_ok(),
        "no registration step may precede delivery"
    );
    server.abort();
    plugin.finish();
}

#[tokio::test]
async fn refused_socket_is_ignored_and_unlinked_and_a_hung_plugin_is_bounded() {
    let live = PluginSocket::start(receipt);
    let hung = PluginSocket::start_optional(|_, _| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        None
    });
    let state = live.root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    live.publish(&state);
    hung.publish(&state);
    let dead = clients::opencode::socket_dir(&state).join(format!("{}.sock", uuid::Uuid::new_v4()));
    drop(UnixListener::bind(&dead).unwrap());
    std::fs::set_permissions(&dead, std::fs::Permissions::from_mode(0o600)).unwrap();
    let started = std::time::Instant::now();
    let listed = list_opencode(&state).await;
    assert!(started.elapsed() < std::time::Duration::from_millis(2500));
    assert!(
        listed["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["session"] == SESSION)
    );
    assert!(!dead.exists(), "a refused socket is unlinked");
    assert!(
        live.path.exists()
            && clients::opencode::socket_dir(&state)
                .join(hung.path.file_name().unwrap())
                .exists(),
        "live and hung sockets are never unlinked"
    );
    server.abort();
    live.finish();
    hung.finish();
}

#[tokio::test]
async fn failed_send_names_the_unreachable_side() {
    let plugin = PluginSocket::start(receipt);
    let state = plugin.root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    plugin.publish(&state);
    let send = |sender: String, recipient: String| {
        let state = state.clone();
        async move {
            broker::rpc(
                &state,
                &Request::Send {
                    id: uuid::Uuid::new_v4().to_string(),
                    sender,
                    recipient,
                    body: "test".into(),
                    parent: None,
                },
            )
            .await
            .unwrap_err()
            .to_string()
        }
    };
    let gone = format!("opencode-{OTHER}");
    let live = format!("opencode-{SESSION}");
    assert!(
        send("user".into(), gone.clone())
            .await
            .contains(&format!("Recipient {gone} is not reachable"))
    );
    assert!(
        send(gone.clone(), live)
            .await
            .contains(&format!("Sender {gone} is not reachable"))
    );
    server.abort();
    plugin.finish();
}
