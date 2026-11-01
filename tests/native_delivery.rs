use agent_bridge::{
    broker::{self, Broker},
    delivery, hook,
    protocol::{Agent, Message, Provider, Request},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    time::timeout,
};

fn id() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("ab-")
        .tempdir_in("/tmp")
        .unwrap()
}

#[tokio::test]
async fn claude_socket_only_history_allows_65_sends_without_receipts_or_status_polling() {
    let dir = temp();
    let socket = dir.path().join("native.sock");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let sink = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let state = dir.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    broker::rpc(
        &state,
        &Request::Register {
            agent: Agent {
                name: "claude-flow".into(),
                provider: Provider::Claude,
                session: id(),
                endpoint: socket.to_string_lossy().into_owned(),
                enabled: true,
            },
            replace: false,
        },
    )
    .await
    .unwrap();
    for index in 0..65 {
        let mid = id();
        let body = format!("Independent handoff {index}");
        let sent = broker::rpc(
            &state,
            &Request::Send {
                id: mid.clone(),
                sender: "user".into(),
                recipient: "claude-flow".into(),
                body: body.clone(),
                parent: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(sent["id"], mid);
        assert_eq!(sent["state"], "pending");
        let (stream, _) = timeout(Duration::from_secs(3), sink.accept())
            .await
            .unwrap()
            .unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["msg_id"], mid);
        assert!(
            frame["message"]["content"]
                .as_str()
                .unwrap()
                .contains(&body)
        );
    }
    server.abort();
    let _ = server.await;
    // In-flight blocking persistence retains broker ownership until it exits.
    let bound = timeout(Duration::from_secs(3), async {
        loop {
            match Broker::bind(state.clone(), "/missing/codex".into()) {
                Ok(bound) => break bound,
                Err(error) if error.to_string().contains("owns this state directory") => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                Err(error) => panic!("Restart failed: {error:#}"),
            }
        }
    })
    .await
    .unwrap();
    let server = tokio::spawn(broker::serve(bound));
    let persisted = rusqlite::Connection::open(dir.path().join("state/messages.sqlite3")).unwrap();
    let uncertain: i64 = persisted
        .query_row(
            "SELECT count(*) FROM messages WHERE state='uncertain'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        uncertain, 65,
        "Socket-only history remains Unknown after recovery"
    );
    assert!(
        timeout(Duration::from_millis(750), sink.accept())
            .await
            .is_err(),
        "Restart must not resend socket-only Unknown history"
    );
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn real_broker_routes_native_claude_frame_and_accepts_correlated_reply() {
    let dir = temp();
    let sink_dir = dir.path().join("claude");
    std::fs::create_dir(&sink_dir).unwrap();
    std::fs::set_permissions(&sink_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = sink_dir.join("native.sock");
    let sink = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let state = dir.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    let server = tokio::spawn(broker::serve(bound));
    let session = id();
    let name = "claude-test";
    broker::rpc(
        &state,
        &Request::Register {
            agent: Agent {
                name: name.into(),
                provider: Provider::Claude,
                session: session.clone(),
                endpoint: socket.to_string_lossy().into_owned(),
                enabled: true,
            },
            replace: false,
        },
    )
    .await
    .unwrap();
    let mid = id();
    let body = "Quotes ' \" and literal $(touch /should-never-run)\nUnicode: 🦀";
    broker::rpc(
        &state,
        &Request::Send {
            id: mid.clone(),
            sender: "user".into(),
            recipient: name.into(),
            body: body.into(),
            parent: None,
        },
    )
    .await
    .unwrap();
    let (frame, _) = timeout(Duration::from_secs(3), sink.accept())
        .await
        .unwrap()
        .unwrap();
    let mut line = String::new();
    BufReader::new(frame).read_line(&mut line).await.unwrap();
    let frame: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(frame["type"], "user");
    assert_eq!(frame["session_id"], session);
    assert_eq!(frame["msg_id"], mid);
    let text = frame["message"]["content"].as_str().unwrap();
    assert_eq!(frame["from"], "user");
    assert!(text.starts_with("<cross-session-message from-name=\"user\">\n"));
    assert!(text.contains(body));
    assert!(text.contains(&format!(
        "Message reference: {mid}\nReply to: user\nDelivered to: {name}"
    )));
    assert!(!text.contains("Agent Bridge"));
    assert!(!text.contains("ack --state-dir"));
    timeout(Duration::from_secs(3), async {
        loop {
            let delivered = broker::rpc(&state, &Request::Get { id: mid.clone() })
                .await
                .unwrap();
            if delivered["state"] == "unknown" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let reply = id();
    broker::rpc(
        &state,
        &Request::Send {
            id: reply.clone(),
            sender: name.into(),
            recipient: "user".into(),
            body: "received".into(),
            parent: Some(mid),
        },
    )
    .await
    .unwrap();
    let got = broker::rpc(&state, &Request::Get { id: reply })
        .await
        .unwrap();
    assert_eq!(got["body"], "received");
    assert_eq!(got["hops"], 1);
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn codex_adapter_never_spawns_an_argv_fallback_for_invalid_sessions() {
    let dir = temp();
    let executable = dir.path().join("must-not-run");
    let marker = dir.path().join("ran");
    std::fs::write(
        &executable,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let agent = Agent {
        name: "codex-test".into(),
        provider: Provider::Codex,
        session: "invalid-session".into(),
        endpoint: String::new(),
        enabled: true,
    };
    let msg = Message {
        id: id(),
        sender: "user".into(),
        recipient: agent.name.clone(),
        recipient_session: agent.session.clone(),
        body: "private literal $(command)".into(),
        parent: None,
        hops: 0,
        state: "sending".into(),
        detail: None,
        created: 0,
        updated: 0,
    };
    assert_eq!(
        delivery::deliver(&agent, &msg, &executable, dir.path())
            .await
            .state,
        "failed"
    );
    assert!(!marker.exists());
}

#[tokio::test]
async fn exclusive_service_lock_and_bad_frames_do_not_break_the_broker() {
    let dir = temp();
    let state = dir.path().join("state");
    let bound = Broker::bind(state.clone(), "/missing/codex".into()).unwrap();
    assert!(Broker::bind(state.clone(), "/missing/codex".into()).is_err());
    let server = tokio::spawn(broker::serve(bound));
    let mut stream = UnixStream::connect(state.join("bridge.sock"))
        .await
        .unwrap();
    stream.write_all(b"{broken}\n").await.unwrap();
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .await
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&response).unwrap()["ok"],
        false
    );
    assert_eq!(
        broker::rpc(&state, &Request::Status).await.unwrap()["paused"],
        false
    );
    server.abort();
    let _ = server.await;
}

#[test]
fn hook_install_is_idempotent_and_preserves_existing_configuration() {
    let original = json!({"permissions":{"deny":["Bash(rm *)"]},"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"existing-hook"}]}]}});
    let installed = hook::merge_hooks(original.clone(), "agent-bridge hook", true).unwrap();
    let again = hook::merge_hooks(installed.clone(), "agent-bridge hook", true).unwrap();
    assert_eq!(installed, again);
    assert_eq!(installed["permissions"], original["permissions"]);
    let removed = hook::merge_hooks(installed, "agent-bridge hook", false).unwrap();
    assert_eq!(
        removed["hooks"]["SessionStart"],
        original["hooks"]["SessionStart"]
    );
}

#[test]
fn claude_endpoint_rejects_symlinks_and_world_accessible_sockets() {
    let dir = temp();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = dir.path().join("native.sock");
    let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(delivery::validate_socket(&socket).is_ok());
    let link = dir.path().join("link.sock");
    std::os::unix::fs::symlink(&socket, &link).unwrap();
    assert!(delivery::validate_socket(&link).is_err());
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o666)).unwrap();
    assert!(delivery::validate_socket(&socket).is_err());
}
