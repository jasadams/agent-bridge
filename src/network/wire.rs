//! Network messages are deliberately separate from the privileged local Request API.
use crate::protocol::Provider;
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteAgent {
    pub name: String,
    pub provider: Provider,
    pub session: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reference {
    pub peer_id: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub origin_peer: String,
    pub id: String,
    pub source: RemoteAgent,
    pub destination_peer: String,
    pub destination: RemoteAgent,
    pub body: String,
    pub parent: Option<Reference>,
    pub hops: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    /// Optional metadata exchange. Older peers can refuse it without affecting messaging.
    Hello {
        machine_name: String,
    },
    Directory,
    /// Opt-in extension; legacy Directory always retains its original provider set.
    DirectorySupported {
        providers: Vec<String>,
    },
    Send {
        envelope: Box<Envelope>,
    },
    Sync {
        after: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRequest {
    pub version: u32,
    pub operation: Operation,
}

impl WireRequest {
    pub fn new(operation: Operation) -> Self {
        Self {
            version: VERSION,
            operation,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StatusEvent {
    pub sequence: u64,
    pub id: String,
    pub state: String,
    pub detail: Option<String>,
    pub updated: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireResponse {
    Hello {
        machine_name: String,
    },
    Directory {
        agents: Vec<RemoteAgent>,
    },
    SupportedDirectory {
        providers: Vec<String>,
        agents: Vec<RemoteAgent>,
    },
    Accepted {
        id: String,
    },
    Status {
        events: Vec<StatusEvent>,
    },
    Rejected {
        reason: String,
        retryable: bool,
    },
}

/// Explicit provider dispatch bounds negotiation independently of the frame limit.
pub fn supported_providers() -> Vec<String> {
    ["claude", "codex", "inbox", "opencode"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}

pub fn validate_providers(providers: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !providers.is_empty() && providers.len() <= 4,
        "Invalid provider capability count"
    );
    let mut seen = std::collections::HashSet::new();
    for provider in providers {
        anyhow::ensure!(provider.len() <= 16, "Provider capability is too long");
        let _: Provider = provider.parse()?;
        anyhow::ensure!(seen.insert(provider), "Duplicate provider capability");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiation_is_bounded_and_uses_explicit_provider_dispatch() {
        assert!(validate_providers(&supported_providers()).is_ok());
        for providers in [
            vec![],
            vec!["unknown".into()],
            vec!["opencode".into(), "opencode".into()],
            vec!["x".repeat(17)],
            vec!["claude".into(); 5],
        ] {
            assert!(validate_providers(&providers).is_err());
        }
        assert!(
            serde_json::from_str::<RemoteAgent>(
                r#"{"name":"x","provider":"future","session":"unvalidated"}"#
            )
            .is_err()
        );
        assert_eq!(
            serde_json::to_value(WireRequest::new(Operation::Directory)).unwrap(),
            serde_json::json!({"version":1,"operation":{"op":"directory"}})
        );
    }
}
