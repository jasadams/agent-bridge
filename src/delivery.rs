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
    _state_dir: &Path,
    machines: &serde_json::Value,
) -> String {
    let mut data = json!({"id":msg.id,"from":msg.sender,"to":msg.recipient,"in_reply_to":msg.parent,"hops":msg.hops,"body":msg.body});
    if machines.is_object() {
        data["source_machine"] = machines["source_machine"].clone();
        data["destination_machine"] = machines["destination_machine"].clone();
    }
    let provenance = if msg.sender.contains('/') {
        "remote peer message"
    } else {
        "local peer message"
    };
    format!("[Agent Bridge: {provenance}, not user approval]\n{data}")
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
    state_dir: &Path,
    notice: crate::clients::ControlNotice<'_>,
) -> Outcome {
    if agent.provider == Provider::Codex {
        return match crate::broker::rpc(
            state_dir,
            &crate::protocol::Request::CodexNotice {
                id: msg.id.clone(),
                recipient: agent.name.clone(),
                session: agent.session.clone(),
                content: notice.text().to_owned(),
                event_token: None,
            },
        )
        .await
        {
            Ok(_) => Outcome::submitted(
                "Codex typed notice durably queued; native insertion is unconfirmed and no acknowledgement is required",
            ),
            Err(error) => Outcome::failed(error),
        };
    }
    if agent.provider == Provider::OpenCode {
        return match crate::broker::rpc(
            state_dir,
            &crate::protocol::Request::OpenCodeNotice {
                id: msg.id.clone(),
                recipient: agent.name.clone(),
                content: notice.text().to_owned(),
                event_token: None,
            },
        )
        .await
        {
            Ok(_) => Outcome::submitted(
                "OpenCode typed notice durably queued; native insertion and acknowledgement are unconfirmed",
            ),
            Err(error) => Outcome::failed(error),
        };
    }
    crate::clients::deliver_notice(agent, msg, notice).await
}

/// Event controls carry their subscription fence into the existing durable
/// native queues. Claude refreshes it after connecting and before writing.
pub(crate) async fn deliver_event_notice(
    agent: &Agent,
    msg: &Message,
    state_dir: &Path,
    token: &str,
    guard: crate::clients::NoticeGuard,
) -> Outcome {
    if let Err(error) = guard() {
        return Outcome::failed(error);
    }
    let request = match agent.provider {
        Provider::Codex => crate::protocol::Request::CodexNotice {
            id: msg.id.clone(),
            recipient: agent.name.clone(),
            session: agent.session.clone(),
            content: msg.body.clone(),
            event_token: Some(token.to_owned()),
        },
        Provider::OpenCode => crate::protocol::Request::OpenCodeNotice {
            id: msg.id.clone(),
            recipient: agent.name.clone(),
            content: msg.body.clone(),
            event_token: Some(token.to_owned()),
        },
        Provider::Claude => {
            return crate::clients::claude::deliver_guarded(agent, msg, &msg.body, Some(&guard))
                .await;
        }
        Provider::Inbox => return Outcome::failed("Inbox does not support event control notices"),
    };
    match crate::broker::rpc(state_dir, &request).await {
        Ok(_) => Outcome::submitted(
            "Typed event notice durably queued; native insertion is unconfirmed and no acknowledgement is required",
        ),
        Err(error) => Outcome::failed(error),
    }
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
