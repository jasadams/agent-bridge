//! The naming standard in code: see docs/naming.md. An ID routes and authorizes;
//! a name is only a label for people. Nothing here is stored or cached.

use crate::protocol::Provider;
use unicode_general_category::{GeneralCategory as Category, get_general_category};

const MAX_LABEL_CHARS: usize = 80;

/// An agent as people should see it. `id` is the exact route; never derive routing from `name`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentRef {
    pub id: String,
    pub name: String,
    pub provider: Option<Provider>,
    pub machine: String,
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

#[cfg(test)]
mod tests {
    use super::*;

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
