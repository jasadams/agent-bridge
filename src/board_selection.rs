//! Mouse selection over a frozen, rendered viewport, independent of Markdown.
use ratatui::{
    buffer::{Buffer, Cell, CellDiffOption},
    layout::{Position, Rect},
    style::{Color, Style},
    text::Span,
};

// A pathological terminal size must not cause another unbounded buffer clone.
const MAX_VIEWPORT_CELLS: usize = 256 * 1024;

#[derive(Clone)]
pub(crate) struct Viewport {
    buffer: Buffer,
}
impl Viewport {
    pub(crate) fn capture(buffer: &Buffer, area: Rect) -> Option<Self> {
        if area.is_empty()
            || usize::from(area.width) * usize::from(area.height) > MAX_VIEWPORT_CELLS
            || buffer.area.intersection(area) != area
        {
            return None;
        }
        let mut frozen = Buffer::empty(area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                frozen[(x, y)] = buffer[(x, y)].clone();
            }
        }
        Some(Self { buffer: frozen })
    }

    pub(crate) fn area(&self) -> Rect {
        self.buffer.area
    }
}

pub(crate) struct Selection {
    viewport: Viewport,
    anchor: Position,
    end: Position,
    dragging: bool,
}
impl Selection {
    pub(crate) fn start(viewport: Viewport, anchor: Position) -> Option<Self> {
        if !viewport.area().contains(anchor) {
            return None;
        }
        Some(Self {
            viewport,
            anchor,
            end: anchor,
            dragging: true,
        })
    }

    pub(crate) fn area(&self) -> Rect {
        self.viewport.area()
    }

    pub(crate) fn is_dragging(&self) -> bool {
        self.dragging
    }

    pub(crate) fn extend(&mut self, point: Position) -> bool {
        if !self.dragging {
            return false;
        }
        let area = self.area();
        let next = Position::new(
            point.x.clamp(area.left(), area.right() - 1),
            point.y.clamp(area.top(), area.bottom() - 1),
        );
        let changed = self.end != next;
        self.end = next;
        changed
    }

    fn bounds(&self) -> (Position, Position) {
        if (self.anchor.y, self.anchor.x) <= (self.end.y, self.end.x) {
            (self.anchor, self.end)
        } else {
            (self.end, self.anchor)
        }
    }

    fn row_bounds(&self, y: u16) -> Option<(u16, u16)> {
        let (start, end) = self.bounds();
        (start.y..=end.y).contains(&y).then_some((
            if y == start.y {
                start.x
            } else {
                self.area().left()
            },
            if y == end.y {
                end.x
            } else {
                self.area().right() - 1
            },
        ))
    }

    /// Repaint the exact body seen at mouse-down, then highlight whole graphemes.
    pub(crate) fn paint(&self, buffer: &mut Buffer) {
        if buffer.area.intersection(self.area()) != self.area() {
            return;
        }
        for y in self.area().top()..self.area().bottom() {
            let selected = self.row_bounds(y);
            let mut x = self.area().left();
            while x < self.area().right() {
                let cell = &self.viewport.buffer[(x, y)];
                let next = x.saturating_add(cell_width(cell)).min(self.area().right());
                let highlighted = self.anchor != self.end
                    && selected.is_some_and(|(left, right)| next > left && x <= right);
                for column in x..next {
                    let mut cell = self.viewport.buffer[(column, y)].clone();
                    if highlighted {
                        cell.set_style(Style::default().fg(Color::Black).bg(Color::Yellow));
                    }
                    buffer[(column, y)] = cell;
                }
                x = next;
            }
        }
    }

    /// Finishing a click or a drag returned to its anchor never changes a clipboard.
    pub(crate) fn finish(&mut self, point: Position) -> Option<String> {
        if !self.dragging {
            return None;
        }
        self.extend(point);
        self.dragging = false;
        if self.anchor == self.end {
            return None;
        }
        let mut text = String::new();
        let (start, end) = self.bounds();
        for y in start.y..=end.y {
            if y != start.y {
                text.push('\n');
            }
            let (left, right) = self.row_bounds(y).unwrap();
            let mut line = String::new();
            let mut x = self.area().left();
            while x < self.area().right() {
                let cell = &self.viewport.buffer[(x, y)];
                let next = x.saturating_add(cell_width(cell)).min(self.area().right());
                if next > left && x <= right {
                    line.push_str(cell.symbol());
                }
                x = next;
            }
            // Remove terminal padding, retaining leading spaces and real Unicode text.
            text.push_str(line.trim_end_matches(' '));
        }
        (!text.trim().is_empty()).then_some(text)
    }
}

