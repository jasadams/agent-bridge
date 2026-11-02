//! Request-local native observations joined with durable Bridge policy and bindings.
//! This module never registers native rows or changes permissions.
use crate::{
    db::Db,
    protocol::{Agent, Provider},
};
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Debug)]
pub struct NativeSession {
    pub session: String,
    pub name: Option<String>,
    pub cwd: Option<String>,
    pub status: Option<Value>,
    /// Native last-update time only. Never synthesize this from process start.
    pub updated: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum NativeSnapshot {
    Complete(Vec<NativeSession>),
    Unavailable(String),
    Incomplete(String),
}

#[derive(Debug, Serialize)]
pub struct Availability {
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Directory {
    pub agents: Vec<Value>,
    pub providers: BTreeMap<String, Availability>,
}

impl Directory {
    /// Old array-only clients cannot represent a partial directory truthfully.
    pub fn complete_agents(self) -> Result<Vec<Value>> {
        for (provider, availability) in &self.providers {
            ensure!(
                availability.state == "complete",
                "{provider} native directory {}: {}",
                availability.state,
                availability.detail.as_deref().unwrap_or("retry discovery")
            );
        }
        Ok(self.agents)
    }
}

fn row(agent: &Agent, name: Option<&str>) -> Value {
    let mut value = json!(agent);
    if let Some(name) = name {
        value["display_name"] = json!(name);
        value["qualified_name"] = json!(format!("{}:{name}", agent.provider.label()));
    }
    value
}

/// Providers not yet converted keep their existing behavior during staged rollout.
/// A supplied native provider never falls back to stored membership on failure.
pub fn project(db: &Db, snapshots: &[(Provider, NativeSnapshot)], all: bool) -> Result<Directory> {
    let overlay = db.agents()?;
    let mut directory = Directory {
        agents: vec![],
        providers: BTreeMap::new(),
    };
    let mut selected = HashSet::new();
    for (provider, snapshot) in snapshots {
        ensure!(
            selected.insert(provider.label()),
            "Duplicate native provider snapshot"
        );
        ensure!(
            *provider != Provider::Inbox,
            "Inbox is not a native directory"
        );
        let (state, detail) = match snapshot {
            NativeSnapshot::Complete(_) => ("complete", None),
            NativeSnapshot::Unavailable(detail) => ("unavailable", Some(detail.clone())),
            NativeSnapshot::Incomplete(detail) => ("incomplete", Some(detail.clone())),
        };
        directory
            .providers
            .insert(provider.label().into(), Availability { state, detail });
        let mut observed = HashSet::new();
        if let NativeSnapshot::Complete(sessions) = snapshot {
            for session in sessions {
                crate::clients::validate_session(provider, &session.session)?;
                ensure!(
                    observed.insert(session.session.as_str()),
                    "Duplicate native session identity"
                );
                let canonical = crate::hook::routing_name(provider, &session.session);
                ensure!(
                    !overlay.iter().any(|a| a.name == canonical
                        && (a.provider != *provider || a.session != session.session)),
                    "Canonical native route conflicts with an explicit alias"
                );
                let mut aliases: Vec<_> = overlay
                    .iter()
                    .filter(|a| a.provider == *provider && a.session == session.session)
                    .cloned()
                    .collect();
                if aliases.is_empty() {
                    ensure!(
                        !overlay.iter().any(|a| a.name == canonical),
                        "Canonical native route conflicts with an explicit alias"
                    );
                    let enabled =
                        db.owner_policy_enabled(provider, &session.session, &canonical)?;
                    aliases.push(Agent {
                        name: canonical,
                        provider: provider.clone(),
                        session: session.session.clone(),
                        endpoint: String::new(),
                        enabled,
                    });
                }
                for mut agent in aliases {
                    let binding_enabled = agent.enabled;
                    agent.enabled =
                        db.owner_policy_enabled(provider, &agent.session, &agent.name)?;
                    if !all && !agent.enabled {
                        continue;
                    }
                    let mut value = row(&agent, session.name.as_deref());
                    value["native_available"] = json!(true);
                    value["cwd"] = json!(session.cwd);
                    value["native_status"] = session.status.clone().unwrap_or(Value::Null);
                    if let Some(updated) = session.updated {
                        value["native_updated_at"] = json!(updated);
                    }
                    if !agent.enabled {
                        value["stale_reason"] = json!("disabled");
                    }
                    // Only Claude's adapter owns this socket readiness proof.
                    // Other providers must supply their own exact-ID delivery checks;
                    // neither native presence nor a connector binding is such proof.
                    if *provider == Provider::Claude {
                        let reason = if agent.endpoint.is_empty() {
                            Some("delivery_binding_missing".into())
                        } else if !binding_enabled {
                            Some("delivery_binding_retired".into())
                        } else {
                            crate::clients::claude::stale_reason(std::path::Path::new(
                                &agent.endpoint,
                            ))
                        };
                        value["deliverable"] =
                            json!(agent.enabled && binding_enabled && reason.is_none());
                        value["delivery_binding_enabled"] =
                            json!(binding_enabled && !agent.endpoint.is_empty());
                        if let Some(reason) = reason {
                            value["delivery_unavailable_reason"] = json!(reason);
                        }
                    }
                    directory.agents.push(value);
                }
            }
        }
        if all {
            for agent in overlay
                .iter()
                .filter(|a| a.provider == *provider && !observed.contains(a.session.as_str()))
            {
                let mut agent = agent.clone();
                agent.enabled = db.owner_policy_enabled(provider, &agent.session, &agent.name)?;
                let mut value = row(&agent, None);
                value["native_available"] = json!(false);
                value["stale_reason"] = json!(if state == "complete" {
                    "native_absent"
                } else {
                    "directory_unavailable"
                });
                directory.agents.push(value);
            }
        }
    }
    for agent in overlay
        .iter()
        .filter(|a| !selected.contains(a.provider.label()))
    {
        let reason = if !agent.enabled {
            Some("disabled".into())
        } else if agent.provider == Provider::Claude {
            crate::clients::claude::stale_reason(std::path::Path::new(&agent.endpoint))
        } else {
            None
        };
        if !all && reason.is_some() {
            continue;
        }
        let mut value = row(
            agent,
            db.display_name(&agent.provider, &agent.session)?.as_deref(),
        );
        if let Some(reason) = reason {
            value["stale_reason"] = json!(reason);
        }
        directory.agents.push(value);
    }
    directory
        .agents
        .sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(directory)
}

/// Explicit registered routes retain precedence and do not require native presence.
/// Native titles for converted providers never fall back to cached metadata.
pub fn resolve(db: &Db, snapshots: &[(Provider, NativeSnapshot)], name: &str) -> Result<Agent> {
    if let Ok(agent) = db.agent(name) {
        return Ok(agent);
    }
    let qualifier = name.split_once(':');
    let requested_provider = qualifier
        .map(|(prefix, _)| prefix.parse::<Provider>())
        .transpose()?;
    let label = qualifier.map(|(_, label)| label).unwrap_or(name);
    let directory = project(db, snapshots, false)?;
    for (provider, availability) in &directory.providers {
        if requested_provider
            .as_ref()
            .is_none_or(|selected| selected.label() == provider)
        {
            ensure!(
                availability.state == "complete",
                "{provider} native directory unavailable; use an exact registered route"
            );
        }
    }
    let mut matches: BTreeMap<(String, String), Vec<Agent>> = BTreeMap::new();
    for row in directory.agents {
        let agent: Agent = serde_json::from_value(row.clone())?;
        if requested_provider
            .as_ref()
            .is_some_and(|selected| *selected != agent.provider)
        {
            continue;
        }
        let canonical = crate::hook::routing_name(&agent.provider, &agent.session);
        let converted = snapshots
            .iter()
            .any(|(provider, _)| *provider == agent.provider);
        // Legacy title lookup keeps its existing canonical-route restriction.
        if !converted && agent.name != canonical {
            continue;
        }
        if row["display_name"].as_str() == Some(label) || (converted && canonical == name) {
            matches
                .entry((agent.provider.label().into(), agent.session.clone()))
                .or_default()
                .push(agent);
        }
    }
    ensure!(
        matches.len() <= 1,
        "Native name is ambiguous; use a stable agent ID"
    );
    let (_, mut aliases) = matches
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("Unknown native agent name; inspect the directory"))?;
    aliases.sort_by_key(|agent| {
        (
            agent.name != crate::hook::routing_name(&agent.provider, &agent.session),
            agent.name.clone(),
        )
    });
    let selected = &aliases[0];
    if selected.name != crate::hook::routing_name(&selected.provider, &selected.session) {
        ensure!(
            aliases
                .iter()
                .all(|agent| agent.endpoint == selected.endpoint),
            "Native identity has ambiguous delivery bindings; use an exact registered alias"
        );
    }
    let binding = db.agent(&selected.name).map_err(|_| anyhow::anyhow!("Native session {} is discovered but has no delivery binding; discovery does not register a transport",selected.name))?;
    ensure!(
        binding.provider == selected.provider && binding.session == selected.session,
        "Native identity and delivery binding disagree"
    );
    Ok(binding)
}
