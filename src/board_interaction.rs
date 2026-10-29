//! Source-aware controls for a general Markdown document. These are editable
//! regions, not requests, permissions, or a task schema.
use crate::board::MAX_BOARD_BYTES;
use anyhow::{Result, ensure};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::{collections::BTreeMap, ops::Range};

#[derive(Clone, Debug)]
pub enum Kind {
    Checkbox {
        checked: bool,
    },
    Text {
        id: String,
        value: String,
        newline: &'static str,
    },
}
#[derive(Clone, Debug)]
pub struct Control {
    pub line: usize,
    pub label: String,
    pub range: Range<usize>,
    pub kind: Kind,
}
impl Control {
    /// Set the requested state without turning an ordinary retry into a toggle.
    pub fn set_checked(&self, markdown: &str, desired: bool) -> Result<String> {
        let Kind::Checkbox { checked } = self.kind else {
            anyhow::bail!("Not a checkbox control");
        };
        if checked == desired {
            return Ok(markdown.to_owned());
        }
        self.replacement(markdown, None)
    }

    /// Unchanged parsed text remains byte-exact, including its original quoting.
    pub fn set_text(&self, markdown: &str, desired: &str) -> Result<String> {
        let Kind::Text { value, .. } = &self.kind else {
            anyhow::bail!("Not a text control");
        };
        ensure!(
            desired.len() <= MAX_BOARD_BYTES,
            "Text response exceeds 64 KiB"
        );
        if value == desired {
            return Ok(markdown.to_owned());
        }
        self.replacement(markdown, Some(desired))
    }

    /// Stable only for the unique field's unchanged parsed value on this board.
    pub fn text_rebase_key(&self, board: &str) -> Option<String> {
        use sha2::{Digest, Sha256};
        let Kind::Text { id, value, .. } = &self.kind else {
            return None;
        };
        let mut digest = Sha256::new();
        for part in [board, id, value] {
            digest.update((part.len() as u64).to_be_bytes());
            digest.update(part.as_bytes());
        }
        Some(
            digest
                .finalize()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        )
    }

    pub fn replacement(&self, markdown: &str, value: Option<&str>) -> Result<String> {
        let replacement = match &self.kind {
            Kind::Checkbox { checked } => if *checked { " " } else { "x" }.to_owned(),
            Kind::Text { newline, .. } => {
                let value = value.unwrap_or_default();
                ensure!(
                    !value.contains('\r'),
                    "Text input must use newline characters, not carriage returns"
                );
                let mut quoted = String::new();
                for line in value.split('\n') {
                    quoted.push_str("> ");
                    quoted.push_str(line);
                    quoted.push_str(newline);
                }
                quoted
            }
        };
        ensure!(
            self.range.end <= markdown.len()
                && markdown.is_char_boundary(self.range.start)
                && markdown.is_char_boundary(self.range.end),
            "Control no longer matches its source"
        );
        ensure!(
            markdown.len() - self.range.len() + replacement.len() <= MAX_BOARD_BYTES,
            "Board exceeds 64 KiB"
        );
        let mut updated = markdown.to_owned();
        updated.replace_range(self.range.clone(), &replacement);
        Ok(updated)
    }
}