fn cell_width(cell: &Cell) -> u16 {
    match cell.diff_option {
        CellDiffOption::ForcedWidth(width) => width.get(),
        _ => Span::raw(cell.symbol()).width().min(u16::MAX as usize) as u16,
    }
    .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport(lines: &[&str]) -> Viewport {
        let area = Rect::new(2, 3, 20, lines.len() as u16);
        let mut buffer = Buffer::empty(area);
        for (row, line) in lines.iter().enumerate() {
            buffer.set_string(area.x, area.y + row as u16, line, Style::default());
        }
        Viewport::capture(&buffer, area).unwrap()
    }

    #[test]
    fn forward_and_reverse_selection_copy_only_the_selected_visible_text() {
        let body = viewport(&["first row", "middle", "last row"]);
        let start = Position::new(5, 3);
        let end = Position::new(5, 5);
        let forward = Selection::start(body.clone(), start).unwrap().finish(end);
        let reverse = Selection::start(body, end).unwrap().finish(start);
        assert_eq!(forward.as_deref(), Some("st row\nmiddle\nlast"));
        assert_eq!(forward, reverse);
    }

    #[test]
    fn wide_and_combining_graphemes_are_never_split_or_duplicated() {
        let body = viewport(&["A地球 e\u{301} 👩‍💻 end"]);
        // Start on the continuation cell of 地, ending on the emoji's continuation.
        let mut selection = Selection::start(body, Position::new(4, 3)).unwrap();
        assert_eq!(
            selection.finish(Position::new(11, 3)).as_deref(),
            Some("地球 e\u{301} 👩‍💻")
        );
    }

    #[test]
    fn frozen_content_and_highlight_survive_new_rendered_data() {
        let body = viewport(&["before update"]);
        let mut selection = Selection::start(body.clone(), Position::new(2, 3)).unwrap();
        selection.extend(Position::new(7, 3));
        let mut target = Buffer::empty(body.area());
        target.set_string(2, 3, "AFTER UPDATE", Style::default());
        selection.paint(&mut target);
        assert_eq!(target[(2, 3)].symbol(), "b");
        assert_eq!(target[(2, 3)].bg, Color::Yellow);
        assert_eq!(
            selection.finish(Position::new(7, 3)).as_deref(),
            Some("before")
        );
    }

    #[test]
    fn outside_release_clamps_and_clicks_or_blank_selections_do_not_copy() {
        let body = viewport(&["first", "last"]);
        assert!(Selection::start(body.clone(), Position::new(0, 0)).is_none());
        let point = Position::new(2, 3);
        assert!(
            Selection::start(body.clone(), point)
                .unwrap()
                .finish(point)
                .is_none()
        );
        let mut selection = Selection::start(body.clone(), point).unwrap();
        selection.extend(Position::new(6, 4));
        assert!(selection.finish(point).is_none());
        assert_eq!(
            Selection::start(body, point)
                .unwrap()
                .finish(Position::new(200, 200))
                .as_deref(),
            Some("first\nlast")
        );
        let blank = viewport(&["   "]);
        assert!(
            Selection::start(blank, point)
                .unwrap()
                .finish(Position::new(4, 3))
                .is_none()
        );
    }

    #[test]
    fn capture_rejects_empty_outside_or_excessive_rectangles() {
        let buffer = Buffer::empty(Rect::new(0, 0, 10, 10));
        assert!(Viewport::capture(&buffer, Rect::default()).is_none());
        assert!(Viewport::capture(&buffer, Rect::new(20, 20, 1, 1)).is_none());
        assert!(Viewport::capture(&buffer, Rect::new(0, 0, 1000, 1000)).is_none());
    }
}
