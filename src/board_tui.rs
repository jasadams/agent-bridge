//! A live Markdown view with revision-checked checkbox and text-field editing.
use crate::{
    board::{Edit, LifecycleState, MAX_BOARD_BYTES, Name, Snapshot, Summary},
    board_interaction::{self, Control, Kind},
    board_messages::Submission,
    board_selection::{Selection, Viewport},
    broker,
    protocol::Request,
};
use anyhow::{Context, Result, ensure};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures::StreamExt;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use pulldown_cmark::{CodeBlockKind, Event as MarkdownEvent, Parser, Tag, TagEnd};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Clear, Paragraph, Wrap},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::IsTerminal,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

const FALLBACK_REFRESH: Duration = Duration::from_secs(1);
// Tables with many sparse cells can expand far beyond their Markdown input.
// Leave such documents readable as plain Markdown instead of allocating a huge grid.
const MAX_TABLE_CELLS: usize = 20_000;
#[cfg(test)]
const MAX_LINK_URL_BYTES: usize = crate::board_links::MAX_URI_BYTES;

struct LinkGlyph {
    text: String,
    style: Style,
    link: usize,
}

fn link_color(index: usize) -> Color {
    let id = index + 1;
    Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
}

// Replace link labels with non-Markdown glyphs of identical display width. This
// lets tui-markdown size and split tables without ever seeing the destinations.
fn markdown_links(safe: &str) -> (String, HashMap<char, LinkGlyph>, Vec<String>) {
    let options = crate::board_code_copy::markdown_options();
    let references = Parser::new_ext(safe, options);
    let definitions = references.reference_definitions();
    let parser = Parser::new_ext(safe, options);
    let events: Vec<_> = parser.into_offset_iter().collect();
    let mut used: HashSet<_> = safe.chars().collect();
    for (event, _) in &events {
        match event {
            MarkdownEvent::Text(text)
            | MarkdownEvent::Code(text)
            | MarkdownEvent::InlineHtml(text)
            | MarkdownEvent::Html(text)
            | MarkdownEvent::InlineMath(text)
            | MarkdownEvent::DisplayMath(text)
            | MarkdownEvent::Start(Tag::CodeBlock(CodeBlockKind::Fenced(text)))
            | MarkdownEvent::Start(Tag::Link { dest_url: text, .. })
            | MarkdownEvent::Start(Tag::Image { dest_url: text, .. }) => used.extend(text.chars()),
            _ => (),
        }
    }
    let mut events = events.into_iter();
    let mut source = String::new();
    let mut copied = 0;
    let mut glyphs = HashMap::new();
    let mut urls = Vec::new();
    let mut target_ids = HashMap::new();
    let mut narrow = 0xf0000;
    let mut wide = 0x20000;
    let mut table_cell = false;
    while let Some((event, range)) = events.next() {
        match event {
            MarkdownEvent::Start(Tag::TableCell) => {
                table_cell = true;
                continue;
            }
            MarkdownEvent::End(TagEnd::TableCell) => {
                table_cell = false;
                continue;
            }
            _ => (),
        }
        let MarkdownEvent::Start(Tag::Link { dest_url, id, .. }) = event else {
            continue;
        };
        let link_source = &safe[range.clone()];
        let mut eligible =
            !link_source.contains('\n') && !(table_cell && link_source.contains("\\|"));
        let mut depth = 1;
        for (event, _) in events.by_ref() {
            match event {
                MarkdownEvent::Start(Tag::Link { .. }) => {
                    depth += 1;
                    eligible = false;
                }
                MarkdownEvent::Start(Tag::Image { .. }) => eligible = false,
                MarkdownEvent::End(TagEnd::Link) => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => (),
            }
        }
        if !eligible || !crate::board_links::valid_target(&dest_url) {
            continue;
        }
        let mut snippet = safe[range.clone()].to_owned();
        if let Some(definition) = definitions.get(&id) {
            snippet.push_str("\n\n");
            snippet.push_str(&safe[definition.span.clone()]);
        }
        let mut label = tui_markdown::from_str(&snippet);
        let [line] = label.lines.as_mut_slice() else {
            continue;
        };
        let label_len = match line.spans.as_slice() {
            [body @ .., open, destination, close]
                if open.content == " ("
                    && destination.content == dest_url.as_ref()
                    && close.content == ")" =>
            {
                body.len()
            }
            _ => continue,
        };
        line.spans.truncate(label_len);
        if !line
            .spans
            .iter()
            .any(|span| Span::raw(safe_line(&span.content)).width() > 0)
        {
            continue;
        }
        source.push_str(&safe[copied..range.start]);
        let link = if let Some(&link) = target_ids.get(dest_url.as_ref()) {
            link
        } else {
            if urls.len() == crate::board_links::MAX_LINKS {
                return (safe.to_owned(), HashMap::new(), Vec::new());
            }
            let link = urls.len();
            target_ids.insert(dest_url.to_string(), link);
            urls.push(dest_url.into_string());
            link
        };
        for span in label.lines.iter().flat_map(|line| &line.spans) {
            let clean = Span::styled(safe_line(&span.content), span.style);
            for glyph in clean.styled_graphemes(Style::default()) {
                let width = Span::raw(glyph.symbol).width();
                if width == 0 {
                    continue;
                }
                if glyphs.len() == crate::board_links::MAX_LINK_CELLS {
                    return (safe.to_owned(), HashMap::new(), Vec::new());
                }
                let next = if width == 1 { &mut narrow } else { &mut wide };
                // Labels without nested images cannot expand beyond the board's
                // source size, leaving enough unused scalars in each range.
                let marker = loop {
                    let marker = char::from_u32(*next).unwrap();
                    *next += 1;
                    if !used.contains(&marker) {
                        break marker;
                    }
                };
                glyphs.insert(
                    marker,
                    LinkGlyph {
                        text: glyph.symbol.to_owned(),
                        style: glyph.style,
                        link,
                    },
                );
                // Entity punctuation preserves the surrounding Markdown's
                // delimiter context and keeps label spaces out of block syntax.
                source.push_str(&format!("&#{};", u32::from(marker)));
            }
        }
        copied = range.end;
    }
    source.push_str(&safe[copied..]);
    (source, glyphs, urls)
}

struct TerminalRestore;
impl Drop for TerminalRestore {
    fn drop(&mut self) {
        crate::board_links::close_output();
        let _ = crossterm::execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture
        );
        ratatui::restore();
    }
}
struct AbortWorker(JoinHandle<()>);
impl Drop for AbortWorker {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Keep Markdown newlines, normalize tabs, and remove terminal control sequences'
/// introducers. This changes display text only, never the stored document.
fn safe_markdown(input: &str) -> String {
    let mut safe = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\n' => safe.push('\n'),
            '\t' => safe.push_str("    "),
            ch if ch.is_control() => (),
            ch => safe.push(ch),
        }
    }
    safe
}

fn safe_line(input: &str) -> String {
    input
        .chars()
        .filter_map(|ch| match ch {
            '\n' | '\r' | '\t' => Some(' '),
            ch if ch.is_control() => None,
            ch => Some(ch),
        })
        .collect()
}

#[cfg(test)]
fn rendered_markdown(
    markdown: &str,
    width: u16,
) -> (Text<'static>, bool, Text<'static>, Vec<String>) {
    let (text, plain, links, urls, _, _) = rendered_board(markdown, width);
    (text, plain, links, urls)
}

fn rendered_board(
    markdown: &str,
    width: u16,
) -> (
    Text<'static>,
    bool,
    Text<'static>,
    Vec<String>,
    Text<'static>,
    Vec<String>,
) {
    let display = board_interaction::display_markdown(markdown);
    let prepared = crate::board_code_copy::Prepared::new(display.as_ref());
    let safe = safe_markdown(display.as_ref());
    let lines = safe.lines().count();
    let columns = safe
        .lines()
        .map(|line| line.bytes().filter(|byte| *byte == b'|').count())
        .max()
        .unwrap_or(0);
    let plain = columns.saturating_mul(lines) > MAX_TABLE_CELLS;
    let (source, glyphs, urls) = if plain {
        (safe.clone(), HashMap::new(), Vec::new())
    } else {
        markdown_links(&prepared.source)
    };
    let rendered = if plain {
        Text::from(safe.as_str())
    } else {
        let options = tui_markdown::Options::default()
            .table_width(width)
            .table_row_dividers(true);
        tui_markdown::from_str_with_options(&source, &options)
    };
    // Entity decoding can introduce controls that were absent from the source,
    // e.g. &#x1b;. Sanitize every final span, not just the Markdown input.
    let mut mask = Text::default();
    let mut code_mask = Text::default();
    let owned = Text {
        alignment: rendered.alignment,
        style: rendered.style,
        lines: rendered
            .lines
            .into_iter()
            .map(|line| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut ids: Vec<Span<'static>> = Vec::new();
                let mut code_ids: Vec<Span<'static>> = Vec::new();
                for span in line.spans {
                    let clean = safe_line(&span.content);
                    let clean = Span::raw(clean);
                    let chars: Vec<_> = clean.styled_graphemes(Style::default()).collect();
                    for ch in &chars {
                        let (text, style, link) = if let Some(glyph) =
                            ch.symbol.chars().next().and_then(|ch| glyphs.get(&ch))
                        {
                            (
                                glyph.text.clone(),
                                glyph.style.patch(span.style),
                                Some(glyph.link),
                            )
                        } else {
                            (ch.symbol.to_owned(), span.style, None)
                        };
                        let code = text.chars().next().and_then(|ch| prepared.glyphs.get(&ch));
                        let (text, style, link, code_style) = if let Some(code) = code {
                            let code_style = Style::default()
                                .fg(crate::board_code_copy::id_color(code.snippet))
                                .bg(if code.button {
                                    Color::Blue
                                } else {
                                    Color::Reset
                                });
                            (
                                code.text.clone(),
                                if code.button {
                                    crate::board_code_copy::button_style()
                                } else {
                                    crate::board_code_copy::code_style()
                                },
                                if code.button { None } else { link },
                                code_style,
                            )
                        } else {
                            (text, style, link, Style::default().fg(Color::Reset))
                        };
                        code_ids.push(Span::styled(text.clone(), code_style));
                        let id_style = Style::default().fg(link.map_or(Color::Reset, link_color));
                        if spans.last().is_some_and(|last| last.style == style)
                            && ids.last().is_some_and(|last| last.style == id_style)
                        {
                            spans.last_mut().unwrap().content.to_mut().push_str(&text);
                            ids.last_mut().unwrap().content.to_mut().push_str(&text);
                        } else {
                            ids.push(Span::styled(text.clone(), id_style));
                            spans.push(Span::styled(text, style));
                        }
                    }
                }
                code_mask.lines.push(Line {
                    alignment: line.alignment,
                    spans: code_ids,
                    ..Line::default()
                });
                mask.lines.push(Line {
                    alignment: line.alignment,
                    spans: ids,
                    ..Line::default()
                });
                Line {
                    alignment: line.alignment,
                    style: line.style,
                    spans,
                }
            })
            .collect(),
    };
    let urls = if markdown
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        Vec::new()
    } else {
        urls
    };
    (
        owned,
        plain,
        mask,
        urls,
        code_mask,
        if plain { Vec::new() } else { prepared.payloads },
    )
}

#[derive(Clone)]
struct Save {
    generation: u64,
    base: Snapshot,
    markdown: String,
    text: bool,
}
#[derive(Clone)]
struct Editor {
    base: Snapshot,
    control: Control,
    draft: String,
    cursor: usize,
    submission: Option<Submission>,
    in_flight: bool,
    uncertain: bool,
}
impl Editor {
    fn key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up => self.move_row(false),
            KeyCode::Down => self.move_row(true),
            KeyCode::Left => {
                self.cursor = self.draft[..self.cursor]
                    .char_indices()
                    .last()
                    .map_or(0, |(i, _)| i)
            }
            KeyCode::Right => {
                self.cursor += self.draft[self.cursor..]
                    .chars()
                    .next()
                    .map_or(0, char::len_utf8)
            }
            KeyCode::Home => {
                self.cursor = self.draft[..self.cursor].rfind('\n').map_or(0, |i| i + 1)
            }
            KeyCode::End => {
                self.cursor += self.draft[self.cursor..]
                    .find('\n')
                    .unwrap_or(self.draft.len() - self.cursor)
            }
            KeyCode::Backspace if self.cursor > 0 => {
                let previous = self.draft[..self.cursor]
                    .char_indices()
                    .last()
                    .map_or(0, |(i, _)| i);
                self.draft.replace_range(previous..self.cursor, "");
                self.cursor = previous;
            }
            KeyCode::Delete if self.cursor < self.draft.len() => {
                let next =
                    self.cursor + self.draft[self.cursor..].chars().next().unwrap().len_utf8();
                self.draft.replace_range(self.cursor..next, "");
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && !ch.is_control() =>
            {
                self.insert(&ch.to_string())
            }
            KeyCode::Enter => self.insert("\n"),
            _ => (),
        }
    }
    fn move_row(&mut self, down: bool) {
        let line_start = self.draft[..self.cursor].rfind('\n').map_or(0, |i| i + 1);
        let line_end = self.cursor
            + self.draft[self.cursor..]
                .find('\n')
                .unwrap_or(self.draft.len() - self.cursor);
        let column = self.draft[line_start..self.cursor].chars().count();
        let target = if down {
            if line_end == self.draft.len() {
                return;
            }
            let start = line_end + 1;
            let end = start
                + self.draft[start..]
                    .find('\n')
                    .unwrap_or(self.draft.len() - start);
            start..end
        } else {
            if line_start == 0 {
                return;
            }
            let end = line_start - 1;
            let start = self.draft[..end].rfind('\n').map_or(0, |i| i + 1);
            start..end
        };
        self.cursor = target.start
            + self.draft[target.clone()]
                .char_indices()
                .nth(column)
                .map_or(target.len(), |(i, _)| i);
    }
    fn insert(&mut self, text: &str) {
        if self.draft.len() + text.len() <= MAX_BOARD_BYTES {
            self.draft.insert_str(self.cursor, text);
            self.cursor += text.len();
        }
    }
}

#[derive(Clone)]
struct LifecycleAction {
    base: Snapshot,
    generation: u64,
    restore: bool,
}
impl LifecycleAction {
    fn verb(&self) -> &'static str {
        if self.restore { "Restore" } else { "Archive" }
    }
}

