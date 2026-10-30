//! Parsed code payloads and inert, width-preserving render markers.
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use std::collections::{HashMap, HashSet};

pub(crate) struct Glyph {
    pub text: String,
    pub snippet: usize,
    pub button: bool,
}
pub(crate) struct Prepared {
    pub source: String,
    pub glyphs: HashMap<char, Glyph>,
    pub payloads: Vec<String>,
}

pub(crate) fn code_style() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD)
}
pub(crate) fn button_style() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD)
}
pub(crate) fn id_color(id: usize) -> Color {
    let id = id + 1;
    Color::Rgb((id >> 16) as u8, (id >> 8) as u8, id as u8)
}
pub(crate) fn color_id(color: Color) -> Option<usize> {
    match color {
        Color::Rgb(r, g, b) => {
            (usize::from(r) << 16 | usize::from(g) << 8 | usize::from(b)).checked_sub(1)
        }
        _ => None,
    }
}
fn clean(input: &str) -> String {
    input
        .chars()
        .flat_map(|ch| match ch {
            '\t' => "    ".chars().collect::<Vec<_>>(),
            '\n' => vec!['\n'],
            ch if ch.is_control() => Vec::new(),
            ch => vec![ch],
        })
        .collect()
}
pub(crate) fn markdown_options() -> Options {
    Options::ENABLE_TABLES
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_HEADING_ATTRIBUTES
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_MATH
        | Options::ENABLE_GFM
        | Options::ENABLE_DEFINITION_LIST
        | Options::ENABLE_SUPERSCRIPT
        | Options::ENABLE_SUBSCRIPT
}

fn without_code_indent(prefix: &str) -> String {
    let whitespace_start = prefix.trim_end_matches([' ', '\t']).len();
    let mut columns = 0usize;
    let mut boundaries = Vec::new();
    for (index, byte) in prefix.bytes().enumerate() {
        boundaries.push((index, columns));
        columns += if byte == b'\t' { 4 - columns % 4 } else { 1 };
    }
    let retained_columns = columns.saturating_sub(4);
    let (end, column) = boundaries
        .into_iter()
        .rev()
        .find(|(index, column)| *index >= whitespace_start && *column <= retained_columns)
        .unwrap_or((whitespace_start, retained_columns));
    let mut retained = prefix[..end].to_owned();
    retained.extend(std::iter::repeat_n(
        ' ',
        retained_columns.saturating_sub(column),
    ));
    retained
}

