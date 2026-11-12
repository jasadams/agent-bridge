//! The naming standard in code: see docs/naming.md. An ID routes and authorizes;
//! a name is only a label for people. Nothing here is stored or cached.

use crate::protocol::Provider;
use unicode_general_category::{GeneralCategory as Category, get_general_category};

const MAX_LABEL_CHARS: usize = 80;

/// An agent as people should see it. `id` is the exact route; never derive routing from `name`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct AgentRef {
    pub id: String,
    pub name: String,
    pub provider: Option<Provider>,
    pub machine: String,
    #[serde(skip)]
    pub local: bool,
}

impl AgentRef {
    /// Local agents show their name; remote agents show `machine/name`.
    pub fn label(&self) -> String {
        if self.local {
            self.name.clone()
        } else {
            format!("{}/{}", self.machine, self.name)
        }
    }
}

/// Names come from other programs and machines: drop control, format and
/// separator characters, bound the length, and treat an empty result as no name.
pub fn sanitize_label(name: &str) -> Option<String> {
    let name: String = name
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
        .take(MAX_LABEL_CHARS)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// A native client's name for a session, sanitized, else the fallback. The one derivation shared by
/// `resolve_agent` and the `list_agents` rows.
pub fn client_label(provider: &Provider, session: &str, native: Option<&str>) -> String {
    native
        .and_then(sanitize_label)
        .unwrap_or_else(|| fallback_label(provider, session))
}

/// The single no-name fallback: provider plus the last four characters of the session, e.g. "Claude 9f3c".
pub fn fallback_label(provider: &Provider, session: &str) -> String {
    let kind = match provider {
        Provider::Claude => "Claude",
        Provider::Codex => "Codex",
        Provider::OpenCode => "OpenCode",
        Provider::Inbox => "Agent",
    };
    let tail: Vec<char> = session.chars().rev().take(4).collect();
    let tail: String = tail.into_iter().rev().collect();
    format!("{kind} {tail}")
}

/// A person typed `input` where an ID was expected and it is not an exact ID. `candidates` are
/// `(id, name)` for every reachable local agent; `unavailable` lists providers that could not be
/// read. A unique name yields its ID; anything else is an error and never a guess.
pub fn match_name(
    input: &str,
    candidates: &[(String, String)],
    unavailable: &[String],
) -> Result<String, String> {
    let wanted = sanitize_label(input).unwrap_or_default();
    let shown = input.trim();
    let mut ids: Vec<&str> = candidates
        .iter()
        .filter(|(_, name)| !wanted.is_empty() && *name == wanted)
        .map(|(id, _)| id.as_str())
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let gaps = if unavailable.is_empty() {
        String::new()
    } else {
        format!(" ({} could not be read)", unavailable.join(", "))
    };
    match ids.as_slice() {
        [] => Err(format!(
            "Unknown agent {shown:?}{gaps}; use an exact ID or a unique name from list_agents"
        )),
        [id] => Ok((*id).to_owned()),
        many => Err(format!(
            "Agent name {shown:?} is ambiguous; use one exact ID: {}",
            many.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(id: &str, name: &str) -> (String, String) {
        (id.into(), name.into())
    }

    #[test]
    fn unique_name_resolves_and_ambiguity_lists_ids() {
        let all = [
            c("claude-a", "Planner"),
            c("codex-b", "Planner"),
            c("claude-c", "Coder"),
        ];
        assert_eq!(match_name(" Coder ", &all, &[]), Ok("claude-c".into()));
        let err = match_name("Planner", &all, &[]).unwrap_err();
        assert!(err.contains("ambiguous") && err.contains("claude-a") && err.contains("codex-b"));
        assert!(
            match_name("Nobody", &all, &[])
                .unwrap_err()
                .contains("Unknown agent")
        );
        assert!(match_name("\n", &all, &[]).is_err());
    }

    #[test]
    fn unreadable_provider_is_reported_for_unknown_names_only() {
        let all = [c("claude-c", "Coder")];
        let gap = ["codex".to_string()];
        assert_eq!(match_name("Coder", &all, &gap), Ok("claude-c".into()));
        assert!(
            match_name("Nobody", &all, &gap)
                .unwrap_err()
                .contains("codex could not be read")
        );
    }

    #[test]
    fn fallback_is_provider_and_session_tail() {
        assert_eq!(
            fallback_label(&Provider::Claude, "0a1b-9f3c"),
            "Claude 9f3c"
        );
        assert_eq!(fallback_label(&Provider::OpenCode, "ab"), "OpenCode ab");
    }

    #[test]
    fn labels_are_sanitized_and_bounded() {
        assert_eq!(sanitize_label(" Plan\u{202e}ner\n"), Some("Planner".into()));
        assert_eq!(sanitize_label("\u{200b}\n "), None);
        assert_eq!(sanitize_label(&"x".repeat(400)).unwrap().len(), 80);
    }
}