struct View {
    board: Name,
    generation: u64,
    picker: Option<(Vec<Summary>, usize)>,
    pick_requested: bool,
    picker_archived: bool,
    picker_epoch: u64,
    lifecycle_confirm: Option<LifecycleAction>,
    pending_lifecycle: Option<LifecycleAction>,
    lifecycle_flight: Option<LifecycleAction>,
    lifecycle_uncertain: Option<LifecycleAction>,
    snapshot: Option<Snapshot>,
    error: Option<String>,
    rendered: Text<'static>,
    link_mask: Text<'static>,
    link_urls: Vec<String>,
    hyperlinks_enabled: bool,
    code_mask: Text<'static>,
    code_payloads: Vec<String>,
    code_cells: HashMap<Position, usize>,
    copy_focus: bool,
    focused_copy: Option<usize>,
    pending_copy: Option<String>,
    width: u16,
    scroll: (u16, u16),
    max_scroll: (u16, u16),
    wrap: bool,
    plain: bool,
    controls: Vec<Control>,
    selected: Option<usize>,
    editor: Option<Editor>,
    pending: Option<Save>,
    saving: bool,
    uncertain_save: Option<Save>,
    message_labels: crate::board_messages::DisplayLabels,
    message_drafts: BTreeMap<String, Editor>,
    message_regions: Vec<(Rect, usize, bool)>,
    message_area: Rect,
    message_scroll: usize,
    pending_message: Option<Submission>,
    owner_details: bool,
    owner_scroll: u16,
    retained_cursor: Option<String>,
    notice: Option<String>,
    update_notice: Option<String>,
    viewport: Option<Viewport>,
    viewport_metadata: Option<(String, Option<i64>)>,
    selection: Option<Selection>,
    mouse_enabled: bool,
}
impl Default for View {
    fn default() -> Self {
        Self {
            board: Name::default(),
            generation: 0,
            picker: None,
            pick_requested: false,
            picker_archived: false,
            picker_epoch: 0,
            lifecycle_confirm: None,
            pending_lifecycle: None,
            lifecycle_flight: None,
            lifecycle_uncertain: None,
            snapshot: None,
            error: None,
            rendered: Text::default(),
            link_mask: Text::default(),
            link_urls: Vec::new(),
            hyperlinks_enabled: true,
            code_mask: Text::default(),
            code_payloads: Vec::new(),
            code_cells: HashMap::new(),
            copy_focus: false,
            focused_copy: None,
            pending_copy: None,
            width: 0,
            scroll: (0, 0),
            max_scroll: (0, 0),
            wrap: true,
            plain: false,
            controls: Vec::new(),
            selected: None,
            editor: None,
            pending: None,
            saving: false,
            uncertain_save: None,
            message_labels: BTreeMap::new(),
            message_drafts: BTreeMap::new(),
            message_regions: Vec::new(),
            message_area: Rect::default(),
            message_scroll: 0,
            pending_message: None,
            owner_details: false,
            owner_scroll: 0,
            retained_cursor: None,
            notice: None,
            update_notice: None,
            viewport: None,
            viewport_metadata: None,
            selection: None,
            mouse_enabled: true,
        }
    }
}
impl View {
    fn writable(&self) -> bool {
        !self.lifecycle_busy()
            && self.uncertain_save.is_none()
            && self.error.is_none()
            && self.snapshot.as_ref().is_some_and(Snapshot::writable)
    }
    fn lifecycle_busy(&self) -> bool {
        self.pending_lifecycle.is_some()
            || self.lifecycle_flight.is_some()
            || self.lifecycle_uncertain.is_some()
    }
    fn confirm_lifecycle(&mut self, restore: bool) {
        if self.editor.is_some()
            || self.saving
            || self.pending.is_some()
            || self.pending_message.is_some()
            || self.uncertain_save.is_some()
            || self.has_message_drafts()
            || self.lifecycle_busy()
        {
            self.notice = Some("Save successfully or explicitly discard retained drafts/uncertain tracking before archive, restore or navigation. Cancel keeps the board and draft.".into());
            return;
        }
        let Some(base) = self.snapshot.clone().filter(|_| self.error.is_none()) else {
            return;
        };
        if base.name == "main" {
            self.notice = Some("Main is protected and cannot be archived.".into());
            return;
        }
        if (restore && base.lifecycle.state != LifecycleState::Archived)
            || (!restore && !base.writable())
        {
            self.notice = Some("Board lifecycle does not allow this action. Expired/purging boards cannot be restored.".into());
            return;
        }
        self.selection = None;
        self.viewport = None;
        self.code_cells.clear();
        self.clear_copy_focus();
        self.lifecycle_confirm = Some(LifecycleAction {
            base,
            generation: self.generation,
            restore,
        });
    }
    fn action_current(&self, action: &LifecycleAction) -> bool {
        self.error.is_none()
            && self.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.revision == action.base.revision
                    && snapshot.control_scope() == action.base.control_scope()
                    && snapshot.lifecycle.state == action.base.lifecycle.state
                    && if action.restore {
                        snapshot.lifecycle.state == LifecycleState::Archived
                    } else {
                        snapshot.writable()
                    }
            })
    }
    fn lifecycle_result(&mut self, action: LifecycleAction, result: Result<Snapshot>) {
        if action.generation != self.generation || action.base.name != self.board.as_str() {
            return;
        }
        self.lifecycle_flight = None;
        let result = result.and_then(|snapshot| {
            ensure!(
                snapshot.name == action.base.name
                    && snapshot.lifecycle.incarnation == action.base.lifecycle.incarnation
                    && snapshot.control_scope() != action.base.control_scope(),
                "Unexpected lifecycle receipt identity"
            );
            ensure!(
                if action.restore {
                    snapshot.writable()
                } else {
                    snapshot.lifecycle.state == LifecycleState::Archived
                },
                "Lifecycle transition is not complete"
            );
            Ok(snapshot)
        });
        match result {
            Ok(snapshot) => {
                self.lifecycle_uncertain = None;
                if self.snapshot.as_ref().is_none_or(|current| {
                    current.control_scope() == action.base.control_scope()
                        && current.revision == action.base.revision
                }) {
                    self.update(Ok(snapshot));
                }
                self.notice = Some(format!(
                    "{} confirmed. Board content and Last Updated preserved.",
                    action.verb()
                ));
            }
            Err(error) => {
                self.notice = Some(format!(
                    "{} result uncertain or rejected: {}. Inspecting originating board; no automatic retry. Ctrl-D explicitly releases tracking after inspection.",
                    action.verb(),
                    safe_line(&format!("{error:#}"))
                ));
                self.lifecycle_uncertain = Some(action);
            }
        }
    }
    fn has_message_drafts(&self) -> bool {
        self.editor.as_ref().is_some_and(|editor| {
            matches!(editor.control.kind, Kind::Message { .. })
                && (!editor.draft.is_empty() || editor.in_flight || editor.submission.is_some())
        }) || self.message_drafts.values().any(|editor| {
            !editor.draft.is_empty() || editor.in_flight || editor.submission.is_some()
        })
    }
    fn draft_key(editor: &Editor) -> String {
        let Kind::Message { id, instance, .. } = &editor.control.kind else {
            unreachable!()
        };
        format!("{}:{id}:{instance}", editor.base.control_scope())
    }
    fn retain_message(&mut self) {
        if let Some(editor) = self.editor.take() {
            self.message_drafts.insert(Self::draft_key(&editor), editor);
        }
    }
    fn open_message(&mut self, control: Control) {
        if !self.writable() {
            self.notice = Some(
                "Board is read-only; original message drafts remain available with Ctrl-E.".into(),
            );
            return;
        }
        let Some(base) = self.snapshot.clone() else {
            return;
        };
        let Kind::Message { id, instance, .. } = &control.kind else {
            return;
        };
        let key = format!("{}:{id}:{instance}", base.control_scope());
        self.editor = Some(self.message_drafts.remove(&key).unwrap_or(Editor {
            base,
            control,
            draft: String::new(),
            cursor: 0,
            submission: None,
            in_flight: false,
            uncertain: false,
        }));
        self.notice = None;
    }
    fn message_key(&mut self, key: KeyEvent, page: u16) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Char('o') && control {
            self.owner_details = !self.owner_details;
            self.owner_scroll = 0;
            return false;
        }
        if self.owner_details {
            match key.code {
                KeyCode::Esc => self.owner_details = false,
                KeyCode::Up | KeyCode::PageUp => {
                    self.owner_scroll = self.owner_scroll.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::PageDown => {
                    self.owner_scroll = self.owner_scroll.saturating_add(1)
                }
                KeyCode::Home => self.owner_scroll = 0,
                _ => (),
            }
            return false;
        }
        if self.editor.as_ref().unwrap().in_flight
            && !matches!(key.code, KeyCode::Tab | KeyCode::BackTab | KeyCode::Esc)
        {
            if matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
                self.scroll.0 = if key.code == KeyCode::PageUp {
                    self.scroll.0.saturating_sub(page.max(1))
                } else {
                    self.scroll.0.saturating_add(page.max(1))
                };
            }
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                self.retain_message();
                self.notice = Some("Message draft retained. Tab/Enter reopens existing inputs; Ctrl-E reopens any retained draft, including removed inputs. Ctrl-D discards all.".into());
            }
            KeyCode::Char('x') if control => {
                let uncertain = self.editor.as_ref().unwrap().uncertain;
                self.editor = None;
                self.notice = Some(if uncertain {
                    "Draft and retry tracking discarded. Previous submission may have been accepted; inspect message history before resending."
                } else { "Message draft discarded; no accepted submission was confirmed." }.into());
            }
            KeyCode::Char('c') if control => {
                if self.has_message_drafts() {
                    self.notice = Some("Message draft retained. Ctrl-X discards this draft; Esc then Ctrl-D discards all before quitting.".into());
                } else {
                    return true;
                }
            }
            KeyCode::Char('r') if control => self.review_message(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.retain_message();
                if !self.controls.is_empty() {
                    let backwards =
                        key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                    let index = match (self.selected, backwards) {
                        (Some(0), true) | (None, true) => self.controls.len() - 1,
                        (Some(index), true) => (index - 1).min(self.controls.len() - 1),
                        (Some(index), false) => (index + 1) % self.controls.len(),
                        (None, false) => 0,
                    };
                    self.selected = Some(index);
                    if matches!(self.controls[index].kind, Kind::Message { .. }) {
                        self.open_message(self.controls[index].clone());
                    }
                }
            }
            KeyCode::Enter if !key.modifiers.contains(KeyModifiers::SHIFT) => {
                if key.kind != KeyEventKind::Repeat {
                    self.prepare_message();
                }
            }
            KeyCode::PageUp => self.scroll.0 = self.scroll.0.saturating_sub(page.max(1)),
            KeyCode::PageDown => self.scroll.0 = self.scroll.0.saturating_add(page.max(1)),
            _ if self.editor.as_ref().unwrap().uncertain => {
                self.notice = Some("Outcome uncertain: Enter retries the exact message ID/text; Ctrl-X explicitly discards. Editing is locked.".into());
            }
            KeyCode::Char('j') if control => self.editor.as_mut().unwrap().insert("\n"),
            _ => self.editor.as_mut().unwrap().key(key),
        }
        false
    }
    fn review_message(&mut self) {
        let (Some(editor), Some(snapshot)) = (&mut self.editor, &self.snapshot) else {
            return;
        };
        if !snapshot.writable() || editor.base.control_scope() != snapshot.control_scope() {
            self.notice = Some("Board lifecycle changed or is read-only; original draft retained. Cannot rebase across archive or restore.".into());
            return;
        }
        if editor.uncertain {
            self.notice = Some(
                "Retry the exact pending message before changing its control or revision.".into(),
            );
            return;
        }
        let key = editor.control.message_key(&editor.base.control_scope());
        if let Some(current) = self
            .controls
            .iter()
            .find(|control| control.message_key(&snapshot.control_scope()) == key)
        {
            editor.control = current.clone();
            editor.base = snapshot.clone();
            self.notice = Some("Current unchanged message target reviewed. Review the board and draft; Enter explicitly sends.".into());
        } else {
            self.notice = Some("Input changed, was retargeted or removed. Draft retained; cannot send it. Ctrl-X discards before selecting a new input.".into());
        }
    }
    fn prepare_message(&mut self) {
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.in_flight || self.pending_message.is_some() {
            return;
        }
        if editor.draft.trim().is_empty() {
            self.notice = Some("Blank messages are not sent; draft retained.".into());
            return;
        }
        // An ambiguous retry retains its original request. A definite rejection
        // also reuses its UUID while the exact payload/control remains unchanged.
        if !editor.uncertain {
            if self.snapshot.as_ref().is_none_or(|current| {
                !current.writable() || current.control_scope() != editor.base.control_scope()
            }) {
                self.notice = Some("Board is read-only or its lifecycle changed; original draft retained. No new message sent.".into());
                return;
            }
            if self
                .snapshot
                .as_ref()
                .is_none_or(|current| current.revision != editor.base.revision)
            {
                self.notice = Some("Board changed; draft retained. Ctrl-R explicitly reviews an unchanged input before sending.".into());
                return;
            }
            let control_id = board_interaction::control_id(&editor.base, &editor.control);
            let same = editor.submission.as_ref().is_some_and(|old| {
                old.board == self.board
                    && old.expected_revision == editor.base.revision
                    && old.control_id == control_id
                    && old.text == editor.draft
            });
            if !same {
                editor.submission = Some(Submission {
                    board: self.board.clone(),
                    expected_revision: editor.base.revision.clone(),
                    control_id,
                    id: uuid::Uuid::new_v4().to_string(),
                    text: editor.draft.clone(),
                });
            }
        }
        self.pending_message = editor.submission.clone();
        editor.in_flight = true;
        self.notice = None;
    }
    fn accepted_message(editor: &Editor, request: &Submission, value: &serde_json::Value) -> bool {
        let Kind::Message { owner, .. } = &editor.control.kind else {
            return false;
        };
        value["accepted"] == true
            && value["id"] == request.id
            && value["message"]["id"] == request.id
            && value["message"]["sender"] == "user"
            && value["message"]["recipient"] == owner.name
            && value["message"]["recipient_session"] == owner.session
            && value["message"]["body"] == request.text
            && editor.draft == request.text
    }
    fn message_result(&mut self, request: Submission, result: Result<serde_json::Value>) {
        let editor = if self.editor.as_ref().is_some_and(|editor| {
            editor
                .submission
                .as_ref()
                .is_some_and(|old| old.id == request.id)
        }) {
            self.editor.as_mut()
        } else {
            self.message_drafts.values_mut().find(|editor| {
                editor
                    .submission
                    .as_ref()
                    .is_some_and(|old| old.id == request.id)
            })
        };
        let Some(editor) = editor else {
            return;
        };
        editor.in_flight = false;
        match result {
            Ok(value) if Self::accepted_message(editor, &request, &value) => {
                editor.draft.clear();
                editor.cursor = 0;
                editor.submission = None;
                editor.uncertain = false;
                let state =
                    crate::protocol::delivery_state(value["state"].as_str().unwrap_or("unknown"));
                self.notice = Some(format!("Message {state}"));
            }
            Ok(value)
                if value["accepted"] == false
                    && matches!(
                        value["code"].as_str(),
                        Some("invalid" | "conflict" | "unavailable" | "rejected")
                    ) =>
            {
                editor.uncertain = false;
                self.notice = Some(format!(
                    "Message not accepted: {}. Draft retained; review before sending again.",
                    safe_line(value["error"].as_str().unwrap_or("Rejected"))
                ));
            }
            _ => {
                editor.uncertain = true;
                self.notice = Some(format!(
                    "Submission outcome uncertain. Draft retained; Enter retries exact ID {} without duplicating. Ctrl-X explicitly discards.",
                    request.id
                ));
            }
        }
    }
    fn switch(&mut self, board: Name) -> bool {
        if self.lifecycle_busy()
            || self.lifecycle_confirm.is_some()
            || self.editor.is_some()
            || self.saving
            || self.pending.is_some()
            || self.pending_message.is_some()
            || self.uncertain_save.is_some()
            || self.has_message_drafts()
        {
            return false;
        }
        if self.board == board {
            self.picker = None;
            self.pick_requested = false;
            self.picker_epoch = self.picker_epoch.wrapping_add(1);
            return false;
        }
        let generation = self.generation.wrapping_add(1);
        let update_notice = self.update_notice.take();
        let mouse_enabled = self.mouse_enabled;
        let hyperlinks_enabled = self.hyperlinks_enabled;
        *self = Self {
            board,
            generation,
            update_notice,
            mouse_enabled,
            hyperlinks_enabled,
            ..Self::default()
        };
        true
    }
    fn read_result(&mut self, board: &Name, generation: u64, result: Result<Snapshot>) -> bool {
        if &self.board != board || self.generation != generation {
            return false;
        }
        if result
            .as_ref()
            .is_ok_and(|snapshot| snapshot.name != self.board.as_str())
        {
            return self.update(Err(anyhow::anyhow!("Broker returned a different board")));
        }
        self.update(result)
    }
    fn update_release_notice(&mut self, notice: Option<String>) -> bool {
        let changed = self.update_notice != notice;
        self.update_notice = notice;
        changed
    }
    /// Return whether visible state changed, keeping native terminal selections
    /// intact during repeated healthy polls or the same repeated outage.
    fn update(&mut self, result: Result<Snapshot>) -> bool {
        match result {
            Ok(snapshot) => {
                let changed = self.error.is_some()
                    || self.snapshot.as_ref().is_none_or(|old| {
                        old.revision != snapshot.revision
                            || old.name != snapshot.name
                            || old.path != snapshot.path
                            || old.exists != snapshot.exists
                            || old.last_updated != snapshot.last_updated
                            || old.lifecycle != snapshot.lifecycle
                    });
                if self.snapshot.as_ref().is_none_or(|old| {
                    old.revision != snapshot.revision || old.lifecycle != snapshot.lifecycle
                }) {
                    self.width = 0;
                    self.clear_copy_focus();
                    self.code_cells.clear();
                    let composer_key = self.editor.as_ref().and_then(|editor| {
                        editor.control.message_key(&editor.base.control_scope())
                    });
                    self.controls = board_interaction::controls(&snapshot.markdown);
                    if self.lifecycle_confirm.as_ref().is_some_and(|action| {
                        action.base.revision != snapshot.revision
                            || action.base.control_scope() != snapshot.control_scope()
                            || action.base.lifecycle.state != snapshot.lifecycle.state
                    }) {
                        self.lifecycle_confirm = None;
                        self.notice = Some("Board changed; archive/restore confirmation cancelled. Inspect the current board before trying again.".into());
                    }
                    // Keep focus only for the exact unchanged composer. Source
                    // positions still reset for ordinary editable responses.
                    self.selected = composer_key.and_then(|key| {
                        self.controls.iter().position(|control| {
                            control.message_key(&snapshot.control_scope()).as_ref() == Some(&key)
                        })
                    });
                    if self
                        .editor
                        .as_ref()
                        .is_some_and(|editor| editor.base.revision != snapshot.revision)
                    {
                        self.notice = Some("Board changed; draft retained. Review latest board; Ctrl-R rebases unchanged field before saving.".into());
                    }
                }
                if let Some(action) = &self.lifecycle_uncertain {
                    let reached = snapshot.lifecycle.incarnation
                        == action.base.lifecycle.incarnation
                        && snapshot.control_scope() != action.base.control_scope()
                        && if action.restore {
                            snapshot.writable()
                        } else {
                            snapshot.lifecycle.state == LifecycleState::Archived
                        };
                    if reached {
                        self.notice = Some(format!(
                            "{} state confirmed by inspection; no mutation retried.",
                            action.verb()
                        ));
                        self.lifecycle_uncertain = None;
                    }
                }
                self.snapshot = Some(snapshot);
                self.error = None;
                changed
            }
            Err(error) => {
                if self.lifecycle_confirm.take().is_some() {
                    self.notice = Some("Archive/restore confirmation cancelled while offline. Inspect the board before trying again.".into());
                }
                let error = safe_line(&format!("{error:#}"));
                let changed = self.error.as_ref() != Some(&error);
                self.error = Some(error);
                changed
            }
        }
    }
    fn paragraph(&self) -> Paragraph<'static> {
        let paragraph = Paragraph::new(self.rendered.clone());
        if self.wrap {
            paragraph.wrap(Wrap { trim: false })
        } else {
            paragraph
        }
    }
    fn draw_messages(&mut self, frame: &mut Frame<'_>, area: Rect) {
        self.message_area = area;
        self.message_regions.clear();
        let mut rows: Vec<_> = self
            .controls
            .iter()
            .enumerate()
            .filter(|(_, control)| matches!(control.kind, Kind::Message { .. }))
            .collect();
        if let Some(editor) = &self.editor
            && matches!(editor.control.kind, Kind::Message { .. })
            && !rows.iter().any(|(_, control)| {
                self.snapshot
                    .as_ref()
                    .and_then(|snapshot| control.message_key(&snapshot.control_scope()))
                    == editor.control.message_key(&editor.base.control_scope())
            })
        {
            rows.push((usize::MAX, &editor.control));
        }
        if area.height < 3 || rows.is_empty() {
            return;
        }
        let visible = usize::from(area.height / 3).max(1);
        self.message_scroll = self.message_scroll.min(rows.len().saturating_sub(visible));
        if let Some(position) = rows
            .iter()
            .position(|(index, _)| self.selected == Some(*index) || *index == usize::MAX)
        {
            if position < self.message_scroll {
                self.message_scroll = position;
            }
            if position >= self.message_scroll + visible {
                self.message_scroll = position + 1 - visible;
            }
        }
        for (slot, (index, control)) in rows
            .iter()
            .skip(self.message_scroll)
            .take(visible)
            .enumerate()
        {
            let Kind::Message { owner, .. } = &control.kind else {
                continue;
            };
            let row = Rect::new(area.x, area.y + slot as u16 * 3, area.width, 3);
            let focused = (self.selected == Some(*index) || *index == usize::MAX)
                && self.editor.as_ref().is_some_and(|editor| {
                    *index == usize::MAX
                        || editor.control.message_key(&editor.base.control_scope())
                            == self
                                .snapshot
                                .as_ref()
                                .and_then(|snapshot| control.message_key(&snapshot.control_scope()))
                });
            let key = self
                .snapshot
                .as_ref()
                .and_then(|snapshot| control.message_key(&snapshot.control_scope()));
            let editor = if focused {
                self.editor.as_ref()
            } else {
                self.message_drafts
                    .values()
                    .find(|editor| editor.control.message_key(&editor.base.control_scope()) == key)
            };
            let label_key = if focused {
                self.editor
                    .as_ref()
                    .and_then(|editor| editor.control.message_key(&editor.base.control_scope()))
            } else {
                key.clone()
            };
            let name = label_key
                .as_ref()
                .and_then(|key| self.message_labels.get(key))
                .cloned()
                .unwrap_or_else(|| crate::board_messages::fallback_label(owner, &control.label));
            let mut title = if rows.len() > visible {
                format!(
                    "{} · {}/{}",
                    safe_line(&name),
                    self.message_scroll + slot + 1,
                    rows.len()
                )
            } else {
                safe_line(&name)
            };
            if let Some(editor) = editor {
                if editor.uncertain {
                    title.push_str(" · Uncertain");
                } else if self.snapshot.as_ref().is_none_or(|snapshot| {
                    snapshot.revision != editor.base.revision
                        || snapshot.control_scope() != editor.base.control_scope()
                }) {
                    title.push_str(" · Review target");
                }
            }
            let block = Block::bordered()
                .title(title)
                .border_style(Style::default().fg(if focused {
                    Color::Yellow
                } else {
                    Color::DarkGray
                }));
            let inner = block.inner(row);
            frame.render_widget(block, row);
            let send_width = inner.width.min(7);
            let input = Rect::new(
                inner.x,
                inner.y,
                inner.width.saturating_sub(send_width),
                inner.height,
            );
            let send = Rect::new(input.right(), inner.y, send_width, inner.height);
            let content = editor.map_or_else(String::new, |editor| {
                if focused {
                    format!(
                        "{}▏{}",
                        safe_markdown(&editor.draft[..editor.cursor]),
                        safe_markdown(&editor.draft[editor.cursor..])
                    )
                } else {
                    safe_markdown(&editor.draft)
                }
            });
            let before = editor.map_or_else(String::new, |editor| {
                safe_markdown(&editor.draft[..editor.cursor])
            });
            let cursor_line = Paragraph::new(format!("{before}▏"))
                .wrap(Wrap { trim: false })
                .line_count(input.width.max(1));
            let scroll = cursor_line
                .saturating_sub(usize::from(input.height))
                .min(u16::MAX as usize) as u16;
            frame.render_widget(
                Paragraph::new(content)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0)),
                input,
            );
            frame.render_widget(
                Paragraph::new(if editor.is_some_and(|editor| editor.uncertain) {
                    "[Retry]"
                } else {
                    "[Send]"
                })
                .style(Style::default().fg(Color::Green)),
                send,
            );
            self.message_regions.push((input, *index, false));
            self.message_regions.push((send, *index, true));
        }
    }
    fn draw(&mut self, frame: &mut Frame<'_>) {
        let mut links_limited = false;
        let text_editor = self
            .editor
            .as_ref()
            .is_some_and(|editor| !matches!(editor.control.kind, Kind::Message { .. }));
        let ordinary_controls = self
            .controls
            .iter()
            .filter(|control| !matches!(control.kind, Kind::Message { .. }))
            .count();
        let message_count = self
            .controls
            .iter()
            .filter(|control| matches!(control.kind, Kind::Message { .. }))
            .count()
            + usize::from(self.editor.as_ref().is_some_and(|editor| {
                matches!(editor.control.kind, Kind::Message { .. }) && self.selected.is_none()
            }));
        let available = frame.area().height.saturating_sub(7);
        let response_height = if text_editor {
            7.min(available / 2)
        } else if ordinary_controls > 0 {
            3.min(available / 3)
        } else {
            0
        };
        let message_height = (message_count.saturating_mul(3).min(u16::MAX as usize) as u16)
            .min(available.saturating_sub(response_height).div_ceil(2).max(3))
            .min(available.saturating_sub(response_height))
            / 3
            * 3;
        let message_editing = self
            .editor
            .as_ref()
            .is_some_and(|editor| matches!(editor.control.kind, Kind::Message { .. }));
        let footer_height = if message_count > 0 { 2 } else { 3 };
        let [header, body, responses, messages, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(response_height),
            Constraint::Length(message_height),
            // Reserve the release-notice line even before a check completes so
            // availability changes never move the document or a mouse selection.
            Constraint::Length(footer_height),
        ])
        .areas(frame.area());
        let document_area = Block::bordered().inner(body);
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.area() != document_area)
        {
            self.selection = None;
            self.notice = Some("Text selection cancelled because the board layout changed.".into());
        }
        let (name, revision) = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                (
                    safe_line(&snapshot.name),
                    self.selection
                        .as_ref()
                        .and(self.viewport_metadata.as_ref())
                        .map_or(snapshot.revision.as_str(), |(revision, _)| {
                            revision.as_str()
                        })
                        .chars()
                        .take(12)
                        .collect::<String>(),
                )
            })
            .unwrap_or_else(|| (self.board.to_string(), "connecting".into()));
        let lifecycle_label = self
            .snapshot
            .as_ref()
            .filter(|snapshot| !snapshot.writable())
            .map(|snapshot| format!(" · READ-ONLY {:?}", snapshot.lifecycle.state))
            .unwrap_or_default();
        frame.render_widget(
            Paragraph::new(format!(
                "Local shared board · {name} · {}{lifecycle_label}",
                safe_line(&revision)
            ))
            .style(Style::default().fg(Color::Cyan)),
            header,
        );
        if let Some(action) = &self.lifecycle_confirm {
            let text = if action.restore {
                format!(
                    "Restore {}?\nArchived: {}\nScheduled permanent deletion: {}\nRestore before that deadline to return this exact board to the active list.\nEnter confirms Restore · Esc cancels",
                    action.base.name,
                    crate::board_time::archive_date(action.base.lifecycle.archived_at),
                    crate::board_time::archive_date(action.base.lifecycle.expires_at)
                )
            } else {
                format!(
                    "Archive {}?\nThe board becomes read-only and leaves the active list.\nAutomatic PERMANENT DELETION after 30 days (30 × 24 hours).\nRestore it before the deadline to keep it. No agent message is sent.\nEnter confirms Archive · Esc cancels",
                    action.base.name
                )
            };
            frame.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(Block::bordered().title("Confirm board lifecycle action")),
                body,
            );
            return;
        }
        if let Some((boards, selected)) = &self.picker {
            let rows: Vec<Line> = boards
                .iter()
                .enumerate()
                .map(|(i, board)| {
                    Line::from(format!(
                        "{} {}{}",
                        if i == *selected { ">" } else { " " },
                        board.name,
                        if self.picker_archived {
                            format!(
                                " · {:?} · archived {} · deletes {}",
                                board.lifecycle.state,
                                crate::board_time::archive_date(board.lifecycle.archived_at),
                                crate::board_time::archive_date(board.lifecycle.expires_at)
                            )
                        } else if board.exists {
                            String::new()
                        } else {
                            " (absent)".into()
                        }
                    ))
                })
                .collect();
            frame.render_widget(
                Paragraph::new(rows)
                    .scroll((
                        selected.saturating_sub(usize::from(body.height.saturating_sub(3))) as u16,
                        0,
                    ))
                    .block(Block::bordered().title(if self.picker_archived {
                        "Archived Boards · Tab active · Enter inspect · Esc cancel"
                    } else {
                        "Boards · Tab archived · j/k select · Enter open · Esc cancel"
                    })),
                body,
            );
            frame.render_widget(
                Paragraph::new(safe_line(self.update_notice.as_deref().unwrap_or(""))),
                footer,
            );
            return;
        }
        let identity = self
            .snapshot
            .as_ref()
            .map(|snapshot| format!("Board {}", safe_line(&snapshot.name)))
            .unwrap_or_default();
        let updated = crate::board_time::footer(
            match self.selection.as_ref().and(self.viewport_metadata.as_ref()) {
                Some((_, timestamp)) => *timestamp,
                None => self
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.last_updated),
            },
            body.width.saturating_sub(4),
        );
        let lifecycle_title = self
            .snapshot
            .as_ref()
            .filter(|snapshot| !snapshot.writable())
            .map(|snapshot| {
                format!(
                    "READ-ONLY {:?} · Archived {} · Deletes {}",
                    snapshot.lifecycle.state,
                    crate::board_time::archive_date(snapshot.lifecycle.archived_at),
                    crate::board_time::archive_date(snapshot.lifecycle.expires_at)
                )
            })
            .unwrap_or(identity);
        let block = Block::bordered()
            .title(lifecycle_title)
            .title_bottom(Line::from(updated).centered());
        let inner = block.inner(body);
        if self.width != inner.width {
            self.clear_copy_focus();
            self.code_cells.clear();
            let markdown = self
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.markdown.as_str())
                .unwrap_or("");
            (
                self.rendered,
                self.plain,
                self.link_mask,
                self.link_urls,
                self.code_mask,
                self.code_payloads,
            ) = rendered_board(markdown, inner.width);
            self.width = inner.width;
        }
        let content_height = self.paragraph().line_count(inner.width);
        self.max_scroll = (
            content_height
                .saturating_sub(usize::from(inner.height))
                .min(usize::from(u16::MAX)) as u16,
            if self.wrap {
                0
            } else {
                self.rendered
                    .width()
                    .saturating_sub(usize::from(inner.width))
                    .min(usize::from(u16::MAX)) as u16
            },
        );
        self.scroll.0 = self.scroll.0.min(self.max_scroll.0);
        self.scroll.1 = self.scroll.1.min(self.max_scroll.1);
        frame.render_widget(block, body);
        frame.render_widget(self.paragraph().scroll(self.scroll), inner);
        if let Some(selection) = &self.selection {
            self.code_cells.clear();
            links_limited = !selection.paint(frame.buffer_mut());
            if self.copy_focus {
                for (&position, &id) in selection.copy_cells() {
                    if self.focused_copy == Some(id) {
                        frame.buffer_mut()[position]
                            .set_style(Style::default().fg(Color::Black).bg(Color::Yellow));
                    }
                }
            }
        } else {
            let mut code_mask = Buffer::empty(inner);
            let code_paragraph = Paragraph::new(self.code_mask.clone());
            let code_paragraph = if self.wrap {
                code_paragraph.wrap(Wrap { trim: false })
            } else {
                code_paragraph
            };
            ratatui::widgets::Widget::render(
                code_paragraph.scroll(self.scroll),
                inner,
                &mut code_mask,
            );
            self.code_cells.clear();
            for y in inner.top()..inner.bottom() {
                for x in inner.left()..inner.right() {
                    let position = Position::new(x, y);
                    let cell = &code_mask[position];
                    if cell.bg == Color::Blue
                        && let Some(id) = crate::board_code_copy::color_id(cell.fg)
                        && id < self.code_payloads.len()
                    {
                        self.code_cells.insert(position, id);
                        if self.copy_focus && self.focused_copy == Some(id) {
                            frame.buffer_mut()[position]
                                .set_style(Style::default().fg(Color::Black).bg(Color::Yellow));
                        }
                    }
                }
            }
            self.viewport = Viewport::capture(frame.buffer_mut(), inner);
            if let Some(viewport) = &mut self.viewport {
                viewport.exclude_copy_buttons(self.code_cells.keys().copied());
                viewport.set_copy_controls(&self.code_cells, &self.code_payloads);
            }
            self.viewport_metadata = self
                .snapshot
                .as_ref()
                .map(|snapshot| (snapshot.revision.clone(), snapshot.last_updated));

            if !self.link_urls.is_empty() && self.hyperlinks_enabled {
                let mut mask = Buffer::empty(inner);
                let paragraph = Paragraph::new(self.link_mask.clone());
                let paragraph = if self.wrap {
                    paragraph.wrap(Wrap { trim: false })
                } else {
                    paragraph
                };
                ratatui::widgets::Widget::render(paragraph.scroll(self.scroll), inner, &mut mask);
                let links = crate::board_links::Hyperlinks::new(&mask, &self.link_urls);
                if let Some(viewport) = &mut self.viewport {
                    viewport.set_links(links.clone());
                }
                links_limited = !links.paint(frame.buffer_mut());
            }
        }
        if self.owner_details
            && let Some(Editor {
                control:
                    Control {
                        kind: Kind::Message { owner, .. },
                        ..
                    },
                ..
            }) = &self.editor
        {
            let details = format!(
                "Exact pinned owner\nName: {}\nProvider: {}\nSession: {}\nEndpoint: {}\n",
                safe_line(&owner.name),
                owner.provider.label(),
                safe_line(&owner.session),
                safe_line(&owner.endpoint)
            );
            let block =
                Block::bordered().title("Owner · arrows/PgUp/PgDn inspect · Esc/Ctrl-O return");
            let inner = block.inner(body);
            let paragraph = Paragraph::new(details).wrap(Wrap { trim: false });
            let limit = paragraph
                .line_count(inner.width)
                .saturating_sub(usize::from(inner.height));
            self.owner_scroll = self.owner_scroll.min(limit.min(u16::MAX as usize) as u16);
            frame.render_widget(Clear, body);
            frame.render_widget(block, body);
            frame.render_widget(paragraph.scroll((self.owner_scroll, 0)), inner);
        }
        if let Some(editor) = &self.editor
            && !matches!(editor.control.kind, Kind::Message { .. })
        {
            let title = format!(
                "Text response: {} · Ctrl-S save · Esc discard · Ctrl-R rebase",
                safe_line(&editor.control.label)
            );
            let block = Block::bordered().title(title);
            let inner = block.inner(responses);
            let before = safe_markdown(&editor.draft[..editor.cursor]);
            let after = safe_markdown(&editor.draft[editor.cursor..]);
            let content = format!("{before}▏{after}");
            let cursor_line = Paragraph::new(format!("{before}▏"))
                .wrap(Wrap { trim: false })
                .line_count(inner.width);
            let scroll = cursor_line
                .saturating_sub(usize::from(inner.height))
                .min(u16::MAX as usize) as u16;
            frame.render_widget(block, responses);
            frame.render_widget(
                Paragraph::new(content)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0)),
                inner,
            );
        } else if ordinary_controls > 0 {
            let title = format!(
                "Responses{} · Tab/Shift-Tab select · Space checkbox · Enter text · {} controls",
                if self.writable() { "" } else { " (read-only)" },
                self.controls.len()
            );
            let block = Block::bordered().title(title);
            let inner = block.inner(responses);
            let selected = self.selected.unwrap_or(0);
            let first = self
                .controls
                .iter()
                .take(selected)
                .filter(|control| !matches!(control.kind, Kind::Message { .. }))
                .count()
                .saturating_sub(1);
            let lines = self
                .controls
                .iter()
                .enumerate()
                .filter(|(_, control)| !matches!(control.kind, Kind::Message { .. }))
                .skip(first)
                .take(usize::from(inner.height))
                .map(|(index, control)| {
                    let marker = match &control.kind {
                        Kind::Checkbox { checked } => {
                            if *checked {
                                "[x]"
                            } else {
                                "[ ]"
                            }
                        }
                        Kind::Text { .. } => "[text]",
                        Kind::Message { .. } => "[message]",
                    };
                    let focused = self.selected == Some(index);
                    let line = Line::from(format!(
                        "{} line {} {marker} {}",
                        if focused { "▶" } else { " " },
                        control.line,
                        safe_line(&control.label)
                    ));
                    if focused {
                        line.style(Style::default().fg(Color::Yellow))
                    } else {
                        line
                    }
                })
                .collect::<Vec<_>>();
            frame.render_widget(block, responses);
            frame.render_widget(Paragraph::new(lines), inner);
        }
        self.draw_messages(frame, messages);
        let status = if self.selection.as_ref().is_some_and(Selection::is_dragging) {
            "Selecting text · view frozen · release mouse to copy · Esc cancels".into()
        } else if self.selection.is_some() {
            format!(
                "{} · view frozen · Esc clears selection",
                self.notice.as_deref().unwrap_or("Text selected")
            )
        } else if let Some(action) = &self.lifecycle_flight {
            format!(
                "{} pending… waiting for the exact receipt; board actions paused.",
                action.verb()
            )
        } else if self.saving {
            "Saving response…".into()
        } else if let Some(error) = &self.error {
            format!(
                "Offline/stale: {error} · retrying · {}",
                self.notice.as_deref().unwrap_or("")
            )
        } else if let Some(notice) = &self.notice {
            safe_line(notice)
        } else if self.snapshot.is_none() {
            "Connecting to the local broker…".into()
        } else if links_limited {
            "Hyperlink output limit reached · use --raw to inspect destinations".into()
        } else if self.plain {
            "Complex table shown as plain Markdown".into()
        } else if self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.markdown.is_empty())
        {
            "Empty board · ask an agent to update it with the board MCP tools".into()
        } else {
            format!(
                "Live · wrap {} · line {}",
                if self.wrap { "on" } else { "off" },
                usize::from(self.scroll.0) + 1
            )
        };
        let mut footer_lines = vec![Line::from(safe_line(&status))];
        if message_count == 0 && !message_editing {
            footer_lines.push(Line::from(if self.editor.is_some() { "Enter newline · arrows edit · PgUp/PgDn board · Ctrl-S save · Esc discard draft · then b boards · Ctrl-C quit" } else if self.mouse_enabled { "a archive · r restore/refresh · b boards · c code copies · m mouse · q quit · j/k scroll · Tab responses · Space toggle · Enter edit" } else { "a archive · r restore/refresh · b boards · c code copies · m mouse · q quit · j/k scroll · Tab responses · Space toggle · Enter edit · r refresh" }));
        }
        footer_lines.push(
            Line::from(safe_line(self.update_notice.as_deref().unwrap_or("")))
                .style(Style::default().fg(Color::Yellow)),
        );
        frame.render_widget(Paragraph::new(footer_lines), footer);
    }
    fn key(&mut self, key: KeyEvent, page: u16) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if self.lifecycle_confirm.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.lifecycle_confirm = None;
                    self.notice = Some("Archive/restore cancelled; board unchanged.".into());
                }
                KeyCode::Enter if key.kind != KeyEventKind::Repeat => {
                    let action = self.lifecycle_confirm.take().unwrap();
                    if self.action_current(&action) {
                        self.pending_lifecycle = Some(action);
                    } else {
                        self.notice = Some(
                            "Board changed; confirmation cancelled. Inspect and confirm again."
                                .into(),
                        );
                    }
                }
                _ => (),
            }
            return false;
        }
        if self.lifecycle_busy() {
            if key.code == KeyCode::Char('d')
                && key.modifiers.contains(KeyModifiers::CONTROL)
                && self.lifecycle_uncertain.is_some()
            {
                self.lifecycle_uncertain = None;
                self.notice = Some("Uncertain lifecycle tracking explicitly released; prior action may have committed. Inspect board before a new action.".into());
                return false;
            }
            // Inspect/scroll/copy remain available; no new navigation or writes.
            if matches!(
                key.code,
                KeyCode::Char('a' | 'b' | 'q')
                    | KeyCode::Esc
                    | KeyCode::Enter
                    | KeyCode::Char(' ')
                    | KeyCode::Tab
                    | KeyCode::BackTab
            ) && !(self.copy_focus
                && matches!(
                    key.code,
                    KeyCode::Tab | KeyCode::BackTab | KeyCode::Enter | KeyCode::Char(' ')
                ))
            {
                self.notice = Some("Archive/restore pending or uncertain. Inspect originating board; Ctrl-D releases uncertain tracking explicitly.".into());
                return false;
            }
        }
        let frozen_copy_key = self.editor.is_none()
            && self.picker.is_none()
            && self
                .selection
                .as_ref()
                .is_some_and(|selection| !selection.is_dragging())
            && ((key.code == KeyCode::Char('c') && key.modifiers.is_empty())
                || (self.copy_focus
                    && matches!(
                        key.code,
                        KeyCode::Tab
                            | KeyCode::BackTab
                            | KeyCode::Enter
                            | KeyCode::Char(' ')
                            | KeyCode::Esc
                    )));
        if !frozen_copy_key && self.selection.take().is_some() && key.code == KeyCode::Esc {
            self.notice = Some("Text selection cancelled.".into());
            return false;
        }
        if self
            .editor
            .as_ref()
            .is_some_and(|editor| matches!(editor.control.kind, Kind::Message { .. }))
        {
            return self.message_key(key, page);
        }
        if self.editor.is_none()
            && key.code == KeyCode::Char('e')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let eligible =
                |editor: &&Editor| !editor.draft.is_empty() || editor.submission.is_some();
            let retained = self
                .message_drafts
                .iter()
                .filter(|(_, editor)| eligible(editor))
                .find(|(key, _)| self.retained_cursor.as_ref().is_none_or(|last| *key > last))
                .or_else(|| {
                    self.message_drafts
                        .iter()
                        .find(|(_, editor)| eligible(editor))
                })
                .map(|(key, _)| key.clone());
            if let Some(key) = retained {
                self.retained_cursor = Some(key.clone());
                self.editor = self.message_drafts.remove(&key);
                self.selected = self.editor.as_ref().and_then(|editor| {
                    let key = editor.control.message_key(&editor.base.control_scope());
                    self.controls.iter().position(|control| {
                        self.snapshot.as_ref().is_some_and(|snapshot| {
                            control.message_key(&snapshot.control_scope()) == key
                        })
                    })
                });
                self.notice = Some("Retained draft reopened for its original target. Ctrl-R reviews current input; Ctrl-X explicitly discards.".into());
            }
            return false;
        }
        if self.editor.is_none()
            && key.code == KeyCode::Char('d')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            if !self.message_drafts.values().any(|editor| editor.in_flight) {
                let uncertain = self.message_drafts.values().any(|editor| editor.uncertain);
                self.message_drafts.clear();
                let save_uncertain = self.uncertain_save.take().is_some();
                self.retained_cursor = None;
                self.notice = Some(if uncertain || save_uncertain {
                    "Drafts and retry tracking discarded. Previous submissions may have been accepted; inspect message history before resending."
                } else { "All retained message drafts discarded; board unchanged." }.into());
            }
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if self.lifecycle_busy()
                || self.editor.is_some()
                || self.saving
                || self.pending.is_some()
                || self.uncertain_save.is_some()
                || self.has_message_drafts()
            {
                self.notice = Some(
                    "Unsent message drafts retained. Send them or Ctrl-D discards all; then quit."
                        .into(),
                );
                return false;
            }
            return true;
        }
        if self.editor.is_some() {
            if self.saving {
                return false;
            }
            match key.code {
                KeyCode::PageUp => self.scroll.0 = self.scroll.0.saturating_sub(page.max(1)),
                KeyCode::PageDown => self.scroll.0 = self.scroll.0.saturating_add(page.max(1)),
                KeyCode::Esc => {
                    let uncertain = self.editor.as_ref().is_some_and(|editor| editor.uncertain);
                    self.editor = None;
                    self.uncertain_save = None;
                    self.notice = Some(if uncertain { "Draft explicitly discarded. Previous save may have committed; inspect board before editing." } else { "Draft cancelled; board unchanged." }.into());
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.prepare_save()
                }
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.rebase_editor()
                }
                _ if self.editor.as_ref().is_some_and(|editor| editor.uncertain) => self.notice = Some("Save outcome uncertain; draft retained. Inspect the board before explicitly discarding with Esc.".into()),
                _ => self.editor.as_mut().unwrap().key(key),
            }
            return false;
        }
        if self.picker.is_some() {
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                self.picker_archived = !self.picker_archived;
                self.picker_epoch = self.picker_epoch.wrapping_add(1);
                self.picker = Some((Vec::new(), 0));
                self.pick_requested = true;
                return false;
            }
            let (boards, selected) = self.picker.as_mut().unwrap();
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.picker = None;
                    self.pick_requested = false;
                    self.picker_epoch = self.picker_epoch.wrapping_add(1);
                }
                KeyCode::Down | KeyCode::Char('j') if !boards.is_empty() => {
                    *selected = (*selected + 1) % boards.len()
                }
                KeyCode::Up | KeyCode::Char('k') if !boards.is_empty() => {
                    *selected = (*selected + boards.len() - 1) % boards.len()
                }
                KeyCode::Enter if !boards.is_empty() => {
                    let board = boards[*selected].name.clone();
                    self.switch(board);
                }
                _ => (),
            }
            return false;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.is_empty() {
            self.copy_focus = true;
            self.focused_copy = self.visible_copies().first().copied();
            self.notice = Some(
                if self.focused_copy.is_some() {
                    "Code copies · Tab/Shift-Tab select · Enter/Space copy · Esc leave"
                } else {
                    "No code copy buttons visible; scroll to code, then press c."
                }
                .into(),
            );
            return false;
        }
        if self.copy_focus {
            match key.code {
                KeyCode::Esc => {
                    self.clear_copy_focus();
                    self.notice = None;
                    return false;
                }
                KeyCode::Tab | KeyCode::BackTab => {
                    let ids = self.visible_copies();
                    if !ids.is_empty() {
                        let backwards = key.code == KeyCode::BackTab
                            || key.modifiers.contains(KeyModifiers::SHIFT);
                        let current = self
                            .focused_copy
                            .and_then(|id| ids.iter().position(|value| *value == id));
                        let next = match (current, backwards) {
                            (Some(index), true) => (index + ids.len() - 1) % ids.len(),
                            (Some(index), false) => (index + 1) % ids.len(),
                            (None, _) => 0,
                        };
                        self.focused_copy = Some(ids[next]);
                    }
                    return false;
                }
                KeyCode::Enter | KeyCode::Char(' ') => {
                    self.queue_focused_copy();
                    return false;
                }
                _ => self.clear_copy_focus(),
            }
        }
        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            if self.uncertain_save.is_some() || self.has_message_drafts() {
                self.notice = Some(
                    "Uncertain saves or unsent drafts retained. Inspect, then Ctrl-D explicitly discards tracking before quitting."
                        .into(),
                );
                return false;
            }
            return true;
        }
        match key.code {
            KeyCode::Char('a') => self.confirm_lifecycle(false),
            KeyCode::Char('r')
                if self
                    .snapshot
                    .as_ref()
                    .is_some_and(|snapshot| !snapshot.writable()) =>
            {
                self.confirm_lifecycle(true)
            }
            KeyCode::Char('b')
                if !self.saving
                    && self.pending.is_none()
                    && self.pending_message.is_none()
                    && self.uncertain_save.is_none()
                    && !self.lifecycle_busy() =>
            {
                if self.has_message_drafts() {
                    self.notice = Some("Send or discard message drafts before switching boards. Ctrl-D discards all; continue editing to cancel navigation.".into());
                } else {
                    self.picker_archived = false;
                    self.picker_epoch = self.picker_epoch.wrapping_add(1);
                    self.pick_requested = true;
                }
            }
            KeyCode::Char('m') => {
                self.mouse_enabled = !self.mouse_enabled;
                self.notice = Some(if self.mouse_enabled {
                    "Copy-on-select mouse enabled; drag board text to copy.".into()
                } else {
                    "Mouse capture disabled; use your terminal's native selection and copy.".into()
                });
            }
            KeyCode::Tab | KeyCode::BackTab if !self.controls.is_empty() && !self.saving => {
                let backwards =
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                self.selected = Some(match (self.selected, backwards) {
                    (Some(0), true) | (None, true) => self.controls.len() - 1,
                    (Some(index), true) => index - 1,
                    (Some(index), false) => (index + 1) % self.controls.len(),
                    (None, false) => 0,
                });
                self.notice = None;
                if let Some(index) = self.selected
                    && matches!(self.controls[index].kind, Kind::Message { .. })
                {
                    self.open_message(self.controls[index].clone());
                }
            }
            KeyCode::Char(' ') if !self.saving => self.toggle(),
            KeyCode::Enter if !self.saving => {
                if !self.writable() {
                    self.notice = Some(
                        "Board is read-only; retained drafts remain recoverable with Ctrl-E."
                            .into(),
                    );
                    return false;
                }
                if let (Some(index), Some(base)) = (self.selected, self.snapshot.as_ref()) {
                    let control = self.controls[index].clone();
                    if matches!(control.kind, Kind::Message { .. }) {
                        self.open_message(control);
                    } else if let Kind::Text { value, .. } = &control.kind {
                        self.editor = Some(Editor {
                            base: base.clone(),
                            draft: value.clone(),
                            cursor: value.len(),
                            control,
                            submission: None,
                            in_flight: false,
                            uncertain: false,
                        });
                        self.notice = None;
                    }
                }
            }
            KeyCode::Down | KeyCode::Char('j') => self.scroll.0 = self.scroll.0.saturating_add(1),
            KeyCode::Up | KeyCode::Char('k') => self.scroll.0 = self.scroll.0.saturating_sub(1),
            KeyCode::PageDown => self.scroll.0 = self.scroll.0.saturating_add(page.max(1)),
            KeyCode::PageUp => self.scroll.0 = self.scroll.0.saturating_sub(page.max(1)),
            KeyCode::Home => self.scroll = (0, 0),
            KeyCode::End => self.scroll.0 = self.max_scroll.0,
            KeyCode::Right | KeyCode::Char('l') => {
                self.wrap = false;
                self.scroll.1 = self.scroll.1.saturating_add(4);
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.wrap = false;
                self.scroll.1 = self.scroll.1.saturating_sub(4);
            }
            KeyCode::Char('w') => self.wrap = !self.wrap,
            _ => (),
        }
        self.code_cells.clear();
        false
    }
    fn clear_copy_focus(&mut self) {
        self.copy_focus = false;
        self.focused_copy = None;
    }
    fn displayed_copy_cells(&self) -> &HashMap<Position, usize> {
        self.selection
            .as_ref()
            .map_or(&self.code_cells, Selection::copy_cells)
    }
    fn displayed_copy_payload(&self, id: usize) -> Option<&str> {
        if let Some(selection) = &self.selection {
            selection.copy_payload(id)
        } else {
            self.code_payloads.get(id).map(String::as_str)
        }
    }
    fn visible_copies(&self) -> Vec<usize> {
        let mut positions: Vec<_> = self.displayed_copy_cells().iter().collect();
        positions.sort_by_key(|(position, _)| (position.y, position.x));
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        for (_, &id) in positions {
            if seen.insert(id) {
                ids.push(id);
            }
        }
        ids
    }
    fn queue_focused_copy(&mut self) {
        if let Some(id) = self.focused_copy.filter(|id| {
            self.displayed_copy_cells()
                .values()
                .any(|visible| visible == id)
        }) {
            self.pending_copy = self.displayed_copy_payload(id).map(str::to_owned);
        }
    }
    fn send_copy(&mut self, text: &str) {
        if text.is_empty() {
            self.notice = Some("Code block is empty; clipboard unchanged.".into());
            return;
        }
        self.notice = Some(match crate::board_clipboard::copy_to_terminal(text) {
            Ok(()) => format!(
                "Copy sent to terminal ({} bytes). Press m for native selection if needed.",
                text.len()
            ),
            Err(error) => format!("Cannot send copy to terminal: {error:#}"),
        });
    }
    fn mouse(&mut self, event: MouseEvent) -> (bool, Option<String>) {
        if !self.mouse_enabled
            || self.picker.is_some()
            || self.lifecycle_confirm.is_some()
            || self.owner_details
        {
            return (false, None);
        }
        let point = Position::new(event.column, event.row);
        if self.message_area.contains(point) {
            if matches!(
                event.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            ) {
                if self
                    .editor
                    .as_ref()
                    .is_some_and(|editor| matches!(editor.control.kind, Kind::Message { .. }))
                {
                    self.retain_message();
                }
                self.selected = None;
                self.message_scroll = if event.kind == MouseEventKind::ScrollUp {
                    self.message_scroll.saturating_sub(1)
                } else {
                    self.message_scroll.saturating_add(1)
                };
                return (true, None);
            }
            if event.kind == MouseEventKind::Down(MouseButton::Left)
                && let Some((_, index, send)) = self
                    .message_regions
                    .iter()
                    .find(|(area, _, _)| area.contains(point))
                    .copied()
            {
                if self.saving
                    || self
                        .editor
                        .as_ref()
                        .is_some_and(|editor| !matches!(editor.control.kind, Kind::Message { .. }))
                {
                    self.notice =
                        Some("Save or cancel the response before composing a message.".into());
                    return (true, None);
                }
                if index != usize::MAX && (self.selected != Some(index) || self.editor.is_none()) {
                    if self.editor.is_some() {
                        self.retain_message();
                    }
                    self.selected = Some(index);
                    self.open_message(self.controls[index].clone());
                }
                self.selection = None;
                self.clear_copy_focus();
                if send {
                    self.prepare_message();
                }
                return (true, None);
            }
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && self
                .viewport
                .as_ref()
                .is_some_and(|viewport| viewport.area().contains(point))
            && self
                .editor
                .as_ref()
                .is_some_and(|editor| matches!(editor.control.kind, Kind::Message { .. }))
        {
            self.retain_message();
        }
        if self.editor.is_none()
            && self
                .selection
                .as_ref()
                .is_none_or(|selection| !selection.is_dragging())
            && event.modifiers.is_empty()
            && event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(id) = self.displayed_copy_cells().get(&point).copied()
        {
            let payload = self.displayed_copy_payload(id).map(str::to_owned);
            self.selection = None;
            self.clear_copy_focus();
            // The next draw may reveal a newer snapshot behind the frozen body.
            self.code_cells.clear();
            return (true, payload);
        }

        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // A duplicate press while dragging cancels rather than selecting
                // unseen data from a newly rendered revision behind the frozen view.
                let previous = self.selection.take();
                if previous.as_ref().is_some_and(Selection::is_dragging) {
                    return (true, None);
                }
                if let Some(viewport) = &self.viewport
                    && viewport.area().contains(point)
                    && self.snapshot.is_some()
                {
                    self.selection = Selection::start(viewport.clone(), point);
                    self.notice = None;
                    return (true, None);
                }
                return (previous.is_some(), None);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(selection) = &mut self.selection {
                    return (selection.extend(point), None);
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(selection) = &mut self.selection
                    && selection.is_dragging()
                {
                    let text = selection.finish(point);
                    if text.is_none() {
                        self.selection = None;
                    }
                    return (true, text);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.selection = None;
                self.clear_copy_focus();
                self.code_cells.clear();
                self.scroll.0 = if event.kind == MouseEventKind::ScrollUp {
                    self.scroll.0.saturating_sub(3)
                } else {
                    self.scroll.0.saturating_add(3)
                };
                return (true, None);
            }
            _ => (),
        }
        (false, None)
    }
    fn paste(&mut self, input: &str) {
        if self.saving {
            return;
        }
        let Some(editor) = &mut self.editor else {
            return;
        };
        if editor.in_flight || editor.uncertain {
            self.notice = Some("Message outcome pending/uncertain; retry the exact message or explicitly discard before editing.".into());
            return;
        }
        let normalized = input.replace("\r\n", "\n").replace('\r', "\n");
        let text = normalized
            .chars()
            .filter(|ch| !ch.is_control() || matches!(ch, '\n' | '\t'))
            .collect::<String>();
        if editor.draft.len().saturating_add(text.len()) > MAX_BOARD_BYTES {
            self.notice = Some("Paste exceeds 64 KiB; draft unchanged.".into());
        } else {
            editor.insert(&text);
        }
    }
    fn toggle(&mut self) {
        if !self.writable() {
            self.notice = Some("Board is read-only; checkbox unchanged.".into());
            return;
        }
        if let (Some(index), Some(base)) = (self.selected, self.snapshot.as_ref())
            && matches!(self.controls[index].kind, Kind::Checkbox { .. })
        {
            match self.controls[index].replacement(&base.markdown, None) {
                Ok(markdown) => {
                    self.pending = Some(Save {
                        generation: self.generation,
                        base: base.clone(),
                        markdown,
                        text: false,
                    })
                }
                Err(error) => self.notice = Some(format!("Cannot toggle: {error:#}")),
            }
        }
    }
    fn prepare_save(&mut self) {
        if self.editor.as_ref().is_some_and(|editor| editor.uncertain) {
            self.notice = Some("Previous save outcome uncertain; inspect board, then explicitly discard the draft before a new save.".into());
            return;
        }
        let Some(editor) = &self.editor else {
            return;
        };
        if self.snapshot.as_ref().is_none_or(|snapshot| {
            !snapshot.writable()
                || snapshot.control_scope() != editor.base.control_scope()
                || snapshot.revision != editor.base.revision
        }) {
            self.notice = Some("Board changed; draft retained. Review latest board; Ctrl-R rebases an unchanged field.".into());
            return;
        }
        match editor
            .control
            .replacement(&editor.base.markdown, Some(&editor.draft))
        {
            Ok(markdown) => {
                self.pending = Some(Save {
                    generation: self.generation,
                    base: editor.base.clone(),
                    markdown,
                    text: true,
                })
            }
            Err(error) => self.notice = Some(format!("Cannot save: {error:#}")),
        }
    }
    fn rebase_editor(&mut self) {
        let (Some(editor), Some(snapshot)) = (&mut self.editor, &self.snapshot) else {
            return;
        };
        if editor.uncertain
            || !snapshot.writable()
            || snapshot.control_scope() != editor.base.control_scope()
        {
            self.notice = Some("Board lifecycle changed or is read-only; original response draft retained. Cannot rebase across archive or restore.".into());
            return;
        }
        let key = editor.control.text_rebase_key(&editor.base.control_scope());
        let current = self.controls.iter().find(|control| {
            key.is_some() && control.text_rebase_key(&snapshot.control_scope()) == key
        });
        if let Some(control) = current {
            editor.base = snapshot.clone();
            editor.control = control.clone();
            self.notice = Some(
                "Draft rebased. Review the latest question on the board before Ctrl-S.".into(),
            );
        } else {
            self.notice = Some("Field changed or disappeared; draft retained and cannot overwrite it. Esc cancels.".into());
        }
    }
    fn saved(&mut self, save: Save, result: Result<(Snapshot, bool)>) {
        if save.generation != self.generation || save.base.name != self.board.as_str() {
            return;
        }
        let result = result.and_then(|(snapshot, applied)| {
            ensure!(
                snapshot.name == self.board.as_str(),
                "Broker returned a different board for a save"
            );
            Ok((snapshot, applied))
        });
        self.saving = false;
        match result {
            Ok((snapshot, applied)) => {
                let lifecycle_changed = self.snapshot.as_ref().is_some_and(|current| {
                    !current.writable() || current.control_scope() != save.base.control_scope()
                });
                // A late save receipt must not roll back a newer lifecycle/read.
                if self.snapshot.as_ref().is_none_or(|current| {
                    current.revision == save.base.revision
                        && current.control_scope() == save.base.control_scope()
                }) {
                    self.update(Ok(snapshot));
                }
                if applied {
                    if save.text && lifecycle_changed {
                        self.notice = Some("Earlier response save accepted, but board lifecycle changed. Original draft retained; cannot rebase or save it into the current lifecycle. Esc explicitly discards.".into());
                    } else {
                        if save.text {
                            self.editor = None;
                        }
                        self.notice = Some(if lifecycle_changed { "Earlier checkbox save accepted before the board lifecycle changed. Current board retained; no new write sent." } else { "Response saved to Markdown." }.into());
                    }
                } else {
                    self.notice = Some(if save.text { "Save conflict; draft retained. Review board; Ctrl-R rebases an unchanged field." } else { "Board changed; checkbox was not toggled. Review and select it again." }.into());
                }
            }
            Err(error) => {
                self.uncertain_save = Some(save);
                if let Some(editor) = &mut self.editor {
                    editor.uncertain = true;
                }
                self.notice = Some(format!(
                    "Save result uncertain: {error:#}. Refresh and inspect the board before retrying; draft retained."
                ));
            }
        }
    }
}

