//! A live Markdown view with revision-checked checkbox and text-field editing.
use crate::{
    board::{Edit, MAX_BOARD_BYTES, Name, Snapshot, Summary},
    board_interaction::{self, Control, Kind},
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
    layout::{Constraint, Layout, Position},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Wrap},
};
use std::{
    collections::{HashMap, HashSet},
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
    let prepared = crate::board_code_copy::Prepared::new(markdown);
    let safe = safe_markdown(markdown);
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
struct Editor {
    base: Snapshot,
    control: Control,
    draft: String,
    cursor: usize,
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

struct View {
    board: Name,
    generation: u64,
    picker: Option<(Vec<Summary>, usize)>,
    pick_requested: bool,
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
    notice: Option<String>,
    update_notice: Option<String>,
    viewport: Option<Viewport>,
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
            notice: None,
            update_notice: None,
            viewport: None,
            selection: None,
            mouse_enabled: true,
        }
    }
}
impl View {
    fn switch(&mut self, board: Name) -> bool {
        if self.editor.is_some() || self.saving || self.pending.is_some() {
            return false;
        }
        if self.board == board {
            self.picker = None;
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
                    });
                if self.snapshot.as_ref().map(|old| &old.revision) != Some(&snapshot.revision) {
                    self.width = 0;
                    self.clear_copy_focus();
                    self.code_cells.clear();
                    self.controls = board_interaction::controls(&snapshot.markdown);
                    // Source positions are only valid for the snapshot selected.
                    // Insertion/reordering must never silently retarget a response.
                    self.selected = None;
                    if self
                        .editor
                        .as_ref()
                        .is_some_and(|editor| editor.base.revision != snapshot.revision)
                    {
                        self.notice = Some("Board changed; draft retained. Review latest board; Ctrl-R rebases unchanged field before saving.".into());
                    }
                }
                self.snapshot = Some(snapshot);
                self.error = None;
                changed
            }
            Err(error) => {
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
    fn draw(&mut self, frame: &mut Frame<'_>) {
        let mut links_limited = false;
        let response_height = if self.editor.is_some() {
            9
        } else if self.controls.is_empty() {
            0
        } else {
            5
        };
        let [header, body, responses, footer] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(response_height),
            // Reserve the release-notice line even before a check completes so
            // availability changes never move the document or a mouse selection.
            Constraint::Length(3),
        ])
        .areas(frame.area());
        let (name, revision) = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                (
                    safe_line(&snapshot.name),
                    snapshot.revision.chars().take(12).collect::<String>(),
                )
            })
            .unwrap_or_else(|| (self.board.to_string(), "connecting".into()));
        frame.render_widget(
            Paragraph::new(format!(
                "Local shared board · {name} · {}",
                safe_line(&revision)
            ))
            .style(Style::default().fg(Color::Cyan)),
            header,
        );
        if let Some((boards, selected)) = &self.picker {
            let rows: Vec<Line> = boards
                .iter()
                .enumerate()
                .map(|(i, board)| {
                    Line::from(format!(
                        "{} {}{}",
                        if i == *selected { ">" } else { " " },
                        board.name,
                        if board.exists { "" } else { " (absent)" }
                    ))
                })
                .collect();
            frame.render_widget(
                Paragraph::new(rows)
                    .scroll((
                        selected.saturating_sub(usize::from(body.height.saturating_sub(3))) as u16,
                        0,
                    ))
                    .block(
                        Block::bordered().title("Boards · j/k select · Enter open · Esc cancel"),
                    ),
                body,
            );
            frame.render_widget(
                Paragraph::new(safe_line(self.update_notice.as_deref().unwrap_or(""))),
                footer,
            );
            return;
        }
        let path = self
            .snapshot
            .as_ref()
            .map(|snapshot| safe_line(&snapshot.path.to_string_lossy()))
            .unwrap_or_default();
        let block = Block::bordered().title(path);
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
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.area() != inner)
        {
            self.selection = None;
            self.notice = Some("Text selection cancelled because the board layout changed.".into());
        }
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
        if let Some(editor) = &self.editor {
            let title = format!(
                "Text response: {} · Ctrl-S save · Esc discard · save/discard before b switches · Ctrl-R rebase",
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
        } else if !self.controls.is_empty() {
            let title = format!(
                "Responses · Tab/Shift-Tab select · Space checkbox · Enter text · {} controls",
                self.controls.len()
            );
            let block = Block::bordered().title(title);
            let inner = block.inner(responses);
            let selected = self.selected.unwrap_or(0);
            let first = selected.saturating_sub(1);
            let lines = self
                .controls
                .iter()
                .enumerate()
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
        let status = if self.selection.as_ref().is_some_and(Selection::is_dragging) {
            "Selecting text · view frozen · release mouse to copy · Esc cancels".into()
        } else if self.selection.is_some() {
            format!(
                "{} · view frozen · Esc clears selection",
                self.notice.as_deref().unwrap_or("Text selected")
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
        frame.render_widget(Paragraph::new(vec![
            Line::from(safe_line(&status)),
            Line::from(if self.editor.is_some() { "Enter newline · arrows edit · PgUp/PgDn board · Ctrl-S save · Esc discard draft · then b boards · Ctrl-C quit" } else if self.mouse_enabled { "Drag text to copy · c code copies · m native mouse · b boards · q quit · j/k scroll · Tab responses · Space toggle · Enter edit" } else { "c code copies · m copy-on-select mouse · b boards · q quit · j/k scroll · Tab responses · Space toggle · Enter edit · r refresh" }),
            Line::from(safe_line(self.update_notice.as_deref().unwrap_or("")))
                .style(Style::default().fg(Color::Yellow)),
        ]), footer);
    }
    fn key(&mut self, key: KeyEvent, page: u16) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
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
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
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
                    self.editor = None;
                    self.notice = Some("Draft cancelled; board unchanged.".into());
                }
                KeyCode::Char('s') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.prepare_save()
                }
                KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.rebase_editor()
                }
                _ => self.editor.as_mut().unwrap().key(key),
            }
            return false;
        }
        if self.picker.is_some() {
            let (boards, selected) = self.picker.as_mut().unwrap();
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.picker = None,
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
            return true;
        }
        match key.code {
            KeyCode::Char('b') if !self.saving => self.pick_requested = true,
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
            }
            KeyCode::Char(' ') if !self.saving => self.toggle(),
            KeyCode::Enter if !self.saving => {
                if let (Some(index), Some(base)) = (self.selected, self.snapshot.as_ref()) {
                    let control = self.controls[index].clone();
                    if let Kind::Text { value, .. } = &control.kind {
                        self.editor = Some(Editor {
                            base: base.clone(),
                            draft: value.clone(),
                            cursor: value.len(),
                            control,
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
        if !self.mouse_enabled || self.picker.is_some() {
            return (false, None);
        }
        let point = Position::new(event.column, event.row);
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
        let Some(editor) = &self.editor else {
            return;
        };
        if self
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.revision != editor.base.revision)
        {
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
        let Kind::Text { id, value, .. } = &editor.control.kind else {
            return;
        };
        let current = self.controls.iter().find(|control| matches!(&control.kind, Kind::Text { id: other, value: latest, .. } if other == id && latest == value));
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
                self.update(Ok(snapshot));
                if applied {
                    if save.text {
                        self.editor = None;
                    }
                    self.notice = Some("Response saved to Markdown.".into());
                } else {
                    self.notice = Some(if save.text { "Save conflict; draft retained. Review board; Ctrl-R rebases an unchanged field." } else { "Board changed; checkbox was not toggled. Review and select it again." }.into());
                }
            }
            Err(error) => {
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

pub async fn run_named(state_dir: PathBuf, board: Name) -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "The board viewer needs an interactive terminal; use board --raw to print Markdown"
    );
    let initial: Snapshot =
        serde_json::from_value(broker::rpc(&state_dir, &Request::board_read(board.clone())).await?)
            .context("Invalid board snapshot from broker")?;
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
    let (selection, current_selection) = tokio::sync::watch::channel((board.clone(), 0u64));
    let (lists, mut list_requests) = mpsc::channel::<()>(1);
    enum Update {
        List(Name, u64, Result<Vec<Summary>>),
        Read(Name, u64, Result<Snapshot>, Option<String>),
        Saved(Save, Result<(Snapshot, bool)>),
    }
    let (updates, mut results) = mpsc::channel(1);
    let state = state_dir.clone();
    let worker = tokio::spawn(async move {
        loop {
            let update = tokio::select! {
                biased;
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
                    if request.is_none() { break; }
                    let (board, generation) = current_selection.borrow().clone();
                    Update::List(board, generation, async {
                        let value = broker::rpc(&state, &Request::BoardList).await?;
                        serde_json::from_value(value).context("Invalid board list from broker")
                    }.await)
                }
                request = requests.recv() => {
                    if request.is_none() { break; }
                    let (board, generation) = current_selection.borrow().clone();
                    let result = async {
                        let value = broker::rpc(&state, &Request::board_read(board.clone())).await?;
                        serde_json::from_value::<Snapshot>(value).context("Invalid board snapshot from broker")
                    }.await;
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
                        let _ = lists.try_send(());
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
                    if let Some(save) = view.pending.take() {
                        if writes.try_send(save).is_ok() { view.saving = true; } else { view.notice = Some("Another save is pending; draft retained.".into()); }
                    }
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
            _ = fallback.tick() => { let _ = refresh.try_send(()); },
            update = results.recv() => {
                let Some(update) = update else { anyhow::bail!("Board refresh worker stopped"); };
                let relevant = match &update {
                    Update::Read(board, generation, ..) => board == &view.board && *generation == view.generation,
                    Update::Saved(save, _) => save.generation == view.generation && save.base.name == view.board.as_str(),
                    Update::List(board, generation, _) => board == &view.board && *generation == view.generation && view.editor.is_none() && !view.saving,
                };
                if !relevant { continue; }
                let snapshot = match &update { Update::Read(_, _, Ok(snapshot), _) | Update::Saved(_, Ok((snapshot, _))) => Some(snapshot), _ => None };
                if let Some(snapshot) = snapshot {
                    let root = snapshot.path.parent().filter(|parent| parent.is_dir()).unwrap_or(&state_dir);
                    if watch.as_ref().is_none_or(|watch| watch.root != root) {
                        // Watching is an optimization; the bounded periodic refresh also
                        // handles unsupported filesystems and atomic replacement races.
                        watch = file_watch(&snapshot.path, &state_dir, refresh.clone()).ok();
                    }
                }
                match update {
                    Update::List(_, _, result) => {
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
                        dirty |= view.read_result(&board, generation, result);
                    },
                    Update::Saved(save, result) => { view.saved(save, result); dirty = true; },
                }
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
        }
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
        ] {
            let mut view = View::default();
            assert!(view.update(Ok(initial.clone())));
            assert!(view.update(Ok(changed.clone())));
            assert!(!view.update(Ok(changed)));
        }
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
        assert_eq!(text.to_string(), "a\\|b[Copy]");
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
        assert!(view.rendered.to_string().contains("[Copy block]"));
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
            assert!(screen.contains("[Copy block]"), "{markdown:?}: {screen}");
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