fn marker(line: &str) -> Option<Option<&str>> {
    let line = line.trim();
    if line == "<!-- /board-input -->" {
        Some(None)
    } else {
        line.strip_prefix("<!-- board-input:")?
            .strip_suffix("-->")
            .map(|id| Some(id.trim()))
    }
}
fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 80
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Find actual Markdown task markers and standalone, uniquely named text fields.
/// Code examples, ordinary [ ] text, HTML comments and field bodies are not controls.
pub fn controls(markdown: &str) -> Vec<Control> {
    let line_starts = std::iter::once(0)
        .chain(
            markdown
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(index, _)| index + 1),
        )
        .collect::<Vec<_>>();
    let mut controls = Vec::new();
    let mut markers = BTreeMap::new();
    let mut html_first = None;
    for (event, range) in Parser::new_ext(markdown, Options::ENABLE_TASKLISTS).into_offset_iter() {
        match event {
            Event::Start(Tag::HtmlBlock) => html_first = Some(true),
            Event::End(TagEnd::HtmlBlock) => html_first = None,
            Event::TaskListMarker(checked) => {
                if let Some(relative) = markdown[range.clone()].find('[') {
                    let start = range.start + relative;
                    if matches!(
                        markdown.as_bytes().get(start..start + 3),
                        Some(b"[ ]" | b"[x]" | b"[X]")
                    ) {
                        let end = markdown[start + 3..]
                            .find('\n')
                            .map_or(markdown.len(), |n| start + 3 + n);
                        controls.push(Control {
                            line: line_starts.partition_point(|offset| *offset <= start),
                            label: markdown[start + 3..end].trim().to_owned(),
                            range: start + 1..start + 2,
                            kind: Kind::Checkbox { checked },
                        });
                    }
                }
            }
            Event::Html(_) => {
                // pulldown emits separate HTML events for lines inside a single
                // comment/block. Only its first line may declare a field marker.
                let first = html_first.unwrap_or(true);
                html_first = html_first.map(|_| false);
                if !first {
                    continue;
                }
                let source = &markdown[range.clone()];
                // A marker must occupy its entire source line and its entire HTML
                // event. This excludes marker-looking text nested inside comments.
                if let Some(value) = marker(source)
                    && !source.trim().contains('\n')
                {
                    let line_start = markdown[..range.start].rfind('\n').map_or(0, |n| n + 1);
                    let line_end = markdown[range.start..]
                        .find('\n')
                        .map_or(markdown.len(), |n| range.start + n + 1);
                    if range.end <= line_end
                        && markdown[line_start..range.start].trim().is_empty()
                        && markdown[range.end..line_end].trim().is_empty()
                    {
                        markers.insert(line_start, value.map(str::to_owned));
                    }
                }
            }
            _ => (),
        }
    }
    let mut offset = 0;
    let mut open: Option<(usize, usize, String, bool, &'static str)> = None;
    let mut fields = Vec::new();
    let mut protected = Vec::new();
    for line in markdown.split_inclusive('\n') {
        let next = offset + line.len();
        if let Some(mark) = markers.get(&offset) {
            match mark {
                Some(id) => {
                    if let Some((_, _, _, invalid, _)) = open.as_mut() {
                        *invalid = true;
                    } else {
                        open = Some((
                            offset,
                            next,
                            id.clone(),
                            !valid_id(id),
                            if line.ends_with("\r\n") { "\r\n" } else { "\n" },
                        ));
                    }
                }
                None => {
                    if let Some((start, body, id, invalid, newline)) = open.take() {
                        protected.push(start..next);
                        let mut values = Vec::new();
                        let mut valid = !invalid;
                        for content in markdown[body..offset].split_inclusive('\n') {
                            let content = content.trim_end_matches('\n').trim_end_matches('\r');
                            if let Some(quoted) = content.strip_prefix('>') {
                                values.push(quoted.strip_prefix(' ').unwrap_or(quoted));
                            } else if content.trim().is_empty() {
                                values.push("");
                            } else {
                                valid = false;
                            }
                        }
                        if valid {
                            fields.push(Control {
                                line: line_starts.partition_point(|offset| *offset <= start),
                                label: id.clone(),
                                range: body..offset,
                                kind: Kind::Text {
                                    id,
                                    value: values.join("\n"),
                                    newline,
                                },
                            });
                        }
                    }
                }
            }
        }
        offset = next;
    }
    if let Some((start, ..)) = open {
        protected.push(start..markdown.len());
    }
    let mut counts = BTreeMap::new();
    for id in markers.values().flatten() {
        *counts.entry(id.clone()).or_insert(0) += 1;
    }
    controls.retain(|control| {
        !protected
            .iter()
            .any(|range| range.contains(&control.range.start))
    });
    controls.extend(fields.into_iter().filter(|control| match &control.kind {
        Kind::Text { id, .. } => counts[id] == 1,
        _ => true,
    }));
    controls.sort_by_key(|control| control.range.start);
    controls
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checkbox_offsets_distinguish_identical_labels_and_ignore_examples() {
        let source = "[ ] plain\n\n`- [ ] code`\n\n```md\n- [ ] example\n```\n\n<!--\n- [ ] hidden\n-->\n\n- [ ] same\n- [X] same\n";
        let controls = controls(source);
        assert_eq!(controls.len(), 2);
        let updated = controls[1].replacement(source, None).unwrap();
        assert!(updated.ends_with("- [ ] same\n- [ ] same\n"));
        assert_eq!(
            &source[..controls[1].range.start],
            &updated[..controls[1].range.start]
        );
    }
    #[test]
    fn unicode_and_crlf_text_updates_preserve_every_byte_outside_the_field() {
        let source = "# 世界\r\n\r\n<!-- board-input: notes -->\r\n> old\r\n<!-- /board-input -->\r\nTail\r\n";
        let parsed = controls(source);
        assert_eq!(parsed.len(), 1);
        assert!(matches!(&parsed[0].kind, Kind::Text { value, .. } if value == "old"));
        let updated = parsed[0]
            .replacement(source, Some("你好\nsecond line"))
            .unwrap();
        assert_eq!(
            updated,
            source.replace("> old\r\n", "> 你好\r\n> second line\r\n")
        );
    }
    #[test]
    fn malformed_nested_and_duplicate_fields_are_not_editable() {
        for source in [
            "<!-- board-input: one -->\n> x\n",
            "<!-- board-input: one -->\n<!-- board-input: two -->\n> x\n<!-- /board-input -->\n<!-- /board-input -->\n",
            "<!-- board-input: one -->\nnot quoted\n<!-- /board-input -->\n",
            "<!-- board-input: bad id -->\n> x\n<!-- /board-input -->\n",
            "<!-- board-input: same -->\n> a\n<!-- /board-input -->\n\n<!-- board-input: same -->\n> b\n<!-- /board-input -->\n",
            "<!-- board-input: same -->\n> a\n<!-- /board-input -->\n\n<!-- board-input: same -->\nmalformed\n<!-- /board-input -->\n",
            "```\n<!-- board-input: example -->\n> x\n<!-- /board-input -->\n```\n",
            "<!-- outer\n<!-- board-input: example -->\n> x\n<!-- /board-input -->\n-->\n",
        ] {
            assert!(controls(source).is_empty(), "{source}");
        }
    }
    #[test]
    fn task_markers_inside_response_fields_are_plain_response_text() {
        let source = "<!-- board-input: notes -->\n> - [ ] not a control\n<!-- /board-input -->\n";
        let parsed = controls(source);
        assert_eq!(parsed.len(), 1);
        assert!(matches!(&parsed[0].kind, Kind::Text { .. }));
        let updated = parsed[0]
            .replacement(source, Some("<!-- /board-input -->\n- [x] text"))
            .unwrap();
        let again = controls(&updated);
        assert_eq!(again.len(), 1);
    }
}
