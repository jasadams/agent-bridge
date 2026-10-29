//! Link provenance stays separate from text until the final trusted output step.
use ratatui::{
    buffer::{Buffer, CellDiffOption},
    layout::Position,
};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    num::NonZeroU16,
    sync::Arc,
};

pub(crate) const MAX_LINKS: usize = 1024;
pub(crate) const MAX_URI_BYTES: usize = 2048;
pub(crate) const MAX_OUTPUT_BYTES: usize = 512 * 1024;
const CLOSE: &str = "\x1b]8;;\x1b\\";

/// Only this constructor may introduce a destination into terminal output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Target(Arc<str>);
impl Target {
    fn parse(value: &str) -> Option<Self> {
        if value.len() > MAX_URI_BYTES
            || value
                .chars()
                .any(|ch| ch.is_control() || ch.is_whitespace() || ch == '\\')
        {
            return None;
        }
        let parsed = url::Url::parse(value).ok()?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
            || parsed.as_str().len() > MAX_URI_BYTES
        {
            return None;
        }
        Some(Self(Arc::from(parsed.as_str())))
    }
}

pub(crate) type SpanLinks = BTreeMap<(usize, usize), Target>;

pub(crate) fn span_links(document: &tui_markdown::LinkedText<'_>, raw: &str) -> SpanLinks {
    // Stripping raw controls must not turn a malformed destination into an
    // executable link. Still display the sanitized compact labels safely.
    if document.metadata_limited
        || document.destinations.len() > MAX_LINKS
        || raw
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
    {
        return SpanLinks::new();
    }
    let targets: Vec<_> = document
        .destinations
        .iter()
        .map(|value| Target::parse(value))
        .collect();
    document
        .spans
        .iter()
        .filter_map(|(&source, id)| {
            targets
                .get(id.0)?
                .as_ref()
                .map(|target| (source, target.clone()))
        })
        .collect()
}

#[derive(Clone, Debug)]
pub(crate) struct VisibleLink {
    pub position: Position,
    pub width: u16,
    pub target: Target,
}

pub(crate) fn enabled() -> bool {
    std::env::var_os("NO_TERM_HYPERLINKS").is_none()
        && std::env::var_os("TERM").is_none_or(|term| term != "dumb")
}

/// Encode only visible leading cells after layout, clipboard capture and
/// selection painting. The original width is explicit; target-only changes
/// participate in Ratatui's ordinary cell diff. Every cell closes independently.
pub(crate) fn fits(buffer: &Buffer, links: &[VisibleLink]) -> bool {
    let bytes = links
        .iter()
        .filter(|link| buffer.area.contains(link.position))
        .fold(0usize, |sum, link| {
            sum.saturating_add(
                link.target.0.len() + CLOSE.len() + 7 + buffer[link.position].symbol().len(),
            )
        });
    bytes <= MAX_OUTPUT_BYTES
}

pub(crate) fn encode(buffer: &mut Buffer, links: &[VisibleLink]) {
    if !fits(buffer, links) {
        return;
    }
    for link in links {
        if !buffer.area.contains(link.position) {
            continue;
        }
        let Some(width) = NonZeroU16::new(link.width) else {
            continue;
        };
        let cell = &mut buffer[link.position];
        let encoded = format!("\x1b]8;;{}\x1b\\{}{CLOSE}", link.target.0, cell.symbol());
        cell.set_symbol(&encoded)
            .set_diff_option(CellDiffOption::ForcedWidth(width));
    }
}

