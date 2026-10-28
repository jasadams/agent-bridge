//! Private native queue transport. Message text never enters process argv/env.
//! Only an already loaded session can be queued; never start/resume a thread.
use crate::{delivery::Outcome, protocol::Message};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{net::UnixStream, time::timeout};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message as WsMessage, protocol::WebSocketConfig},
};

type Socket = WebSocketStream<UnixStream>;
const MAX_NATIVE_FRAME: usize = 1024 * 1024;
async fn rpc(socket: &mut Socket, id: u64, method: &str, params: Value) -> Result<Value> {
    socket
        .send(WsMessage::Text(
            json!({"id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))
        .await?;
    for _ in 0..128 {
        match socket.next().await.context("Native connection closed")?? {
            WsMessage::Text(text) => {
                let value: Value = serde_json::from_str(&text)?;
                if value["id"].as_u64() == Some(id) {
                    // Native errors may echo submitted text. Never store/log them.
                    ensure!(
                        value.get("error").is_none(),
                        "Native queue protocol rejected the request; check Codex version and session availability"
                    );
                    return value
                        .get("result")
                        .cloned()
                        .context("Malformed native response");
                }
                if value.get("method").is_some() && value.get("id").is_some() {
                    bail!("Unexpected native approval request; no automatic approval is permitted");
                }
            }
            WsMessage::Close(_) => bail!("Native connection closed"),
            _ => {}
        }
    }
    bail!("Native notification budget exceeded")
}
async fn prepare(path: &Path, home: &Path, session: &str) -> Result<Socket> {
    crate::delivery::validate_socket(path)?;
    let stream = UnixStream::connect(path).await?;
    crate::delivery::validate_peer(&stream, unsafe { libc::geteuid() })?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(MAX_NATIVE_FRAME))
        .max_frame_size(Some(MAX_NATIVE_FRAME));
    let (mut socket, _) =
        tokio_tungstenite::client_async_with_config("ws://localhost/", stream, Some(config))
            .await?;
    let initialized=rpc(&mut socket,1,"initialize",json!({"clientInfo":{"name":"agent_bridge_queue","version":crate::VERSION},"capabilities":{"experimentalApi":true}})).await?;
    let actual = initialized["codexHome"]
        .as_str()
        .context("Native daemon did not identify its home")?;
    ensure!(
        Path::new(actual).canonicalize()? == home.canonicalize()?,
        "Native daemon belongs to another Codex home"
    );
    socket
        .send(WsMessage::Text(
            json!({"method":"initialized"}).to_string().into(),
        ))
        .await?;
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for page in 0..16 {
        let loaded = rpc(
            &mut socket,
            2 + page,
            "thread/loaded/list",
            json!({"cursor":cursor,"limit":100}),
        )
        .await?;
        let data = loaded["data"]
            .as_array()
            .context("Invalid loaded-session response")?;
        ensure!(data.len() <= 100, "Native session page exceeded budget");
        if data.iter().any(|v| v.as_str() == Some(session)) {
            return Ok(socket);
        }
        cursor = loaded["nextCursor"].as_str().map(str::to_string);
        let Some(next) = &cursor else {
            break;
        };
        ensure!(
            next.len() <= 4096 && seen.insert(next.clone()),
            "Invalid native session cursor"
        );
    }
    bail!("Destination session is not loaded; bridge will not start or resume it")
}
async fn deliver_at(paths: Vec<PathBuf>, home: &Path, msg: &Message, text: &str) -> Outcome {
    if validate_session(&msg.recipient_session).is_err() {
        return Outcome::failed("Invalid Codex session UUID");
    }
    let mut socket = None;
    for path in paths.into_iter().take(8) {
        if let Ok(Ok(found)) = timeout(
            Duration::from_secs(2),
            prepare(&path, home, &msg.recipient_session),
        )
        .await
        {
            socket = Some(found);
            break;
        }
    }
    let Some(mut socket) = socket else {
        return Outcome::failed(
            "No compatible private native daemon has the loaded destination session; check Codex home/version. Insecure command-line fallback is disabled.",
        );
    };
    // From the first queue write onwards, loss/error is ambiguous. Never try a
    // different daemon or automatically retry a potentially accepted submission.
    let input = json!([{"type":"text","text":text,"text_elements":[]}]);
    let result = rpc(
        &mut socket,
        100,
        "thread/queue/add",
        json!({"threadId":msg.recipient_session,"clientUserMessageId":msg.id,"input":input}),
    )
    .await;
    match result {
        Ok(value)
            if value["queuedSubmission"]["clientUserMessageId"] == msg.id
                && value["queuedSubmission"]["input"] == input
                && value["queuedSubmission"]["id"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty()) =>
        {
            Outcome::submitted(
                "Private native queue accepted the message; recipient acknowledgement and model execution are unconfirmed",
            )
        }
        _ => Outcome::uncertain(
            "Native queue did not confirm the exact submission; inspect native conversation before explicitly retrying",
        ),
    }
}
pub(crate) async fn deliver(msg: &Message, text: &str) -> Outcome {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::service::home().ok().map(|v| v.join(".codex")));
    let Some(home) = home else {
        return Outcome::failed("Cannot locate Codex home");
    };
    // Bound the entire lookup+write exchange, including silent or busy daemons.
    match timeout(
        Duration::from_secs(20),
        deliver_at(super::codex_discovery::socket_paths(), &home, msg, text),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => Outcome::uncertain(
            "Native queue timed out; inspect native conversation before explicitly retrying",
        ),
    }
}

/// Stable native identity, never a mutable display name.
pub fn validate_session(session: &str) -> anyhow::Result<()> {
    uuid::Uuid::parse_str(session)
        .context("Codex registration requires a thread UUID, not a mutable display name")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    async fn fixture(
        mode: &str,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        PathBuf,
        tokio::task::JoinHandle<Vec<Value>>,
    ) {
        let root = tempfile::Builder::new()
            .prefix("ab-native-sec-")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = root.path().join("native.sock");
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mode = mode.to_string();
        let expected_home = home.clone();
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let mut requests = Vec::new();
            while let Some(Ok(WsMessage::Text(text))) = socket.next().await {
                let value: Value = serde_json::from_str(&text).unwrap();
                let method = value["method"].as_str().unwrap();
                requests.push(value.clone());
                if method == "initialized" {
                    continue;
                }
                let result = match method {
                    "initialize" => {
                        json!({"codexHome":if mode=="wrong-home" {"/".into()}else{expected_home.to_string_lossy().to_string()}})
                    }
                    "thread/loaded/list" => {
                        json!({"data":if mode=="not-loaded" {vec![]}else{vec!["123e4567-e89b-12d3-a456-426614174000"]},"nextCursor":null})
                    }
                    "thread/queue/add" => {
                        if mode == "lost-response" {
                            break;
                        }
                        if mode == "reject" {
                            socket
                                .send(WsMessage::Text(
                                    json!({"id":value["id"],"error":{"message":"echo secret"}})
                                        .to_string()
                                        .into(),
                                ))
                                .await
                                .unwrap();
                            break;
                        }
                        json!({"queuedSubmission":{"id":"queued-id","clientUserMessageId":if mode=="wrong-id" {json!("other")}else{value["params"]["clientUserMessageId"].clone()},"input":value["params"]["input"]}})
                    }
                    other => panic!("unexpected method {other}"),
                };
                socket
                    .send(WsMessage::Text(
                        json!({"id":value["id"],"result":result}).to_string().into(),
                    ))
                    .await
                    .unwrap();
            }
            requests
        });
        (root, path, home, task)
    }
    fn message() -> Message {
        Message {
            id: uuid::Uuid::new_v4().to_string(),
            recipient_session: "123e4567-e89b-12d3-a456-426614174000".into(),
            sender: "user".into(),
            recipient: "codex-test".into(),
            body: "PRIVATE_BODY".into(),
            parent: None,
            hops: 0,
            state: "sending".into(),
            detail: None,
            created: 0,
            updated: 0,
        }
    }
    #[tokio::test]
    async fn private_queue_uses_exact_session_and_id_without_start_resume_or_process() {
        let (_root, path, home, task) = fixture("ok").await;
        let msg = message();
        let outcome = deliver_at(vec![path], &home, &msg, "literal $(command) secret").await;
        assert_eq!(outcome.state, "submitted");
        let requests = task.await.unwrap();
        assert_eq!(
            requests
                .iter()
                .filter_map(|v| v["method"].as_str())
                .collect::<Vec<_>>(),
            vec![
                "initialize",
                "initialized",
                "thread/loaded/list",
                "thread/queue/add"
            ]
        );
        assert_eq!(requests[3]["params"]["threadId"], msg.recipient_session);
        assert_eq!(requests[3]["params"]["clientUserMessageId"], msg.id);
        assert_eq!(
            requests[3]["params"]["input"][0]["text"],
            "literal $(command) secret"
        );
    }
    #[tokio::test]
    async fn wrong_home_or_unloaded_session_never_receives_body() {
        for mode in ["wrong-home", "not-loaded"] {
            let (_root, path, home, task) = fixture(mode).await;
            assert_eq!(
                deliver_at(vec![path], &home, &message(), "PRIVATE_BODY")
                    .await
                    .state,
                "failed"
            );
            let requests = task.await.unwrap();
            assert!(
                !serde_json::to_string(&requests)
                    .unwrap()
                    .contains("PRIVATE_BODY")
            );
        }
    }
    #[tokio::test]
    async fn lost_rejected_or_mismatched_queue_reply_is_uncertain_and_not_retried() {
        for mode in ["lost-response", "reject", "wrong-id"] {
            let (_root, path, home, task) = fixture(mode).await;
            let outcome =
                deliver_at(vec![path.clone(), path], &home, &message(), "PRIVATE_BODY").await;
            assert_eq!(outcome.state, "uncertain");
            assert!(!outcome.detail.contains("echo secret"));
            let requests = task.await.unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|v| v["method"] == "thread/queue/add")
                    .count(),
                1
            );
        }
    }
}
