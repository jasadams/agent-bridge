//! Shared board composer identity and submission contract.
use crate::{board::Name, protocol::Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Owner {
    pub name: String,
    pub provider: Provider,
    pub session: String,
    pub endpoint: String,
}
impl Owner {
    pub fn matches(&self, agent: &crate::protocol::Agent) -> bool {
        agent.enabled
            && self.name == agent.name
            && self.provider == agent.provider
            && self.session == agent.session
            && self.endpoint == agent.endpoint
    }
}
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub id: String,
    pub instance: String,
    pub label: String,
    pub owner: Owner,
}
impl Marker {
    pub fn valid(&self) -> bool {
        uuid::Uuid::parse_str(&self.instance).is_ok_and(|id| id.to_string() == self.instance)
            && crate::board_interaction::valid_id(&self.id)
            && crate::db::valid_name(&self.owner.name)
            && self.owner.name != "user"
            && !self.label.trim().is_empty()
            && self.label.len() <= 500
            && [
                &self.id,
                &self.label,
                &self.owner.name,
                &self.owner.session,
                &self.owner.endpoint,
            ]
            .iter()
            .all(|s| !s.chars().any(char::is_control) && !s.contains("-->") && !s.contains("<!--"))
            && crate::clients::validate_session(&self.owner.provider, &self.owner.session).is_ok()
            && match self.owner.provider {
                Provider::Codex | Provider::Inbox => self.owner.endpoint.is_empty(),
                _ => {
                    !self.owner.endpoint.is_empty()
                        && self.owner.endpoint.len() <= 4096
                        && std::path::Path::new(&self.owner.endpoint).is_absolute()
                }
            }
    }
}
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Submission {
    pub board: Name,
    pub expected_revision: String,
    pub control_id: String,
    pub id: String,
    pub text: String,
}
impl Submission {
    pub fn valid(&self) -> bool {
        let hash = |s: &str| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
        uuid::Uuid::parse_str(&self.id).is_ok_and(|id| id.to_string() == self.id)
            && hash(&self.expected_revision)
            && hash(&self.control_id)
            && !self.text.trim().is_empty()
            && self.text.len() <= crate::protocol::MAX_BODY
    }
}
pub struct Reply {
    pub status: u16,
    pub body: Value,
}
impl Reply {
    pub fn error(status: u16, code: &str, error: impl std::fmt::Display) -> Self {
        Self {
            status,
            body: serde_json::json!({"accepted":false,"code":code,"error":error.to_string()}),
        }
    }
}

#[derive(Debug)]
pub(crate) struct IdConflict(pub &'static str);
impl std::fmt::Display for IdConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for IdConflict {}
