//! Trusted terminal hyperlink output; Markdown and clipboard text stay plain.
use ratatui::{
    buffer::{Buffer, Cell, CellDiffOption},
    layout::Position,
    style::Color,
    text::Span,
};
use std::{num::NonZeroU16, sync::Arc};

pub(crate) const MAX_LINKS: usize = 1024;
pub(crate) const MAX_LINK_CELLS: usize = 16_384;
pub(crate) const MAX_URI_BYTES: usize = 2048;
pub(crate) const MAX_OUTPUT_BYTES: usize = 512 * 1024;

pub(crate) fn enabled() -> bool {
    std::env::var_os("NO_TERM_HYPERLINKS").is_none()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

pub(crate) fn valid_target(value: &str) -> bool {
    if value.len() > MAX_URI_BYTES
        || !value.is_ascii()
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
    {
        return false;
    }
    url::Url::parse(value)
        .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

pub(crate) fn close_output() {
    use std::io::Write;
    let mut output = std::io::stdout().lock();
    let _ = output.write_all(b"\x1b]8;;\x1b\\");
    let _ = output.flush();
}

fn cell_width(cell: &Cell) -> u16 {
    match cell.diff_option {
        CellDiffOption::ForcedWidth(width) => width.get(),
        _ => Span::raw(cell.symbol()).width().min(u16::MAX as usize) as u16,
    }
    .max(1)
}

#[derive(Clone, Default)]
pub(crate) struct Hyperlinks {
    cells: Vec<(Position, usize)>,
    urls: Arc<[String]>,
    limited: bool,
}
impl Hyperlinks {
    pub(crate) fn new(mask: &Buffer, urls: &[String]) -> Self {
        let mut cells = Vec::new();
        for y in mask.area.top()..mask.area.bottom() {
            for x in mask.area.left()..mask.area.right() {
                if let Color::Rgb(red, green, blue) = mask[(x, y)].fg {
                    let id =
                        (usize::from(red) << 16) | (usize::from(green) << 8) | usize::from(blue);
                    if id > 0 && id <= urls.len() {
                        if cells.len() == MAX_LINK_CELLS {
                            return Self {
                                limited: true,
                                ..Self::default()
                            };
                        }
                        cells.push((Position::new(x, y), id - 1));
                    }
                }
            }
        }
        Self {
            cells,
            urls: Arc::from(urls),
            limited: false,
        }
    }

    pub(crate) fn paint(&self, buffer: &mut Buffer) -> bool {
        // Count a conservative per-cell encoding before mutating any cell. The
        // actual grouped output can only be smaller; overflow stays all-plain.
        let bytes = self.cells.iter().fold(0usize, |sum, (position, link)| {
            sum.saturating_add(self.urls[*link].len() + 14 + buffer[*position].symbol().len())
        });
        if self.limited || bytes > MAX_OUTPUT_BYTES {
            return false;
        }
        let mut cells = self.cells.iter().peekable();
        while let Some(&(start, link)) = cells.next() {
            let cell = &buffer[start];
            let style = cell.style();
            let mut text = cell.symbol().to_owned();
            let mut end = start.x + cell_width(cell);
            while let Some(&&(point, next_link)) = cells.peek() {
                if point.y != start.y || point.x > end || next_link != link {
                    break;
                }
                if point.x < end {
                    cells.next();
                    continue;
                }
                let cell = &buffer[point];
                if cell.style() != style {
                    break;
                }
                text.push_str(cell.symbol());
                end += cell_width(cell);
                cells.next();
            }
            let width = NonZeroU16::new(end - start.x).unwrap();
            buffer[start]
                .set_symbol(&format!(
                    "\x1b]8;;{}\x1b\\{text}\x1b]8;;\x1b\\",
                    self.urls[link]
                ))
                .set_diff_option(CellDiffOption::ForcedWidth(width));
            for x in start.x + 1..end {
                buffer[(x, start.y)].set_diff_option(CellDiffOption::Skip);
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        backend::{Backend, CrosstermBackend},
        layout::Rect,
        style::{Modifier, Style},
    };

    fn links(buffer: &Buffer, target: &str) -> Hyperlinks {
        let mut mask = buffer.clone();
        for cell in &mut mask.content {
            cell.fg = Color::Rgb(0, 0, 1);
        }
        Hyperlinks::new(&mask, &[target.to_owned()])
    }

    #[test]
    fn only_valid_bounded_http_targets_are_accepted() {
        assert!(valid_target("https://example.test/a?q=x#part"));
        for target in [
            "https://",
            "https://example.test/\\evil",
            "javascript:alert(1)",
            "file:///tmp/x",
            "https://example.test/\x1b]52;c;evil\x07",
            "https://example.test/a b",
            "https://é.example/",
        ] {
            assert!(!valid_target(target), "{target:?}");
        }
        assert!(!valid_target(&format!(
            "https://example.test/{}",
            "x".repeat(MAX_URI_BYTES)
        )));
    }

    #[test]
    fn target_only_changes_emit_balanced_backend_output() {
        let plain = Buffer::with_lines(["label"]);
        let mut old = plain.clone();
        let mut new = plain.clone();
        assert!(links(&plain, "https://example.test/old").paint(&mut old));
        assert!(links(&plain, "https://example.test/new").paint(&mut new));
        let diff = old.diff(&new);
        assert_eq!(diff.len(), 1);
        let mut bytes = Vec::new();
        let mut backend = CrosstermBackend::new(&mut bytes);
        backend.draw(diff.into_iter()).unwrap();
        Backend::flush(&mut backend).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("\x1b]8;;https://example.test/new\x1b\\label\x1b]8;;\x1b\\"));
    }

    #[test]
    fn output_and_cell_budgets_disable_whole_frame_before_mutation() {
        for (width, height, target) in [
            (
                100,
                10,
                format!("https://example.test/{}", "x".repeat(1500)),
            ),
            (129, 128, "https://example.test/".to_owned()),
        ] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, width, height));
            let original = buffer.clone();
            assert!(!links(&buffer, &target).paint(&mut buffer));
            assert_eq!(buffer, original);
        }
        let mut exact = Buffer::empty(Rect::new(0, 0, 128, 128));
        assert!(links(&exact, "http://a").paint(&mut exact));
    }

    #[test]
    fn natural_styled_wide_glyph_replaced_by_short_link_clears_exposed_tail() {
        for glyph in ["地", "❤️"] {
            let mut previous = Buffer::empty(Rect::new(0, 0, 4, 1));
            previous.set_string(
                0,
                0,
                glyph,
                Style::default().add_modifier(Modifier::CROSSED_OUT),
            );
            let mut next = Buffer::empty(previous.area);
            next.set_string(0, 0, "X", Style::default());
            let mut mask = Buffer::empty(previous.area);
            mask[(0, 0)].fg = Color::Rgb(0, 0, 1);
            assert!(Hyperlinks::new(&mask, &["https://example.test/".to_owned()]).paint(&mut next));
            assert!(
                previous
                    .diff(&next)
                    .iter()
                    .any(|(x, y, _)| (*x, *y) == (1, 0)),
                "old styled trailing column must be redrawn"
            );
        }
    }
}
