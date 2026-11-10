//! Board-specific monitor DTOs and safe, bounded Markdown structure.
use crate::{
    board::{self, Name, Snapshot},
    board_interaction::{self, Kind},
    broker::Broker,
};
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResponseRequest {
    pub board: Name,
    pub expected_revision: String,
    pub control_id: String,
    pub value: Desired,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LifecycleRequest {
    pub board: Name,
    pub expected_revision: String,
}
impl LifecycleRequest {
    pub(crate) fn valid(&self, name: &Name) -> bool {
        &self.board == name && hash(&self.expected_revision)
    }
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
use board_interaction::control_id;
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Node {
    Control {
        id: String,
    },
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
fn structure(snapshot: &Snapshot, controls: &[board_interaction::Control]) -> Option<Vec<Node>> {
    let mut bytes = snapshot.markdown.as_bytes().to_vec();
    let mut fields = Vec::new();
    for control in controls {
        match control.kind {
            Kind::Text { .. } => {
                let start = snapshot.markdown[..control.range.start.saturating_sub(1)]
                    .rfind('\n')
                    .map_or(0, |n| n + 1);
                let end = snapshot.markdown[control.range.end..]
                    .find('\n')
                    .map_or(bytes.len(), |n| control.range.end + n + 1);
                fields.push((start, control));
                for byte in &mut bytes[start..end] {
                    if *byte != b'\n' && *byte != b'\r' {
                        *byte = b' ';
                    }
                }
                bytes[start..start + 8].copy_from_slice(b"<!-- -->");
            }
            Kind::Message { .. } => {
                for byte in &mut bytes[control.range.clone()] {
                    if *byte != b'\n' && *byte != b'\r' {
                        *byte = b' ';
                    }
                }
            }
            _ => (),
        }
    }
    let markdown = String::from_utf8(bytes).ok()?;
    markdown_nodes(
        &markdown,
        Some(ControlHooks {
            snapshot,
            controls,
            fields: &fields,
        }),
    )
}
/// Board-only context: lets the shared parser place interactive controls into the tree.
struct ControlHooks<'a> {
    snapshot: &'a Snapshot,
    controls: &'a [board_interaction::Control],
    fields: &'a [(usize, &'a board_interaction::Control)],
}
/// Safe, bounded Markdown structure shared by boards and topic posts. Raw HTML is downgraded to
/// text, links must be http(s), and the tree is capped in size and depth. `hooks` is `Some` only
/// for boards; topic posts have no controls.
fn markdown_nodes(markdown: &str, hooks: Option<ControlHooks<'_>>) -> Option<Vec<Node>> {
    const MAX_NODES: usize = 4096;
    const MAX_DEPTH: usize = 48;
    let mut stack = vec![Container::plain("span")];
    let mut count = 0;
    for (event, range) in Parser::new_ext(
        markdown,
        Options::ENABLE_TABLES | Options::ENABLE_TASKLISTS | Options::ENABLE_STRIKETHROUGH,
    )
    .into_offset_iter()
    {
        count += 1;
        if matches!(event, Event::Html(_))
            && let Some(hooks) = hooks.as_ref()
            && let Some((_, control)) = hooks.fields.iter().find(|(start, _)| *start == range.start)
        {
            stack.last_mut()?.children.push(Node::Control {
                id: control_id(hooks.snapshot, control),
            });
            continue;
        }
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
            Event::TaskListMarker(checked) => {
                let control = hooks.as_ref().and_then(|hooks| {
                    hooks
                        .controls
                        .iter()
                        .find(|control| {
                            matches!(control.kind, Kind::Checkbox { .. })
                                && range.contains(&control.range.start)
                        })
                        .map(|control| (hooks.snapshot, control))
                });
                stack
                    .last_mut()?
                    .children
                    .push(if let Some((snapshot, control)) = control {
                        Node::Control {
                            id: control_id(snapshot, control),
                        }
                    } else {
                        Node::Text {
                            text: if checked { "☑ " } else { "☐ " }.into(),
                        }
                    });
            }
        }
    }
    if stack.len() != 1 {
        return None;
    }
    Some(stack.pop()?.children)
}
/// Parsed nodes for a topic post body. `None` when the body exceeds the render bounds, in which
/// case readers show the plain text. Topic posts carry no board controls.
pub(crate) fn topic_nodes(markdown: &str) -> Option<Value> {
    markdown_nodes(markdown, None).map(|nodes| json!(nodes))
}
/// Total node JSON allowed in one topic page. Bounded so a page of maximum-size posts cannot
/// multiply the response well past the stored bodies.
const TOPIC_NODES_BUDGET: usize = 2 * 1024 * 1024;
/// Adds `body_nodes` to every post of a serialized topic page. A post whose body is too complex,
/// or that would exceed the page budget, gets `null` and is shown as plain text.
pub(crate) fn annotate_topic_page(page: &mut Value) {
    annotate_topic_page_within(page, TOPIC_NODES_BUDGET);
}
fn annotate_topic_page_within(page: &mut Value, budget: usize) {
    let Some(posts) = page.get_mut("posts").and_then(Value::as_array_mut) else {
        return;
    };
    let mut used = 0usize;
    for post in posts {
        let Some(post) = post.as_object_mut() else {
            continue;
        };
        let nodes = post
            .get("body")
            .and_then(Value::as_str)
            .and_then(topic_nodes)
            .filter(|nodes| {
                used = used.saturating_add(nodes.to_string().len());
                used <= budget
            });
        post.insert("body_nodes".into(), nodes.unwrap_or(Value::Null));
    }
}
fn nodes_text_contains_input(nodes: &[Node]) -> bool {
    nodes.iter().any(|node| match node {
        Node::Text { text } => text.contains("<!-- board-input:"),
        Node::Element { children, .. } => nodes_text_contains_input(children),
        Node::Control { .. } => false,
    })
}
pub(crate) fn view(snapshot: &Snapshot) -> Value {
    let controls=board_interaction::controls(&snapshot.markdown).into_iter().map(|control|{
        let id=control_id(snapshot,&control);
        match &control.kind {
            Kind::Checkbox{checked}=>json!({"id":id,"kind":"checkbox","line":control.line,"label":control.label,"checked":checked}),
            Kind::Message{id:field_id,owner,..}=>json!({"id":id,"kind":"message","line":control.line,"label":control.label,"field_id":field_id,"owner":owner,"rebase_key":control.message_key(&snapshot.control_scope())}),
            Kind::Text{id:field_id,value,..}=>json!({"id":id,"kind":"text","line":control.line,"label":control.label,"field_id":field_id,"value":value,"rebase_key":control.text_rebase_key(&snapshot.control_scope())}),
        }
    }).collect::<Vec<_>>();
    let display = board_interaction::display_markdown(&snapshot.markdown);
    let parsed_controls = board_interaction::controls(&snapshot.markdown);
    let rendered = structure(snapshot, &parsed_controls);
    let notice = rendered.is_none();
    let nodes = rendered.unwrap_or_else(|| {
        vec![Node::Element {
            tag: "pre",
            children: vec![Node::Text {
                text: display.to_string(),
            }],
            href: None,
            start: None,
        }]
    });
    let invalid_fields = nodes_text_contains_input(&nodes);
    let mut result = json!({"name":snapshot.name,"revision":snapshot.revision,"exists":snapshot.exists,"last_updated":snapshot.last_updated,"last_updated_source":snapshot.last_updated_source,"markdown":snapshot.markdown,"display_markdown":display.as_ref(),"nodes":nodes,"controls":controls});
    if invalid_fields {
        result["control_notice"] = json!(
            "Unrecognized input markers and code examples remain readable and cannot be edited here."
        );
    }
    if notice {
        result["render_notice"] = json!("This document is complex; showing plain text.");
    }
    lifecycle_metadata(&mut result, snapshot);
    result
}

