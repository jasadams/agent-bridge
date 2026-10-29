use agent_bridge::{
    broker::{self, Broker},
    protocol::{Agent, EventSelector, Provider, Request},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::{io::AsyncReadExt, net::UnixListener, time::timeout};

fn inbox(name: &str) -> Agent {
    Agent {
        name: name.into(),
        provider: Provider::Inbox,
        session: name.into(),
        endpoint: String::new(),
        enabled: true,
    }
}
fn subscribe(agent: &str, events: Vec<EventSelector>) -> Request {
    Request::Subscribe {
        agent: agent.into(),
        events,
    }
}
async fn notice(listener: &UnixListener) -> Value {
    timeout(Duration::from_secs(3), async {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            timeout(Duration::from_secs(2), socket.read_to_end(&mut bytes))
                .await
                .unwrap()
                .unwrap();
            // Native liveness checks connect without sending a control frame.
            if bytes.is_empty() {
                continue;
            }
            let frame: Value = serde_json::from_slice(&bytes).unwrap();
            let text = frame["message"]["content"].as_str().unwrap();
            assert!(!text.contains("cross-session-message"));
            break serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn actual_broker_claude_control_preserves_quick_close_reopen_and_history() {
    let root = tempfile::Builder::new()
        .prefix("ab-events-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone();
    let path = state.join("claude.sock");
    let listener = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let receiver = Agent {
        name: "coordinator".into(),
        provider: Provider::Claude,
        session: uuid::Uuid::new_v4().to_string(),
        endpoint: path.to_str().unwrap().into(),
        enabled: true,
    };
    broker
        .request(Request::Register {
            agent: receiver.clone(),
            replace: false,
        })
        .unwrap();
    broker
        .request(Request::Register {
            agent: inbox("existing"),
            replace: false,
        })
        .unwrap();
    let events = vec![EventSelector::AgentOpened {}, EventSelector::AgentClosed {}];
    let snapshot = broker
        .request(subscribe("coordinator", events.clone()))
        .unwrap();
    assert_eq!(snapshot["agents"].as_array().unwrap().len(), 2);
    assert!(
        snapshot["agents"]
            .as_array()
            .unwrap()
            .iter()
            .all(|agent| agent.get("endpoint").is_none() && agent["route"] != "user")
    );
    let task = tokio::spawn(broker::serve(bound));
    broker
        .request(Request::Register {
            agent: inbox("worker"),
            replace: false,
        })
        .unwrap();
    broker
        .request(Request::Unregister {
            name: "worker".into(),
            session: "worker".into(),
        })
        .unwrap();
    broker
        .request(Request::Register {
            agent: inbox("worker"),
            replace: false,
        })
        .unwrap();
    let mut received = Vec::new();
    for _ in 0..3 {
        received.push(notice(&listener).await);
    }
    assert_eq!(
        received
            .iter()
            .map(|event| event["event"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["agent.opened", "agent.closed", "agent.opened"]
    );
    assert!(
        received
            .iter()
            .all(|event| event["provider"] == "inbox" && event["session"] == "worker")
    );
    assert!(
        broker
            .request(Request::History {
                agent: None,
                limit: 100
            })
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
    broker.request(subscribe("coordinator", events)).unwrap();
    broker
        .request(Request::Register {
            agent: inbox("worker"),
            replace: false,
        })
        .unwrap();
    assert!(
        timeout(Duration::from_millis(400), notice(&listener))
            .await
            .is_err()
    );
    broker
        .request(Request::Unsubscribe {
            agent: "coordinator".into(),
            events: None,
        })
        .unwrap();
    broker
        .request(Request::Unregister {
            name: "worker".into(),
            session: "worker".into(),
        })
        .unwrap();
    assert!(
        timeout(Duration::from_millis(400), notice(&listener))
            .await
            .is_err()
    );
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn selectors_are_strict_and_failed_changes_are_atomic() {
    for bad in [
        json!({"event":"agent.opened","board":"main"}),
        json!({"event":"board.changed"}),
        json!({"event":"unknown"}),
    ] {
        assert!(serde_json::from_value::<EventSelector>(bad).is_err());
    }
    for op in [
        "board_subscribe",
        "named_board_subscribe",
        "board_unsubscribe",
        "named_board_unsubscribe",
    ] {
        assert!(
            serde_json::from_value::<Request>(
                json!({"op":op,"agent":"coordinator","board":"main"})
            )
            .is_err()
        );
    }
    let root = tempfile::tempdir().unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    let agent = Agent {
        provider: Provider::Codex,
        session: uuid::Uuid::new_v4().to_string(),
        ..inbox("coordinator")
    };
    broker
        .request(Request::Register {
            agent,
            replace: false,
        })
        .unwrap();
    assert!(broker.request(subscribe("coordinator", vec![])).is_err());
    assert!(
        broker
            .request(subscribe(
                "coordinator",
                vec![
                    EventSelector::AgentOpened {},
                    EventSelector::BoardChanged {
                        board: "missing".parse().unwrap()
                    }
                ]
            ))
            .is_err()
    );
    let reply = broker
        .request(Request::Unsubscribe {
            agent: "coordinator".into(),
            events: None,
        })
        .unwrap();
    assert_eq!(reply["events"], json!([]));
    let first = broker
        .request(subscribe(
            "coordinator",
            vec![EventSelector::AgentOpened {}],
        ))
        .unwrap();
    let repeat = broker
        .request(subscribe(
            "coordinator",
            vec![EventSelector::AgentOpened {}],
        ))
        .unwrap();
    assert_eq!(first, repeat);
    let additive = broker
        .request(subscribe(
            "coordinator",
            vec![EventSelector::AgentClosed {}],
        ))
        .unwrap();
    assert_eq!(additive["events"].as_array().unwrap().len(), 2);
    assert!(
        broker
            .request(Request::Unsubscribe {
                agent: "coordinator".into(),
                events: Some(vec![
                    EventSelector::AgentOpened {},
                    EventSelector::BoardChanged {
                        board: "missing".parse().unwrap()
                    }
                ])
            })
            .is_err()
    );
    let reply = broker
        .request(Request::Unsubscribe {
            agent: "coordinator".into(),
            events: Some(vec![EventSelector::AgentOpened {}]),
        })
        .unwrap();
    assert_eq!(reply["events"], json!([{"event":"agent.closed"}]));
}

#[tokio::test]
async fn oversized_lifecycle_snapshot_rejects_the_entire_selector_change() {
    let root = tempfile::tempdir().unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    broker
        .request(Request::Register {
            agent: Agent {
                provider: Provider::Codex,
                session: uuid::Uuid::new_v4().to_string(),
                ..inbox("coordinator")
            },
            replace: false,
        })
        .unwrap();
    // 256 identities are allowed, but this prospective snapshot has 257.
    for index in 0..256 {
        broker
            .request(Request::Register {
                agent: inbox(&format!("worker-{index}")),
                replace: false,
            })
            .unwrap();
    }
    let result = broker.request(subscribe(
        "coordinator",
        vec![
            EventSelector::AgentOpened {},
            EventSelector::BoardChanged {
                board: Default::default(),
            },
        ],
    ));
    assert!(result.unwrap_err().to_string().contains("256 identities"));
    assert_eq!(
        broker
            .request(Request::Unsubscribe {
                agent: "coordinator".into(),
                events: None
            })
            .unwrap()["events"],
        json!([])
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_lifecycle_sources_deduplicate_aliases_and_preserve_confirmed_closures() {
    use std::io::{BufRead, BufReader, Write};
    let root = tempfile::Builder::new()
        .prefix("ab-native-events-")
        .tempdir_in("/tmp")
        .unwrap();
    let state = root.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone();
    let endpoint = state.join("coordinator.sock");
    let listener = UnixListener::bind(&endpoint).unwrap();
    std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
    broker
        .request(Request::Register {
            agent: Agent {
                name: "coordinator".into(),
                provider: Provider::Claude,
                session: uuid::Uuid::new_v4().to_string(),
                endpoint: endpoint.to_str().unwrap().into(),
                enabled: true,
            },
            replace: false,
        })
        .unwrap();
    let codex = uuid::Uuid::new_v4().to_string();
    let absent = uuid::Uuid::new_v4().to_string();
    for session in [&codex, &absent] {
        broker
            .observe_codex(&json!({"id":session,"status":{"type":"idle"},"name":"before restart"}))
            .unwrap();
    }
    broker
        .request(subscribe(
            "coordinator",
            vec![EventSelector::AgentOpened {}, EventSelector::AgentClosed {}],
        ))
        .unwrap();
    let task = tokio::spawn(broker::serve(bound));
    broker
        .reconcile_codex_loaded(&std::collections::HashSet::from([codex.clone()]))
        .unwrap();
    let closed = notice(&listener).await;
    assert_eq!(closed["event"], "agent.closed");
    assert_eq!(closed["session"], absent);
    broker
        .observe_codex(&json!({"id":codex,"status":{"type":"idle"},"name":"renamed"}))
        .unwrap();
    broker
        .request(Request::Register {
            agent: Agent {
                name: "codex-alias".into(),
                provider: Provider::Codex,
                session: codex.clone(),
                endpoint: String::new(),
                enabled: true,
            },
            replace: false,
        })
        .unwrap();
    assert!(
        timeout(Duration::from_millis(350), notice(&listener))
            .await
            .is_err(),
        "Unchanged loaded baseline, rename and alias are not openings"
    );
    broker
        .observe_codex(&json!({"id":codex,"status":{"type":"notLoaded"},"name":"unloaded"}))
        .unwrap();
    let unloaded = notice(&listener).await;
    assert_eq!(unloaded["event"], "agent.closed");
    assert!(
        !broker
            .request(Request::List { all: false })
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["session"] == codex)
    );
    broker
        .observe_codex(&json!({"id":codex,"status":{"type":"idle"},"name":"loaded again"}))
        .unwrap();
    assert_eq!(notice(&listener).await["event"], "agent.opened");
    broker.codex_offline(Some(&codex)).unwrap();
    broker
        .observe_codex(&json!({"id":codex,"status":{"type":"idle"},"name":"reconnected"}))
        .unwrap();
    for event in ["agent.closed", "agent.opened"] {
        let value = notice(&listener).await;
        assert_eq!(value["event"], event);
        assert_eq!(value["session"], codex);
    }
    broker.codex_offline(None).unwrap();
    assert_eq!(notice(&listener).await["event"], "agent.closed");

    // The real OpenCode registration RPC verifies a private native probe endpoint.
    let instance = uuid::Uuid::new_v4().to_string();
    let plugin_path = state.join(format!("{instance}.sock"));
    let plugin = std::os::unix::net::UnixListener::bind(&plugin_path).unwrap();
    std::fs::set_permissions(&plugin_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let session = "ses_0123456789abcdefghijklmnop";
    let plugin_worker = std::thread::spawn(move || {
        for _ in 0..4 {
            let (mut socket, _) = plugin.accept().unwrap();
            let mut line = String::new();
            BufReader::new(socket.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["op"], "probe");
            assert_eq!(request["session"], session);
            writeln!(
                socket,
                "{}",
                json!({"ok":true,"protocol":2,"session":session,"instance":instance})
            )
            .unwrap();
        }
    });
    let observe = || Request::OpenCodeObserve {
        session: session.into(),
        endpoint: plugin_path.to_str().unwrap().into(),
        title: Some("Native OpenCode".into()),
        ready: true,
    };
    let opened = broker.request_async(observe()).await.unwrap();
    assert_eq!(notice(&listener).await["event"], "agent.opened");
    broker.request_async(observe()).await.unwrap();
    assert!(
        timeout(Duration::from_millis(350), notice(&listener))
            .await
            .is_err()
    );
    broker
        .request(Request::OpenCodeClose {
            session: session.into(),
            endpoint: plugin_path.to_str().unwrap().into(),
            generation: opened["generation"].as_str().unwrap().into(),
        })
        .unwrap();
    broker.request_async(observe()).await.unwrap();
    for event in ["agent.closed", "agent.opened"] {
        let value = notice(&listener).await;
        assert_eq!(value["event"], event);
        assert_eq!(value["provider"], "opencode");
    }
    // Expiry and immediate verified reopen share one administration request;
    // neither transition may disappear between periodic polling snapshots.
    rusqlite::Connection::open(state.join("messages.sqlite3"))
        .unwrap()
        .execute(
            "UPDATE opencode_leases SET expires=0 WHERE session=?",
            [session],
        )
        .unwrap();
    broker.request_async(observe()).await.unwrap();
    for event in ["agent.closed", "agent.opened"] {
        let value = notice(&listener).await;
        assert_eq!(value["event"], event);
        assert_eq!(value["provider"], "opencode");
    }
    plugin_worker.join().unwrap();

    // Missed Claude SessionEnd is conservative: the first failed socket probe
    // retains the registration; only a second consecutive failure closes it.
    let worker_path = state.join("worker.sock");
    let worker_socket = UnixListener::bind(&worker_path).unwrap();
    std::fs::set_permissions(&worker_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let claude_session = uuid::Uuid::new_v4().to_string();
    let claude = Agent {
        name: format!("claude-{claude_session}"),
        provider: Provider::Claude,
        session: claude_session.clone(),
        endpoint: worker_path.to_str().unwrap().into(),
        enabled: true,
    };
    broker
        .request(Request::Register {
            agent: claude.clone(),
            replace: false,
        })
        .unwrap();
    assert_eq!(notice(&listener).await["event"], "agent.opened");
    let removed_path = state.join("previous-native.sock");
    // A delayed end hook from a prior endpoint cannot close a replacement
    // registration using the same native UUID. Removed sockets need not exist.
    assert!(
        broker
            .request(Request::SessionClose {
                name: claude.name.clone(),
                session: claude.session.clone(),
                endpoint: removed_path.to_str().unwrap().into()
            })
            .is_err()
    );
    assert!(
        broker
            .request(Request::List { all: true })
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["session"] == claude.session && agent["enabled"] == true)
    );
    let alias_path = state.join("old-alias.sock");
    let alias_socket = UnixListener::bind(&alias_path).unwrap();
    std::fs::set_permissions(&alias_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    broker
        .request(Request::Register {
            agent: Agent {
                name: "old-claude-alias".into(),
                endpoint: alias_path.to_str().unwrap().into(),
                ..claude.clone()
            },
            replace: false,
        })
        .unwrap();
    drop(alias_socket);
    std::fs::remove_file(&alias_path).unwrap();
    broker.reap_claude_sessions().unwrap();
    broker.reap_claude_sessions().unwrap();
    assert!(
        timeout(Duration::from_millis(350), notice(&listener))
            .await
            .is_err(),
        "An old dead alias cannot close the live canonical native session"
    );
    drop(worker_socket);
    std::fs::remove_file(&worker_path).unwrap();
    broker.reap_claude_sessions().unwrap();
    assert!(
        timeout(Duration::from_millis(350), notice(&listener))
            .await
            .is_err()
    );
    broker.reap_claude_sessions().unwrap();
    let closed = notice(&listener).await;
    assert_eq!(closed["event"], "agent.closed");
    assert_eq!(closed["session"], claude_session);
    broker
        .request(Request::SessionClose {
            name: claude.name.clone(),
            session: claude.session.clone(),
            endpoint: claude.endpoint.clone(),
        })
        .unwrap();
    broker.reap_claude_sessions().unwrap();
    assert!(
        timeout(Duration::from_millis(350), notice(&listener))
            .await
            .is_err()
    );
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn snapshot_byte_bound_is_atomic_and_native_titles_are_bounded_for_events() {
    let root = tempfile::tempdir().unwrap();
    let bound = Broker::bind(root.path().join("state"), "/missing/codex".into()).unwrap();
    let broker = &bound.broker;
    broker
        .request(Request::Register {
            agent: Agent {
                provider: Provider::Codex,
                session: uuid::Uuid::new_v4().to_string(),
                ..inbox("coordinator")
            },
            replace: false,
        })
        .unwrap();
    for _ in 0..17 {
        broker.observe_codex(&json!({"id":uuid::Uuid::new_v4().to_string(),"status":{"type":"idle"},"name":"界".repeat(2000)})).unwrap();
    }
    let error = broker
        .request(subscribe(
            "coordinator",
            vec![
                EventSelector::AgentOpened {},
                EventSelector::BoardChanged {
                    board: Default::default(),
                },
            ],
        ))
        .unwrap_err();
    assert!(error.to_string().contains("65536 bytes"));
    assert_eq!(
        broker
            .request(Request::Unsubscribe {
                agent: "coordinator".into(),
                events: None
            })
            .unwrap()["events"],
        json!([])
    );
}
