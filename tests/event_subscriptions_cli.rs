//! Executable CLI event selectors against an isolated real broker.
use agent_bridge::{
    broker::{self, Broker},
    protocol::{Agent, Provider, Request},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path};

async fn cli(state: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_agent-bridge"))
        .arg("--state-dir")
        .arg(state)
        .args(args)
        .output()
        .await
        .unwrap()
}
async fn result(state: &Path, args: &[&str]) -> Value {
    let output = cli(state, args).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test]
async fn cli_adds_removes_and_returns_effective_selectors_and_snapshot() {
    let root = tempfile::Builder::new()
        .prefix("ab-event-cli-")
        .tempdir_in("/tmp")
        .unwrap();
    let bound = Broker::bind(root.path().into(), "/missing/codex".into()).unwrap();
    let broker = bound.broker.clone();
    let service = tokio::spawn(broker::serve(bound));
    let socket = root.path().join("coordinator.sock");
    let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    broker
        .request(Request::Register {
            agent: Agent {
                name: "coordinator".into(),
                provider: Provider::Claude,
                session: uuid::Uuid::new_v4().to_string(),
                endpoint: socket.to_string_lossy().into(),
                enabled: true,
            },
            replace: false,
        })
        .unwrap();
    broker
        .request(Request::Register {
            agent: Agent {
                name: "worker".into(),
                provider: Provider::Inbox,
                session: "worker".into(),
                endpoint: String::new(),
                enabled: true,
            },
            replace: false,
        })
        .unwrap();
    let lifecycle = r#"[{"event":"agent.opened"},{"event":"agent.closed"}]"#;
    let board = r#"[{"event":"board.changed","board":"main"}]"#;
    let first = result(
        root.path(),
        &["subscribe", "--agent", "coordinator", "--events", lifecycle],
    )
    .await;
    assert_eq!(
        first["events"],
        serde_json::from_str::<Value>(lifecycle).unwrap()
    );
    assert!(
        first["agents"]
            .as_array()
            .unwrap()
            .iter()
            .any(|agent| agent["route"] == "worker")
    );
    assert_eq!(first["persistent"], false);
    let repeated = result(
        root.path(),
        &["subscribe", "--agent", "coordinator", "--events", lifecycle],
    )
    .await;
    assert_eq!(repeated["events"], first["events"]);
    let added = result(
        root.path(),
        &["subscribe", "--agent", "coordinator", "--events", board],
    )
    .await;
    assert_eq!(added["events"].as_array().unwrap().len(), 3);
    let removed = result(
        root.path(),
        &["unsubscribe", "--agent", "coordinator", "--events", board],
    )
    .await;
    assert_eq!(removed["events"], first["events"]);
    let cleared = result(root.path(), &["unsubscribe", "--agent", "coordinator"]).await;
    assert_eq!(cleared["events"], json!([]));
    for events in [
        "[]",
        r#"[{"event":"unknown"}]"#,
        r#"[{"event":"board.changed"}]"#,
        r#"[{"event":"agent.opened","board":"main"}]"#,
        r#"[{"event":"board.changed","board":"absent"}]"#,
    ] {
        for command in ["subscribe", "unsubscribe"] {
            let output = cli(
                root.path(),
                &[command, "--agent", "coordinator", "--events", events],
            )
            .await;
            assert!(!output.status.success(), "accepted {command} {events}");
        }
    }
    for old in [
        "subscribe-board",
        "unsubscribe-board",
        "subscribe_board",
        "unsubscribe_board",
    ] {
        assert!(
            !cli(root.path(), &[old, "--agent", "coordinator"])
                .await
                .status
                .success()
        );
    }
    service.abort();
}
