//! Private native input transport. Message text never enters process argv/env.
//! Only an already loaded session can receive input; never start/resume a thread.
use crate::{delivery::Outcome, protocol::Message};
use anyhow::{Context, Result, bail, ensure};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
use tokio::{net::UnixStream, time::timeout};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{Message as WsMessage, protocol::WebSocketConfig},
};

type Socket = WebSocketStream<UnixStream>;
const MAX_NATIVE_FRAME: usize = 1024 * 1024;
#[derive(Debug)]
struct Rejection {
    queue_full: bool,
}
impl std::fmt::Display for Rejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Native protocol rejected the request; response text is withheld")
    }
}
impl std::error::Error for Rejection {}

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
                // A server request can use the same numeric ID as our RPC.
                // Reject it before interpreting any result/error; never approve it.
                if value.get("method").is_some() && value.get("id").is_some() {
                    bail!("Unexpected native request; no automatic approval is permitted");
                }
                if value.get("method").is_none() && value["id"].as_u64() == Some(id) {
                    ensure!(
                        value.get("result").is_some() ^ value.get("error").is_some(),
                        "Contradictory or missing native response payload"
                    );
                    if let Some(error) = value.get("error") {
                        return Err(Rejection {
                            queue_full: error.as_object().is_some_and(|error| error.len() == 2)
                                && error["code"] == -32600
                                && error["message"]
                                    == "queue cannot contain more than 100 submissions",
                        }
                        .into());
                    }
                    return value
                        .get("result")
                        .cloned()
                        .context("Malformed native response");
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
#[cfg(test)]
async fn deliver_at(paths: Vec<PathBuf>, home: &Path, msg: &Message, text: &str) -> Outcome {
    deliver_at_tracking(paths, home, msg, text, &AtomicBool::new(false), None).await
}
async fn deliver_at_tracking(
    paths: Vec<PathBuf>,
    home: &Path,
    msg: &Message,
    text: &str,
    attempted: &AtomicBool,
    guard: Option<&super::NoticeGuard>,
) -> Outcome {
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
            "No compatible private native daemon has the loaded destination session; check Codex home/protocol. Insecure command-line fallback is disabled.",
        );
    };
    // Reads cannot insert input. A stale/malformed selection fails before any write.
    let selection: Result<Option<String>> = async {
        let metadata = rpc(&mut socket, 30, "thread/read", json!({"threadId":msg.recipient_session})).await?;
        let thread = &metadata["thread"];
        ensure!(thread["id"] == msg.recipient_session, "Native metadata identified another thread");
        let status = thread["status"]["type"].as_str().context("Missing native thread status")?;
        ensure!(["idle", "active"].contains(&status), "Native thread is not available for delivery");
        let capability = match thread.get("canAcceptDirectInput") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => bail!("Invalid native direct-input capability"),
        };
        if status == "active" && capability {
            let turns = rpc(&mut socket, 31, "thread/turns/list", json!({"threadId":msg.recipient_session,"limit":1,"sortDirection":"desc","itemsView":"summary"})).await?;
            let data = turns["data"].as_array().context("Invalid native turn page")?;
            ensure!(data.len() == 1 && data[0]["status"] == "inProgress", "Native active turn changed before submission; explicitly retry");
            let turn = data[0]["id"].as_str().context("Missing native active turn ID")?;
            validate_session(turn)?;
            Ok(Some(turn.to_owned()))
        } else {
            Ok(None)
        }
    }.await;
    let turn = match selection {
        Ok(turn) => turn,
        Err(_) => {
            return Outcome::failed(
                "Native protocol metadata or active turn could not be verified; no input was submitted. Check compatibility and explicitly retry",
            );
        }
    };
    // Select exactly one write. Every ambiguous result after it remains uncertain;
    // never fall back to another method, daemon, or implicit retry.
    let input = json!([{"type":"text","text":text,"text_elements":[]}]);
    let (method, params) = if let Some(turn) = &turn {
        (
            "turn/steer",
            json!({"threadId":msg.recipient_session,"clientUserMessageId":msg.id,"input":input,"expectedTurnId":turn}),
        )
    } else {
        (
            "thread/queue/add",
            json!({"threadId":msg.recipient_session,"clientUserMessageId":msg.id,"input":input}),
        )
    };
    if let Some(guard) = guard
        && let Err(error) = guard()
    {
        return Outcome::failed(error);
    }
    attempted.store(true, Ordering::Relaxed);
    match rpc(&mut socket, 100, method, params).await {
        Ok(value) if turn.as_ref().is_some_and(|turn| value["turnId"] == *turn) => {
            Outcome::submitted(
                "Native active turn accepted peer input; execution waits for a safe native boundary, and recipient acknowledgement is unconfirmed",
            )
        }
        Ok(value)
            if turn.is_none()
                && value["queuedSubmission"]["clientUserMessageId"] == msg.id
                && value["queuedSubmission"]["input"] == input
                && value["queuedSubmission"]["id"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty()) =>
        {
            Outcome::submitted(
                "Native queue accepted peer input; it may wait for the whole active turn to finish. Recipient acknowledgement is unconfirmed",
            )
        }
        Err(error)
            if turn.is_none()
                && error
                    .downcast_ref::<Rejection>()
                    .is_some_and(|error| error.queue_full) =>
        {
            Outcome::failed(
                "Native queue is full (100 submissions); this latest request was rejected without insertion. Earlier explicitly retried attempts may still execute; inspect native history before retrying",
            )
        }
        _ => Outcome::uncertain(
            "Native input did not confirm the exact submission; inspect the native conversation before explicitly retrying. No fallback or automatic retry was attempted",
        ),
    }
}

