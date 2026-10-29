//! OpenCode V1 plugin transport. A receipt confirms native insertion, never model acknowledgement.
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

pub fn validate_endpoint(endpoint: &str) -> Result<PathBuf> {
    let path = super::claude::validate_socket(Path::new(endpoint))?;
    instance(&path)?;
    Ok(path)
}

fn instance(path: &Path) -> Result<String> {
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
        receipt.ok,
        "OpenCode rejected request: {}",
        receipt.error.as_deref().unwrap_or("unspecified")
    );
    ensure!(
        receipt.session.as_deref() == frame.get("session").and_then(Value::as_str),
        "OpenCode receipt session mismatch"
    );
    ensure!(
        receipt.instance.as_deref() == Some(instance(path)?.as_str()),
        "OpenCode receipt instance mismatch"
    );
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
            timeout(
                Duration::from_secs(5),
                exchange(
                    &path,
                    &json!({"op":"probe","session":agent.session}),
                    &mut written,
                ),
            )
            .await??;
            Ok(())
        })
}

pub(crate) async fn deliver(agent: &Agent, msg: &Message, content: &str) -> Outcome {
    if let Err(error) = validate_session(&agent.session).and_then(|_| {
        ensure!(
            agent.session == msg.recipient_session,
            "OpenCode delivery session mismatch"
        );
        uuid::Uuid::parse_str(&msg.id).context("Invalid broker message ID")?;
        Ok(())
    }) {
        return Outcome::failed(error);
    }
    let path = match validate_endpoint(&agent.endpoint) {
        Ok(path) => path,
        Err(error) => return Outcome::failed(error),
    };
    let mut written = false;
    let frame = json!({"op":"deliver","session":agent.session,"id":msg.id,"content":content});
    let result = timeout(
        Duration::from_secs(15),
        exchange(&path, &frame, &mut written),
    )
    .await;
    match result {
        Ok(Ok(receipt)) => {
            let native = format!("msg_{}", msg.id.replace('-', "").to_ascii_lowercase());
            if receipt.id.as_deref() != Some(msg.id.as_str())
                || receipt.native_message_id.as_deref() != Some(native.as_str())
            {
                return Outcome::uncertain("OpenCode receipt message identity mismatch");
            }
            Outcome::submitted(
                "OpenCode plugin verified native user-message insertion; model execution and acknowledgement remain unconfirmed",
            )
        }
        error => {
            let detail = format!("OpenCode delivery receipt failed: {error:?}");
            if written {
                Outcome::uncertain(detail)
            } else {
                Outcome::failed(detail)
            }
        }
    }
}
