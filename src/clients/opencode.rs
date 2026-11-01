//! OpenCode plugin transport. A receipt confirms native insertion, never model acknowledgement.
use crate::{
    delivery::Outcome,
    protocol::{Agent, MAX_FRAME, Message},
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

pub fn validate_session(session: &str) -> Result<()> {
    ensure!(
        session.starts_with("ses_")
            && session.len() == 30
            && session[4..].bytes().all(|b| b.is_ascii_alphanumeric()),
        "OpenCode requires a native ses_ session ID"
    );
    Ok(())
}

/// Plain text presentation, with immutable tool identities separate from labels.
/// Stable instructions belong to the plugin's system context, not each message.
pub(crate) fn message_content(msg: &Message, machines: &Value) -> String {
    use unicode_general_category::{GeneralCategory as Category, get_general_category};
    let label = machines["sender_name"].as_str().unwrap_or(&msg.sender);
    let label: String = label
        .chars()
        .filter(|c| {
            !matches!(
                get_general_category(*c),
                Category::Control
                    | Category::Format
                    | Category::LineSeparator
                    | Category::ParagraphSeparator
            )
        })
        .take(160)
        .collect();
    let label = if label.trim().is_empty() {
        msg.sender.as_str()
    } else {
        label.trim()
    };
    let parent = msg
        .parent
        .as_ref()
        .map(|id| format!(" parent={id}"))
        .unwrap_or_default();
    format!(
        "{label}:\n{}\n\n[peer id={} from={} to={}{parent}]",
        msg.body, msg.id, msg.sender, msg.recipient
    )
}

pub fn validate_endpoint(endpoint: &str) -> Result<PathBuf> {
    let path = super::claude::validate_socket(Path::new(endpoint))?;
    endpoint_instance(&path)?;
    Ok(path)
}

pub(crate) fn endpoint_instance(path: &Path) -> Result<String> {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("Missing OpenCode instance")?;
    ensure!(
        path.extension().is_some_and(|s| s == "sock"),
        "Invalid OpenCode endpoint extension"
    );
    let id = uuid::Uuid::parse_str(stem)
        .context("OpenCode endpoint must identify its plugin instance")?;
    ensure!(
        id.to_string() == stem,
        "OpenCode endpoint instance must be canonical"
    );
    Ok(stem.to_owned())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    ok: bool,
    protocol: u32,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    generation: Option<String>,
    #[serde(default)]
    attempt: Option<String>,
    #[serde(default)]
    session: Option<String>,
    #[serde(default)]
    instance: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    native_message_id: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default, rename = "uncertain")]
    _uncertain: Option<bool>,
}

async fn exchange(path: &Path, frame: &Value, written: &mut bool) -> Result<Receipt> {
    let mut stream = UnixStream::connect(path).await?;
    crate::delivery::validate_peer(&stream, unsafe { libc::geteuid() })?;
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    ensure!(
        bytes.len() <= MAX_FRAME,
        "OpenCode request exceeds frame limit"
    );
    // Once any write is attempted, loss of the receipt cannot establish rejection.
    *written = true;
    stream.write_all(&bytes).await?;
    let mut response = Vec::new();
    BufReader::new(stream)
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut response)
        .await?;
    ensure!(
        response.len() <= MAX_FRAME && response.last() == Some(&b'\n'),
        "Invalid OpenCode receipt frame"
    );
    let receipt: Receipt = serde_json::from_slice(&response).context("Invalid OpenCode receipt")?;
    ensure!(
        receipt.protocol == 2,
        "OpenCode plugin protocol version mismatch"
    );
    ensure!(
        receipt.session.as_deref() == frame.get("session").and_then(Value::as_str),
        "OpenCode receipt session mismatch"
    );
    ensure!(
        receipt.instance.as_deref() == Some(endpoint_instance(path)?.as_str()),
        "OpenCode receipt instance mismatch"
    );
    for key in [
        "id",
        "namespace",
        "generation",
        "attempt",
        "native_message_id",
    ] {
        if let Some(expected) = frame.get(key).and_then(Value::as_str) {
            let actual = match key {
                "id" => receipt.id.as_deref(),
                "namespace" => receipt.namespace.as_deref(),
                "generation" => receipt.generation.as_deref(),
                "attempt" => receipt.attempt.as_deref(),
                _ => receipt.native_message_id.as_deref(),
            };
            ensure!(actual == Some(expected), "OpenCode receipt {key} mismatch");
        }
    }
    Ok(receipt)
}

/// Registration uses the same connected-peer and exact-session checks as delivery.
/// Run on a blocking worker when called from the async broker.
pub(crate) fn verify_registration(agent: &Agent) -> Result<()> {
    validate_session(&agent.session)?;
    let path = validate_endpoint(&agent.endpoint)?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut written = false;
            let receipt = timeout(
                Duration::from_secs(5),
                exchange(
                    &path,
                    &json!({"op":"probe","session":agent.session}),
                    &mut written,
                ),
            )
            .await??;
            ensure!(receipt.ok, "OpenCode exact-session probe rejected");
            Ok(())
        })
}

pub(crate) async fn deliver(_agent: &Agent, _msg: &Message, _content: &str) -> Outcome {
    Outcome::failed(
        "OpenCode delivery requires the broker's durable lease, queue and admission context",
    )
}

pub(crate) async fn deliver_dispatch(
    agent: &Agent,
    dispatch: &crate::opencode_state::Dispatch,
) -> Outcome {
    let path = match validate_endpoint(&agent.endpoint) {
        Ok(path) => path,
        Err(error) => return Outcome::uncertain(error),
    };
    let mut written = false;
    let result = timeout(
        Duration::from_secs(15),
        exchange(&path, &dispatch.frame, &mut written),
    )
    .await;
    match result {
        Ok(Ok(receipt)) if receipt.ok && receipt.status.as_deref() == Some("inserted") => {
            Outcome::submitted(
                "OpenCode native insertion verified by exact readback; this does not prove model reading or task completion",
            )
        }
        Ok(Ok(receipt))
            if !receipt.ok
                && receipt.status.as_deref() == Some("deferred")
                && dispatch.frame["op"] == "deliver" =>
        {
            Outcome {
                state: "queued",
                detail: format!(
                    "OpenCode deferred before insertion: {}",
                    receipt
                        .error
                        .as_deref()
                        .unwrap_or("busy or admission unavailable")
                ),
            }
        }
        other => Outcome::uncertain(format!(
            "OpenCode insertion requires read-only reconciliation: {other:?}"
        )),
    }
}
