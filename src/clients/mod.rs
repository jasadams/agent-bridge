//! Built-in native client boundary. Inbox storage, durable queues, permissions,
//! retries and routing remain broker-owned; adapters only handle native clients.
pub mod claude;
pub mod claude_discovery;
pub mod codex;
pub mod codex_discovery;
pub mod lifecycle;
pub mod opencode;

use crate::{
    delivery::Outcome,
    protocol::{Agent, Message, Provider},
};
use anyhow::{Context, Result, ensure};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capability {
    Messages,
    ControlNotices,
    LifecycleRegistration,
}

/// Controls remain distinct from ordinary peer messages, including at dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlNotice<'a> {
    BoardChanged(&'a str),
    Pairing(&'a str),
}
impl<'a> ControlNotice<'a> {
    pub(crate) fn text(self) -> &'a str {
        match self {
            Self::BoardChanged(text) | Self::Pairing(text) => text,
        }
    }
}

/// One explicit dispatch point for every built-in native client. Inbox has no
/// native adapter and must not silently acquire native capabilities.
#[derive(Clone, Copy, Debug)]
pub enum Adapter {
    Claude,
    Codex,
    OpenCode,
}
impl Adapter {
    pub fn for_provider(provider: &Provider) -> Option<Self> {
        match provider {
            Provider::Claude => Some(Self::Claude),
            Provider::Codex => Some(Self::Codex),
            Provider::OpenCode => Some(Self::OpenCode),
            Provider::Inbox => None,
        }
    }

    pub fn supports(self, capability: Capability) -> bool {
        matches!(
            (self, capability),
            (
                Self::Claude | Self::Codex | Self::OpenCode,
                Capability::Messages
                    | Capability::ControlNotices
                    | Capability::LifecycleRegistration
            )
        )
    }

    pub fn validate_session(self, session: &str) -> Result<()> {
        match self {
            Self::Claude => claude::validate_session(session),
            Self::Codex => codex::validate_session(session),
            Self::OpenCode => opencode::validate_session(session),
        }
    }

    pub fn validate_endpoint(self, endpoint: &str) -> Result<String> {
        match self {
            Self::Claude => Ok(claude::validate_socket(Path::new(endpoint))?
                .to_string_lossy()
                .into_owned()),
            Self::OpenCode => Ok(opencode::validate_endpoint(endpoint)?
                .to_string_lossy()
                .into_owned()),
            Self::Codex => {
                ensure!(
                    endpoint.is_empty(),
                    "Only Claude registrations accept a socket endpoint"
                );
                Ok(String::new())
            }
        }
    }

    pub(crate) fn message_content(
        self,
        msg: &Message,
        state_dir: &Path,
        machines: &serde_json::Value,
    ) -> String {
        match self {
            Self::Claude => claude::claude_content(msg, machines),
            Self::Codex | Self::OpenCode => {
                crate::delivery::envelope_with_machines(msg, state_dir, machines)
            }
        }
    }

    async fn deliver(self, agent: &Agent, msg: &Message, content: &str) -> Outcome {
        match self {
            Self::Claude => claude::deliver(agent, msg, content).await,
            Self::Codex => codex::deliver(msg, content).await,
            Self::OpenCode => opencode::deliver(agent, msg, content).await,
        }
    }

    pub fn hook_endpoint(self) -> Result<String> {
        match self {
            Self::Claude => std::env::var("CLAUDE_CODE_MESSAGING_SOCKET")
                .context("Claude did not export an inbox socket; check /status"),
            Self::Codex => Ok(String::new()),
            Self::OpenCode => anyhow::bail!("OpenCode lifecycle is owned by its plugin"),
        }
    }
}

pub fn supports(provider: &Provider, capability: Capability) -> bool {
    Adapter::for_provider(provider).is_some_and(|adapter| adapter.supports(capability))
}

/// Common bounds also apply to broker-owned Inbox identities. Remote directory
/// callers retain their additional wire-level bounds at the trust boundary.
pub fn validate_session(provider: &Provider, session: &str) -> Result<()> {
    ensure!(
        !session.is_empty() && session.len() <= 200,
        "Invalid session ID"
    );
    if let Some(adapter) = Adapter::for_provider(provider) {
        adapter.validate_session(session)?;
    }
    Ok(())
}

/// Remote directories carry identities, never local native endpoint paths.
/// Retain their stricter wire boundary and original error context.
pub fn validate_remote_session(provider: &Provider, session: &str) -> Result<()> {
    ensure!(
        *provider != Provider::OpenCode,
        "OpenCode sessions are local-only; remote support is not implemented"
    );
    ensure!(
        !session.is_empty() && session.len() <= 200 && !session.chars().any(char::is_control),
        "Invalid remote session"
    );
    if let Some(adapter) = Adapter::for_provider(provider) {
        adapter
            .validate_session(session)
            .context("Native remote session must be a UUID")?;
    }
    Ok(())
}

pub fn validate_registration(agent: &mut Agent) -> Result<()> {
    validate_session(&agent.provider, &agent.session)?;
    if let Some(adapter) = Adapter::for_provider(&agent.provider) {
        agent.endpoint = adapter.validate_endpoint(&agent.endpoint)?;
    } else {
        ensure!(
            agent.endpoint.is_empty(),
            "Only Claude registrations accept a socket endpoint"
        );
    }
    Ok(())
}

pub(crate) async fn deliver_content(agent: &Agent, msg: &Message, content: &str) -> Outcome {
    let Some(adapter) = Adapter::for_provider(&agent.provider) else {
        return Outcome::failed("Client does not support native messages");
    };
    if !adapter.supports(Capability::Messages) {
        return Outcome::failed("Client does not support native messages");
    }
    adapter.deliver(agent, msg, content).await
}

pub(crate) async fn deliver_notice(
    agent: &Agent,
    msg: &Message,
    notice: ControlNotice<'_>,
) -> Outcome {
    let Some(adapter) = Adapter::for_provider(&agent.provider)
        .filter(|adapter| adapter.supports(Capability::ControlNotices))
    else {
        return Outcome::failed("Client does not support typed control notices");
    };
    adapter.deliver(agent, msg, notice.text()).await
}

/// Observe only sessions already present in each native application. Dropping
/// this future cancels both observers when the broker shuts down.
pub(crate) async fn observe(broker: crate::broker::Broker, codex_home: Option<PathBuf>) {
    let codex_broker = broker.clone();
    let codex_observer = async move {
        if let Some(home) = codex_home {
            codex_discovery::run(codex_broker, home).await;
        }
        std::future::pending::<()>().await;
    };
    tokio::join!(claude_discovery::run(broker), codex_observer);
}

#[cfg(test)]
mod tests;
