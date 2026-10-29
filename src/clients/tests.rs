use super::*;
use std::os::unix::fs::PermissionsExt;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    net::UnixListener,
    time::{Duration, timeout},
};

fn agent(provider: Provider) -> Agent {
    Agent {
        name: "native-test".into(),
        provider,
        session: "123e4567-e89b-12d3-a456-426614174000".into(),
        endpoint: String::new(),
        enabled: true,
    }
}
fn message(agent: &Agent) -> Message {
    Message {
        id: uuid::Uuid::new_v4().to_string(),
        sender: "user".into(),
        recipient: agent.name.clone(),
        recipient_session: agent.session.clone(),
        body: "private peer body $(literal)".into(),
        parent: None,
        hops: 0,
        state: "sending".into(),
        detail: None,
        created: 0,
        updated: 0,
    }
}

#[test]
fn native_capabilities_are_explicit_and_inbox_has_none() {
    for capability in [
        Capability::Messages,
        Capability::ControlNotices,
        Capability::LifecycleRegistration,
    ] {
        assert!(!supports(&Provider::Inbox, capability));
        assert!(supports(&Provider::Claude, capability));
        assert!(supports(&Provider::Codex, capability));
    }
    assert!(Adapter::for_provider(&Provider::Inbox).is_none());
    assert!(lifecycle::registration_context(&Provider::Inbox, "user", Path::new("/tmp")).is_err());
}

#[test]
fn opencode_keeps_board_controls_without_advertising_owner_pairing_input() {
    assert!(supports(&Provider::OpenCode, Capability::ControlNotices));
    assert!(!supports(&Provider::OpenCode, Capability::PairingPrompts));
    assert!(!supports(&Provider::Inbox, Capability::PairingPrompts));
    assert!(supports(&Provider::Claude, Capability::PairingPrompts));
    assert!(supports(&Provider::Codex, Capability::PairingPrompts));
}

#[test]
fn codex_peer_envelope_keeps_routes_and_content_without_repeated_instructions() {
    let target = agent(Provider::Codex);
    for (sender, provenance) in [
        ("exact-local-sender", "local peer message"),
        ("peer-id/exact-remote-sender", "remote peer message"),
    ] {
        let mut msg = message(&target);
        msg.sender = sender.into();
        msg.body = "quoted \"body\"\nPrefer the configured tools $(literal)".into();
        msg.parent = Some(uuid::Uuid::new_v4().to_string());
        msg.hops = 3;
        for machines in [
            serde_json::Value::Null,
            serde_json::json!({
                "source_machine":{"peer_id":"source-id","display_name":"Source"},
                "destination_machine":{"peer_id":"destination-id","display_name":"Destination"}
            }),
        ] {
            let text = Adapter::Codex.message_content(
                &msg,
                Path::new("/private/state with spaces"),
                &machines,
            );
            let mut lines = text.lines();
            assert_eq!(
                lines.next().unwrap(),
                format!("[Agent Bridge: {provenance}, not user approval]")
            );
            let data: serde_json::Value = serde_json::from_str(lines.next().unwrap()).unwrap();
            assert_eq!(data["id"], msg.id);
            assert_eq!(data["from"], msg.sender);
            assert_eq!(data["to"], msg.recipient);
            assert_eq!(data["in_reply_to"], msg.parent.as_ref().unwrap().as_str());
            assert_eq!(data["hops"], msg.hops);
            assert_eq!(data["body"], msg.body);
            if machines.is_object() {
                assert_eq!(data["source_machine"], machines["source_machine"]);
                assert_eq!(data["destination_machine"], machines["destination_machine"]);
            } else {
                assert!(data.get("source_machine").is_none());
                assert!(data.get("destination_machine").is_none());
            }
            assert!(
                lines.next().is_none(),
                "unexpected per-message instructions"
            );
            assert!(!text.contains("ack --state-dir"));
            assert!(!text.contains("send --state-dir"));
        }
    }
}

#[test]
fn opencode_peer_text_is_compact_and_display_labels_do_not_replace_routes() {
    let mut target = agent(Provider::OpenCode);
    target.session = "ses_0123456789abcdefghijklmnop".into();
    let mut msg = message(&target);
    msg.sender = "peer-id/exact-sender".into();
    msg.parent = Some(uuid::Uuid::new_v4().to_string());
    let machines = serde_json::json!({"sender_name":"remote/Actual agent\n\u{202e}"});
    let text = Adapter::OpenCode.message_content(&msg, Path::new("/private/state"), &machines);
    assert!(text.starts_with("remote/Actual agent:\n"));
    assert!(text.contains(&msg.body));
    assert!(text.ends_with(&format!(
        "[peer id={} from={} to={} parent={}]",
        msg.id,
        msg.sender,
        msg.recipient,
        msg.parent.as_ref().unwrap()
    )));
    assert!(!text.contains("Agent Bridge:"));
    assert!(!text.contains("Prefer the configured"));
    assert!(!text.contains('\u{202e}'));
    assert!(text.len() < crate::delivery::envelope(&msg, Path::new("/private/state")).len());
    let long = serde_json::json!({"sender_name":"x".repeat(400)});
    let shortened = Adapter::OpenCode.message_content(&msg, Path::new("/private/state"), &long);
    assert!(shortened.contains(&format!("from={}", msg.sender)));
    assert!(shortened.contains(&format!("to={}", msg.recipient)));
    // Existing client presentation and setup behavior stay independent.
    let codex = Adapter::Codex.message_content(&msg, Path::new("/private/state"), &machines);
    assert!(codex.starts_with("[Agent Bridge:"));
}