pub(crate) fn close_output() -> io::Result<()> {
    let mut output = io::stdout().lock();
    output.write_all(CLOSE.as_bytes())?;
    output.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{
        backend::{Backend, CrosstermBackend},
        layout::Rect,
        text::{Line, Span},
        widgets::{Paragraph, Widget, Wrap},
    };

    #[test]
    fn only_bounded_http_destinations_can_reach_output() {
        for value in [
            "https://example.test/a?q=one&two=3#part",
            "http://example.test/地球",
        ] {
            assert!(Target::parse(value).is_some(), "{value}");
        }
        for value in [
            "javascript:alert(1)",
            "data:text/html,hello",
            "file:///tmp/test",
            "mailto:user@example.test",
            "/relative",
            "https://",
            "https://example.test/\x1b]52;c;secret\x07",
            "https://example.test/\u{9c}",
            "https://example.test/a\nb",
            "https://example.test/\\bad",
            " https://example.test/",
        ] {
            assert!(Target::parse(value).is_none(), "{value:?}");
        }
        assert!(
            Target::parse(&format!(
                "https://example.test/{}",
                "x".repeat(MAX_URI_BYTES)
            ))
            .is_none()
        );
        let raw = "[label](https://example.test/)\x1b]52;c;bad\x07";
        let rendered = tui_markdown::from_str_with_links(raw, &tui_markdown::Options::default());
        assert!(span_links(&rendered, raw).is_empty());
    }

    #[test]
    fn compact_tables_keep_explicit_ownership_across_styles_and_unicode_wrapping() {
        let raw = "| Ticket |\n|---|\n| [**CDP-1277**](https://example.test/tickets/CDP-1277) [地球é👩‍👩‍👧‍👦](https://example.test/second) |\n| [SAME](https://example.test/adj-a)[SAME](https://example.test/adj-b) |";
        let document = tui_markdown::from_str_with_links(
            raw,
            &tui_markdown::Options::default().table_width(9),
        );
        assert!(!document.text.to_string().contains("https:"));
        let links = span_links(&document, raw);
        let mut labels: BTreeMap<String, String> = BTreeMap::new();
        for ((line, span), target) in links {
            labels
                .entry(target.0.to_string())
                .or_default()
                .push_str(&document.text.lines[line].spans[span].content);
        }
        assert_eq!(labels["https://example.test/tickets/CDP-1277"], "CDP-1277");
        assert_eq!(labels["https://example.test/second"], "地球é👩‍👩‍👧‍👦");
        assert_eq!(labels["https://example.test/adj-a"], "SAME");
        assert_eq!(labels["https://example.test/adj-b"], "SAME");
        assert!(
            document.text.width() < 25,
            "URL must not determine cell width"
        );
    }

    #[test]
    fn long_reference_definition_is_deduplicated_and_wrapped_output_stays_bounded() {
        let destination = format!("https://example.test/{}", "x".repeat(1800));
        let raw = format!(
            "| Link |\n|---|\n| [{}][ref] {} |\n\n[ref]: {destination}\n",
            "x".repeat(2000),
            "[x][ref] ".repeat(3000)
        );
        assert!(raw.len() <= 64 * 1024);
        let document = tui_markdown::from_str_with_links(
            &raw,
            &tui_markdown::Options::default().table_width(9),
        );
        assert!(!document.metadata_limited);
        assert_eq!(document.destinations, [destination]);
        // The upstream parser has its own reference-expansion budget. The
        // first long label also forces many fragments from one resolved URL.
        assert!(document.spans.len() > 400);
        assert!(document.spans.len() < tui_markdown::MAX_LINK_SPANS);
        assert!(document.spans.values().all(|id| id.0 == 0));
        assert!(document.text.lines.len() > 1000, "fixture must wrap");
        let sources = span_links(&document, &raw);
        let area = Rect::new(0, 0, 12, 200);
        let mut buffer = Buffer::empty(area);
        let mut visible = Vec::new();
        Paragraph::new(document.text).render_with_source(
            area,
            &mut buffer,
            |position, width, source| {
                if let Some(target) =
                    source.and_then(|source| sources.get(&(source.line, source.span)))
                {
                    visible.push(VisibleLink {
                        position,
                        width,
                        target: target.clone(),
                    });
                }
            },
        );
        assert!(
            visible.len() > 300,
            "wrapped fragments must exceed output budget"
        );
        assert!(!fits(&buffer, &visible));
        let plain = buffer.clone();
        encode(&mut buffer, &visible);
        assert_eq!(
            buffer, plain,
            "over-budget frame must remain entirely plain"
        );
    }

    #[test]
    fn attribution_budget_allows_boundary_then_disables_the_whole_map_stickily() {
        let options = tui_markdown::Options::default().table_width(5);
        let label = "x".repeat(tui_markdown::MAX_LINK_SPANS);
        let raw = format!("| Link |\n|---|\n| [{label}](https://example.test/) |\n");
        let document = tui_markdown::from_str_with_links(&raw, &options);
        assert!(!document.metadata_limited);
        assert_eq!(document.spans.len(), tui_markdown::MAX_LINK_SPANS);
        let raw = format!(
            "| Link |\n|---|\n| [{label}x](https://example.test/) |\n\n[later](https://example.test/later)"
        );
        let document = tui_markdown::from_str_with_links(&raw, &options);
        assert!(document.metadata_limited);
        assert!(
            document.spans.is_empty(),
            "later links must not repopulate overflowed metadata"
        );
        assert!(span_links(&document, &raw).is_empty());
        let ordinary = format!("| Link |\n|---|\n| {label}x |\n\nlater");
        assert!(
            document.text.to_string()
                == tui_markdown::from_str_with_options(&ordinary, &options).to_string(),
            "overflow must not change visible text or table geometry"
        );
    }

    #[test]
    fn paragraph_provenance_preserves_layout_and_distinguishes_identical_borrowed_spans() {
        let text = Line::from(vec![
            Span::raw("same"),
            Span::raw(" "),
            Span::raw("same"),
            Span::raw(" 地球é"),
        ]);
        for wrap in [false, true] {
            for horizontal in [0, 2] {
                let area = Rect::new(2, 3, 7, 4);
                let mut paragraph = Paragraph::new(text.clone()).scroll((0, horizontal));
                if wrap {
                    paragraph = paragraph.wrap(Wrap { trim: false });
                }
                let mut ordinary = Buffer::empty(area);
                paragraph.clone().render(area, &mut ordinary);
                let mut annotated = Buffer::empty(area);
                let mut observed = Vec::new();
                paragraph.render_with_source(area, &mut annotated, |position, width, source| {
                    observed.push((position, width, source.unwrap()));
                });
                assert_eq!(ordinary, annotated);
                assert!(observed.iter().any(|(_, _, source)| source.span == 0));
                assert!(observed.iter().any(|(_, _, source)| source.span == 2));
                for (position, width, source) in observed {
                    assert!(area.contains(position));
                    assert!(width > 0);
                    assert!(
                        text.spans[source.span]
                            .content
                            .is_char_boundary(source.byte)
                    );
                }
            }
        }
    }

    #[test]
    fn backend_emits_balanced_links_and_target_only_changes_force_a_cell_diff() {
        let area = Rect::new(0, 0, 5, 1);
        let mut plain = Buffer::empty(area);
        plain.set_string(0, 0, "地a b", ratatui::style::Style::default());
        let link = |url: &str| VisibleLink {
            position: Position::new(0, 0),
            width: 2,
            target: Target::parse(url).unwrap(),
        };
        let mut before = plain.clone();
        encode(&mut before, &[link("https://example.test/old")]);
        let mut after = plain.clone();
        encode(&mut after, &[link("https://example.test/new")]);
        assert!(
            before
                .diff(&after)
                .iter()
                .any(|(x, y, _)| (*x, *y) == (0, 0))
        );
        assert!(
            after
                .diff(&plain)
                .iter()
                .any(|(x, y, _)| (*x, *y) == (0, 0))
        );
        let mut bytes = Vec::new();
        let mut backend = CrosstermBackend::new(&mut bytes);
        backend
            .draw(std::iter::once((0, 0, &after[(0, 0)])))
            .unwrap();
        Backend::flush(&mut backend).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("\x1b]8;;https://example.test/new\x1b\\地\x1b]8;;\x1b\\"));
        assert_eq!(
            output.matches("\x1b]8;;https:").count(),
            output.matches(CLOSE).count()
        );
        assert_eq!(after[(2, 0)].symbol(), "a");
    }

    #[test]
    fn backend_wide_link_preserves_its_continuation_and_unchanged_neighbor() {
        let previous = Buffer::with_lines(["abZ"]);
        let mut next = Buffer::with_lines(["❤️Z"]);
        encode(
            &mut next,
            &[VisibleLink {
                position: Position::new(0, 0),
                width: 2,
                target: Target::parse("https://example.test/emoji").unwrap(),
            }],
        );
        let diff = previous.diff(&next);
        assert_eq!(
            diff.iter().map(|(x, y, _)| (*x, *y)).collect::<Vec<_>>(),
            [(0, 0)],
            "a continuation clear would paint at the wide glyph's advanced cursor and erase Z"
        );
        let mut bytes = Vec::new();
        let mut backend = CrosstermBackend::new(&mut bytes);
        backend.draw(diff.into_iter()).unwrap();
        Backend::flush(&mut backend).unwrap();
        let output = String::from_utf8(bytes).unwrap();
        assert!(output.contains("\x1b]8;;https://example.test/emoji\x1b\\❤️\x1b]8;;\x1b\\"));
        assert!(!output.split_once(CLOSE).unwrap().1.contains(' '));
    }

    #[test]
    fn output_budget_disables_the_whole_frame_without_partial_link_state() {
        let area = Rect::new(0, 0, 100, 10);
        let mut buffer = Buffer::empty(area);
        let target = Target::parse(&format!("https://example.test/{}", "x".repeat(1500))).unwrap();
        let links: Vec<_> = (0..10)
            .flat_map(|y| (0..100).map(move |x| Position::new(x, y)))
            .map(|position| VisibleLink {
                position,
                width: 1,
                target: target.clone(),
            })
            .collect();
        assert!(!fits(&buffer, &links));
        encode(&mut buffer, &links);
        assert!(
            buffer
                .content()
                .iter()
                .all(|cell| !cell.symbol().contains('\x1b'))
        );
    }

    #[test]
    fn wide_links_shrinking_to_linked_or_plain_ascii_refresh_the_trailing_cell() {
        for glyph in ["❤️", "地"] {
            for linked in [true, false] {
                for background in [ratatui::style::Color::Reset, ratatui::style::Color::Yellow] {
                    let area = Rect::new(0, 0, 4, 1);
                    let mut before = Buffer::empty(area);
                    let style = ratatui::style::Style::default()
                        .add_modifier(ratatui::style::Modifier::UNDERLINED);
                    before.set_string(0, 0, glyph, style.bg(background));
                    let mut after = Buffer::empty(area);
                    after.set_string(0, 0, "X", style);
                    let target = Target::parse("https://example.test/").unwrap();
                    encode(
                        &mut before,
                        &[VisibleLink {
                            position: Position::new(0, 0),
                            width: 2,
                            target: target.clone(),
                        }],
                    );
                    if linked {
                        encode(
                            &mut after,
                            &[VisibleLink {
                                position: Position::new(0, 0),
                                width: 1,
                                target,
                            }],
                        );
                    }
                    assert!(
                        before
                            .diff(&after)
                            .iter()
                            .any(|(x, y, _)| (*x, *y) == (1, 0)),
                        "the old wide link must be explicitly cleared, even if the trailing plain buffer cell is unchanged"
                    );
                }
            }
        }
    }
}