impl Prepared {
    pub(crate) fn new(markdown: &str) -> Self {
        let events: Vec<_> = Parser::new_ext(markdown, markdown_options())
            .into_offset_iter()
            .collect();
        let mut used: HashSet<char> = markdown.chars().collect();
        // Avoid collisions with entity-decoded text, including code-block info.
        for (event, _) in &events {
            match event {
                Event::Text(text)
                | Event::Code(text)
                | Event::Html(text)
                | Event::InlineHtml(text) => used.extend(text.chars()),
                Event::Start(Tag::CodeBlock(pulldown_cmark::CodeBlockKind::Fenced(text))) => {
                    used.extend(text.chars())
                }
                _ => (),
            }
        }
        let mut result = Self {
            source: String::new(),
            glyphs: HashMap::new(),
            payloads: Vec::new(),
        };
        let mut narrow = 0x10000;
        let mut wide = 0x20000;
        let mut copied = 0;
        let mut events = events.into_iter();
        while let Some((event, range)) = events.next() {
            let (payload, block, indented) = match event {
                Event::Code(text) => (text.to_string(), false, false),
                Event::Start(Tag::CodeBlock(kind)) => {
                    let mut text = String::new();
                    for (event, _) in events.by_ref() {
                        match event {
                            Event::Text(part) => text.push_str(&part),
                            Event::End(TagEnd::CodeBlock) => break,
                            _ => (),
                        }
                    }
                    (
                        text,
                        true,
                        matches!(kind, pulldown_cmark::CodeBlockKind::Indented),
                    )
                }
                _ => continue,
            };
            if range.start < copied {
                continue;
            }
            if indented {
                // Indented-block ranges begin AFTER the parser consumes the
                // four-column code indent. Keeping that indent would turn the
                // generated header itself into a new indented code block, where
                // entities remain literal. Remove only that parser-confirmed
                // indent; retain surrounding quote/list container prefixes.
                let line_start = markdown[..range.start]
                    .rfind('\n')
                    .map_or(0, |index| index + 1);
                result
                    .source
                    .push_str(&clean(&markdown[copied..line_start]));
                result.source.push_str(&clean(&without_code_indent(
                    &markdown[line_start..range.start],
                )));
            } else {
                result
                    .source
                    .push_str(&clean(&markdown[copied..range.start]));
            }
            let id = result.payloads.len();
            let displayed = clean(&payload);
            result.payloads.push(payload);
            if block {
                result.markers("[⧉]", id, true, &mut used, &mut narrow, &mut wide);
                result.source.push_str("  \n");
                for line in displayed.split_inclusive('\n') {
                    result.markers(
                        line.trim_end_matches('\n'),
                        id,
                        false,
                        &mut used,
                        &mut narrow,
                        &mut wide,
                    );
                    result.source.push_str("  \n");
                }
                result.source.push('\n');
            } else {
                result.markers(&displayed, id, false, &mut used, &mut narrow, &mut wide);
                result.markers("[⧉]", id, true, &mut used, &mut narrow, &mut wide);
            }
            copied = range.end;
        }
        result.source.push_str(&clean(&markdown[copied..]));
        result
    }
    fn markers(
        &mut self,
        text: &str,
        snippet: usize,
        button: bool,
        used: &mut HashSet<char>,
        narrow: &mut u32,
        wide: &mut u32,
    ) {
        for glyph in Span::raw(text).styled_graphemes(Style::default()) {
            let width = Span::raw(glyph.symbol).width().clamp(1, 2);
            let counter = if glyph
                .symbol
                .chars()
                .any(|ch| Span::raw(ch.to_string()).width() == 2)
            {
                &mut *wide
            } else {
                &mut *narrow
            };
            let marker = loop {
                let candidate = char::from_u32(*counter).expect("Bounded board marker space");
                *counter += 1;
                if !candidate.is_control()
                    && Span::raw(candidate.to_string()).width() == width
                    && used.insert(candidate)
                {
                    break candidate;
                }
            };
            self.glyphs.insert(
                marker,
                Glyph {
                    text: glyph.symbol.to_owned(),
                    snippet,
                    button,
                },
            );
            self.source.push_str(&format!("&#{};", u32::from(marker)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parser_payloads_preserve_inline_semantics_and_whole_block_whitespace() {
        let markdown = "Run `` a`b `` and `a\\b`\n\n| command |\n|---|\n| `地\\|x` |\n\n```sh\n  echo 'x'\n\tlast\n```\n\n    indented\n";
        let code = Prepared::new(markdown);
        assert_eq!(
            code.payloads,
            ["a`b", "a\\b", "地|x", "  echo 'x'\n\tlast\n", "indented\n"]
        );
        assert_eq!(
            code.glyphs
                .values()
                .filter(|glyph| glyph.button && glyph.text == "[")
                .count(),
            5
        );
    }
    #[test]
    fn repeated_spans_have_independent_ids_and_hostile_payload_is_only_in_data() {
        let code = Prepared::new("`same` `same`\n\n```\n\x1b]52;c;hostile\x07\n```\n");
        assert_eq!(code.payloads[0], "same");
        assert_eq!(code.payloads[1], "same");
        assert!(code.payloads[2].contains('\x1b'));
        assert!(!code.source.contains('\x1b'));
        assert!(!code.source.contains("hostile"));
        for id in 0..3 {
            assert!(
                code.glyphs
                    .values()
                    .any(|glyph| glyph.button && glyph.snippet == id)
            );
        }
    }
}