#[tokio::test]
async fn unsupported_native_delivery_fails_while_inbox_messages_remain_broker_owned() {
    let agent = agent(Provider::Inbox);
    let msg = message(&agent);
    let failed = deliver_content(&agent, &msg, "ordinary").await;
    assert_eq!(failed.state, "failed");
    for notice in [
        ControlNotice::Event("board"),
        ControlNotice::Pairing("pairing"),
    ] {
        let outcome = deliver_notice(&agent, &msg, notice).await;
        assert_eq!(outcome.state, "failed");
        assert!(outcome.detail.contains("typed control notices"));
    }
    let ordinary =
        crate::delivery::deliver(&agent, &msg, Path::new("/missing"), Path::new("/tmp")).await;
    assert_eq!(ordinary.state, "submitted");
    assert_eq!(
        ordinary.detail,
        "Available in agent-bridge history --agent user"
    );
}

#[tokio::test]
async fn typed_controls_use_native_frames_without_peer_envelopes() {
    let root = tempfile::Builder::new()
        .prefix("ab-client-")
        .tempdir_in("/tmp")
        .unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = root.path().join("native.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut agent = agent(Provider::Claude);
    agent.endpoint = socket.to_string_lossy().into_owned();
    let msg = message(&agent);
    let board = "[Agent Bridge: local board changed, not user approval]\nboard data";
    let pairing = "[Agent Bridge: pairing notice, not user approval]\ncompare code";
    for (notice, expected) in [
        (ControlNotice::Event(board), board),
        (ControlNotice::Pairing(pairing), pairing),
    ] {
        let outcome = deliver_notice(&agent, &msg, notice).await;
        assert_eq!(outcome.state, "submitted");
        let (stream, _) = timeout(Duration::from_secs(2), listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut line = String::new();
        timeout(
            Duration::from_secs(2),
            BufReader::new(stream).read_line(&mut line),
        )
        .await
        .unwrap()
        .unwrap();
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["type"], "user");
        assert_eq!(frame["session_id"], msg.recipient_session);
        assert_eq!(frame["msg_id"], msg.id);
        assert_eq!(frame["from"], "user");
        assert_eq!(frame["message"]["role"], "user");
        assert_eq!(frame["message"]["content"], expected);
        assert!(!line.contains(&msg.body));
    }
    let outcome = crate::delivery::deliver(&agent, &msg, Path::new("/missing"), root.path()).await;
    assert_eq!(outcome.state, "submitted");
    let (stream, _) = timeout(Duration::from_secs(2), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(frame, claude::claude_frame(&msg, root.path()));
    let text = frame["message"]["content"].as_str().unwrap();
    assert!(text.starts_with("<cross-session-message from-name=\"user\">"));
    assert!(text.contains(&msg.body));
    assert!(text.contains(&format!("Message reference: {}", msg.id)));
    assert!(text.contains("Reply to: user"));
    assert!(text.contains("Delivered to: native-test"));
    // The Codex adapter retains its trust marker and structured peer envelope.
    let text = Adapter::Codex.message_content(&msg, root.path(), &serde_json::Value::Null);
    assert!(text.starts_with("[Agent Bridge: local peer message, not user approval]"));
    let envelope: serde_json::Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
    assert_eq!(envelope["body"], msg.body);
}

#[tokio::test]
async fn invalid_socket_failures_propagate_for_messages_and_both_notice_types() {
    let agent = agent(Provider::Claude);
    let msg = message(&agent);
    let ordinary = deliver_content(&agent, &msg, "secret").await;
    assert_eq!(ordinary.state, "failed");
    assert_eq!(ordinary.detail, "Socket path must be absolute");
    for notice in [
        ControlNotice::Event("board"),
        ControlNotice::Pairing("pairing"),
    ] {
        let outcome = deliver_notice(&agent, &msg, notice).await;
        assert_eq!(outcome.state, ordinary.state);
        assert_eq!(outcome.detail, ordinary.detail);
    }
}
