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
    /// Session-selected pairing prompts require a supported owner-input workflow.
    PairingPrompts,
    LifecycleRegistration,
}

/// Native observations, never a persisted agent catalog. Endpoint selection is
/// request-local transport information, not evidence of native membership.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NativeState {
    pub session: String,
    pub display_name: Option<String>,
    pub status: Option<String>,
    pub directory: Option<String>,
    pub updated_ms: Option<i64>,
    pub transport_endpoint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateError {
    Unavailable(String),
    Incomplete(String),
}

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(f, "native state unavailable: {detail}"),
            Self::Incomplete(detail) => write!(f, "native state incomplete: {detail}"),
        }
    }
}
impl std::error::Error for StateError {}

/// Synchronous reads run outside the DB lock on the existing blocking worker.
/// Empty/None mean confirmed absence; errors must never become empty success.
/// Only supporting providers age-filter lists; exact lookup is never filtered.
pub trait ConnectorState {
    fn provider(&self) -> Provider;
    fn list(&self, max_age_days: u32) -> std::result::Result<Vec<NativeState>, StateError>;
    fn get(&self, session: &str) -> std::result::Result<Option<NativeState>, StateError>;
}

/// Refresh authorization after asynchronous native preflight and before the first write.
pub(crate) type NoticeGuard = std::sync::Arc<dyn Fn() -> Result<()> + Send + Sync>;

#[cfg(test)]
mod topic_race_fixture {
    use super::*;
    use crate::topics::{AgentRef, TopicLimits, TopicPublication, TopicRef};

    pub struct Fixture {
        pub _bound: crate::broker::BoundServer,
        pub local: String,
        pub topic: TopicRef,
        pub identity: AgentRef,
        pub agent: Agent,
        pub message: Message,
    }

    impl Fixture {
        pub fn new(path: &Path, agent: Agent) -> Self {
            let bound =
                crate::broker::Broker::bind(path.to_path_buf(), "/missing/codex".into()).unwrap();
            let local = libp2p_identity::Keypair::generate_ed25519()
                .public()
                .to_peer_id()
                .to_string();
            let identity = AgentRef {
                bridge_id: None,
                agent_id: format!("{}-{}", agent.provider.label(), agent.session),
            };
            let (topic, message) = {
                let mut db = bound.broker.db.lock().unwrap();
                db.register(&agent, false).unwrap();
                let topic = db
                    .topic_create(&local, "Adapter race", &TopicLimits::default())
                    .unwrap()
                    .reference;
                db.topic_push_set(&local, &topic, &identity, "all").unwrap();
                db.topic_publish(
                    &local,
                    &TopicPublication {
                        topic: topic.clone(),
                        id: uuid::Uuid::new_v4().to_string(),
                        author: AgentRef {
                            bridge_id: None,
                            agent_id: "inbox-user".into(),
                        },
                        body: "race".into(),
                        parent: None,
                        mentions: vec![],
                    },
                    &TopicLimits::default(),
                )
                .unwrap();
                let (message, claimed_agent) = db.claim().unwrap().unwrap();
                assert_eq!(claimed_agent.name, agent.name);
                (topic, message)
            };
            Self {
                _bound: bound,
                local,
                topic,
                identity,
                agent,
                message,
            }
        }

        pub fn guard(&self, revoke_before_check: bool) -> NoticeGuard {
            let broker = self._bound.broker.clone();
            let actual = broker.topic_push_guard(&self.agent, &self.message.id);
            let local = self.local.clone();
            let topic = self.topic.clone();
            let identity = self.identity.clone();
            std::sync::Arc::new(move || {
                if revoke_before_check {
                    let mut db = broker.db.lock().unwrap();
                    db.topic_push_set(&local, &topic, &identity, "manual")?;
                    db.topic_push_set(&local, &topic, &identity, "all")?;
                }
                actual()
            })
        }

        pub fn unsubscribe(&self) {
            self._bound
                .broker
                .db
                .lock()
                .unwrap()
                .topic_push_set(&self.local, &self.topic, &self.identity, "manual")
                .unwrap();
        }
    }
}

/// Controls remain distinct from ordinary peer messages, including at dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlNotice<'a> {
    Event(&'a str),
    Pairing(&'a str),
}
impl<'a> ControlNotice<'a> {
    pub(crate) fn text(self) -> &'a str {
        match self {
            Self::Event(text) | Self::Pairing(text) => text,
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
            ) | (Self::Claude | Self::Codex, Capability::PairingPrompts)
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
            Self::OpenCode => opencode::message_content(msg, machines),
            Self::Codex => crate::delivery::envelope_with_machines(msg, state_dir, machines),
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
        !session.is_empty() && session.len() <= 200 && !session.chars().any(char::is_control),
        "Invalid remote session"
    );
    if let Some(adapter) = Adapter::for_provider(provider) {
        adapter
            .validate_session(session)
            .with_context(|| match provider {
                Provider::OpenCode => {
                    "Native remote OpenCode session must use ses_ plus 26 ASCII letters/digits"
                }
                _ => "Native remote session must be a UUID",
            })?;
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
    if agent.provider == Provider::Codex {
        return Outcome::failed(
            "Codex controls require the broker's bounded durable notice admission",
        );
    }
    let Some(adapter) = Adapter::for_provider(&agent.provider)
        .filter(|adapter| adapter.supports(Capability::ControlNotices))
    else {
        return Outcome::failed("Client does not support typed control notices");
    };
    adapter.deliver(agent, msg, notice.text()).await
}

/// Observe loaded Codex sessions. Dropping this future cancels the observer.
pub(crate) async fn observe(broker: crate::broker::Broker, codex_home: Option<PathBuf>) {
    let codex_broker = broker.clone();
    let codex_observer = async move {
        if let Some(home) = codex_home {
            codex_discovery::run(codex_broker, home).await;
        }
        std::future::pending::<()>().await;
    };
    codex_observer.await;
}

#[cfg(test)]
mod tests;
