//! Shared board composer identity and submission contract.
use crate::{board::Name, protocol::Provider};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Presentation is separate from the immutable marker and submission identity.
pub(crate) type DisplayLabels = std::collections::BTreeMap<String, String>;

fn compact_label(value: &str) -> Option<String> {
    let mut label = String::new();
    for ch in value.trim().chars().filter(|ch| !ch.is_control() && !matches!(*ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2066}'..='\u{2069}')) {
        if label.len() + ch.len_utf8() > 64 { break; }
        label.push(ch);
    }
    (!label.trim().is_empty()).then(|| label.trim().to_owned())
}

fn generated_name(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    let value = lower.as_str();
    ["claude-", "codex-", "opencode-"].iter().any(|prefix| {
        value.find(prefix).is_some_and(|index| {
            value[index + prefix.len()..]
                .get(..36)
                .is_some_and(|id| uuid::Uuid::parse_str(id).is_ok())
        })
    }) || value.contains("opencode-ses_")
}

pub(crate) fn fallback_label(owner: &Owner, authored: &str) -> String {
    if !generated_name(&owner.name)
        && owner.name != crate::clients::lifecycle::routing_name(&owner.provider, &owner.session)
        && let Some(alias) = compact_label(&owner.name)
    {
        return alias;
    }
    if !authored.contains(&owner.name)
        && !generated_name(authored.trim_start_matches("Message "))
        && let Some(label) = compact_label(authored)
    {
        return label;
    }
    match owner.provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
        Provider::OpenCode => "OpenCode",
        Provider::Inbox => "Agent",
    }
    .into()
}

pub(crate) fn display_labels(
    db: &crate::db::Db,
    snapshot: &crate::board::Snapshot,
) -> anyhow::Result<DisplayLabels> {
    let mut labels = DisplayLabels::new();
    for control in crate::board_interaction::controls(&snapshot.markdown) {
        let crate::board_interaction::Kind::Message { owner, .. } = &control.kind else {
            continue;
        };
        let mut label = fallback_label(owner, &control.label);
        // A same-name replacement is never a source of metadata for its predecessor.
        if let Ok(agent) = db.agent(&owner.name)
            && owner.matches(&agent)
            && owner.name
                == crate::clients::lifecycle::routing_name(&owner.provider, &owner.session)
            && let Some(title) = db.display_name(&owner.provider, &owner.session)?
            && let Some(title) = compact_label(&title)
            && !generated_name(&title)
        {
            label = title;
        }
        if let Some(key) = control.message_key(&snapshot.control_scope()) {
            labels.insert(key, label);
        }
    }
    let mut totals = std::collections::BTreeMap::new();
    for label in labels
        .values()
        .filter(|label| ["Claude", "Codex", "OpenCode", "Agent"].contains(&label.as_str()))
    {
        *totals.entry(label.clone()).or_insert(0usize) += 1;
    }
    let mut ordinals = std::collections::BTreeMap::new();
    for label in labels.values_mut() {
        if totals.get(label).is_some_and(|total| *total > 1) {
            let ordinal = ordinals.entry(label.clone()).or_insert(0usize);
            *ordinal += 1;
            *label = format!("{label} ({ordinal})");
        }
    }
    Ok(labels)
}

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

#[cfg(test)]
mod display_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn native_labels_are_bounded_presentation_and_never_borrow_replacement_metadata() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut db = crate::db::Db::open(&root.path().join("db")).unwrap();
        let session = "885b5a56-af8d-4a72-9b43-8ad930e945b6";
        let name = format!("claude-{session}");
        let mut agent = crate::protocol::Agent {
            name: name.clone(),
            provider: Provider::Claude,
            session: session.into(),
            endpoint: "/tmp/fixture.sock".into(),
            enabled: true,
        };
        db.register(&agent, false).unwrap();
        let marker = Marker {
            id: "compose".into(),
            instance: uuid::Uuid::new_v4().to_string(),
            label: format!("Message {name}"),
            owner: Owner {
                name,
                provider: Provider::Claude,
                session: session.into(),
                endpoint: agent.endpoint.clone(),
            },
        };
        let markdown = format!(
            "<!-- board-message: {} -->\n",
            serde_json::to_string(&marker).unwrap()
        );
        let old = crate::board::read_named(root.path(), &Name::default()).unwrap();
        let snapshot = crate::board::update_named(
            root.path(),
            &Name::default(),
            &old.revision,
            &[crate::board::Edit::Set { markdown }],
        )
        .unwrap();
        assert_eq!(
            display_labels(&db, &snapshot)
                .unwrap()
                .values()
                .next()
                .unwrap(),
            "Claude"
        );
        db.observe_claude_name(session, "Coordinator").unwrap();
        let first = display_labels(&db, &snapshot).unwrap();
        assert_eq!(first.values().next().unwrap(), "Coordinator");
        db.observe_claude_name(session, "Planner").unwrap();
        assert_eq!(
            display_labels(&db, &snapshot)
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            first.keys().collect::<Vec<_>>()
        );
        assert_eq!(
            display_labels(&db, &snapshot)
                .unwrap()
                .values()
                .next()
                .unwrap(),
            "Planner"
        );
        db.observe_claude_name(session, &format!("\u{1b}{}", "界".repeat(10000)))
            .unwrap();
        let bounded = display_labels(&db, &snapshot).unwrap();
        let title = bounded.values().next().unwrap();
        assert!(title.len() <= 64 && !title.chars().any(char::is_control));
        assert_eq!(
            compact_label("Plan\u{202e}ner\n界").as_deref(),
            Some("Planner界")
        );
        assert!(generated_name(&format!("Message {}", marker.owner.name)));
        agent.session = uuid::Uuid::new_v4().to_string();
        db.register(&agent, true).unwrap();
        db.observe_claude_name(&agent.session, "Replacement owner")
            .unwrap();
        assert_eq!(
            display_labels(&db, &snapshot)
                .unwrap()
                .values()
                .next()
                .unwrap(),
            "Claude"
        );
        assert_eq!(
            fallback_label(&marker.owner, "Support desk"),
            "Support desk"
        );
        let mut multiple = snapshot.clone();
        let mut unknown = marker.clone();
        unknown.id = "unknown".into();
        unknown.owner.session = uuid::Uuid::new_v4().to_string();
        unknown.owner.name = format!("claude-{}", unknown.owner.session);
        unknown.label = format!("Message {}", unknown.owner.name);
        multiple.markdown.push_str(&format!(
            "<!-- board-message: {} -->\n",
            serde_json::to_string(&unknown).unwrap()
        ));
        let defaults = display_labels(&db, &multiple).unwrap();
        assert_eq!(
            defaults
                .values()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            ["Claude (1)".to_owned(), "Claude (2)".to_owned()].into()
        );
        assert_eq!(
            crate::board::read_named(root.path(), &Name::default())
                .unwrap()
                .revision,
            snapshot.revision
        );
    }
}