struct FileWatch {
    root: PathBuf,
    _watcher: RecommendedWatcher,
}
fn board_event(event: &notify::Event, board: &Path, parent: &Path) -> bool {
    !matches!(event.kind, notify::EventKind::Access(_))
        && event
            .paths
            .iter()
            .any(|path| path == board || path == parent)
}

fn file_watch(path: &Path, state_dir: &Path, refresh: mpsc::Sender<()>) -> Result<FileWatch> {
    let parent = path
        .parent()
        .context("Board path has no parent")?
        .to_path_buf();
    let root = if parent.is_dir() {
        parent.clone()
    } else {
        state_dir.to_path_buf()
    };
    let board = path.to_path_buf();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if event.map_or(true, |event| board_event(&event, &board, &parent)) {
            let _ = refresh.try_send(());
        }
    })?;
    watcher.watch(&root, RecursiveMode::NonRecursive)?;
    Ok(FileWatch {
        root,
        _watcher: watcher,
    })
}

fn cached_release_notice(state_dir: &Path) -> Option<String> {
    // A missing/unreadable optional cache must not interfere with board use.
    let status = crate::update::cached_status(state_dir).ok()?;
    let available = status.available_version?;
    Some(format!(
        "Update available: {} → {available} · run agent-bridge update",
        status.current_version
    ))
}

