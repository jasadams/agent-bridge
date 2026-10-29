//! A live Markdown view with revision-checked checkbox and text-field editing.
use crate::{
    board::{Edit, MAX_BOARD_BYTES, Snapshot},
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
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Position},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Paragraph, Wrap},
};
use std::{
    borrow::Cow,
    io::IsTerminal,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::{sync::mpsc, task::JoinHandle};

const FALLBACK_REFRESH: Duration = Duration::from_secs(1);
// Tables with many sparse cells can expand far beyond their Markdown input.
// Leave such documents readable as plain Markdown instead of allocating a huge grid.
const MAX_TABLE_CELLS: usize = 20_000;

struct TerminalRestore;
impl Drop for TerminalRestore {
    fn drop(&mut self) {
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

fn rendered_markdown(markdown: &str, width: u16) -> (Text<'static>, bool) {
    let safe = safe_markdown(markdown);
    let lines = safe.lines().count();
    let columns = safe
        .lines()
        .map(|line| line.bytes().filter(|byte| *byte == b'|').count())
        .max()
        .unwrap_or(0);
    let plain = columns.saturating_mul(lines) > MAX_TABLE_CELLS;
    let rendered = if plain {
        Text::from(safe.as_str())
    } else {
        let options = tui_markdown::Options::default()
            .table_width(width)
            .table_row_dividers(true);
        tui_markdown::from_str_with_options(&safe, &options)
    };
    // Entity decoding can introduce controls that were absent from the source,
    // e.g. &#x1b;. Sanitize every final span, not just the Markdown input.
    let owned = Text {
        alignment: rendered.alignment,
        style: rendered.style,
        lines: rendered
            .lines
            .into_iter()
            .map(|line| Line {
                alignment: line.alignment,
                style: line.style,
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| Span {
                        style: span.style,
                        content: Cow::Owned(safe_line(&span.content)),
                    })
                    .collect(),
            })
            .collect(),
    };
    (owned, plain)
}

#[derive(Clone)]
struct Save {
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
    snapshot: Option<Snapshot>,
    error: Option<String>,
    rendered: Text<'static>,
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
            snapshot: None,
            error: None,
            rendered: Text::default(),
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
            .unwrap_or_else(|| ("main".into(), "connecting".into()));
        frame.render_widget(
            Paragraph::new(format!(
                "Local shared board · {name} · {}",
                safe_line(&revision)
            ))
            .style(Style::default().fg(Color::Cyan)),
            header,
        );
        let path = self
            .snapshot
            .as_ref()
            .map(|snapshot| safe_line(&snapshot.path.to_string_lossy()))
            .unwrap_or_default();
        let block = Block::bordered().title(path);
        let inner = block.inner(body);
        if self.width != inner.width {
            let markdown = self
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.markdown.as_str())
                .unwrap_or("");
            (self.rendered, self.plain) = rendered_markdown(markdown, inner.width);
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
            selection.paint(frame.buffer_mut());
        } else {
            self.viewport = Viewport::capture(frame.buffer_mut(), inner);
        }
        if let Some(editor) = &self.editor {
            let title = format!(
                "Text response: {} · Ctrl-S save · Esc cancel · Ctrl-R review/rebase",
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
            Line::from(if self.editor.is_some() { "Enter newline · arrows edit · PgUp/PgDn board · Ctrl-S save · Esc cancel · Ctrl-C quit" } else if self.mouse_enabled { "Drag text to copy · m native mouse · q quit · j/k scroll · Tab responses · Space toggle · Enter edit" } else { "m copy-on-select mouse · q quit · j/k scroll · Tab responses · Space toggle · Enter edit · r refresh" }),
            Line::from(safe_line(self.update_notice.as_deref().unwrap_or("")))
                .style(Style::default().fg(Color::Yellow)),
        ]), footer);
    }
    fn key(&mut self, key: KeyEvent, page: u16) -> bool {
        if key.kind == KeyEventKind::Release {
            return false;
        }
        if self.selection.take().is_some() && key.code == KeyCode::Esc {
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
        if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            return true;
        }
        match key.code {
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
        false
    }
    fn mouse(&mut self, event: MouseEvent) -> (bool, Option<String>) {
        if !self.mouse_enabled {
            return (false, None);
        }
        let point = Position::new(event.column, event.row);
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
    ensure!(
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
        "The board viewer needs an interactive terminal; use board --raw to print Markdown"
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
    enum Update {
        Read(Result<Snapshot>, Option<String>),
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
                        let value = broker::rpc(&state, &Request::BoardUpdate {
                            expected_revision: save.base.revision.clone(),
                            edits: vec![Edit::Set { markdown: save.markdown.clone() }],
                            actor: None,
                        }).await?;
                        let applied = value["applied"].as_bool().context("Invalid save response from broker")?;
                        let snapshot = serde_json::from_value::<Snapshot>(value["board"].clone()).context("Invalid saved board snapshot")?;
                        Ok((snapshot, applied))
                    }.await;
                    Update::Saved(save, result)
                }
                request = requests.recv() => {
                    if request.is_none() { break; }
                    let result = async {
                        let value = broker::rpc(&state, &Request::BoardRead).await?;
                        serde_json::from_value::<Snapshot>(value).context("Invalid board snapshot from broker")
                    }.await;
                    // Availability is broker-owned cached metadata. Reading it
                    // never fetches a release or changes the board document.
                    let notice = cached_release_notice(&state);
                    Update::Read(result, notice)
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
    let mut view = View::default();
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
                    let previous_mouse_mode = view.mouse_enabled;
                    if view.key(key, terminal.size()?.height.saturating_sub(5)) { break; }
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
                    if let Some(text) = copy {
                        view.notice = Some(match crate::board_clipboard::copy_to_terminal(&text) {
                            Ok(()) => format!("Copy sent to terminal ({} bytes). Press m for native selection if needed.", text.len()),
                            Err(error) => format!("Cannot send copy to terminal: {error:#}"),
                        });
                    }
                    dirty |= changed;
                }
                Some(Ok(Event::Resize(..))) => { view.selection = None; view.viewport = None; dirty = true; },
                Some(Ok(_)) => (),
                Some(Err(error)) => return Err(error).context("Cannot read terminal input"),
                None => break,
            },
            _ = fallback.tick() => { let _ = refresh.try_send(()); },
            update = results.recv() => {
                let Some(update) = update else { anyhow::bail!("Board refresh worker stopped"); };
                let snapshot = match &update { Update::Read(Ok(snapshot), _) | Update::Saved(_, Ok((snapshot, _))) => Some(snapshot), _ => None };
                if let Some(snapshot) = snapshot {
                    let root = snapshot.path.parent().filter(|parent| parent.is_dir()).unwrap_or(&state_dir);
                    if watch.as_ref().is_none_or(|watch| watch.root != root) {
                        // Watching is an optimization; the bounded periodic refresh also
                        // handles unsupported filesystems and atomic replacement races.
                        watch = file_watch(&snapshot.path, &state_dir, refresh.clone()).ok();
                    }
                }
                match update {
                    Update::Read(result, notice) => {
                        dirty |= view.update_release_notice(notice);
                        dirty |= view.update(result);
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
    #[test]
    fn raw_and_entity_encoded_terminal_controls_never_reach_spans() {
        let (text, _) = rendered_markdown(
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
        let (text, plain) = rendered_markdown(&markdown, 80);
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
}
