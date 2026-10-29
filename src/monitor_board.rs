//! Board-specific monitor DTOs and safe, bounded Markdown structure.
use crate::{
    board::{self, Name, Snapshot},
    board_interaction::{self, Control, Kind},
    broker::Broker,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResponseRequest {
    pub board: Name,
    pub expected_revision: String,
    pub control_id: String,
    pub value: Desired,
}
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Desired {
    Checkbox { checked: bool },
    Text { text: String },
}
impl ResponseRequest {
    pub fn valid(&self, name: &Name) -> bool {
        &self.board == name
            && hash(&self.expected_revision)
            && hash(&self.control_id)
            && match &self.value {
                Desired::Text { text } => {
                    text.len() <= board::MAX_BOARD_BYTES && !text.contains('\r')
                }
                _ => true,
            }
    }
}
pub(crate) fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}
fn control_id(snapshot: &Snapshot, control: &Control) -> String {
    let mut digest = Sha256::new();
    for part in [
        snapshot.name.as_str(),
        snapshot.revision.as_str(),
        match &control.kind {
            Kind::Checkbox { .. } => "checkbox",
            Kind::Text { .. } => "text",
        },
    ] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    digest.update((control.range.start as u64).to_be_bytes());
    digest.update((control.range.end as u64).to_be_bytes());
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Node {
    Text {
        text: String,
    },
    Element {
        tag: &'static str,
        children: Vec<Node>,
        #[serde(skip_serializing_if = "Option::is_none")]
        href: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        start: Option<u64>,
    },
}
struct Container {
    tag: &'static str,
    children: Vec<Node>,
    href: Option<String>,
    start: Option<u64>,
}
impl Container {
    fn plain(tag: &'static str) -> Self {
        Self {
            tag,
            children: Vec::new(),
            href: None,
            start: None,
        }
    }
    fn finish(mut self) -> Node {
        if self.tag == "thead" {
            self.children = vec![Node::Element {
                tag: "tr",
                children: self.children,
                href: None,
                start: None,
            }];
        }
        Node::Element {
            tag: self.tag,
            children: self.children,
            href: self.href,
            start: self.start,
        }
    }
}
fn safe_url(value: &str) -> Option<String> {
    if value.len() > 2048 || value.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return None;
    }
    let uri: hyper::Uri = value.parse().ok()?;
    if matches!(uri.scheme_str(), Some("http" | "https")) && uri.authority().is_some() {
        Some(value.into())
    } else {
        None
    }
}
fn structure(markdown: &str) -> Option<Vec<Node>> {
    const MAX_NODES: usize = 4096;
    const MAX_DEPTH: usize = 48;
    let mut stack = vec![Container::plain("span")];
    let mut count = 0;
    for event in Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH,
    ) {
        count += 1;
        if count > MAX_NODES || stack.len() > MAX_DEPTH {
            return None;
        }
        match event {
            Event::Start(tag) => {
                let mut node = Container::plain(match &tag {
                    Tag::Paragraph => "p",
                    Tag::Heading { level, .. } => match level {
                        HeadingLevel::H1 => "h1",
                        HeadingLevel::H2 => "h2",
                        HeadingLevel::H3 => "h3",
                        HeadingLevel::H4 => "h4",
                        HeadingLevel::H5 => "h5",
                        HeadingLevel::H6 => "h6",
                    },
                    Tag::BlockQuote(_) => "blockquote",
                    Tag::List(Some(_)) => "ol",
                    Tag::List(None) => "ul",
                    Tag::Item => "li",
                    Tag::CodeBlock(_) => "pre",
                    Tag::Emphasis => "em",
                    Tag::Strong => "strong",
                    Tag::Strikethrough => "del",
                    Tag::Table(_) => "table",
                    Tag::TableHead => "thead",
                    Tag::TableRow => "tr",
                    Tag::TableCell => {
                        if stack.iter().any(|n| n.tag == "thead") {
                            "th"
                        } else {
                            "td"
                        }
                    }
                    Tag::Link { .. } => "a",
                    _ => "span",
                });
                if let Tag::Link { dest_url, .. } = &tag {
                    node.href = safe_url(dest_url);
                    if node.href.is_none() {
                        node.tag = "span";
                    }
                }
                if let Tag::List(Some(start)) = &tag {
                    node.start = Some(*start);
                }
                stack.push(node);
            }
            Event::End(_) => {
                if stack.len() > 1 {
                    let node = stack.pop()?.finish();
                    stack.last_mut()?.children.push(node);
                }
            }
            Event::Text(text)
            | Event::Html(text)
            | Event::InlineHtml(text)
            | Event::FootnoteReference(text) => stack.last_mut()?.children.push(Node::Text {
                text: text.into_string(),
            }),
            Event::Code(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                stack.last_mut()?.children.push(Node::Element {
                    tag: "code",
                    children: vec![Node::Text {
                        text: text.into_string(),
                    }],
                    href: None,
                    start: None,
                })
            }
            Event::SoftBreak => stack
                .last_mut()?
                .children
                .push(Node::Text { text: "\n".into() }),
            Event::HardBreak | Event::Rule => stack.last_mut()?.children.push(Node::Element {
                tag: if matches!(event, Event::Rule) {
                    "hr"
                } else {
                    "br"
                },
                children: vec![],
                href: None,
                start: None,
            }),
            Event::TaskListMarker(checked) => stack.last_mut()?.children.push(Node::Text {
                text: if checked { "☑ " } else { "☐ " }.into(),
            }),
        }
    }
    if stack.len() != 1 {
        return None;
    }
    Some(stack.pop()?.children)
}
pub(crate) fn view(snapshot: &Snapshot) -> Value {
    let controls=board_interaction::controls(&snapshot.markdown).into_iter().map(|control|{
        let id=control_id(snapshot,&control);
        match &control.kind {
            Kind::Checkbox{checked}=>json!({"id":id,"kind":"checkbox","line":control.line,"label":control.label,"checked":checked}),
            Kind::Text{id:field_id,value,..}=>json!({"id":id,"kind":"text","line":control.line,"label":control.label,"field_id":field_id,"value":value,"rebase_key":control.text_rebase_key(&snapshot.name)}),
        }
    }).collect::<Vec<_>>();
    let rendered = structure(&snapshot.markdown);
    let notice = rendered.is_none();
    let nodes = rendered.unwrap_or_else(|| {
        vec![Node::Element {
            tag: "pre",
            children: vec![Node::Text {
                text: snapshot.markdown.clone(),
            }],
            href: None,
            start: None,
        }]
    });
    let mut result = json!({"name":snapshot.name,"revision":snapshot.revision,"exists":snapshot.exists,"markdown":snapshot.markdown,"nodes":nodes,"controls":controls});
    if notice {
        result["render_notice"] =
            json!("This document is complex; showing its exact Markdown as plain text.");
    }
    result
}
pub(crate) struct Reply {
    pub status: u16,
    pub body: Value,
}
fn reply(status: u16, code: &str, error: &str) -> Reply {
    Reply {
        status,
        body: json!({"code":code,"error":error}),
    }
}
fn conflict(snapshot: &Snapshot) -> Reply {
    Reply {
        status: 409,
        body: json!({"applied":false,"conflict":true,"board":view(snapshot),"code":"conflict","error":"The board or control changed. Review the current board before saving again."}),
    }
}
impl Broker {
    pub(crate) fn monitor_boards(&self, generation: u64) -> Reply {
        let _db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Monitor access expired");
        }
        match board::list(&self.state_dir) {
            Ok(boards) => Reply {
                status: 200,
                body: json!({"boards":boards}),
            },
            Err(_) => reply(500, "unavailable", "Boards unavailable"),
        }
    }
    pub(crate) fn monitor_board(
        &self,
        generation: u64,
        name: &Name,
        conditional: Option<&str>,
    ) -> Reply {
        let _db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Monitor access expired");
        }
        match board::read_named(&self.state_dir, name) {
            Ok(snapshot) => Reply {
                status: 200,
                body: if conditional == Some(snapshot.revision.as_str()) {
                    json!({"name":snapshot.name,"revision":snapshot.revision,"unchanged":true})
                } else {
                    view(&snapshot)
                },
            },
            Err(_) => reply(
                404,
                "not_found",
                "Board unavailable; select an existing board",
            ),
        }
    }
    pub(crate) fn monitor_board_response(
        &self,
        generation: u64,
        name: &Name,
        request: ResponseRequest,
    ) -> Reply {
        let _db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Monitor access expired");
        }
        if !request.valid(name) {
            return reply(400, "invalid_response", "Invalid board response");
        }
        let current = match board::read_named(&self.state_dir, name) {
            Ok(v) => v,
            Err(_) => {
                return reply(
                    404,
                    "not_found",
                    "Board unavailable; select an existing board",
                );
            }
        };
        if current.revision != request.expected_revision {
            return conflict(&current);
        }
        let Some(control) = board_interaction::controls(&current.markdown)
            .into_iter()
            .find(|control| control_id(&current, control) == request.control_id)
        else {
            return conflict(&current);
        };
        let updated = match (&control.kind, &request.value) {
            (Kind::Checkbox { .. }, Desired::Checkbox { checked }) => {
                control.set_checked(&current.markdown, *checked)
            }
            (Kind::Text { .. }, Desired::Text { text }) => {
                control.set_text(&current.markdown, text)
            }
            _ => return conflict(&current),
        };
        let updated = match updated {
            Ok(v) => v,
            Err(_) => {
                return reply(
                    400,
                    "invalid_response",
                    "Response cannot fit this board or contains invalid text",
                );
            }
        };
        if updated == current.markdown {
            return Reply {
                status: 200,
                body: json!({"applied":true,"board":view(&current),"unchanged":true}),
            };
        }
        match self.update_board_locked(
            name,
            &current.revision,
            &[board::Edit::Set { markdown: updated }],
            None,
        ) {
            Ok(snapshot) => Reply {
                status: 200,
                body: json!({"applied":true,"board":view(&snapshot)}),
            },
            Err(error) => {
                if let Some(conflict_error) = error.downcast_ref::<board::Conflict>() {
                    conflict(&conflict_error.current)
                } else {
                    // Directory fsync can fail after rename. Never claim unapplied.
                    Reply {
                        status: 500,
                        body: json!({"error":"The save outcome is uncertain. Read the current board and review before retrying.","code":"uncertain","uncertain":true,"name":name}),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, crate::broker::BoundServer, Snapshot) {
        let root = tempfile::tempdir().unwrap();
        let bound = Broker::bind(root.path().into(), "/missing/codex".into()).unwrap();
        let old = board::read(root.path()).unwrap();
        let snapshot = board::update_named(root.path(), &Name::default(), &old.revision, &[board::Edit::Set {
            markdown: "# 世界\r\n- [X] same\r\n- [ ] same\r\n\r\n<!-- board-input: answer -->\r\n> old\r\n<!-- /board-input -->\r\nTail\r\n".into(),
        }]).unwrap();
        (root, bound, snapshot)
    }
    fn request(snapshot: &Snapshot, index: usize, value: Desired) -> ResponseRequest {
        ResponseRequest {
            board: snapshot.name.parse().unwrap(),
            expected_revision: snapshot.revision.clone(),
            control_id: control_id(
                snapshot,
                &board_interaction::controls(&snapshot.markdown)[index],
            ),
            value,
        }
    }
    #[tokio::test]
    async fn desired_states_noop_exact_bytes_rebase_and_cross_board_conflicts() {
        let (root, bound, snapshot) = setup();
        let broker = &bound.broker;
        broker.monitor_control.lock().unwrap().enable(0);
        let same = broker.monitor_board_response(
            1,
            &Name::default(),
            request(&snapshot, 0, Desired::Checkbox { checked: true }),
        );
        assert_eq!(same.status, 200);
        assert_eq!(same.body["unchanged"], true);
        assert_eq!(
            board::read(root.path()).unwrap().markdown,
            snapshot.markdown
        );
        let saved = broker.monitor_board_response(
            1,
            &Name::default(),
            request(&snapshot, 1, Desired::Checkbox { checked: true }),
        );
        assert_eq!(saved.status, 200);
        let current = board::read(root.path()).unwrap();
        assert_eq!(
            current.markdown,
            snapshot.markdown.replacen("- [ ] same", "- [x] same", 1)
        );
        assert_eq!(
            broker
                .monitor_board_response(
                    1,
                    &Name::default(),
                    request(&snapshot, 1, Desired::Checkbox { checked: false })
                )
                .status,
            409
        );
        let old_key = board_interaction::controls(&snapshot.markdown)[2].text_rebase_key("main");
        assert_eq!(
            old_key,
            board_interaction::controls(&current.markdown)[2].text_rebase_key("main")
        );
        assert_ne!(
            old_key,
            board_interaction::controls(&current.markdown)[2].text_rebase_key("other")
        );
        let other: Name = "other".parse().unwrap();
        board::create(root.path(), &other).unwrap();
        let initial = board::read_named(root.path(), &other).unwrap();
        let other_snapshot = board::update_named(
            root.path(),
            &other,
            &initial.revision,
            &[board::Edit::Set {
                markdown: current.markdown.clone(),
            }],
        )
        .unwrap();
        let mut forged = request(&current, 1, Desired::Checkbox { checked: false });
        forged.board = other.clone();
        forged.expected_revision = other_snapshot.revision;
        assert_eq!(broker.monitor_board_response(1, &other, forged).status, 409);
        let response = broker.monitor_board_response(
            1,
            &Name::default(),
            request(
                &current,
                2,
                Desired::Text {
                    text: "你好\n<!-- /board-input -->\n- [x] data".into(),
                },
            ),
        );
        assert_eq!(response.status, 200);
        let latest = board::read(root.path()).unwrap();
        assert!(
            latest
                .markdown
                .ends_with("<!-- /board-input -->\r\nTail\r\n")
        );
        assert_eq!(board_interaction::controls(&latest.markdown).len(), 3);
    }
    #[tokio::test]
    async fn queued_job_checks_revocation_after_database_queue() {
        let (root, bound, snapshot) = setup();
        let broker = bound.broker.clone();
        broker.monitor_control.lock().unwrap().enable(0);
        let guard = broker.db.lock().unwrap();
        let queued = broker.clone();
        let response = request(&snapshot, 1, Desired::Checkbox { checked: true });
        let (ready, waiting) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready.send(()).unwrap();
            queued.monitor_board_response(1, &Name::default(), response)
        });
        waiting.recv().unwrap();
        broker.monitor_control.lock().unwrap().enable(1);
        drop(guard);
        assert_eq!(worker.join().unwrap().status, 401);
        assert_eq!(
            board::read(root.path()).unwrap().markdown,
            snapshot.markdown
        );
    }
    #[test]
    fn renderer_never_produces_resources_html_or_unsafe_navigation_and_bounds_work() {
        let source = "# title\n\n<script>alert(1)</script>\n\n![ALT](https://images.example/tracker)\n\n[x](javascript:alert(1)) [safe](https://example.com/a)\n\n|a|b|\n|-|-|\n|1|2|\n";
        let nodes = serde_json::to_value(structure(source).unwrap()).unwrap();
        fn inspect(value: &Value) {
            if let Some(object) = value.as_object() {
                if let Some(tag) = object.get("tag") {
                    assert!(!matches!(
                        tag.as_str(),
                        Some("img" | "script" | "iframe" | "svg")
                    ));
                }
                if let Some(href) = object.get("href") {
                    assert!(href.as_str().unwrap().starts_with("https://example.com/"));
                }
                for value in object.values() {
                    inspect(value);
                }
            } else if let Some(array) = value.as_array() {
                for value in array {
                    inspect(value);
                }
            }
        }
        inspect(&nodes);
        let text = nodes.to_string();
        assert!(text.contains("ALT"));
        assert!(text.contains("<script>"));
        assert!(!text.contains("images.example"));
        assert!(!text.contains("javascript:"));
        assert!(structure(&"- item\n".repeat(4000)).is_none());
    }
}