pub async fn run(state_dir: PathBuf) -> Result<()> {
    run_named(state_dir, Name::default()).await
}

async fn read_composer_snapshot(
    state: &Path,
    board: &Name,
) -> Result<(Snapshot, crate::board_messages::DisplayLabels)> {
    // Keep the frozen legacy read contract intact. New brokers can inspect main
    // with presentation metadata; older brokers retain the safe main fallback.
    let value = match broker::rpc(
        state,
        &Request::BoardInspect {
            board: board.clone(),
        },
    )
    .await
    {
        Ok(value) => value,
        Err(_) if board.is_main() => broker::rpc(state, &Request::BoardRead).await?,
        Err(error) => return Err(error),
    };
    let labels = match value.get("message_labels") {
        Some(value) => {
            serde_json::from_value(value.clone()).context("Invalid composer labels from broker")?
        }
        None => BTreeMap::new(),
    };
    Ok((
        serde_json::from_value(value).context("Invalid board snapshot from broker")?,
        labels,
    ))
}

pub async fn run_named(state_dir: PathBuf, board: Name) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "The board viewer needs an interactive terminal; use board --raw to print Markdown"
    );
    let (initial, initial_labels) = read_composer_snapshot(&state_dir, &board).await?;
    ensure!(
        initial.name == board.as_str(),
        "Broker returned a different board"
    );
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let _restore = TerminalRestore;
    let mut terminal = ratatui::try_init().context("Cannot initialize the board terminal")?;
    crossterm::execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture)
        .context("Cannot enable safe terminal paste")?;
    let mut events = EventStream::new();
    let (refresh, mut requests) = mpsc::channel(1);
    let (writes, mut write_requests) = mpsc::channel::<Save>(1);
    let (messages, mut message_requests) = mpsc::channel::<Submission>(1);
    let (selection, current_selection) = tokio::sync::watch::channel((board.clone(), 0u64));
    let (lists, mut list_requests) = mpsc::channel::<(bool, u64)>(1);
    let (lifecycle, mut lifecycle_requests) = mpsc::channel::<LifecycleAction>(1);
    enum Update {
        List(Name, u64, bool, u64, Result<Vec<Summary>>),
        Lifecycle(LifecycleAction, Result<Snapshot>),
        Read(
            Name,
            u64,
            Result<(Snapshot, crate::board_messages::DisplayLabels)>,
            Option<String>,
        ),
        Saved(Save, Result<(Snapshot, bool)>),
        Message(Submission, Result<serde_json::Value>),
    }
    let (updates, mut results) = mpsc::channel(1);
    let state = state_dir.clone();
    let worker = tokio::spawn(async move {
        loop {
            let update = tokio::select! {
                biased;
                action = lifecycle_requests.recv() => {
                    let Some(action) = action else { break; };
                    let result = async {
                        let board: Name = action.base.name.parse()?;
                        let expected_revision = action.base.revision.clone();
                        let request = if action.restore { Request::BoardRestore { board, expected_revision } } else { Request::BoardArchive { board, expected_revision } };
                        serde_json::from_value::<Snapshot>(broker::rpc(&state, &request).await?).context("Invalid archive/restore receipt")
                    }.await;
                    Update::Lifecycle(action, result)
                }
                message = message_requests.recv() => {
                    let Some(request) = message else { break; };
                    let result = broker::rpc(&state, &Request::BoardMessageSend { request: request.clone() }).await;
                    Update::Message(request, result)
                }
                save = write_requests.recv() => {
                    let Some(save) = save else { break; };
                    let result = async {
                        let name: Name = save.base.name.parse()?;
                        let value = broker::rpc(&state, &Request::board_update(name, save.base.revision.clone(), vec![Edit::Set { markdown: save.markdown.clone() }], None)).await?;
                        let applied = value["applied"].as_bool().context("Invalid save response from broker")?;
                        let snapshot = serde_json::from_value::<Snapshot>(value["board"].clone()).context("Invalid saved board snapshot")?;
                        Ok((snapshot, applied))
                    }.await;
                    Update::Saved(save, result)
                }
                request = list_requests.recv() => {
                    let Some((archived, epoch)) = request else { break; };
                    let (board, generation) = current_selection.borrow().clone();
                    Update::List(board, generation, archived, epoch, async {
                        let request = if archived { Request::BoardArchivedList } else { Request::BoardList };
                        let value = broker::rpc(&state, &request).await?;
                        serde_json::from_value(value).context("Invalid board list from broker")
                    }.await)
                }
                request = requests.recv() => {
                    if request.is_none() { break; }
                    let (board, generation) = current_selection.borrow().clone();
                    let result = read_composer_snapshot(&state, &board).await;
                    // Availability is broker-owned cached metadata. Reading it
                    // never fetches a release or changes the board document.
                    let notice = cached_release_notice(&state);
                    Update::Read(board, generation, result, notice)
                }
            };
            if updates.send(update).await.is_err() {
                break;
            }
        }
    });
    let _worker = AbortWorker(worker);
    let _ = refresh.try_send(());
    let mut fallback = tokio::time::interval(FALLBACK_REFRESH);
    fallback.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut view = View {
        board,
        hyperlinks_enabled: crate::board_links::enabled(),
        ..View::default()
    };
    view.message_labels = initial_labels;
    view.update(Ok(initial));
    let mut watch: Option<FileWatch> = None;
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| view.draw(frame))?;
            dirty = false;
        }
        tokio::select! {
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            event = events.next() => match event {
                Some(Ok(Event::Key(key))) => {
                    let previous_generation = view.generation;
                    let previous_mouse_mode = view.mouse_enabled;
                    if view.key(key, terminal.size()?.height.saturating_sub(5)) { break; }
                    if let Some(text) = view.pending_copy.take() { view.send_copy(&text); }
                    if view.pick_requested {
                        view.pick_requested = false;
                        if lists.try_send((view.picker_archived, view.picker_epoch)).is_err() { view.pick_requested = true; view.notice = Some("Board catalog request pending…".into()); }
                    }
                    if previous_generation != view.generation {
                        selection.send_replace((view.board.clone(), view.generation));
                        watch = None;
                        let _ = refresh.try_send(());
                    }
                    if previous_mouse_mode != view.mouse_enabled {
                        if view.mouse_enabled {
                            crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
                        } else {
                            crossterm::execute!(std::io::stdout(), DisableMouseCapture)?;
                        }
                    }
                    if key.code == KeyCode::Char('r') && view.editor.is_none() { let _ = refresh.try_send(()); }
                    dirty = true;
                }
                Some(Ok(Event::Paste(text))) => { view.paste(&text); dirty = true; }
                Some(Ok(Event::Mouse(event))) => {
                    let (changed, copy) = view.mouse(event);
                    if let Some(text) = copy { view.send_copy(&text); }
                    dirty |= changed;
                }
                Some(Ok(Event::Resize(..))) => { view.selection = None; view.viewport = None; view.code_cells.clear(); view.clear_copy_focus(); dirty = true; },
                Some(Ok(_)) => (),
                Some(Err(error)) => return Err(error).context("Cannot read terminal input"),
                None => break,
            },
            _ = fallback.tick() => {
                let _ = refresh.try_send(());
                if view.pick_requested && lists.try_send((view.picker_archived, view.picker_epoch)).is_ok() { view.pick_requested = false; }
            },
            update = results.recv() => {
                let Some(update) = update else { anyhow::bail!("Board refresh worker stopped"); };
                let relevant = match &update {
                    Update::Read(board, generation, ..) => board == &view.board && *generation == view.generation,
                    Update::Message(..) => true,
                    Update::Saved(save, _) => save.generation == view.generation && save.base.name == view.board.as_str(),
                    Update::List(board, generation, archived, epoch, _) => board == &view.board && *generation == view.generation && *archived == view.picker_archived && *epoch == view.picker_epoch && view.editor.is_none() && !view.saving,
                    Update::Lifecycle(action, _) => action.generation == view.generation && action.base.name == view.board.as_str(),
                };
                if !relevant { continue; }
                let snapshot = match &update { Update::Read(_, _, Ok((snapshot, _)), _) | Update::Saved(_, Ok((snapshot, _))) => Some(snapshot), _ => None };
                if snapshot.is_some() {
                    // RPC snapshots carry logical identity only. The private file
                    // watcher is an internal optimization, never a display value.
                    let path = state_dir.join("boards").join(format!("{}.md", view.board));
                    let root = path.parent().filter(|parent| parent.is_dir()).unwrap_or(&state_dir);
                    if watch.as_ref().is_none_or(|watch| watch.root != root) {
                        // Watching is an optimization; the bounded periodic refresh also
                        // handles unsupported filesystems and atomic replacement races.
                        watch = file_watch(&path, &state_dir, refresh.clone()).ok();
                    }
                }
                match update {
                    Update::Lifecycle(action, result) => { view.lifecycle_result(action, result); let _ = refresh.try_send(()); dirty = true; },
                    Update::List(_, _, _, _, result) => {
                        match result {
                            Ok(mut boards) => {
                                boards.sort_by(|a, b| a.name.cmp(&b.name));
                                let selected = boards.iter().position(|board| board.name == view.board).unwrap_or(0);
                                view.picker = Some((boards, selected));
                            }
                            Err(error) => view.notice = Some(format!("Cannot list boards; upgrade/reconnect the broker if needed: {error:#}")),
                        }
                        dirty = true;
                    }
                    Update::Read(board, generation, result, notice) => {
                        dirty |= view.update_release_notice(notice);
                        let result = result.map(|(snapshot, labels)| {
                            if view.message_labels != labels { view.message_labels = labels; dirty = true; }
                            snapshot
                        });
                        dirty |= view.read_result(&board, generation, result);
                    },
                    Update::Saved(save, result) => { view.saved(save, result); dirty = true; },
                    Update::Message(request, result) => { view.message_result(request, result); dirty = true; },
                }
            }
        }
        // Drain actions after every event source, including mouse Send clicks.
        if let Some(action) = view.pending_lifecycle.take() {
            view.lifecycle_flight = Some(action.clone());
            if lifecycle.try_send(action.clone()).is_err() {
                view.lifecycle_result(action, Err(anyhow::anyhow!("Lifecycle worker unavailable")));
            }
        }
        if let Some(request) = view.pending_message.take()
            && messages.try_send(request.clone()).is_err()
        {
            view.message_result(
                request,
                Err(anyhow::anyhow!("Submission worker unavailable")),
            );
        }
        if let Some(save) = view.pending.take() {
            if writes.try_send(save).is_ok() {
                view.saving = true;
            } else {
                view.notice = Some("Another save is pending; draft retained.".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    fn snapshot(markdown: &str, revision: &str) -> Snapshot {
        Snapshot {
            name: "main".into(),
            markdown: markdown.into(),
            revision: revision.into(),
            path: PathBuf::from("/private/boards/main.md"),
            exists: true,
            last_updated: None,
            last_updated_source: Default::default(),
            lifecycle: Default::default(),
            file_identity: None,
        }
    }
    fn lifecycle_snapshot(
        markdown: &str,
        revision: &str,
        generation: &str,
        state: LifecycleState,
    ) -> Snapshot {
        let mut value = snapshot(markdown, revision);
        value.name = "project".into();
        value.lifecycle.incarnation = Some("00000000-0000-4000-8000-000000000002".into());
        value.lifecycle.lifecycle_generation = Some(generation.into());
        value.lifecycle.state = state;
        if !value.writable() {
            value.lifecycle.archived_at = Some(1_768_435_200_000);
            value.lifecycle.expires_at = Some(1_771_027_200_000);
        }
        value
    }
    #[test]
    fn lifecycle_confirmation_is_explicit_pinned_and_main_is_protected() {
        let mut main = View::default();
        main.update(Ok(snapshot("# Main", "one")));
        press(&mut main, KeyCode::Char('a'), KeyModifiers::NONE);
        assert!(main.lifecycle_confirm.is_none());
        assert!(main.notice.as_deref().unwrap().contains("protected"));
        let mut view = View {
            board: "project".parse().unwrap(),
            ..View::default()
        };
        view.update(Ok(lifecycle_snapshot(
            "same bytes",
            "one",
            "first",
            LifecycleState::Active,
        )));
        press(&mut view, KeyCode::Char('a'), KeyModifiers::NONE);
        assert_eq!(
            view.lifecycle_confirm.as_ref().unwrap().base.revision,
            "one"
        );
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        assert!(view.pending_lifecycle.is_none());
        press(&mut view, KeyCode::Char('a'), KeyModifiers::NONE);
        view.update(Ok(lifecycle_snapshot(
            "same bytes",
            "two",
            "second",
            LifecycleState::Active,
        )));
        assert!(view.lifecycle_confirm.is_none());
        assert!(view.pending_lifecycle.is_none());
        press(&mut view, KeyCode::Char('a'), KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let action = view.pending_lifecycle.take().unwrap();
        assert_eq!(action.base.revision, "two");
        assert!(!action.restore);
        assert!(view.pending_message.is_none());
    }
    #[test]
    fn external_archive_retains_response_and_same_bytes_restore_cannot_rebase() {
        let markdown = field("answer");
        let mut view = View {
            board: "project".parse().unwrap(),
            ..View::default()
        };
        view.update(Ok(lifecycle_snapshot(
            &markdown,
            "one",
            "first",
            LifecycleState::Active,
        )));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste(" retained");
        let old_scope = view.editor.as_ref().unwrap().base.control_scope();
        view.update(Ok(lifecycle_snapshot(
            &markdown,
            "two",
            "archived",
            LifecycleState::Archived,
        )));
        press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(view.pending.is_none());
        assert_eq!(view.editor.as_ref().unwrap().draft, "answer retained");
        view.update(Ok(lifecycle_snapshot(
            &markdown,
            "three",
            "restored",
            LifecycleState::Active,
        )));
        press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(view.pending.is_none());
        assert_eq!(
            view.editor.as_ref().unwrap().base.control_scope(),
            old_scope
        );
        assert!(!view.switch(Name::default()));
    }
    #[test]
    fn archived_message_only_recovers_exact_uncertain_request_and_keeps_old_draft_scope() {
        let markdown = message_field("coordinator", "alpha");
        let mut view = View {
            board: "project".parse().unwrap(),
            ..View::default()
        };
        view.update(Ok(lifecycle_snapshot(
            &markdown,
            "one",
            "first",
            LifecycleState::Active,
        )));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste("exact message");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let original = view.pending_message.take().unwrap();
        view.message_result(original.clone(), Err(anyhow::anyhow!("timeout")));
        let draft_key = View::draft_key(view.editor.as_ref().unwrap());
        view.update(Ok(lifecycle_snapshot(
            &markdown,
            "two",
            "archived",
            LifecycleState::Archived,
        )));
        press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let retried = view.pending_message.take().unwrap();
        assert_eq!(
            serde_json::to_value(&retried).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
        view.message_result(retried, Err(anyhow::anyhow!("still uncertain")));
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        assert!(view.message_drafts.contains_key(&draft_key));
        view.update(Ok(lifecycle_snapshot(
            &markdown,
            "three",
            "restored",
            LifecycleState::Active,
        )));
        view.open_message(view.controls[0].clone());
        assert!(view.editor.as_ref().unwrap().draft.is_empty());
        assert!(view.message_drafts.contains_key(&draft_key));
    }
    #[test]
    fn lifecycle_phase_refreshes_and_late_save_cannot_replace_archived_snapshot() {
        let markdown = field("answer");
        let mut view = View {
            board: "project".parse().unwrap(),
            ..View::default()
        };
        let active = lifecycle_snapshot(&markdown, "one", "first", LifecycleState::Active);
        view.update(Ok(active.clone()));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste(" saved");
        view.prepare_save();
        let save = view.pending.take().unwrap();
        let accepted =
            lifecycle_snapshot(&save.markdown, "accepted", "first", LifecycleState::Active);
        let archived =
            lifecycle_snapshot(&save.markdown, "two", "archived", LifecycleState::Archived);
        view.update(Ok(archived.clone()));
        view.saved(save, Ok((accepted, true)));
        assert_eq!(view.editor.as_ref().unwrap().draft, "answer saved");
        assert_eq!(
            view.editor.as_ref().unwrap().base.control_scope(),
            active.control_scope()
        );
        assert!(
            view.notice
                .as_deref()
                .unwrap()
                .contains("Original draft retained")
        );
        assert_eq!(
            view.snapshot.as_ref().unwrap().control_scope(),
            archived.control_scope()
        );
        assert!(!view.writable());
        let mut purging = archived;
        purging.lifecycle.state = LifecycleState::Purging;
        assert!(view.update(Ok(purging)));
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE); // Explicitly discard retained draft.
        press(&mut view, KeyCode::Char('r'), KeyModifiers::NONE);
        assert!(view.lifecycle_confirm.is_none());
    }

    #[test]
    fn accepted_late_save_retains_draft_after_same_bytes_restore_or_recreation() {
        for recreated in [false, true] {
            let mut view = View {
                board: "project".parse().unwrap(),
                ..View::default()
            };
            let active =
                lifecycle_snapshot(&field("answer"), "one", "first", LifecycleState::Active);
            view.update(Ok(active.clone()));
            press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
            press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
            view.paste(" saved");
            view.prepare_save();
            let save = view.pending.take().unwrap();
            let receipt =
                lifecycle_snapshot(&save.markdown, "accepted", "first", LifecycleState::Active);
            let mut current =
                lifecycle_snapshot(&save.markdown, "restored", "next", LifecycleState::Active);
            if recreated {
                current.lifecycle.incarnation = Some("00000000-0000-4000-8000-000000000003".into());
            }
            view.update(Ok(current.clone()));
            view.saved(save, Ok((receipt, true)));
            let editor = view
                .editor
                .as_ref()
                .expect("late old receipt must retain original draft");
            assert_eq!(editor.draft, "answer saved");
            assert_eq!(editor.base.control_scope(), active.control_scope());
            assert_eq!(
                view.snapshot.as_ref().unwrap().control_scope(),
                current.control_scope()
            );
            assert!(view.pending.is_none());
            press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
            press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
            assert!(view.pending.is_none());
            assert_eq!(view.editor.as_ref().unwrap().draft, "answer saved");
            assert_eq!(
                view.editor.as_ref().unwrap().base.control_scope(),
                active.control_scope()
            );
            assert!(!view.switch(Name::default()));
            press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
            assert!(view.editor.is_none());
        }
    }

    #[test]
    fn uncertain_checkbox_blocks_archive_navigation_until_explicit_tracking_discard() {
        let mut view = View {
            board: "project".parse().unwrap(),
            ..View::default()
        };
        view.update(Ok(lifecycle_snapshot(
            "- [ ] check",
            "one",
            "first",
            LifecycleState::Active,
        )));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        view.toggle();
        let save = view.pending.take().unwrap();
        view.saved(save, Err(anyhow::anyhow!("receipt lost")));
        assert!(view.uncertain_save.is_some());
        assert!(!view.switch(Name::default()));
        view.confirm_lifecycle(false);
        assert!(view.lifecycle_confirm.is_none());
        assert!(!view.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 10));
        press(&mut view, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(view.uncertain_save.is_none());
        view.confirm_lifecycle(false);
        assert!(view.lifecycle_confirm.is_some());
    }
    #[test]
    fn restore_confirmation_expires_on_phase_refresh_and_picker_cancel_invalidates_epoch() {
        let mut view = View {
            board: "project".parse().unwrap(),
            ..View::default()
        };
        let mut archived = lifecycle_snapshot("same", "one", "archived", LifecycleState::Archived);
        view.update(Ok(archived.clone()));
        view.confirm_lifecycle(true);
        assert!(view.lifecycle_confirm.is_some());
        archived.lifecycle.state = LifecycleState::Purging;
        view.update(Ok(archived));
        assert!(view.lifecycle_confirm.is_none());
        view.confirm_lifecycle(true);
        assert!(view.lifecycle_confirm.is_none());
        view.picker = Some((Vec::new(), 0));
        view.pick_requested = true; // A full catalog channel scheduled a fallback retry.
        let epoch = view.picker_epoch;
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        assert!(view.picker.is_none());
        assert_ne!(view.picker_epoch, epoch);
        assert!(
            !view.pick_requested,
            "cancel must not enqueue a new-epoch fallback request"
        );
        view.picker = Some((Vec::new(), 0));
        view.pick_requested = true;
        assert!(!view.switch(view.board.clone()));
        assert!(view.picker.is_none());
        assert!(!view.pick_requested, "same-board close cancels retry too");
    }

    fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }
    fn mouse_event(kind: MouseEventKind, point: Position) -> MouseEvent {
        MouseEvent {
            kind,
            column: point.x,
            row: point.y,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn release_notice_is_separate_from_document_and_preserves_mouse_selection() {
        let mut view = View::default();
        let original = snapshot("COPY_ME original", "one");
        view.update(Ok(original.clone()));
        let mut terminal = Terminal::new(TestBackend::new(100, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let area = view.viewport.as_ref().unwrap().area();
        let start = Position::new(area.x, area.y);
        let end = Position::new(area.x + 6, area.y);
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), end));

        let notice = Some("Update available: 0.2.0 → 0.2.1 · run agent-bridge update".into());
        assert!(view.update_release_notice(notice.clone()));
        assert!(!view.update_release_notice(notice));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(buffer_text(&terminal).contains("Update available: 0.2.0 → 0.2.1"));
        assert!(buffer_text(&terminal).contains("run agent-bridge update"));
        assert_eq!(view.selection.as_ref().unwrap().area(), area);
        assert_eq!(view.snapshot.as_ref().unwrap().markdown, original.markdown);
        assert_eq!(view.snapshot.as_ref().unwrap().revision, original.revision);
        assert!(view.pending.is_none());

        assert!(view.update_release_notice(None));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(!buffer_text(&terminal).contains("Update available"));
        assert_eq!(view.selection.as_ref().unwrap().area(), area);
    }

    fn message_field(id: &str, route: &str) -> String {
        let marker = crate::board_messages::Marker {
            instance: "00000000-0000-4000-8000-000000000001".into(),
            id: id.into(),
            label: format!("Message {route}"),
            owner: crate::board_messages::Owner {
                name: route.into(),
                provider: crate::protocol::Provider::Inbox,
                session: route.into(),
                endpoint: String::new(),
            },
        };
        format!(
            "<!-- board-message: {} -->\n",
            serde_json::to_string(&marker).unwrap()
        )
    }
    fn accepted_receipt(request: &Submission, recipient: &str, session: &str) -> serde_json::Value {
        serde_json::json!({"accepted":true,"id":request.id,"state":"queued","message":{"id":request.id,"sender":"user","recipient":recipient,"recipient_session":session,"body":request.text}})
    }
    #[test]
    fn rendered_composer_declaration_is_hidden_but_raw_and_control_remain_exact() {
        let marker = message_field("coordinator", "alpha");
        let raw = format!("# Context\n{marker}\nAfter input\n");
        let original = raw.clone();
        let (rendered, _, _, _) = rendered_markdown(&raw, 40);
        let visible = rendered.to_string();
        assert!(visible.contains("Context") && visible.contains("After input"));
        assert!(!visible.contains("board-message") && !visible.contains("endpoint"));
        assert_eq!(raw, original);
        assert_eq!(board_interaction::controls(&raw).len(), 1);
    }
    fn message_view() -> View {
        let mut view = View::default();
        view.update(Ok(snapshot(
            &message_field("coordinator", "alpha"),
            &"a".repeat(64),
        )));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        assert!(
            view.editor.is_some(),
            "Tab focuses an editable box directly"
        );
        view
    }
    #[test]
    fn stacked_message_boxes_click_switch_and_scroll_without_moving_the_board() {
        let markdown = "# Context\n\n".to_owned()
            + &"Board content\n\n".repeat(30)
            + &message_field("one", "alpha")
            + &message_field("two", "beta")
            + &message_field("three", "gamma");
        let mut view = View::default();
        view.update(Ok(snapshot(&markdown, &"a".repeat(64))));
        let mut terminal = Terminal::new(TestBackend::new(60, 18)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let board = view.viewport.as_ref().unwrap().area();
        assert!(board.height >= 3);
        let (input, _, _) = view.message_regions[0];
        view.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: input.x,
            row: input.y,
            modifiers: KeyModifiers::NONE,
        });
        view.paste("alpha draft");
        assert_eq!(view.editor.as_ref().unwrap().draft, "alpha draft");
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        view.paste("beta draft");
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(view.message_regions.iter().any(|(_, index, _)| *index == 2));
        assert_eq!(view.viewport.as_ref().unwrap().area(), board);
        press(&mut view, KeyCode::PageDown, KeyModifiers::NONE);
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(view.scroll.0 > 0);
        assert_eq!(view.viewport.as_ref().unwrap().area(), board);
        assert_eq!(view.message_drafts.len(), 2);
        assert!(buffer_text(&terminal).contains("gamma"));
        terminal.resize(Rect::new(0, 0, 30, 12)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(view.viewport.as_ref().unwrap().area().height >= 2);
        assert!(buffer_text(&terminal).contains("gamma"));
        assert!(!buffer_text(&terminal).contains("[message]"));
    }
    #[test]
    fn message_enter_submits_exact_multiline_and_pending_enter_cannot_duplicate() {
        let mut view = message_view();
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert!(view.pending_message.is_none(), "blank Enter cannot submit");
        view.paste("first");
        press(&mut view, KeyCode::Enter, KeyModifiers::SHIFT);
        view.paste("第二行");
        press(&mut view, KeyCode::Char('j'), KeyModifiers::CONTROL);
        view.paste("third");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let request = view.pending_message.take().unwrap();
        assert!(view.notice.is_none(), "Pending send adds no routine status");
        assert_eq!(request.text, "first\n第二行\nthird");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste("must not change pending text");
        assert!(view.pending_message.is_none());
        assert_eq!(view.editor.as_ref().unwrap().draft, request.text);
        assert_eq!(
            view.snapshot.as_ref().unwrap().markdown,
            message_field("coordinator", "alpha")
        );
        view.message_result(
            request.clone(),
            Ok(accepted_receipt(&request, "alpha", "alpha")),
        );
        assert!(
            view.editor.is_some(),
            "successful send retains composer focus"
        );
        assert!(view.editor.as_ref().unwrap().draft.is_empty());
        assert!(
            view.notice.as_deref() == Some("Message pending"),
            "Accepted send displays its public delivery outcome"
        );
        view.paste("next");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert_ne!(view.pending_message.as_ref().unwrap().id, request.id);
    }
    #[test]
    fn historical_receipts_display_only_public_delivery_outcomes() {
        for state in [
            "queued",
            "sending",
            "submitted",
            "acknowledged",
            "failed",
            "blocked",
            "cancelled",
            "uncertain",
            "pending",
            "delivered",
            "unknown",
        ] {
            let mut view = message_view();
            view.paste("exact historical message");
            press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
            let request = view.pending_message.take().unwrap();
            let mut receipt = accepted_receipt(&request, "alpha", "alpha");
            receipt["state"] = serde_json::json!(state);
            view.message_result(request, Ok(receipt));
            assert_eq!(
                view.notice.as_deref(),
                Some(format!("Message {}", crate::protocol::delivery_state(state)).as_str())
            );
        }
    }
    #[test]
    fn uncertain_message_retries_same_id_and_body_and_rejects_unmatched_acceptance() {
        let mut view = message_view();
        view.paste("exact original");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let first = view.pending_message.take().unwrap();
        view.message_result(first.clone(), Err(anyhow::anyhow!("timeout")));
        view.paste("not allowed");
        assert_eq!(view.editor.as_ref().unwrap().draft, first.text);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let retry = view.pending_message.take().unwrap();
        assert_eq!(retry.id, first.id);
        assert_eq!(retry.text, first.text);
        view.message_result(
            retry.clone(),
            Ok(serde_json::json!({"accepted":true,"id":"wrong-id"})),
        );
        assert_eq!(view.editor.as_ref().unwrap().draft, first.text);
        assert!(view.editor.as_ref().unwrap().submission.is_some());
        press(&mut view, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(view.editor.is_none());
        assert!(
            view.notice
                .as_ref()
                .unwrap()
                .contains("may have been accepted")
        );
    }
    #[test]
    fn uncertain_retained_draft_discard_keeps_outcome_warning() {
        let mut view = message_view();
        view.paste("possibly sent");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let request = view.pending_message.take().unwrap();
        view.message_result(request, Err(anyhow::anyhow!("timeout")));
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(view.message_drafts.is_empty());
        assert!(
            view.notice
                .as_ref()
                .unwrap()
                .contains("may have been accepted")
        );
    }
    #[test]
    fn message_drafts_are_independent_and_require_explicit_discard_before_switch_or_quit() {
        let mut view = View::default();
        view.update(Ok(snapshot(
            &(message_field("first", "alpha") + &message_field("second", "beta")),
            &"a".repeat(64),
        )));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste("draft alpha");
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        view.paste("draft beta");
        press(&mut view, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(view.editor.as_ref().unwrap().draft, "draft alpha");
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!view.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 10));
        press(&mut view, KeyCode::Char('b'), KeyModifiers::NONE);
        assert!(!view.pick_requested);
        assert!(!view.switch("other".parse().unwrap()));
        press(&mut view, KeyCode::Char('d'), KeyModifiers::CONTROL);
        assert!(view.switch("other".parse().unwrap()));
        assert!(view.message_drafts.is_empty());
    }
    #[test]
    fn refreshed_or_retargeted_message_controls_cannot_send_stale_drafts() {
        let mut view = message_view();
        view.paste("keep this");
        view.update(Ok(snapshot(
            &format!("new prose\n{}", message_field("coordinator", "alpha")),
            &"b".repeat(64),
        )));
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert!(view.pending_message.is_none());
        press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(view.editor.as_ref().unwrap().draft, "keep this");
        assert_eq!(view.editor.as_ref().unwrap().base.revision, "b".repeat(64));
        view.update(Ok(snapshot(
            &message_field("coordinator", "beta"),
            &"c".repeat(64),
        )));
        press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert!(view.pending_message.is_none());
        assert_eq!(view.editor.as_ref().unwrap().draft, "keep this");
        assert!(
            matches!(&view.editor.as_ref().unwrap().control.kind, Kind::Message {owner,..} if owner.name=="alpha")
        );
    }
    #[test]
    fn message_focus_survives_refresh_and_tab_leaves_for_existing_response_controls() {
        let markdown = message_field("first", "alpha")
            + &message_field("second", "beta")
            + "\n- [ ] Existing checkbox\n\n<!-- board-input: answer -->\n> old answer\n<!-- /board-input -->\n";
        let mut view = View::default();
        view.update(Ok(snapshot(&markdown, &"a".repeat(64))));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste("first draft");
        view.update(Ok(snapshot(
            &format!("new context\n{markdown}"),
            &"b".repeat(64),
        )));
        assert_eq!(view.selected, Some(0));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        assert!(
            matches!(&view.editor.as_ref().unwrap().control.kind, Kind::Message {owner,..} if owner.name=="beta")
        );
        view.paste("second draft");
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        assert!(view.editor.is_none());
        assert!(matches!(
            view.controls[view.selected.unwrap()].kind,
            Kind::Checkbox { .. }
        ));
        press(&mut view, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(
            view.pending
                .as_ref()
                .unwrap()
                .markdown
                .contains("[x] Existing checkbox")
        );
        view.pending = None;
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            view.editor.as_ref().unwrap().control.kind,
            Kind::Text { .. }
        ));
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert!(view.editor.as_ref().unwrap().draft.ends_with('\n'));
        assert!(view.pending_message.is_none());
        assert_eq!(view.message_drafts.len(), 2);
    }
    #[test]
    fn exact_owner_inspection_is_keyboard_only_and_does_not_send_or_change_draft() {
        let mut view = message_view();
        view.paste("retain draft");
        press(&mut view, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert!(view.owner_details);
        let mut terminal = Terminal::new(TestBackend::new(40, 25)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(buffer_text(&terminal).contains("Session: alpha"));
        press(&mut view, KeyCode::Down, KeyModifiers::NONE);
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!view.owner_details);
        assert_eq!(view.editor.as_ref().unwrap().draft, "retain draft");
        assert!(view.pending_message.is_none());
    }
    #[test]
    fn removed_message_input_draft_remains_keyboard_accessible_without_rerouting() {
        let mut view = message_view();
        view.paste("original instruction");
        view.update(Ok(snapshot("Input removed by author", &"b".repeat(64))));
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(view.editor.as_ref().unwrap().draft, "original instruction");
        assert!(
            matches!(&view.editor.as_ref().unwrap().control.kind, Kind::Message {owner,..} if owner.name=="alpha")
        );
        press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert!(view.pending_message.is_none());
        press(&mut view, KeyCode::Char('x'), KeyModifiers::CONTROL);
        assert!(!view.has_message_drafts());
    }
    #[test]
    fn retained_orphans_cycle_without_discarding_any_draft_or_retry_identity() {
        let mut view = View::default();
        view.update(Ok(snapshot(
            &(message_field("first", "alpha") + &message_field("second", "beta")),
            &"a".repeat(64),
        )));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste("uncertain alpha");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let request = view.pending_message.take().unwrap();
        view.message_result(request.clone(), Err(anyhow::anyhow!("timeout")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        view.paste("unsent beta");
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        view.update(Ok(snapshot("Both inputs removed", &"b".repeat(64))));
        press(&mut view, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(view.editor.as_ref().unwrap().draft, "uncertain alpha");
        assert_eq!(
            view.editor
                .as_ref()
                .unwrap()
                .submission
                .as_ref()
                .unwrap()
                .id,
            request.id
        );
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(view.editor.as_ref().unwrap().draft, "unsent beta");
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert_eq!(view.editor.as_ref().unwrap().draft, "uncertain alpha");
        assert!(view.editor.as_ref().unwrap().uncertain);
        assert_eq!(view.message_drafts.len(), 1);
    }
    #[test]
    fn mismatched_receipt_session_does_not_clear_a_draft() {
        let mut view = message_view();
        view.paste("exact request");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let request = view.pending_message.take().unwrap();
        view.message_result(
            request.clone(),
            Ok(accepted_receipt(&request, "alpha", "replacement-session")),
        );
        assert_eq!(view.editor.as_ref().unwrap().draft, "exact request");
        assert!(view.editor.as_ref().unwrap().uncertain);
    }
    #[test]
    fn recreated_message_instance_requires_new_composer_even_with_same_id_and_owner() {
        let mut view = message_view();
        view.paste("old unsent draft");
        let recreated = message_field("coordinator", "alpha").replace(
            "00000000-0000-4000-8000-000000000001",
            "00000000-0000-4000-8000-000000000002",
        );
        view.update(Ok(snapshot(&recreated, &"b".repeat(64))));
        press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert!(view.pending_message.is_none());
        assert_eq!(view.editor.as_ref().unwrap().draft, "old unsent draft");
        assert_eq!(view.editor.as_ref().unwrap().base.revision, "a".repeat(64));
    }
    #[test]
    fn rejected_message_keeps_editable_draft_and_narrow_render_preserves_target() {
        let mut view = message_view();
        view.paste("retain");
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let request = view.pending_message.take().unwrap();
        view.message_result(
            request.clone(),
            Ok(serde_json::json!({"accepted":false,"code":"rejected","error":"Queue is full"})),
        );
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        let retry = view.pending_message.take().unwrap();
        assert_eq!(retry.id, request.id);
        view.message_result(
            retry,
            Ok(serde_json::json!({"accepted":false,"code":"rejected","error":"Queue is full"})),
        );
        view.paste(" edited");
        assert_eq!(view.editor.as_ref().unwrap().draft, "retain edited");
        for width in [18, 40, 100] {
            let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            assert!(buffer_text(&terminal).contains("alpha"));
            assert!(!buffer_text(&terminal).contains("inbox:alpha"));
            assert!(
                matches!(&view.editor.as_ref().unwrap().control.kind, Kind::Message { owner, .. } if owner.name == "alpha" && owner.session == "alpha" && owner.provider == crate::protocol::Provider::Inbox)
            );
            assert_eq!(view.editor.as_ref().unwrap().draft, "retain edited");
        }
    }

    #[test]
    fn release_notice_preserves_draft_focus_and_scroll() {
        let mut view = View::default();
        view.update(Ok(snapshot(&field("original"), "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste("draft");
        let mut terminal = Terminal::new(TestBackend::new(100, 25)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let draft = view.editor.as_ref().unwrap().draft.clone();
        let selected = view.selected;
        let scroll = view.scroll;

        view.update_release_notice(Some(
            "Update available: 0.2.0 → 0.2.1 · run agent-bridge update".into(),
        ));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(buffer_text(&terminal).contains("Update available"));
        assert_eq!(view.editor.as_ref().unwrap().draft, draft);
        assert_eq!(view.selected, selected);
        assert_eq!(view.scroll, scroll);
        assert!(view.pending.is_none());
    }

    #[test]
    fn missing_release_cache_does_not_create_files_or_a_notice() {
        let temp = tempfile::tempdir().unwrap();
        assert!(cached_release_notice(temp.path()).is_none());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn mouse_drag_copies_the_frozen_visible_revision_and_ignores_hover() {
        let mut view = View::default();
        view.update(Ok(snapshot("COPY_ME original", "one")));
        let mut terminal = Terminal::new(TestBackend::new(70, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let area = view.viewport.as_ref().unwrap().area();
        let start = Position::new(area.x, area.y);
        let end = Position::new(area.x + 6, area.y);
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Moved, start)),
            (false, None)
        );
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start)),
            (true, None)
        );
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), end)),
            (true, None)
        );
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), end)),
            (false, None)
        );
        view.update(Ok(snapshot("AFTER concurrent edit", "two")));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(terminal.backend().buffer()[(area.x, area.y)].symbol(), "C");
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Right), end)),
            (false, None)
        );
        let (dirty, text) = view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end));
        assert!(dirty);
        assert_eq!(text.as_deref(), Some("COPY_ME"));
        assert!(view.pending.is_none(), "copying never edits Markdown");
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(terminal.backend().buffer()[(area.x, area.y)].symbol(), "C");
        assert_eq!(
            terminal.backend().buffer()[(area.x, area.y)].bg,
            Color::Yellow
        );
        assert!(!view.selection.as_ref().unwrap().is_dragging());
        assert!(!buffer_text(&terminal).contains("release mouse to copy"));
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), start)),
            (false, None)
        );
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end)),
            (false, None)
        );
        assert!(!view.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 10));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(view.selection.is_none());
        assert_eq!(terminal.backend().buffer()[(area.x, area.y)].symbol(), "A");
    }

    #[test]
    fn completed_selection_can_be_replaced_or_cleared_by_clicking() {
        let mut view = View::default();
        view.update(Ok(snapshot("first second", "one")));
        let mut terminal = Terminal::new(TestBackend::new(70, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let area = view.viewport.as_ref().unwrap().area();
        let start = Position::new(area.x, area.y);
        let end = Position::new(area.x + 4, area.y);
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end)),
            (true, Some("first".into()))
        );
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let second = Position::new(area.x + 6, area.y);
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), second));
        assert_eq!(
            view.mouse(mouse_event(
                MouseEventKind::Up(MouseButton::Left),
                Position::new(area.x + 11, area.y)
            )),
            (true, Some("second".into()))
        );
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(
            terminal.backend().buffer()[(start.x, start.y)].bg,
            Color::Reset
        );
        assert_eq!(
            terminal.backend().buffer()[(second.x, second.y)].bg,
            Color::Yellow
        );
        // Clicking the selected body clears it without copying again.
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), second));
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), second)),
            (true, None)
        );
        assert!(view.selection.is_none());
        terminal.draw(|frame| view.draw(frame)).unwrap();
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end));
        // Clicking outside the document also clears the retained selection.
        assert_eq!(
            view.mouse(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                Position::new(0, 0)
            )),
            (true, None)
        );
        assert!(view.selection.is_none());
    }

    #[test]
    fn selection_cancels_on_layout_scroll_and_escape_and_cannot_start_in_chrome() {
        let mut view = View::default();
        view.update(Ok(snapshot("body content", "one")));
        let mut terminal = Terminal::new(TestBackend::new(70, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(
            view.mouse(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                Position::new(0, 0)
            )),
            (false, None)
        );
        let area = view.viewport.as_ref().unwrap().area();
        let start = Position::new(area.x, area.y);
        let end = Position::new(area.x + 5, area.y);
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), end));
        view.update(Ok(snapshot("body content\n\n- [ ] New response", "two")));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(
            view.selection.is_none(),
            "a new Responses pane changes the frozen body rectangle"
        );
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end)),
            (false, None)
        );
        let area = view.viewport.as_ref().unwrap().area();
        let start = Position::new(area.x, area.y);
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        assert!(!view.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 10));
        assert!(view.selection.is_none());
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        assert_eq!(
            view.mouse(mouse_event(MouseEventKind::ScrollDown, start)),
            (true, None)
        );
        assert!(view.selection.is_none());
        assert_eq!(view.scroll.0, 3);
    }

    #[test]
    fn mouse_mode_toggle_preserves_text_editor_input_and_disables_capture_actions() {
        let mut view = View::default();
        view.update(Ok(snapshot(
            "<!-- board-input: notes -->\n> draft\n<!-- /board-input -->\n",
            "one",
        )));
        let mut terminal = Terminal::new(TestBackend::new(70, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('m'), KeyModifiers::NONE);
        assert_eq!(view.editor.as_ref().unwrap().draft, "draftm");
        assert!(view.mouse_enabled);
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(!view.mouse_enabled);
        let area = view.viewport.as_ref().unwrap().area();
        assert_eq!(
            view.mouse(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                Position::new(area.x, area.y)
            )),
            (false, None)
        );
        assert!(view.selection.is_none());
        press(&mut view, KeyCode::Char('m'), KeyModifiers::NONE);
        assert!(view.mouse_enabled);
    }

    #[test]
    fn identical_reads_do_not_redraw_but_errors_recovery_and_metadata_changes_do() {
        let initial = snapshot("same board", "one");
        let mut view = View::default();
        assert!(view.update(Ok(initial.clone())));
        assert!(!view.update(Ok(initial.clone())));
        assert!(view.update(Err(anyhow::anyhow!("offline"))));
        assert!(!view.update(Err(anyhow::anyhow!("offline"))));
        assert!(view.update(Err(anyhow::anyhow!("different failure"))));
        assert!(view.update(Ok(initial.clone())));
        assert!(!view.update(Ok(initial.clone())));
        for changed in [
            Snapshot {
                name: "other".into(),
                ..initial.clone()
            },
            Snapshot {
                path: PathBuf::from("/other/board.md"),
                ..initial.clone()
            },
            Snapshot {
                exists: false,
                ..initial.clone()
            },
            Snapshot {
                last_updated: Some(1_768_435_200_000),
                ..initial.clone()
            },
        ] {
            let mut view = View::default();
            assert!(view.update(Ok(initial.clone())));
            assert!(view.update(Ok(changed.clone())));
            assert!(!view.update(Ok(changed)));
        }
    }
    #[test]
    fn last_updated_border_is_visible_while_scrolling_and_follows_frozen_selection() {
        let markdown = (0..100)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        let mut original = snapshot(&markdown, "old-revision");
        original.last_updated = Some(1_768_435_200_000);
        for width in [10_u16, 32, 90] {
            let mut view = View::default();
            view.update(Ok(original.clone()));
            let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let bottom = |terminal: &Terminal<TestBackend>| {
                (0..width)
                    .map(|x| terminal.backend().buffer()[(x, 16)].symbol())
                    .collect::<String>()
            };
            let title = crate::board_time::footer(original.last_updated, width.saturating_sub(4));
            assert!(bottom(&terminal).contains(&title));
            view.scroll = (40, 0);
            terminal.draw(|frame| view.draw(frame)).unwrap();
            assert!(bottom(&terminal).contains(&title));
            assert!(
                !view
                    .snapshot
                    .as_ref()
                    .unwrap()
                    .markdown
                    .contains("Last Updated")
            );
        }
        let mut view = View::default();
        view.update(Ok(original));
        let mut terminal = Terminal::new(TestBackend::new(90, 20)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        view.selection = Selection::start(view.viewport.clone().unwrap(), Position::new(2, 2));
        let mut newer = snapshot("new content", "new-revision");
        newer.last_updated = Some(1_784_073_600_000);
        view.update(Ok(newer.clone()));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let frozen = buffer_text(&terminal);
        assert!(frozen.contains("old-revision"));
        assert!(!frozen.contains("new-revision"));
        assert!(frozen.contains(&crate::board_time::footer(Some(1_768_435_200_000), 86)));
        view.selection = None;
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let current = buffer_text(&terminal);
        assert!(current.contains("new-revision"));
        assert!(current.contains(&crate::board_time::footer(newer.last_updated, 86)));
    }

    #[test]
    fn generic_markdown_headings_tables_and_unicode_render() {
        let mut view = View::default();
        view.update(Ok(snapshot(
            "# Any subject\n\n| Planet | Note |\n|---|---|\n| 地球 | Ready ✅ |\n",
            "one",
        )));
        let mut terminal = Terminal::new(TestBackend::new(70, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let text = buffer_text(&terminal);
        assert!(text.contains("Any subject"));
        assert!(text.contains("Planet"));
        assert!(text.contains("Ready"));
        assert!(text.contains('地'));
        assert!(text.contains('│'));
    }

    fn hyperlink_segments(buffer: &Buffer) -> Vec<(Position, String, String)> {
        let mut links = Vec::new();
        for y in buffer.area.top()..buffer.area.bottom() {
            for x in buffer.area.left()..buffer.area.right() {
                if let Some(link) = buffer[(x, y)].symbol().strip_prefix("\x1b]8;;") {
                    let (url, label) = link.split_once("\x1b\\").unwrap();
                    let label = label.strip_suffix("\x1b]8;;\x1b\\").unwrap();
                    assert!(!url.chars().any(char::is_control));
                    assert!(!label.chars().any(char::is_control));
                    links.push((Position::new(x, y), url.to_owned(), label.to_owned()));
                }
            }
        }
        links
    }

    #[test]
    fn paragraph_and_table_links_use_label_width_and_osc8_buffer_cells() {
        let url = "https://github.com/immutable/data-platform/pull/714";
        for markdown in [
            format!("[dp#714]({url})"),
            format!("| PR | State |\n|---|---|\n| [dp#714]({url}) | ready |"),
        ] {
            let mut view = View::default();
            view.update(Ok(snapshot(&markdown, "one")));
            let mut terminal = Terminal::new(TestBackend::new(70, 18)).unwrap();
            let mut drawn = Buffer::default();
            terminal
                .draw(|frame| {
                    view.draw(frame);
                    drawn = frame.buffer_mut().clone();
                })
                .unwrap();
            let links = hyperlink_segments(&drawn);
            assert_eq!(links.len(), 1);
            let (start, target, label) = &links[0];
            assert_eq!(target, url);
            assert_eq!(label, "dp#714");
            assert!(!view.rendered.to_string().contains(url));
            assert!(view.rendered.width() < 30);
            assert_eq!(drawn[*start].fg, Color::Blue);
            assert!(
                drawn[*start]
                    .modifier
                    .contains(ratatui::style::Modifier::UNDERLINED)
            );
            assert_eq!(
                drawn[*start].diff_option,
                ratatui::buffer::CellDiffOption::ForcedWidth(std::num::NonZeroU16::new(6).unwrap())
            );
            for x in start.x + 1..start.x + 6 {
                assert_eq!(
                    drawn[(x, start.y)].diff_option,
                    ratatui::buffer::CellDiffOption::Skip
                );
            }
            assert!(
                buffer_text(&terminal)
                    .contains(&format!("\x1b]8;;{url}\x1b\\dp#714\x1b]8;;\x1b\\"))
            );
        }
    }

    #[test]
    fn unsupported_and_overlong_links_keep_the_destination_fallback() {
        for url in [
            "file:///tmp/test".to_owned(),
            "javascript:alert(1)".into(),
            "mailto:person@example.com".into(),
            "../relative".into(),
            format!("https://example.com/{}", "x".repeat(MAX_LINK_URL_BYTES)),
        ] {
            let (text, plain, _, urls) = rendered_markdown(&format!("[label]({url})"), 80);
            assert!(!plain);
            assert!(urls.is_empty());
            assert_eq!(text.to_string(), format!("label ({url})"));
        }
        let url = format!(
            "https://example.com/{}",
            "x".repeat(MAX_LINK_URL_BYTES - 20)
        );
        assert_eq!(url.len(), MAX_LINK_URL_BYTES);
        let (text, _, _, urls) = rendered_markdown(&format!("[label]({url})"), 80);
        assert_eq!(text.to_string(), "label");
        assert_eq!(urls, [url]);
    }

    #[test]
    fn entity_encoded_controls_in_links_never_escape_into_output() {
        let markdown = "[bad] (unused) [url](https://example.com/&#x1b;]8;evil&#x7;) [safe&#x1b;label&#7;](https://example.com/safe)";
        let mut view = View::default();
        view.update(Ok(snapshot(markdown, "one")));
        let mut terminal = Terminal::new(TestBackend::new(110, 18)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let links = hyperlink_segments(terminal.backend().buffer());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "https://example.com/safe");
        assert_eq!(links[0].2, "safelabel");
        for span in view.rendered.lines.iter().flat_map(|line| &line.spans) {
            assert!(!span.content.chars().any(char::is_control));
        }
        assert!(!buffer_text(&terminal).contains('\x07'));
        assert!(!buffer_text(&terminal).contains("\x1b]8;evil"));
    }

    #[test]
    fn wrapped_paragraph_and_table_labels_link_every_segment() {
        let label = "abcdefghijklmnopqrstuvwx";
        for markdown in [
            format!("[{label}](https://example.com)"),
            format!("| H |\n|---|\n| [{label}](https://example.com) |\n"),
        ] {
            let mut view = View::default();
            view.update(Ok(snapshot(&markdown, "one")));
            let mut terminal = Terminal::new(TestBackend::new(16, 25)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let links = hyperlink_segments(terminal.backend().buffer());
            assert!(links.len() >= 2, "{links:?}");
            assert!(links.iter().all(|(_, url, _)| url == "https://example.com"));
            assert_eq!(
                links
                    .iter()
                    .map(|(_, _, text)| text.as_str())
                    .collect::<String>(),
                label
            );
            assert!(links.windows(2).all(|rows| rows[0].0.y < rows[1].0.y));
        }
    }

    #[test]
    fn link_copy_and_partial_highlight_use_the_frozen_visible_label() {
        let mut view = View::default();
        view.update(Ok(snapshot("[dp#714](https://example.com)", "one")));
        let mut terminal = Terminal::new(TestBackend::new(70, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let area = view.viewport.as_ref().unwrap().area();
        let start = Position::new(area.x + 1, area.y);
        let end = Position::new(area.x + 4, area.y);
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), end));
        view.update(Ok(snapshot("[changed](https://example.org)", "two")));
        let mut drawn = Buffer::default();
        terminal
            .draw(|frame| {
                view.draw(frame);
                drawn = frame.buffer_mut().clone();
            })
            .unwrap();
        let links = hyperlink_segments(&drawn);
        assert_eq!(
            links
                .iter()
                .map(|(_, _, label)| label.as_str())
                .collect::<String>(),
            "dp#714"
        );
        assert!(links.iter().all(|(_, url, _)| url == "https://example.com"));
        assert_eq!(drawn[start].bg, Color::Yellow);
        assert_ne!(drawn[(area.x, area.y)].bg, Color::Yellow);
        assert_eq!(links[1].2, "p#71");
        let (_, copied) = view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end));
        assert_eq!(copied.as_deref(), Some("p#71"));
    }

    #[test]
    fn scrolling_and_panning_keep_link_segments_on_the_visible_label() {
        let markdown = (0..20)
            .map(|row| {
                format!("row{row:02} [abcdefghijklmnopqrstuvwx](https://example.com/{row})  \n")
            })
            .collect::<String>();
        let mut view = View::default();
        view.update(Ok(snapshot(&markdown, "one")));
        let mut terminal = Terminal::new(TestBackend::new(18, 13)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        view.scroll.0 = 3;
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let links = hyperlink_segments(terminal.backend().buffer());
        assert_eq!(links[0].1, "https://example.com/1");
        assert_eq!(links[0].2, "abcdefghijklmnop");
        assert_eq!(links[0].0.y, view.viewport.as_ref().unwrap().area().y + 1);
        view.wrap = false;
        view.scroll = (2, 10);
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let links = hyperlink_segments(terminal.backend().buffer());
        assert_eq!(links[0].1, "https://example.com/2");
        assert_eq!(links[0].2, "efghijklmnopqrst");
        assert_eq!(links[0].0.x, view.viewport.as_ref().unwrap().area().x);
    }

    #[test]
    fn reference_links_styles_and_unicode_survive_table_splitting() {
        let mut view = View::default();
        view.update(Ok(snapshot(
            "| H |\n|---|\n| [**地球** e\u{301} 👩‍💻][PR] |\n\n[pr]: https://example.com",
            "one",
        )));
        let mut terminal = Terminal::new(TestBackend::new(14, 22)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let links = hyperlink_segments(terminal.backend().buffer());
        let visible = links
            .iter()
            .map(|(_, _, text)| text.as_str())
            .collect::<String>();
        assert!(visible.contains("地球"), "{links:?}");
        assert!(visible.contains("e\u{301}"), "{links:?}");
        assert!(visible.contains("👩‍💻"), "{links:?}");
        assert!(
            terminal.backend().buffer()[links[0].0]
                .modifier
                .contains(ratatui::style::Modifier::BOLD)
        );
    }

    #[test]
    fn styled_grapheme_boundaries_keep_the_mask_aligned() {
        let mut view = View::default();
        view.update(Ok(snapshot("[`👩‍`💻x](https://example.com)", "one")));
        let mut terminal = Terminal::new(TestBackend::new(20, 15)).unwrap();
        let mut drawn = Buffer::default();
        terminal
            .draw(|frame| {
                view.draw(frame);
                drawn = frame.buffer_mut().clone();
            })
            .unwrap();
        let links = hyperlink_segments(&drawn);
        assert_eq!(
            links
                .iter()
                .map(|(_, _, label)| label.as_str())
                .collect::<String>(),
            "👩‍💻x"
        );
    }

    #[test]
    fn multiline_links_fall_back_and_single_line_container_links_are_hyperlinks() {
        for markdown in [
            "[a  \nb](https://example.com)",
            "> [a  \n> b](https://example.com)",
            "- [a  \n  b](https://example.com)",
            "[a\nb](https://example.com)",
            "[a](https://example.com\n \"title\")",
        ] {
            let mut view = View::default();
            view.update(Ok(snapshot(markdown, "one")));
            let mut terminal = Terminal::new(TestBackend::new(30, 15)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let links = hyperlink_segments(terminal.backend().buffer());
            assert!(links.is_empty(), "{markdown}: {links:?}");
            assert!(view.link_urls.is_empty());
            let expected = tui_markdown::from_str(markdown).to_string();
            assert!(expected.contains("(https://example.com)"));
            assert_eq!(view.rendered.to_string(), expected);
        }
        for markdown in [
            "> [label](https://example.com)",
            "- [label](https://example.com)",
        ] {
            let mut view = View::default();
            view.update(Ok(snapshot(markdown, "one")));
            let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let links = hyperlink_segments(terminal.backend().buffer());
            assert_eq!(links.len(), 1, "{markdown}: {links:?}");
            assert_eq!(links[0].1, "https://example.com");
            assert_eq!(links[0].2, "label");
            assert!(!view.rendered.to_string().contains("https://example.com"));
        }
    }

    #[test]
    fn nested_image_labels_fall_back_without_expanding_link_markers() {
        let markdown = format!(
            "[start](https://example.com/start)\n\n{}\n\n[finish](https://example.com/finish)\n\n[i]: https://example.com/{}",
            "[![][i]](https://example.com/outer)\n\n".repeat(20),
            "x".repeat(2048)
        );
        assert!(markdown.len() < MAX_BOARD_BYTES);
        let (source, glyphs, urls) = markdown_links(&markdown);
        assert_eq!(glyphs.len(), "startfinish".len());
        assert!(source.len() <= markdown.len() + 40 * glyphs.len());
        assert_eq!(
            urls,
            ["https://example.com/start", "https://example.com/finish"]
        );
        let (text, _, _, urls) = rendered_markdown(&markdown, 80);
        assert_eq!(
            urls,
            ["https://example.com/start", "https://example.com/finish"]
        );
        let fallback = markdown
            .replace("[start](https://example.com/start)", "start")
            .replace("[finish](https://example.com/finish)", "finish");
        assert_eq!(
            text.to_string(),
            tui_markdown::from_str(&fallback).to_string()
        );
        let mut view = View::default();
        view.update(Ok(snapshot(&markdown, "one")));
        let mut terminal = Terminal::new(TestBackend::new(80, 15)).unwrap();
        for expected in ["start", "finish"] {
            if expected == "finish" {
                view.scroll.0 = u16::MAX;
            }
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let links = hyperlink_segments(terminal.backend().buffer());
            assert_eq!(links.len(), 1, "{links:?}");
            assert_eq!(links[0].2, expected);
            assert_eq!(links[0].1, format!("https://example.com/{expected}"));
        }
    }

    #[test]
    fn labels_with_inline_or_reference_images_keep_the_original_rendering() {
        for markdown in [
            "[![alt](https://example.com/image)](https://example.com/outer)",
            "[![alt][image]](https://example.com/outer)\n\n[image]: https://example.com/image",
        ] {
            let (source, glyphs, urls) = markdown_links(markdown);
            assert_eq!(source, markdown);
            assert!(glyphs.is_empty());
            assert!(urls.is_empty());
            let (text, _, _, _) = rendered_markdown(markdown, 80);
            assert_eq!(
                text.to_string(),
                tui_markdown::from_str(markdown).to_string()
            );
            assert!(text.to_string().contains("(https://example.com/outer)"));
        }
    }

    #[test]
    fn link_label_spaces_cannot_create_markdown_structure() {
        for label in ["    x", " a ", " ", "a  b"] {
            let (text, _, mask, urls) =
                rendered_markdown(&format!("[{label}](https://example.com)"), 80);
            assert_eq!(text.to_string(), label);
            assert_eq!(urls, ["https://example.com"]);
            assert!(text.lines.iter().flat_map(|line| &line.spans).all(|span| {
                span.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::UNDERLINED)
            }));
            assert!(
                mask.lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .all(|span| span.style.fg == Some(link_color(0)))
            );
        }
        for markdown in [
            "outside [ a ](https://example.com) padding",
            "| H |\n|---|\n| [ a ](https://example.com) |",
        ] {
            let (text, _, mask, _) = rendered_markdown(markdown, 80);
            let owned = mask
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .filter(|span| span.style.fg == Some(link_color(0)))
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert_eq!(owned, " a ");
            assert_eq!(text.to_string(), mask.to_string());
        }
    }

    #[test]
    fn links_preserve_adjacent_emphasis_delimiters() {
        for markdown in [
            "_a_[label](https://example.com)",
            "[label](https://example.com)_a_",
        ] {
            let (text, _, _, _) = rendered_markdown(markdown, 80);
            assert!(!text.to_string().contains('_'));
            let span = text
                .lines
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.contains('a') && !span.content.contains("label"))
                .unwrap();
            assert!(
                span.style
                    .add_modifier
                    .contains(ratatui::style::Modifier::ITALIC)
            );
        }
    }

    #[test]
    fn reference_definitions_requiring_container_context_keep_the_fallback() {
        for markdown in [
            "[shown][pr]\n\n> [pr]:\n> https://example.com",
            "[a][r]\n\n> [r]: https://x.example\n> \"title\"",
        ] {
            let mut view = View::default();
            view.update(Ok(snapshot(markdown, "one")));
            let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            assert!(view.link_urls.is_empty());
            assert!(hyperlink_segments(terminal.backend().buffer()).is_empty());
            assert_eq!(
                view.rendered.to_string(),
                tui_markdown::from_str(markdown).to_string()
            );
        }
    }

    #[test]
    fn table_code_links_with_escaped_pipes_keep_parsed_text_and_independent_copy() {
        let markdown = "| H |\n|---|\n| [`a\\|b`](https://example.com) |";
        let mut view = View::default();
        view.update(Ok(snapshot(markdown, "one")));
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(view.link_urls, ["https://example.com"]);
        assert!(view.rendered.to_string().contains("a|b"));
        assert!(!view.rendered.to_string().contains("a\\|b"));
        assert!(!view.rendered.to_string().contains("(https://example.com)"));
        let area = view.viewport.as_ref().unwrap().area();
        let start = Position::new(area.x + 2, area.y + 3);
        let end = Position::new(start.x + 2, start.y);
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), end));
        let (_, copied) = view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end));
        assert_eq!(copied.as_deref(), Some("a|b"));
        let (text, _, _, urls) = rendered_markdown("[`a\\|b`](https://example.com)", 80);
        assert_eq!(urls, ["https://example.com"]);
        assert_eq!(text.to_string(), "a\\|b[⧉]");
    }

    #[test]
    fn decoded_fence_info_cannot_alias_a_link_marker() {
        let markdown = "[x](https://a.example)\n\n```&#983040;\ncode\n```";
        let mut view = View::default();
        view.update(Ok(snapshot(markdown, "one")));
        let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let links = hyperlink_segments(terminal.backend().buffer());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1, "https://a.example");
        assert_eq!(links[0].2, "x");
        assert!(view.rendered.to_string().contains("[⧉]"));
        assert_eq!(view.code_payloads, ["code\n"]);
    }

    #[test]
    fn non_ascii_destinations_keep_the_fallback() {
        for url in [
            "https://example.com/地球",
            "https://é.example",
            "https://example.com/&#233;",
        ] {
            let markdown = format!("[label]({url})");
            let mut view = View::default();
            view.update(Ok(snapshot(&markdown, "one")));
            let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            assert!(view.link_urls.is_empty());
            assert!(hyperlink_segments(terminal.backend().buffer()).is_empty());
            assert_eq!(
                view.rendered.to_string(),
                tui_markdown::from_str(&markdown).to_string()
            );
        }
        let (text, _, _, urls) = rendered_markdown("[label](https://example.com/%C3%A9)", 80);
        assert_eq!(text.to_string(), "label");
        assert_eq!(urls, ["https://example.com/%C3%A9"]);
    }

    #[test]
    fn empty_or_sanitized_empty_labels_keep_the_destination_visible() {
        for markdown in [
            "before [](https://a.example) after",
            "before [&#x1b;&#7;](https://a.example) after",
            "before [\u{200d}](https://a.example) after",
        ] {
            let mut view = View::default();
            view.update(Ok(snapshot(markdown, "one")));
            let mut terminal = Terminal::new(TestBackend::new(80, 18)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            assert!(view.link_urls.is_empty());
            assert!(hyperlink_segments(terminal.backend().buffer()).is_empty());
            assert!(view.rendered.to_string().contains("(https://a.example)"));
            assert!(
                view.rendered
                    .lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .all(|span| !span.content.chars().any(char::is_control))
            );
        }
    }
    #[test]
    fn repeated_long_reference_targets_are_deduplicated_before_storage() {
        let url = format!("https://example.test/{}", "x".repeat(1800));
        let markdown = format!("{}\n\n[x]: {url}\n", "[x] ".repeat(15_000));
        assert!(markdown.len() < MAX_BOARD_BYTES);
        let (_, glyphs, urls) = markdown_links(&markdown);
        // pulldown-cmark deliberately caps reference-expansion fuel; unresolved
        // references keep their ordinary rendering rather than bypassing it.
        assert!(!glyphs.is_empty());
        assert!(glyphs.len() < 15_000);
        assert_eq!(urls, [url]);
        assert!(glyphs.values().all(|glyph| glyph.link == 0));
    }

    #[test]
    fn runtime_hyperlink_opt_out_survives_named_board_switches() {
        let mut view = View {
            hyperlinks_enabled: false,
            ..View::default()
        };
        view.update(Ok(snapshot("[label](https://example.test/)", "one")));
        let mut terminal = Terminal::new(TestBackend::new(40, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(hyperlink_segments(terminal.backend().buffer()).is_empty());
        assert!(view.switch("other".parse().unwrap()));
        assert!(!view.hyperlinks_enabled);
    }

    #[test]
    fn document_link_budgets_fall_back_completely_without_later_repopulation() {
        let exact = format!(
            "[{}](https://example.test/)",
            "x".repeat(crate::board_links::MAX_LINK_CELLS)
        );
        assert_eq!(
            markdown_links(&exact).1.len(),
            crate::board_links::MAX_LINK_CELLS
        );
        let overflow = format!(
            "[{}x](https://example.test/) [later](https://example.test/later)",
            "x".repeat(crate::board_links::MAX_LINK_CELLS)
        );
        let (source, glyphs, urls) = markdown_links(&overflow);
        assert_eq!(source, overflow);
        assert!(glyphs.is_empty() && urls.is_empty());
        let many = (0..=crate::board_links::MAX_LINKS)
            .map(|id| format!("[x](https://example.test/{id}) "))
            .collect::<String>();
        assert!(many.len() < MAX_BOARD_BYTES);
        let (source, glyphs, urls) = markdown_links(&many);
        assert_eq!(source, many);
        assert!(glyphs.is_empty() && urls.is_empty());
    }

    #[test]
    fn literal_and_code_labels_never_alias_real_link_targets() {
        let markdown = "SAME `[SAME](https://example.test/code)` \\[SAME](https://example.test/escaped) [SAME](https://example.test/a)[SAME](https://example.test/b)";
        let mut view = View::default();
        view.update(Ok(snapshot(markdown, "one")));
        let mut terminal = Terminal::new(TestBackend::new(160, 18)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let links = hyperlink_segments(terminal.backend().buffer());
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].1, "https://example.test/a");
        assert_eq!(links[1].1, "https://example.test/b");
        assert_eq!(links[0].2, "SAME");
        assert_eq!(links[1].2, "SAME");
    }

    #[test]
    fn raw_and_entity_encoded_terminal_controls_never_reach_spans() {
        let (text, _, _, _) = rendered_markdown(
            "# Safe\u{1b}]52;c;secret\u{7}\n&#x1b;]0;title&#7; &#x9b;2J",
            80,
        );
        for span in text.lines.iter().flat_map(|line| &line.spans) {
            assert!(
                !span.content.chars().any(char::is_control),
                "{:?}",
                span.content
            );
        }
        assert!(
            !safe_line("error\u{1b}]0;evil\u{7}\r\nnext")
                .chars()
                .any(char::is_control)
        );
    }
    #[test]
    fn updates_preserve_scroll_and_errors_preserve_last_good_content() {
        let mut view = View::default();
        let markdown = (0..100)
            .map(|line| format!("line {line}  \n"))
            .collect::<String>();
        view.update(Ok(snapshot(&markdown, "one")));
        let mut terminal = Terminal::new(TestBackend::new(70, 10)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        view.scroll.0 = 20;
        view.update(Ok(snapshot(
            &markdown.replace("line 50", "updated 50"),
            "two",
        )));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(view.scroll.0, 20);
        view.update(Err(anyhow::anyhow!("offline")));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(buffer_text(&terminal).contains("Offline/stale"));
        assert_eq!(view.snapshot.as_ref().unwrap().revision, "two");
    }
    #[test]
    fn pathological_sparse_tables_fall_back_to_plain_markdown() {
        let markdown = format!("{}\n{}", "|x".repeat(1000), "|\n".repeat(100));
        let (text, plain, _, _) = rendered_markdown(&markdown, 80);
        assert!(plain);
        assert!(text.lines.len() > 50);
    }
    #[test]
    fn file_reads_do_not_trigger_refresh_feedback() {
        use notify::event::{AccessKind, DataChange, ModifyKind};
        let board = Path::new("/private/boards/main.md");
        let parent = board.parent().unwrap();
        let read =
            notify::Event::new(notify::EventKind::Access(AccessKind::Read)).add_path(board.into());
        assert!(!board_event(&read, board, parent));
        let update =
            notify::Event::new(notify::EventKind::Modify(ModifyKind::Data(DataChange::Any)))
                .add_path(board.into());
        assert!(board_event(&update, board, parent));
        let unrelated = notify::Event::new(notify::EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/private/messages.sqlite3"));
        assert!(!board_event(&unrelated, board, parent));
    }

    fn press(view: &mut View, code: KeyCode, modifiers: KeyModifiers) {
        assert!(!view.key(KeyEvent::new(code, modifiers), 5));
    }
    fn field(value: &str) -> String {
        format!("# Question\n\n<!-- board-input: notes -->\n> {value}\n<!-- /board-input -->\n")
    }
    #[test]
    fn draft_requires_successful_save_or_explicit_discard_before_switching() {
        let mut view = View::default();
        view.update(Ok(snapshot(&field("answer"), "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('b'), KeyModifiers::NONE);
        assert_eq!(view.editor.as_ref().unwrap().draft, "answerb");
        assert!(!view.pick_requested);
        let other: Name = "project".parse().unwrap();
        assert!(!view.switch(other.clone()));
        press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
        let save = view.pending.take().unwrap();
        view.saving = true;
        assert!(!view.switch(other.clone()));
        view.saved(save.clone(), Err(anyhow::anyhow!("connection lost")));
        assert_eq!(view.editor.as_ref().unwrap().draft, "answerb");
        assert!(view.notice.as_ref().unwrap().contains("uncertain"));
        assert!(!view.switch(other.clone()));
        view.saved(
            save.clone(),
            Ok((snapshot(&field("someone else"), "two"), false)),
        );
        assert_eq!(view.editor.as_ref().unwrap().draft, "answerb");
        assert!(!view.switch(other.clone()));
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        assert!(view.switch(other));
        assert!(view.editor.is_none());

        let mut view = View::default();
        view.update(Ok(snapshot(&field("answer"), "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
        let save = view.pending.take().unwrap();
        view.saved(save, Ok((snapshot(&field("answer"), "two"), true)));
        assert!(view.editor.is_none());
        assert!(view.switch("project".parse().unwrap()));
    }

    #[test]
    fn picker_cancel_preserves_board_and_switch_resets_board_state() {
        let mut view = View::default();
        view.update(Ok(snapshot("- [ ] answer", "one")));
        view.selected = Some(0);
        view.scroll = (4, 6);
        view.error = Some("old error".into());
        view.notice = Some("old notice".into());
        view.update_notice = Some("release notice".into());
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let viewport = view.viewport.clone().unwrap();
        view.selection = Selection::start(viewport, Position::new(2, 2));
        view.picker = Some((
            vec![Summary {
                name: "project".parse().unwrap(),
                exists: true,
                revision: "two".into(),
                last_updated: None,
                last_updated_source: Default::default(),
                lifecycle: Default::default(),
            }],
            0,
        ));
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        // Escape first clears an active selection, retaining the picker.
        if view.picker.is_some() {
            press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        }
        assert_eq!(view.board, Name::default());
        assert_eq!(view.selected, Some(0));
        view.picker = Some((
            vec![Summary {
                name: "project".parse().unwrap(),
                exists: true,
                revision: "two".into(),
                last_updated: None,
                last_updated_source: Default::default(),
                lifecycle: Default::default(),
            }],
            0,
        ));
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(view.board.as_str(), "project");
        assert_eq!(view.generation, 1);
        assert!(view.snapshot.is_none());
        assert!(view.controls.is_empty());
        assert!(view.selected.is_none());
        assert!(view.selection.is_none());
        assert!(view.viewport.is_none());
        assert!(view.error.is_none());
        assert!(view.notice.is_none());
        assert_eq!(view.scroll, (0, 0));
        assert_eq!(view.update_notice.as_deref(), Some("release notice"));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(buffer_text(&terminal).contains("project"));
    }

    #[test]
    fn stale_reads_errors_and_save_conflicts_are_ignored_even_after_returning_to_main() {
        let mut view = View::default();
        view.update(Ok(snapshot("- [ ] original", "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char(' '), KeyModifiers::NONE);
        let save = view.pending.take().unwrap();
        assert!(view.switch("project".parse().unwrap()));
        assert!(!view.read_result(&Name::default(), 0, Ok(snapshot("stale", "two"))));
        assert!(!view.read_result(&Name::default(), 0, Err(anyhow::anyhow!("old outage"))));
        assert!(view.snapshot.is_none());
        assert!(view.error.is_none());
        assert!(view.switch(Name::default()));
        assert!(!view.read_result(&Name::default(), 0, Ok(snapshot("stale", "two"))));
        view.saving = true;
        view.saved(save, Ok((snapshot("old conflict", "two"), false)));
        assert!(view.saving, "an old result must not clear a current save");
        assert!(view.snapshot.is_none());
        assert!(view.notice.is_none());
        view.saved(
            Save {
                generation: 0,
                base: snapshot("old", "one"),
                markdown: "old".into(),
                text: false,
            },
            Err(anyhow::anyhow!("old uncertain write")),
        );
        assert!(view.notice.is_none());
        assert!(view.read_result(&Name::default(), 2, Ok(snapshot("current", "three"))));
        assert_eq!(view.snapshot.as_ref().unwrap().markdown, "current");
    }

    #[tokio::test]
    async fn terminal_response_save_delivers_generic_named_board_event() {
        use crate::protocol::{Agent, EventSelector, Provider};
        use std::os::unix::fs::PermissionsExt;
        use tokio::{io::AsyncReadExt, net::UnixListener, time::timeout};

        let root = tempfile::Builder::new()
            .prefix("ab-tui-events-")
            .tempdir_in("/tmp")
            .unwrap();
        let bound = broker::Broker::bind(root.path().into(), "/missing/codex".into()).unwrap();
        let broker = bound.broker.clone();
        let service = tokio::spawn(broker::serve(bound));
        let board: Name = "terminal".parse().unwrap();
        broker
            .request(Request::BoardCreate {
                board: board.clone(),
            })
            .unwrap();
        let initial: Snapshot =
            serde_json::from_value(broker.request(Request::board_read(board.clone())).unwrap())
                .unwrap();
        let seeded = broker
            .request(Request::board_update(
                board.clone(),
                initial.revision,
                vec![Edit::Set {
                    markdown: "- [ ] Review terminal response\n".into(),
                }],
                None,
            ))
            .unwrap();
        assert_eq!(seeded["applied"], true);
        let socket_path = root.path().join("native.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        broker
            .request(Request::Register {
                agent: Agent {
                    name: "terminal-watcher".into(),
                    provider: Provider::Claude,
                    session: uuid::Uuid::new_v4().to_string(),
                    endpoint: socket_path.to_string_lossy().into(),
                    enabled: true,
                },
                replace: false,
            })
            .unwrap();
        let (captured, mut received) = mpsc::channel(8);
        let receiver = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                socket.read_to_end(&mut bytes).await.unwrap();
                if !bytes.is_empty() {
                    captured.send(bytes).await.unwrap();
                }
            }
        });
        broker
            .request(Request::Subscribe {
                board_revisions: std::collections::BTreeMap::from([(
                    board.clone(),
                    seeded["board"]["revision"].as_str().unwrap().to_owned(),
                )]),
                agent: "terminal-watcher".into(),
                events: vec![EventSelector::BoardChanged {
                    board: board.clone(),
                }],
            })
            .unwrap();
        let mut view = View::default();
        assert!(view.switch(board.clone()));
        view.update(Ok(serde_json::from_value(seeded["board"].clone()).unwrap()));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char(' '), KeyModifiers::NONE);
        let save = view.pending.take().unwrap();
        assert_eq!(save.base.name, "terminal");
        // Same revision-checked request emitted by the terminal worker.
        let result = broker::rpc(
            root.path(),
            &Request::board_update(
                board.clone(),
                save.base.revision.clone(),
                vec![Edit::Set {
                    markdown: save.markdown.clone(),
                }],
                None,
            ),
        )
        .await
        .unwrap();
        view.saved(
            save,
            Ok((
                serde_json::from_value(result["board"].clone()).unwrap(),
                true,
            )),
        );
        assert!(view.snapshot.as_ref().unwrap().markdown.contains("[x]"));
        assert_eq!(crate::board::read(root.path()).unwrap().markdown, "");
        let bytes = timeout(Duration::from_secs(8), received.recv())
            .await
            .unwrap()
            .unwrap();
        let notice: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let text = notice["message"]["content"].as_str().unwrap();
        assert!(text.contains("\"board\":\"terminal\""), "{text}");
        assert!(
            text.contains(&view.snapshot.as_ref().unwrap().revision),
            "{text}"
        );
        assert!(text.contains("not user approval"));
        assert_eq!(
            broker
                .request(Request::History {
                    agent: None,
                    limit: 100
                })
                .unwrap(),
            serde_json::json!([])
        );
        receiver.abort();
        service.abort();
    }

    #[test]
    fn controls_toggle_exact_snapshot_and_refresh_clears_focus() {
        let source = "# Review\n\n- [ ] same\n- [ ] same\n";
        let mut view = View::default();
        view.update(Ok(snapshot(source, "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char(' '), KeyModifiers::NONE);
        let save = view.pending.take().unwrap();
        assert_eq!(save.base.revision, "one");
        assert_eq!(save.markdown, "# Review\n\n- [ ] same\n- [x] same\n");
        view.update(Ok(snapshot(
            "# Different question\n\n- [ ] inserted\n- [ ] same\n",
            "two",
        )));
        assert!(view.selected.is_none());
        press(&mut view, KeyCode::Char(' '), KeyModifiers::NONE);
        assert!(
            view.pending.is_none(),
            "old focus must not target a changed question"
        );
        press(&mut view, KeyCode::BackTab, KeyModifiers::SHIFT);
        assert_eq!(view.selected, Some(1));
    }
    #[test]
    fn unicode_editor_pins_revision_and_retains_draft_on_conflict() {
        let mut view = View::default();
        view.update(Ok(snapshot(&field("地球"), "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut view, KeyCode::Backspace, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('海'), KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('q'), KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
        let save = view.pending.take().unwrap();
        assert!(save.markdown.contains("> 地海\n> q\n"));
        let latest = format!("New context\n{}", field("地球"));
        view.saved(save, Ok((snapshot(&latest, "two"), false)));
        assert_eq!(view.editor.as_ref().unwrap().draft, "地海\nq");
        assert_eq!(view.editor.as_ref().unwrap().base.revision, "one");
        press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(view.pending.is_none());
        press(&mut view, KeyCode::PageDown, KeyModifiers::NONE);
        assert_eq!(
            view.scroll.0, 5,
            "latest question remains scrollable while editing"
        );
        press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(view.editor.as_ref().unwrap().base.revision, "two");
        press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
        let save = view.pending.take().unwrap();
        assert!(save.markdown.starts_with("New context\n"));
        assert_eq!(save.base.revision, "two");
    }
    #[test]
    fn concurrent_response_cannot_be_overwritten_by_rebasing_a_draft() {
        let mut view = View::default();
        view.update(Ok(snapshot(&field("original"), "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste(" my draft");
        view.update(Ok(snapshot(&field("someone else's response"), "two")));
        press(&mut view, KeyCode::Char('r'), KeyModifiers::CONTROL);
        assert_eq!(view.editor.as_ref().unwrap().base.revision, "one");
        assert_eq!(view.editor.as_ref().unwrap().draft, "original my draft");
        press(&mut view, KeyCode::Char('s'), KeyModifiers::CONTROL);
        assert!(view.pending.is_none());
        press(&mut view, KeyCode::Esc, KeyModifiers::NONE);
        assert!(view.editor.is_none());
        assert_eq!(
            view.snapshot.as_ref().unwrap().markdown,
            field("someone else's response")
        );
    }
    #[test]
    fn paste_is_literal_bounded_text_and_never_save_or_cancel_commands() {
        let mut view = View::default();
        view.update(Ok(snapshot(&field(""), "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste("yes\r\n\u{13}\u{1b}q 🌍");
        assert_eq!(view.editor.as_ref().unwrap().draft, "yes\nq 🌍");
        assert!(view.pending.is_none());
        view.paste(&"x".repeat(MAX_BOARD_BYTES));
        assert_eq!(view.editor.as_ref().unwrap().draft, "yes\nq 🌍");
        press(&mut view, KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(view.editor.as_ref().unwrap().cursor, 3);
        press(&mut view, KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(view.editor.as_ref().unwrap().cursor, 0);
        press(&mut view, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(view.editor.as_ref().unwrap().cursor, 4);
    }
    #[test]
    fn long_wrapped_draft_keeps_cursor_visible_and_outage_status_wins() {
        let mut view = View::default();
        view.update(Ok(snapshot(&field(""), "one")));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        view.paste(&format!("{}CURSOR_END", "long ".repeat(100)));
        let mut terminal = Terminal::new(TestBackend::new(30, 20)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(buffer_text(&terminal).contains('▏'));
        view.notice = Some("Response saved\u{1b}]0;unsafe\u{7}".into());
        view.update(Err(anyhow::anyhow!("offline")));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert!(buffer_text(&terminal).contains("Offline/stale"));
        assert!(!buffer_text(&terminal).chars().any(char::is_control));
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 1, 1))
            .unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
    }

    #[test]
    fn tiny_and_resized_views_do_not_panic_and_keys_scroll() {
        let mut view = View::default();
        view.update(Ok(snapshot("# Heading\n\nText", "one")));
        let mut terminal = Terminal::new(TestBackend::new(1, 1)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        view.key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE), 10);
        assert_eq!(view.scroll.0, 10);
        assert!(view.key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), 10));
        assert!(!view.key(
            KeyEvent::new_with_kind(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release
            ),
            10
        ));
    }
    fn button_position(view: &View, id: usize) -> Position {
        view.code_cells
            .iter()
            .filter(|(_, value)| **value == id)
            .map(|(point, _)| *point)
            .min_by_key(|point| (point.y, point.x))
            .unwrap()
    }

    #[test]
    fn code_copy_buttons_bind_exact_inline_list_table_and_offscreen_block_payloads() {
        let block = (0..30)
            .map(|index| format!("  echo '{index}'\\x\n"))
            .collect::<String>();
        let markdown = format!(
            "Run `` a`b `` then `地球` and `地球`.\n\n- `list --flag`\n\n| cmd |\n|---|\n| `table\\|x` |\n\n```sh\n{block}```\n"
        );
        let mut view = View::default();
        view.update(Ok(snapshot(&markdown, "one")));
        let mut terminal = Terminal::new(TestBackend::new(80, 25)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(
            view.code_payloads,
            [
                "a`b",
                "地球",
                "地球",
                "list --flag",
                "table|x",
                block.as_str()
            ]
        );
        for id in 0..view.code_payloads.len() {
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let point = button_position(&view, id);
            let (_, copied) =
                view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), point));
            assert_eq!(copied.as_deref(), Some(view.code_payloads[id].as_str()));
            assert!(
                view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), point))
                    .1
                    .is_none()
            );
            assert!(view.pending.is_none());
        }
        assert!(!buffer_text(&terminal).contains("echo '29'"));
        assert!(view.rendered.lines.iter().flat_map(|line| &line.spans).any(|span| span.content.contains("地球") && span.style.bg == Some(Color::DarkGray)));
    }

    #[test]
    fn copy_focus_is_separate_from_responses_works_without_mouse_and_invalidates_on_changes() {
        let mut view = View::default();
        view.update(Ok(snapshot("`one` `two`\n\n- [ ] response", "one")));
        let mut terminal = Terminal::new(TestBackend::new(50, 20)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(view.selected, Some(0));
        terminal.draw(|frame| view.draw(frame)).unwrap();
        press(&mut view, KeyCode::Char('m'), KeyModifiers::NONE);
        terminal.draw(|frame| view.draw(frame)).unwrap();
        press(&mut view, KeyCode::Char('c'), KeyModifiers::NONE);
        assert_eq!(view.focused_copy, Some(0));
        press(&mut view, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(view.focused_copy, Some(1));
        assert_eq!(view.selected, Some(0));
        press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(view.pending_copy.take().as_deref(), Some("two"));
        assert!(!view.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 10));
        assert!(!view.copy_focus);
        press(&mut view, KeyCode::Char('c'), KeyModifiers::NONE);
        press(&mut view, KeyCode::Char('w'), KeyModifiers::NONE);
        assert!(!view.copy_focus);
        assert!(view.code_cells.is_empty());
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let old_point = button_position(&view, 0);
        view.update(Ok(snapshot("`replacement`", "two")));
        assert!(view.code_cells.is_empty());
        view.mouse_enabled = true;
        assert!(
            view.mouse(mouse_event(
                MouseEventKind::Down(MouseButton::Left),
                old_point
            ))
            .1
            .is_none()
        );
        view.selection = None;
        terminal.draw(|frame| view.draw(frame)).unwrap();
        assert_eq!(view.code_payloads, ["replacement"]);
        assert!(view.switch("other".parse().unwrap()));
        assert!(view.code_cells.is_empty());
        assert!(view.code_payloads.is_empty());
    }

    #[test]
    fn code_buttons_are_not_hyperlinks_or_drag_selection_text() {
        let mut view = View::default();
        view.update(Ok(snapshot(
            "before [`cmd --flag`](https://example.com) after",
            "one",
        )));
        let mut terminal = Terminal::new(TestBackend::new(80, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let links = hyperlink_segments(terminal.backend().buffer());
        assert_eq!(
            links
                .iter()
                .map(|(_, _, text)| text.as_str())
                .collect::<String>(),
            "cmd --flag"
        );
        let area = view.viewport.as_ref().unwrap().area();
        let start = Position::new(area.x, area.y);
        let end = Position::new(area.right() - 1, area.y);
        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
        view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), end));
        let (_, copied) = view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end));
        assert_eq!(copied.as_deref(), Some("before cmd --flag after"));
        assert!(view.pending.is_none());
    }

    #[test]
    fn narrow_wrap_pan_scroll_and_resize_use_the_same_visible_button_mask() {
        let mut view = View::default();
        view.update(Ok(snapshot(
            "`long command with 地球 and --flag` then `next`\n\n```\nline1\nline2\n```",
            "one",
        )));
        let mut terminal = Terminal::new(TestBackend::new(18, 13)).unwrap();
        for wrap in [true, false] {
            view.wrap = wrap;
            for scroll in [(0, 0), (1, 4), (3, 8)] {
                view.scroll = scroll;
                terminal.draw(|frame| view.draw(frame)).unwrap();
                for id in view.visible_copies() {
                    terminal.draw(|frame| view.draw(frame)).unwrap();
                    let point = button_position(&view, id);
                    assert_eq!(
                        view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), point))
                            .1
                            .as_deref(),
                        Some(view.code_payloads[id].as_str())
                    );
                }
            }
        }
        terminal
            .resize(ratatui::layout::Rect::new(0, 0, 28, 16))
            .unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        for id in view.visible_copies() {
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let point = button_position(&view, id);
            assert_eq!(
                view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), point))
                    .1
                    .as_deref(),
                Some(view.code_payloads[id].as_str())
            );
        }
    }
    #[test]
    fn completed_selection_buttons_copy_displayed_old_payload_on_first_click_and_keyboard() {
        for keyboard in [false, true] {
            let mut view = View::default();
            view.update(Ok(snapshot("prose `displayed old` tail", "one")));
            let mut terminal = Terminal::new(TestBackend::new(70, 15)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let button = button_position(&view, 0);
            let area = view.viewport.as_ref().unwrap().area();
            let start = Position::new(area.x, area.y);
            let end = Position::new(area.x + 3, area.y);
            view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), start));
            view.mouse(mouse_event(MouseEventKind::Drag(MouseButton::Left), end));
            view.mouse(mouse_event(MouseEventKind::Up(MouseButton::Left), end));
            assert!(!view.selection.as_ref().unwrap().is_dragging());
            view.update(Ok(snapshot("prose `NEW wrong payload` tail", "two")));
            terminal.draw(|frame| view.draw(frame)).unwrap();
            assert!(buffer_text(&terminal).contains("displayed old"));
            assert_eq!(view.code_payloads, ["NEW wrong payload"]);
            if keyboard {
                press(&mut view, KeyCode::Char('c'), KeyModifiers::NONE);
                assert_eq!(view.focused_copy, Some(0));
                terminal.draw(|frame| view.draw(frame)).unwrap();
                press(&mut view, KeyCode::Enter, KeyModifiers::NONE);
                assert_eq!(view.pending_copy.take().as_deref(), Some("displayed old"));
                assert!(view.selection.is_some());
            } else {
                let (_, copied) =
                    view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), button));
                assert_eq!(copied.as_deref(), Some("displayed old"));
                assert!(view.selection.is_none());
            }
            assert!(view.pending.is_none());
            assert_eq!(view.snapshot.as_ref().unwrap().revision, "two");
        }
    }

    #[test]
    fn empty_block_reports_unchanged_clipboard_without_claiming_a_copy() {
        let mut view = View::default();
        view.update(Ok(snapshot("```sh\n```\n", "one")));
        let mut terminal = Terminal::new(TestBackend::new(50, 15)).unwrap();
        terminal.draw(|frame| view.draw(frame)).unwrap();
        let button = button_position(&view, 0);
        let (_, copied) = view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), button));
        assert_eq!(copied.as_deref(), Some(""));
        view.send_copy(copied.as_deref().unwrap());
        assert_eq!(
            view.notice.as_deref(),
            Some("Code block is empty; clipboard unchanged.")
        );
        assert!(view.pending.is_none());
    }
    #[test]
    fn indented_code_blocks_render_copy_buttons_in_top_level_and_container_contexts() {
        for (markdown, expected) in [
            (
                "# Indented\n\n    first\n      second\n    third\n",
                "first\n  second\nthird\n",
            ),
            ("# Tabs\n\n\tfirst\n\t  second\n", "first\n  second\n"),
            (
                ">     first\n>       second\n>     third\n",
                "first\n  second\nthird\n",
            ),
            (
                "- before\n\n      first\n        second\n      third\n",
                "first\n  second\nthird\n",
            ),
            (
                "> - before\n>\n>       first\n>         second\n>       third\n",
                "first\n  second\nthird\n",
            ),
        ] {
            let mut view = View::default();
            view.update(Ok(snapshot(markdown, "one")));
            let mut terminal = Terminal::new(TestBackend::new(80, 25)).unwrap();
            terminal.draw(|frame| view.draw(frame)).unwrap();
            let screen = buffer_text(&terminal);
            assert!(screen.contains("[⧉]"), "{markdown:?}: {screen}");
            assert!(!screen.contains("&#"), "{markdown:?}: {screen}");
            assert!(!screen.contains("```"), "{markdown:?}: {screen}");
            assert_eq!(view.code_payloads, [expected], "{markdown:?}");
            let point = button_position(&view, 0);
            let (_, copied) =
                view.mouse(mouse_event(MouseEventKind::Down(MouseButton::Left), point));
            assert_eq!(copied.as_deref(), Some(expected), "{markdown:?}");
            assert!(view.pending.is_none());
        }
    }
}