fn lifecycle_metadata(value: &mut Value, snapshot: &Snapshot) {
    let metadata =
        serde_json::to_value(&snapshot.lifecycle).expect("Scalar lifecycle metadata serializes");
    let Value::Object(metadata) = metadata else {
        unreachable!("Lifecycle DTO is an object")
    };
    value
        .as_object_mut()
        .expect("Board view is an object")
        .extend(metadata);
    value["read_only"] = json!(!snapshot.writable());
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
/// Content identity plus authoritative metadata observed by the live board feed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct BoardIdentity {
    pub message_labels: crate::board_messages::DisplayLabels,
    pub revision: String,
    pub exists: bool,
    pub last_updated: Option<i64>,
    pub last_updated_source: board::LastUpdatedSource,
    pub read_only: bool,
    #[serde(flatten)]
    pub lifecycle: board::Lifecycle,
}

impl Broker {
    pub(crate) fn monitor_board_revision(
        &self,
        generation: u64,
        name: &Name,
    ) -> Result<BoardIdentity, Reply> {
        let db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return Err(reply(401, "unauthorized", "Web access expired"));
        }
        crate::board_lifecycle::inspect(&db, &self.state_dir, name, crate::db::now())
            .and_then(|snapshot| {
                Ok(BoardIdentity {
                    message_labels: crate::board_messages::display_labels(&db, &snapshot)?,
                    revision: snapshot.revision,
                    exists: snapshot.exists,
                    last_updated: snapshot.last_updated,
                    last_updated_source: snapshot.last_updated_source,
                    read_only: snapshot.lifecycle.state != board::LifecycleState::Active,
                    lifecycle: snapshot.lifecycle,
                })
            })
            .map_err(|_| {
                reply(
                    404,
                    "not_found",
                    "Board unavailable; select an existing board",
                )
            })
    }
    pub(crate) fn monitor_boards(&self, generation: u64) -> Reply {
        let db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Web access expired");
        }
        match self.list_boards_locked(&db) {
            Ok(boards) => Reply {
                status: 200,
                body: json!({"boards":boards}),
            },
            Err(_) => reply(500, "unavailable", "Boards unavailable"),
        }
    }
    pub(crate) fn monitor_archived_boards(&self, generation: u64) -> Reply {
        let db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Web access expired");
        }
        match crate::board_lifecycle::archived(&db, &self.state_dir, crate::db::now()) {
            Ok(boards) => Reply {
                status: 200,
                body: json!({"boards":boards}),
            },
            Err(_) => reply(500, "unavailable", "Archived boards unavailable"),
        }
    }

    pub(crate) fn monitor_board_lifecycle(
        &self,
        generation: u64,
        name: &Name,
        request: LifecycleRequest,
        restore: bool,
    ) -> Reply {
        let db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Web access expired");
        }
        if !request.valid(name) {
            return reply(400, "invalid", "Invalid board lifecycle request");
        }
        if !restore && name.is_main() {
            return reply(
                422,
                "protected",
                "Main is always available and cannot be archived",
            );
        }
        let now = crate::db::now();
        let current = match crate::board_lifecycle::inspect(&db, &self.state_dir, name, now) {
            Ok(board) => board,
            Err(_) => {
                return reply(
                    404,
                    "not_found",
                    "Board unavailable; it may have been permanently deleted",
                );
            }
        };
        if restore && current.lifecycle.state == board::LifecycleState::Purging {
            return Reply {
                status: 410,
                body: json!({"code":"expired","error":"Archive expired; restoration is no longer available","board":view(&current)}),
            };
        }
        if (restore && current.writable())
            || (!restore && current.writable() && current.revision != request.expected_revision)
        {
            return conflict(&current);
        }
        let result = if restore {
            crate::board_lifecycle::restore(
                &db,
                &self.state_dir,
                name,
                &request.expected_revision,
                now,
            )
        } else {
            self.archive_board_locked(&db, name, &request.expected_revision, now)
        };
        match result {
            Ok(board) => Reply {
                status: 200,
                body: json!({"applied":true,"board":view(&board)}),
            },
            Err(error) => {
                if let Some(conflict_error) = error.downcast_ref::<board::Conflict>() {
                    return conflict(&conflict_error.current);
                }
                let observed =
                    crate::board_lifecycle::inspect(&db, &self.state_dir, name, now).ok();
                let pending = observed
                    .as_ref()
                    .is_some_and(|board| board.lifecycle.state == board::LifecycleState::Restoring);
                let mut response = reply(
                    if pending { 503 } else { 500 },
                    if pending { "pending" } else { "uncertain" },
                    if pending {
                        "Restore is pending recovery; inspect this board before an explicit retry"
                    } else {
                        "The lifecycle operation was not confirmed; inspect this board before retrying"
                    },
                );
                if let Some(board) = observed {
                    response.body["board"] = view(&board);
                }
                response
            }
        }
    }
    pub(crate) fn monitor_board(
        &self,
        generation: u64,
        name: &Name,
        conditional: Option<&str>,
    ) -> Reply {
        let db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Web access expired");
        }
        match crate::board_lifecycle::inspect(&db, &self.state_dir, name, crate::db::now()) {
            Ok(snapshot) => {
                let labels = match crate::board_messages::display_labels(&db, &snapshot) {
                    Ok(labels) => labels,
                    Err(_) => return reply(500, "unavailable", "Agent labels could not be read"),
                };
                let mut body = if conditional == Some(snapshot.revision.as_str()) {
                    let mut value = json!({"name":snapshot.name,"revision":snapshot.revision,"exists":snapshot.exists,"last_updated":snapshot.last_updated,"last_updated_source":snapshot.last_updated_source,"unchanged":true});
                    lifecycle_metadata(&mut value, &snapshot);
                    value
                } else {
                    view(&snapshot)
                };
                body["message_labels"] = json!(labels);
                Reply { status: 200, body }
            }
            Err(_) => reply(
                404,
                "not_found",
                "Board unavailable; select an existing board",
            ),
        }
    }
    pub(crate) fn monitor_board_message(
        &self,
        generation: u64,
        name: &Name,
        request: crate::board_messages::Submission,
    ) -> Reply {
        let db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Web access expired");
        }
        if &request.board != name || !request.valid() {
            return reply(400, "invalid", "Invalid board message submission");
        }
        let result = self.board_message_submit_locked(&db, &request);
        let mut body = result.body;
        if matches!(body["code"].as_str(), Some("conflict" | "archived"))
            && let Ok(snapshot) =
                crate::board_lifecycle::inspect(&db, &self.state_dir, name, crate::db::now())
        {
            body["board"] = view(&snapshot);
        }
        Reply {
            status: result.status,
            body,
        }
    }
    pub(crate) fn monitor_board_response(
        &self,
        generation: u64,
        name: &Name,
        request: ResponseRequest,
    ) -> Reply {
        let db = self.db.lock().unwrap();
        let access = self.monitor_control.lock().unwrap();
        if !access.current(generation) {
            return reply(401, "unauthorized", "Web access expired");
        }
        if !request.valid(name) {
            return reply(400, "invalid_response", "Invalid board response");
        }
        let current = match self.read_board_locked(&db, name) {
            Ok(v) => v,
            Err(_) => {
                if let Ok(board) =
                    crate::board_lifecycle::inspect(&db, &self.state_dir, name, crate::db::now())
                    && !board.writable()
                {
                    return Reply {
                        status: 409,
                        body: json!({"code":"archived","error":"This board is not active; the response was not saved","board":view(&board)}),
                    };
                }
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
                body: json!({"applied":true,"board":view(&current),"unchanged":true,"saved_control_id":control_id(&current, &control)}),
            };
        }
        match self.update_board_locked(
            &db,
            name,
            &current.revision,
            &[board::Edit::Set { markdown: updated }],
            None,
        ) {
            Ok(snapshot) => {
                let saved = board_interaction::controls(&snapshot.markdown)
                    .into_iter()
                    .find(|candidate| candidate.range.start == control.range.start)
                    .map(|candidate| control_id(&snapshot, &candidate));
                Reply {
                    status: 200,
                    body: json!({"applied":true,"board":view(&snapshot),"saved_control_id":saved}),
                }
            }
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
    fn structure(markdown: &str) -> Option<Vec<Node>> {
        let snapshot = Snapshot {
            name: "main".into(),
            markdown: markdown.into(),
            revision: "a".repeat(64),
            path: std::path::PathBuf::new(),
            exists: true,
            last_updated: None,
            last_updated_source: board::LastUpdatedSource::Unknown,
            lifecycle: Default::default(),
            file_identity: None,
        };
        super::structure(&snapshot, &board_interaction::controls(markdown))
    }
    fn setup() -> (tempfile::TempDir, crate::broker::BoundServer, Snapshot) {
        let root = tempfile::tempdir().unwrap();
        let bound = Broker::bind(root.path().into(), "/missing/codex".into()).unwrap();
        let old = board::read(root.path()).unwrap();
        let snapshot = board::update_named(root.path(), &Name::default(), &old.revision, &[board::Edit::Set {
            markdown: "# 世界\r\n- [X] same\r\n- [ ] same\r\n\r\n<!-- board-input: answer -->\r\n> old\r\n<!-- /board-input -->\r\nTail\r\n".into(),
        }]).unwrap();
        (root, bound, snapshot)
    }
    #[test]
    fn topic_nodes_render_structure_and_stay_inert() {
        let source = "# Title\n\n- a\n- b\n\n| h |\n|---|\n| c |\n\n```sh\nls\n```\n\n[ok](https://example.com/x) [bad](javascript:alert(1)) <script>alert(1)</script>\n";
        let nodes = topic_nodes(source).unwrap().to_string();
        for tag in ["h1", "ul", "li", "table", "thead", "th", "td", "pre", "a"] {
            assert!(
                nodes.contains(&format!("\"tag\":\"{tag}\"")),
                "missing {tag}: {nodes}"
            );
        }
        assert!(nodes.contains("\"href\":\"https://example.com/x\""));
        assert!(!nodes.contains("javascript:"), "unsafe link kept: {nodes}");
        assert!(!nodes.contains("\"tag\":\"script\""));
        assert!(
            nodes.contains("\"text\":\"<script>\""),
            "raw HTML must survive only as text: {nodes}"
        );
    }
    #[test]
    fn topic_nodes_have_no_board_controls() {
        let nodes = topic_nodes(
            "- [x] done\n- [ ] todo\n\n<!-- board-input: a -->\n> x\n<!-- /board-input -->\n",
        )
        .unwrap()
        .to_string();
        assert!(!nodes.contains("\"kind\":\"control\""), "{nodes}");
        assert!(nodes.contains('☑') && nodes.contains('☐'), "{nodes}");
        assert!(nodes.contains("board-input: a"), "{nodes}");
    }
    #[test]
    fn topic_nodes_are_bounded() {
        assert!(topic_nodes(&format!("{}x", "> ".repeat(60))).is_none());
        assert!(topic_nodes(&"a\n\n".repeat(5000)).is_none());
        assert!(topic_nodes("").unwrap().as_array().unwrap().is_empty());
    }
    #[test]
    fn topic_pages_get_nodes_within_a_budget() {
        let page = || json!({"posts":[{"id":"a","body":"**one**"},{"id":"b","body":"two"},{"id":"c","body":"three"}],"high_water":3});
        let mut full = page();
        annotate_topic_page(&mut full);
        assert!(
            full["posts"]
                .as_array()
                .unwrap()
                .iter()
                .all(|post| post["body_nodes"].is_array())
        );
        assert_eq!(full["high_water"], 3);
        assert_eq!(full["posts"][0]["body"], "**one**");
        // A tight budget keeps earlier posts and degrades later ones to plain text.
        let first = topic_nodes("**one**").unwrap().to_string().len();
        let mut small = page();
        annotate_topic_page_within(&mut small, first);
        assert!(small["posts"][0]["body_nodes"].is_array());
        assert!(small["posts"][1]["body_nodes"].is_null());
        assert!(small["posts"][2]["body_nodes"].is_null());
        // Pages without posts, and malformed posts, are left alone or degrade safely.
        let mut none = json!({"other":1});
        annotate_topic_page(&mut none);
        assert_eq!(none, json!({"other":1}));
        let mut odd = json!({"posts":[1,{"body":7}]});
        annotate_topic_page(&mut odd);
        assert_eq!(odd["posts"][0], 1);
        assert!(odd["posts"][1]["body_nodes"].is_null());
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
    async fn main_cosmetic_labels_do_not_change_board_identity() {
        let (root, bound, old) = setup();
        let broker = &bound.broker;
        let session = "885b5a56-af8d-4a72-9b43-8ad930e945b6";
        let owner = crate::board_messages::Owner {
            name: "coordinator".into(),
            provider: crate::protocol::Provider::Claude,
            session: session.into(),
            endpoint: "/tmp/coordinator.sock".into(),
        };
        let marker = crate::board_messages::Marker {
            id: "main-title".into(),
            instance: uuid::Uuid::new_v4().to_string(),
            label: "Message coordinator".into(),
            owner: owner.clone(),
        };
        let before = board::update_named(
            root.path(),
            &Name::default(),
            &old.revision,
            &[board::Edit::Append {
                text: format!(
                    "\n<!-- board-message: {} -->\n",
                    serde_json::to_string(&marker).unwrap()
                ),
            }],
        )
        .unwrap();
        broker
            .db
            .lock()
            .unwrap()
            .register(
                &crate::protocol::Agent {
                    name: owner.name.clone(),
                    provider: owner.provider.clone(),
                    session: owner.session.clone(),
                    endpoint: String::new(),
                },
                false,
            )
            .unwrap();
        broker.monitor_control.lock().unwrap().enable(0);
        let first = broker.monitor_board(1, &Name::default(), None);
        assert_eq!(first.status, 200);
        assert!(
            first.body["message_labels"]
                .as_object()
                .unwrap()
                .values()
                .any(|value| value == "coordinator")
        );
        let identity = broker
            .monitor_board_revision(1, &Name::default())
            .unwrap_or_else(|reply| {
                panic!(
                    "Main label identity read failed: HTTP {}: {}",
                    reply.status, reply.body
                )
            });
        let updated = broker
            .monitor_board_revision(1, &Name::default())
            .unwrap_or_else(|reply| {
                panic!(
                    "Main label identity read failed: HTTP {}: {}",
                    reply.status, reply.body
                )
            });
        assert_eq!(identity.message_labels, updated.message_labels);
        assert_eq!(identity.revision, updated.revision);
        let after = board::read(root.path()).unwrap();
        assert_eq!(
            (after.markdown, after.revision, after.last_updated),
            (before.markdown, before.revision, before.last_updated)
        );
    }

    #[test]
    fn inline_controls_are_validated_and_source_associated() {
        let source = "# 世界\r\n- [ ] one\r\n  - [x] nested\r\n\r\n<!-- board-input: a -->\r\n> old\r\n<!-- /board-input -->\r\n<!-- board-input: b -->\r\n<!-- /board-input -->\r\n\r\n```md\r\n- [ ] inert\r\n<!-- board-input: example -->\r\n```\r\n";
        let snapshot = Snapshot {
            name: "main".into(),
            revision: "a".repeat(64),
            markdown: source.into(),
            exists: true,
            last_updated: None,
            last_updated_source: board::LastUpdatedSource::Unknown,
            lifecycle: Default::default(),
            file_identity: None,
            path: std::path::PathBuf::new(),
        };
        let result = view(&snapshot);
        let nodes = result["nodes"].to_string();
        assert_eq!(result["controls"].as_array().unwrap().len(), 4);
        for control in result["controls"].as_array().unwrap() {
            assert_eq!(nodes.matches(control["id"].as_str().unwrap()).count(), 1);
        }
        assert!(!nodes.contains("board-input: a"));
        assert!(!nodes.contains("board-input: b"));
        assert!(nodes.contains("board-input: example"));
        assert_eq!(result["markdown"], source);
        for source in [
            "<!-- board-input: a -->\n> one\n<!-- /board-input -->\n<!-- board-input: a -->\n> two\n<!-- /board-input -->\n",
            "<!-- board-input: a -->\n<!-- board-input: b -->\n> nested\n<!-- /board-input -->\n",
            "<!-- board-input: a -->\n> unclosed\n",
        ] {
            let mut snapshot = snapshot.clone();
            snapshot.markdown = source.into();
            let result = view(&snapshot);
            assert!(result["controls"].as_array().unwrap().is_empty());
            assert!(result["nodes"].to_string().contains("board-input:"));
        }
    }
    #[tokio::test]
    async fn message_send_retry_and_conflict_preserve_board_timestamp_and_projection() {
        use crate::protocol::{Agent, Provider, Request};
        use std::os::unix::fs::MetadataExt;
        let (root, bound, old) = setup();
        let broker = &bound.broker;
        let owner = Agent {
            name: "timestamp-recipient".into(),
            provider: Provider::Inbox,
            session: uuid::Uuid::new_v4().to_string(),
            endpoint: String::new(),
        };
        broker
            .request(Request::Register {
                name: owner.name.clone(),
                provider: owner.provider.clone(),
                session: owner.session.clone(),
                replace: false,
            })
            .unwrap();
        let added = broker
            .request(Request::AddBoardMessageInput {
                agent: owner.name,
                board: Default::default(),
                expected_revision: old.revision,
                id: "timestamp-message".into(),
                label: "Message recipient".into(),
            })
            .unwrap();
        assert_eq!(added["applied"], true);
        let before: Snapshot = serde_json::from_value(added["board"].clone()).unwrap();
        assert!(before.last_updated.is_some());
        let file_before = std::fs::metadata(board::read(&broker.state_dir).unwrap().path).unwrap();
        broker.monitor_control.lock().unwrap().enable(0);
        let control = board_interaction::controls(&before.markdown)
            .into_iter()
            .find(|control| matches!(control.kind, Kind::Message { .. }))
            .unwrap();
        let mut submission = crate::board_messages::Submission {
            board: Default::default(),
            expected_revision: before.revision.clone(),
            control_id: control_id(&before, &control),
            id: uuid::Uuid::new_v4().to_string(),
            text: "Only a native message, never a board edit".into(),
        };
        let sent = broker.monitor_board_message(1, &Name::default(), submission.clone());
        assert_eq!(sent.status, 200);
        assert_eq!(sent.body["accepted"], true);
        assert_eq!(
            broker
                .monitor_board_message(1, &Name::default(), submission.clone())
                .body["duplicate"],
            true
        );
        submission.id = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            broker
                .request(Request::BoardMessageSend {
                    request: submission.clone()
                })
                .unwrap()["accepted"],
            true
        );
        submission.expected_revision = "0".repeat(64);
        submission.id = uuid::Uuid::new_v4().to_string();
        let conflict = broker.monitor_board_message(1, &Name::default(), submission);
        assert_eq!(conflict.status, 409);
        assert_eq!(
            conflict.body["board"]["last_updated"],
            before.last_updated.unwrap()
        );
        assert!(
            !conflict.body["board"]["display_markdown"]
                .as_str()
                .unwrap()
                .contains("board-message:")
        );
        let after: Snapshot =
            serde_json::from_value(broker.request(Request::BoardRead).unwrap()).unwrap();
        assert_eq!(after.markdown, before.markdown);
        assert_eq!(after.revision, before.revision);
        assert_eq!(after.last_updated, before.last_updated);
        assert_eq!(after.last_updated_source, before.last_updated_source);
        let file_after = std::fs::metadata(board::read(&broker.state_dir).unwrap().path).unwrap();
        assert_eq!(file_after.ino(), file_before.ino());
        assert_eq!(
            file_after.modified().unwrap(),
            file_before.modified().unwrap()
        );
        assert_eq!(board::read(root.path()).unwrap().markdown, before.markdown);
    }

    #[tokio::test]
    async fn message_metadata_is_hidden_only_in_rendered_view() {
        let (root, _bound, old) = setup();
        let marker = crate::board_messages::Marker {
            id: "coordinator".into(),
            instance: uuid::Uuid::new_v4().to_string(),
            label: "Message coordinator".into(),
            owner: crate::board_messages::Owner {
                name: "coordinator".into(),
                provider: crate::protocol::Provider::Inbox,
                session: uuid::Uuid::new_v4().to_string(),
                endpoint: String::new(),
            },
        };
        let declaration = format!(
            "<!-- board-message: {} -->",
            serde_json::to_string(&marker).unwrap()
        );
        let source = format!(
            "# Friendly board 世界\r\n{declaration}\r\nVisible text\r\n<!-- literal comment -->\r\n- [ ] Approval 世界\r\n\r\n<!-- board-input: answer -->\r\n> unchanged 世界\r\n<!-- /board-input -->\r\n"
        );
        let snapshot = board::update_named(
            root.path(),
            &Name::default(),
            &old.revision,
            &[board::Edit::Set {
                markdown: source.clone(),
            }],
        )
        .unwrap();
        let result = view(&snapshot);
        assert_eq!(result["markdown"], source);
        assert_eq!(result["controls"][0]["kind"], "message");
        let rendered = result["nodes"].to_string();
        assert!(!rendered.contains("board-message:"));
        assert_eq!(result["controls"].as_array().unwrap().len(), 3);
        assert_eq!(result["controls"][1]["kind"], "checkbox");
        assert_eq!(result["controls"][2]["kind"], "text");
        for control in &result["controls"].as_array().unwrap()[1..] {
            assert_eq!(rendered.matches(control["id"].as_str().unwrap()).count(), 1);
        }
        assert!(!rendered.contains("board-input: answer"));
        assert!(!rendered.contains("unchanged 世界"));
        assert!(
            !result["display_markdown"]
                .as_str()
                .unwrap()
                .contains("board-message:")
        );
        assert!(rendered.contains("Visible text") && rendered.contains("literal comment"));

        let mut complex = snapshot.clone();
        complex.markdown = format!("{declaration}\n{}", "- item\n".repeat(4000));
        let fallback = view(&complex);
        assert!(fallback.get("render_notice").is_some());
        assert_eq!(fallback["markdown"], complex.markdown);
        assert!(!fallback["nodes"].to_string().contains("board-message:"));

        // Non-controls stay visible, including malformed declarations and code
        // examples; only successfully parsed interactive metadata is hidden.
        for source in [
            "<!-- board-message: malformed -->\n".to_owned(),
            format!("```markdown\n{declaration}\n```\n"),
        ] {
            let mut snapshot = snapshot.clone();
            snapshot.markdown = source.clone();
            let result = view(&snapshot);
            assert_eq!(result["markdown"], source);
            assert!(result["nodes"].to_string().contains("board-message:"));
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