pub(crate) async fn deliver(msg: &Message, text: &str) -> Outcome {
    deliver_guarded(msg, text, None).await
}

pub(crate) async fn deliver_guarded(
    msg: &Message,
    text: &str,
    guard: Option<super::NoticeGuard>,
) -> Outcome {
    let home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| crate::service::home().ok().map(|v| v.join(".codex")));
    let Some(home) = home else {
        return Outcome::failed("Cannot locate Codex home");
    };
    deliver_until_guarded(
        super::codex_discovery::socket_paths(),
        &home,
        msg,
        text,
        tokio::time::sleep(Duration::from_secs(20)),
        guard,
    )
    .await
}

// The deadline future starts with the exchange in production. Tests can gate it
// on the fixture's exact read/write boundary without relying on setup timing.
#[cfg(test)]
async fn deliver_until(
    paths: Vec<PathBuf>,
    home: &Path,
    msg: &Message,
    text: &str,
    deadline: impl std::future::Future<Output = ()>,
) -> Outcome {
    deliver_until_guarded(paths, home, msg, text, deadline, None).await
}

async fn deliver_until_guarded(
    paths: Vec<PathBuf>,
    home: &Path,
    msg: &Message,
    text: &str,
    deadline: impl std::future::Future<Output = ()>,
    guard: Option<super::NoticeGuard>,
) -> Outcome {
    let attempted = AtomicBool::new(false);
    tokio::select! {
        result=deliver_at_tracking(paths, home, msg, text, &attempted, guard.as_ref())=>result,
        _=deadline=>if attempted.load(Ordering::Relaxed) {
            Outcome::uncertain("Native input timed out after this submission attempt; inspect native conversation before explicitly retrying")
        } else {
            Outcome::failed("Native lookup timed out before this submission attempt; no input was submitted by this attempt")
        },
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
        std::sync::Arc<tokio::sync::Notify>,
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
        let ready = std::sync::Arc::new(tokio::sync::Notify::new());
        let signal = ready.clone();
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
                        let version = if mode.contains("v166") {
                            "0.166.0"
                        } else {
                            "0.165.0"
                        };
                        let user_agent = if mode.contains("unknown-version") {
                            "agent_bridge_queue/0.167.0".to_owned()
                        } else if mode.contains("spoof-version") {
                            format!("agent_bridge_queue/{version}.evil")
                        } else if mode.contains("prefix-empty") {
                            format!("/{version}")
                        } else if mode.contains("prefix-malformed") {
                            format!("other/extra/{version}")
                        } else if mode.contains("prefix-other") {
                            format!("first_observer/{version}")
                        } else {
                            format!("agent_bridge_queue/{version} test")
                        };
                        let mut result = json!({"userAgent":user_agent,"codexHome":if mode=="wrong-home" {"/".into()}else{expected_home.to_string_lossy().to_string()}});
                        if mode.contains("agent-missing") {
                            result.as_object_mut().unwrap().remove("userAgent");
                        } else if mode.contains("agent-object") {
                            result["userAgent"] = json!({"version":"diagnostic only"});
                        }
                        result
                    }
                    "thread/loaded/list" => {
                        let page = requests
                            .iter()
                            .filter(|r| r["method"] == "thread/loaded/list")
                            .count();
                        let absent = mode == "not-loaded"
                            || mode == "cursor-repeat"
                            || mode == "page-budget"
                            || (mode == "pagination" && page == 1);
                        json!({"data":if absent {json!([])}else{json!(["123e4567-e89b-12d3-a456-426614174000"])},"nextCursor":if mode=="cursor-repeat" {json!("same")}else if mode=="page-budget" {json!(format!("page{page}"))}else if mode=="pagination" && page==1 {json!("next")}else{Value::Null}})
                    }
                    "thread/read" => {
                        if mode == "read-collision" {
                            socket.send(WsMessage::Text(json!({"id":value["id"],"method":"item/commandExecution/requestApproval","result":{"thread":{"id":"123e4567-e89b-12d3-a456-426614174000"}},"params":{}}).to_string().into())).await.unwrap();
                            break;
                        }
                        if mode == "read-timeout" {
                            signal.notify_one();
                            // Remain held until the exchange deadline drops its
                            // socket. There is no competing success-response timer.
                            let closed = socket.next().await;
                            assert!(matches!(
                                closed,
                                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_)))
                            ));
                            break;
                        }
                        if mode.starts_with("read-unsupported") {
                            socket.send(WsMessage::Text(json!({"id":value["id"],"error":{"code":-32601,"message":"secret unsupported"}}).to_string().into())).await.unwrap();
                            break;
                        }
                        let mut thread = json!({"id":if mode.starts_with("wrong-thread") {"other"} else {"123e4567-e89b-12d3-a456-426614174000"},"status":{"type":if mode.starts_with("unknown-status") {"mystery"} else if mode.starts_with("active") {"active"} else {"idle"}},"canAcceptDirectInput":!mode.contains("cap-false")});
                        if mode.contains("cap-missing") {
                            thread
                                .as_object_mut()
                                .unwrap()
                                .remove("canAcceptDirectInput");
                        }
                        if mode.contains("cap-malformed") {
                            thread["canAcceptDirectInput"] = json!("true");
                        }
                        json!({"thread":thread})
                    }
                    "thread/turns/list" => {
                        let turn = json!({"id":"123e4567-e89b-12d3-a456-426614174001","status":if mode.contains("stale-turn") {"completed"} else {"inProgress"}});
                        json!({"data":if mode.contains("no-turn") {json!([])} else if mode.contains("many-turns") {json!([turn.clone(),turn])} else {json!([turn])},"nextCursor":null})
                    }
                    "thread/queue/add" | "turn/steer" => {
                        if mode.contains("write-timeout") {
                            signal.notify_one();
                            // Remain held until the exchange deadline drops its
                            // socket. There is no competing success-response timer.
                            let closed = socket.next().await;
                            assert!(matches!(
                                closed,
                                None | Some(Err(_)) | Some(Ok(WsMessage::Close(_)))
                            ));
                            break;
                        }
                        if mode.contains("collision") {
                            socket.send(WsMessage::Text(json!({"id":value["id"],"method":"item/commandExecution/requestApproval","result":{"turnId":"123e4567-e89b-12d3-a456-426614174001"},"params":{}}).to_string().into())).await.unwrap();
                            break;
                        }
                        if mode.contains("overflow") || mode.contains("notifications-ok") {
                            for _ in 0..if mode.contains("overflow") { 128 } else { 127 } {
                                socket
                                    .send(WsMessage::Text(
                                        json!({"method":"test/event","params":{}})
                                            .to_string()
                                            .into(),
                                    ))
                                    .await
                                    .unwrap();
                            }
                            if mode.contains("overflow") {
                                break;
                            }
                        }
                        if mode.contains("large-frame") {
                            let _ = socket
                                .send(WsMessage::Text("x".repeat(MAX_NATIVE_FRAME + 1).into()))
                                .await;
                            break;
                        }
                        if mode.contains("queue-full") || mode.contains("contradictory") {
                            let mut reply = json!({"id":value["id"],"error":{"code":-32600,"message":"queue cannot contain more than 100 submissions"}});
                            if mode.contains("other-code") {
                                reply["error"]["code"] = json!(-32000);
                            }
                            if mode.contains("error-data") {
                                reply["error"]["data"] = json!("PRIVATE_BODY");
                            }
                            if mode.contains("contradictory") {
                                reply["result"] = json!({"queuedSubmission":{"id":"inserted"}});
                            }
                            socket
                                .send(WsMessage::Text(reply.to_string().into()))
                                .await
                                .unwrap();
                            break;
                        }
                        if mode.contains("lost-response") {
                            break;
                        }
                        if method == "turn/steer" {
                            if mode.contains("reject") {
                                socket.send(WsMessage::Text(json!({"id":value["id"],"error":{"code":-32600,"message":"secret stale turn"}}).to_string().into())).await.unwrap();
                                break;
                            }
                            let result = json!({"turnId":if mode.contains("wrong-turn") {"123e4567-e89b-12d3-a456-426614174002"} else {"123e4567-e89b-12d3-a456-426614174001"}});
                            socket.send(WsMessage::Text(json!({"id":if mode.contains("wrong-rpc-id") {json!(999)}else{value["id"].clone()},"result":result}).to_string().into())).await.unwrap();
                            break;
                        }

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
                if socket
                    .send(WsMessage::Text(
                        json!({"id":value["id"],"result":result}).to_string().into(),
                    ))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            requests
        });
        (root, path, home, task, ready)
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
    async fn event_authorization_refreshes_after_native_preflight_before_any_write() {
        let (_root, path, home, task, _ready) = fixture("ok").await;
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed = calls.clone();
        let guard: super::super::NoticeGuard = std::sync::Arc::new(move || {
            observed.fetch_add(1, Ordering::Relaxed);
            anyhow::bail!("subscription expired while native metadata was read")
        });
        let outcome = deliver_until_guarded(
            vec![path],
            &home,
            &message(),
            "typed event",
            tokio::time::sleep(Duration::from_secs(5)),
            Some(guard),
        )
        .await;
        assert_eq!(outcome.state, "failed");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        let requests = task.await.unwrap();
        assert!(requests.iter().any(|r| r["method"] == "thread/read"));
        assert!(
            !requests
                .iter()
                .any(|r| r["method"] == "thread/queue/add" || r["method"] == "turn/steer")
        );
    }

    #[tokio::test]
    async fn private_queue_uses_exact_session_and_id_without_start_resume_or_process() {
        let (_root, path, home, task, _ready) = fixture("ok").await;
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
                "thread/read",
                "thread/queue/add"
            ]
        );
        assert_eq!(requests[4]["params"]["threadId"], msg.recipient_session);
        assert_eq!(requests[4]["params"]["clientUserMessageId"], msg.id);
        assert_eq!(
            requests[4]["params"]["input"][0]["text"],
            "literal $(command) secret"
        );
    }
    #[tokio::test]
    async fn wrong_home_or_unloaded_session_never_receives_body() {
        for mode in ["wrong-home", "not-loaded"] {
            let (_root, path, home, task, _ready) = fixture(mode).await;
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
            let (_root, path, home, task, _ready) = fixture(mode).await;
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
    #[tokio::test]
    async fn active_capability_selects_exactly_one_write_and_preserves_text() {
        for (mode, expected) in [
            ("active", "turn/steer"),
            ("active-v166", "turn/steer"),
            ("active-agent-missing", "turn/steer"),
            ("active-agent-object", "turn/steer"),
            ("idle-unknown-version", "thread/queue/add"),
            ("active-unknown-version-cap-false", "thread/queue/add"),
            ("active-unknown-version-cap-missing", "thread/queue/add"),
            ("idle-v166", "thread/queue/add"),
            ("active-v166-cap-false", "thread/queue/add"),
            ("active-v166-cap-missing", "thread/queue/add"),
            ("active-v166-spoof-version", "turn/steer"),
            ("active-v166-prefix-other", "turn/steer"),
            ("active-v166-prefix-empty", "turn/steer"),
            ("active-v166-prefix-malformed", "turn/steer"),
            ("active-cap-false", "thread/queue/add"),
            ("active-cap-missing", "thread/queue/add"),
            ("active-unknown-version", "turn/steer"),
            ("active-spoof-version", "turn/steer"),
            ("active-prefix-other", "turn/steer"),
            ("active-prefix-empty", "turn/steer"),
            ("active-prefix-malformed", "turn/steer"),
        ] {
            let (_root, path, home, task, _ready) = fixture(mode).await;
            let msg = message();
            assert_eq!(
                deliver_at(vec![path], &home, &msg, "PRIVATE_BODY")
                    .await
                    .state,
                "submitted",
                "{mode}"
            );
            let requests = task.await.unwrap();
            let writes = requests
                .iter()
                .filter(|r| {
                    ["turn/steer", "thread/queue/add"].contains(&r["method"].as_str().unwrap())
                })
                .collect::<Vec<_>>();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0]["method"], expected);
            assert_eq!(writes[0]["params"]["clientUserMessageId"], msg.id);
            assert_eq!(writes[0]["params"]["input"][0]["text"], "PRIVATE_BODY");
            if expected == "turn/steer" {
                assert_eq!(
                    writes[0]["params"]["expectedTurnId"],
                    "123e4567-e89b-12d3-a456-426614174001"
                );
                let lookup = requests
                    .iter()
                    .find(|r| r["method"] == "thread/turns/list")
                    .unwrap();
                assert_eq!(lookup["params"]["limit"], 1);
                assert_eq!(lookup["params"]["sortDirection"], "desc");
                assert_eq!(lookup["params"]["itemsView"], "summary");
            }
        }
    }
    #[tokio::test]
    async fn unusable_fresh_metadata_or_changed_turn_fails_before_any_write() {
        for mode in [
            "wrong-thread",
            "unknown-status",
            "active-cap-malformed",
            "active-v166-cap-malformed",
            "active-v166-stale-turn",
            "active-v166-no-turn",
            "active-v166-many-turns",
            "active-unknown-version-cap-malformed",
            "active-unknown-version-stale-turn",
            "active-unknown-version-many-turns",
            "unknown-status-unknown-version",
            "wrong-thread-unknown-version",
            "read-unsupported-unknown-version",
            "active-stale-turn",
            "active-no-turn",
            "active-many-turns",
            "read-unsupported",
            "read-collision",
        ] {
            let (_root, path, home, task, _ready) = fixture(mode).await;
            assert_eq!(
                deliver_at(vec![path], &home, &message(), "PRIVATE_BODY")
                    .await
                    .state,
                "failed",
                "{mode}"
            );
            assert!(
                !serde_json::to_string(&task.await.unwrap())
                    .unwrap()
                    .contains("PRIVATE_BODY"),
                "{mode}"
            );
        }
    }
    #[tokio::test]
    async fn attempted_steer_never_falls_back_and_ambiguous_protocol_stays_uncertain() {
        for mode in [
            "active-lost-response",
            "active-v166-lost-response",
            "active-v166-reject",
            "active-v166-wrong-turn",
            "active-v166-wrong-rpc-id",
            "active-v166-collision",
            "active-reject",
            "active-wrong-turn",
            "active-wrong-rpc-id",
            "active-collision",
            "active-overflow",
            "active-large-frame",
            "contradictory",
            "queue-full-other-code",
            "queue-full-error-data",
        ] {
            let (_root, path, home, task, _ready) = fixture(mode).await;
            let outcome =
                deliver_at(vec![path.clone(), path], &home, &message(), "PRIVATE_BODY").await;
            assert_eq!(outcome.state, "uncertain", "{mode}");
            assert!(!outcome.detail.contains("secret"));
            let requests = task.await.unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| ["turn/steer", "thread/queue/add"]
                        .contains(&r["method"].as_str().unwrap()))
                    .count(),
                1,
                "{mode}"
            );
        }
    }
    #[tokio::test]
    async fn exact_queue_capacity_error_semantics_are_definite_and_notifications_are_bounded() {
        for (mode, state) in [
            ("queue-full", "failed"),
            ("queue-full-v166", "failed"),
            ("queue-full-unknown-version", "failed"),
            ("active-v166-notifications-ok", "submitted"),
            ("active-notifications-ok", "submitted"),
        ] {
            let (_root, path, home, task, _ready) = fixture(mode).await;
            assert_eq!(
                deliver_at(vec![path], &home, &message(), "PRIVATE_BODY")
                    .await
                    .state,
                state
            );
            task.await.unwrap();
        }
    }
    #[tokio::test]
    async fn bounded_exchange_tracks_whether_input_was_attempted() {
        for (mode, expected) in [("read-timeout", "failed"), ("write-timeout", "uncertain")] {
            let (_root, path, home, task, ready) = fixture(mode).await;
            let msg = message();
            let deadline = async {
                ready.notified().await;
                tokio::time::sleep(Duration::from_millis(30)).await;
            };
            let result = timeout(
                Duration::from_secs(3),
                deliver_until(vec![path], &home, &msg, "PRIVATE_BODY", deadline),
            )
            .await
            .unwrap();
            assert_eq!(result.state, expected);
            let requests = task.await.unwrap();
            assert!(requests.iter().any(|r| r["method"]
                == if expected == "failed" {
                    "thread/read"
                } else {
                    "thread/queue/add"
                }));
        }
    }
    #[tokio::test]
    async fn loaded_pagination_cursor_and_candidate_limits_fail_closed() {
        for (mode, state, count) in [
            ("pagination", "submitted", 2),
            ("cursor-repeat", "failed", 2),
            ("page-budget", "failed", 16),
        ] {
            let (_root, path, home, task, _ready) = fixture(mode).await;
            assert_eq!(
                deliver_at(vec![path], &home, &message(), "PRIVATE_BODY")
                    .await
                    .state,
                state
            );
            let requests = task.await.unwrap();
            assert_eq!(
                requests
                    .iter()
                    .filter(|r| r["method"] == "thread/loaded/list")
                    .count(),
                count
            );
        }
        let (_root, path, home, task, _ready) = fixture("ok").await;
        let mut candidates = vec![PathBuf::from("/missing/private-native.sock"); 8];
        candidates.push(path);
        assert_eq!(
            deliver_at(candidates, &home, &message(), "PRIVATE_BODY")
                .await
                .state,
            "failed"
        );
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    #[tokio::test]
    async fn insecure_socket_rejected_before_connection_and_alternate_home_selection_is_exact() {
        let (_root, path, home, task, _ready) = fixture("ok").await;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(
            deliver_at(vec![path], &home, &message(), "PRIVATE_BODY")
                .await
                .state,
            "failed"
        );
        assert!(!task.is_finished());
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let (_a, wrong, _, first, _ready) = fixture("wrong-home").await;
        let (_b, right, home, second, _ready) = fixture("ok").await;
        assert_eq!(
            deliver_at(vec![wrong, right], &home, &message(), "PRIVATE_BODY")
                .await
                .state,
            "submitted"
        );
        assert_eq!(first.await.unwrap().len(), 1);
        assert!(
            second
                .await
                .unwrap()
                .iter()
                .any(|r| r["method"] == "thread/queue/add")
        );
    }
}
