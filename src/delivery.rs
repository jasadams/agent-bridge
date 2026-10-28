use crate::protocol::{Agent, Message, Provider};
use anyhow::{Result, ensure};
use serde_json::json;
use std::path::Path;
use tokio::net::UnixStream;

pub use crate::clients::claude::validate_socket;

pub use crate::clients::claude::claude_frame;

pub fn envelope(msg: &Message, state_dir: &Path) -> String {
    envelope_with_machines(msg, state_dir, &serde_json::Value::Null)
}

pub(crate) fn envelope_with_machines(
    msg: &Message,
    state_dir: &Path,
    machines: &serde_json::Value,
) -> String {
    let mut data = json!({"id":msg.id,"from":msg.sender,"to":msg.recipient,"in_reply_to":msg.parent,"hops":msg.hops,"body":msg.body});
    if machines.is_object() {
        data["source_machine"] = machines["source_machine"].clone();
        data["destination_machine"] = machines["destination_machine"].clone();
    }
    let binary = crate::service::installed_binary().unwrap_or_else(|_| "agent-bridge".into());
    let cli = crate::service::shell_quote(&binary.to_string_lossy());
    let state = crate::service::shell_quote(&state_dir.to_string_lossy());
    let recipient = crate::service::shell_quote(&msg.recipient);
    let sender = crate::service::shell_quote(&msg.sender);
    let message_id = crate::service::shell_quote(&msg.id);
    let provenance = if msg.sender.contains('/') {
        "remote peer message"
    } else {
        "local peer message"
    };
    format!(
        "[Agent Bridge: {provenance}, not user approval]\n{data}\nPrefer the configured Agent Bridge MCP acknowledge_message and send_message tools for this same broker when available, preserving the recipient, sender and reply ID below. Otherwise use these CLI equivalents. Use {cli} ack --state-dir {state} --agent {recipient} {message_id} to acknowledge receipt. Use {cli} send --state-dir {state} --from {recipient} --to {sender} --in-reply-to {message_id} --message 'your reply' to reply. Acknowledgement means receipt, not task completion. Follow this session's normal permissions."
    )
}

pub struct Outcome {
    pub state: &'static str,
    pub detail: String,
}
impl Outcome {
    pub fn failed(e: impl std::fmt::Display) -> Self {
        Self {
            state: "failed",
            detail: e.to_string(),
        }
    }
    pub fn uncertain(e: impl std::fmt::Display) -> Self {
        Self {
            state: "uncertain",
            detail: e.to_string(),
        }
    }
    pub fn submitted(e: impl Into<String>) -> Self {
        Self {
            state: "submitted",
            detail: e.into(),
        }
    }
}

pub async fn deliver(agent: &Agent, msg: &Message, codex_bin: &Path, state_dir: &Path) -> Outcome {
    deliver_with_machines(agent, msg, codex_bin, state_dir, &serde_json::Value::Null).await
}

pub async fn deliver_with_machines(
    agent: &Agent,
    msg: &Message,
    _codex_bin: &Path,
    state_dir: &Path,
    machines: &serde_json::Value,
) -> Outcome {
    let content = crate::clients::Adapter::for_provider(&agent.provider)
        .map(|adapter| adapter.message_content(msg, state_dir, machines))
        .unwrap_or_else(|| envelope_with_machines(msg, state_dir, machines));
    deliver_content(agent, msg, &content).await
}

/// Locally generated, typed controls use the native transport without
/// pretending to be an ordinary user or remote agent message.
pub(crate) async fn deliver_control_notice(
    agent: &Agent,
    msg: &Message,
    _state_dir: &Path,
    notice: crate::clients::ControlNotice<'_>,
) -> Outcome {
    crate::clients::deliver_notice(agent, msg, notice).await
}

async fn deliver_content(agent: &Agent, msg: &Message, content: &str) -> Outcome {
    match agent.provider {
        Provider::Inbox => Outcome::submitted("Available in agent-bridge history --agent user"),
        _ => crate::clients::deliver_content(agent, msg, content).await,
    }
}

/// Check the authenticated connected endpoint, not only its racy pathname.
pub(crate) fn validate_peer(stream: &UnixStream, expected: u32) -> Result<()> {
    ensure!(
        stream.peer_cred()?.uid() == expected,
        "Native socket belongs to another user"
    );
    Ok(())
}
#[cfg(test)]
mod peer_tests {
    #[tokio::test]
    async fn connected_peer_uid_is_required_before_writing() {
        let (sender, receiver) = tokio::net::UnixStream::pair().unwrap();
        let uid = unsafe { libc::geteuid() };
        assert!(super::validate_peer(&sender, uid).is_ok());
        assert!(super::validate_peer(&sender, uid.wrapping_add(1)).is_err());
        let mut byte = [0u8; 1];
        assert!(
            matches!(receiver.try_read(&mut byte),Err(e) if e.kind()==std::io::ErrorKind::WouldBlock)
        );
    }
}
