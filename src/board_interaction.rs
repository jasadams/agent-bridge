//! Source-aware controls for a general Markdown document. These are editable
//! regions, not requests, permissions, or a task schema.
use crate::board::MAX_BOARD_BYTES;
use anyhow::{Result, ensure};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use std::{collections::BTreeMap, ops::Range};

#[derive(Clone, Debug)]
pub enum Kind {
    Message {
        id: String,
        instance: String,
        owner: crate::board_messages::Owner,
    },
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

    /// Stable across unrelated board edits, but binds the label and exact owner.
    pub fn message_key(&self, board: &str) -> Option<String> {
        let Kind::Message {
            id,
            instance,
            owner,
        } = &self.kind
        else {
            return None;
        };
        Some(digest_parts(&[
            board,
            id,
            instance,
            &self.label,
            &owner.name,
            owner.provider.label(),
            &owner.session,
            &owner.endpoint,
        ]))
    }

    pub fn replacement(&self, markdown: &str, value: Option<&str>) -> Result<String> {
        let replacement = match &self.kind {
            Kind::Message { .. } => anyhow::bail!("Message composers do not edit board Markdown"),
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

fn digest_parts(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part.as_bytes());
    }
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn control_id(snapshot: &crate::board::Snapshot, control: &Control) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in [
        snapshot.name.as_str(),
        snapshot.revision.as_str(),
        match &control.kind {
            Kind::Checkbox { .. } => "checkbox",
            Kind::Text { .. } => "text",
            Kind::Message { .. } => "message",
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
pub(crate) fn valid_id(id: &str) -> bool {
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
    let mut message_markers = Vec::new();
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
                let source = &markdown[range.clone()];
                // Adjacent complete standalone comments may share one HTML
                // block. Continue parsing those, while keeping every line of
                // an outer/multiline comment opaque.
                html_first = html_first.map(|_| {
                    first
                        && source.trim().starts_with("<!--")
                        && source.trim().ends_with("-->")
                        && source.matches("<!--").count() == 1
                        && source.matches("-->").count() == 1
                });
                if !first {
                    continue;
                }
                // A marker must occupy its entire source line and its entire HTML
                // event. This excludes marker-looking text nested inside comments.
                let line_start = markdown[..range.start].rfind('\n').map_or(0, |n| n + 1);
                let line_end = markdown[range.start..]
                    .find('\n')
                    .map_or(markdown.len(), |n| range.start + n + 1);
                if range.end <= line_end
                    && markdown[line_start..range.start].trim().is_empty()
                    && markdown[range.end..line_end].trim().is_empty()
                    && let Some(raw) = source
                        .trim()
                        .strip_prefix("<!-- board-message:")
                        .and_then(|s| s.strip_suffix("-->"))
                {
                    message_markers.push((line_start, range.clone(), raw.to_owned()));
                }
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
    let mut messages = Vec::new();
    let mut malformed_message = false;
    for (start, range, raw) in message_markers {
        if protected.iter().any(|region| region.contains(&start)) {
            continue;
        }
        // Even a malformed declaration poisons another occurrence of its ID.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw)
            && let Some(id) = value.get("id").and_then(|id| id.as_str())
        {
            *counts.entry(id.to_owned()).or_insert(0) += 1;
        }
        let parsed = serde_json::from_str::<crate::board_messages::Marker>(&raw);
        if parsed.as_ref().is_err() || parsed.as_ref().is_ok_and(|marker| !marker.valid()) {
            malformed_message = true;
        }
        if let Ok(marker) = parsed
            && marker.valid()
        {
            messages.push(Control {
                line: line_starts.partition_point(|offset| *offset <= start),
                label: marker.label,
                range,
                kind: Kind::Message {
                    id: marker.id,
                    instance: marker.instance,
                    owner: marker.owner,
                },
            });
        }
    }
    controls.extend(messages.into_iter().filter(|control| match &control.kind {
        Kind::Message { id, .. } => !malformed_message && counts.get(id) == Some(&1),
        _ => false,
    }));
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

/// Presentation-only source: keep composer metadata out of rendered Markdown.
/// The original document remains the authority for parsing and submission.
pub fn display_markdown(markdown: &str) -> std::borrow::Cow<'_, str> {
    let hidden = controls(markdown)
        .into_iter()
        .filter(|control| matches!(control.kind, Kind::Message { .. }))
        .map(|control| control.range)
        .collect::<Vec<_>>();
    if hidden.is_empty() {
        return std::borrow::Cow::Borrowed(markdown);
    }
    let mut displayed = String::with_capacity(markdown.len());
    let mut offset = 0;
    for range in hidden {
        displayed.push_str(&markdown[offset..range.start]);
        displayed.push('\n');
        offset = range.end;
    }
    displayed.push_str(&markdown[offset..]);
    std::borrow::Cow::Owned(displayed)
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
    fn message_marker(id: &str) -> String {
        format!(
            r#"<!-- board-message: {{"id":"{id}","instance":"123e4567-e89b-42d3-a456-426614174001","label":"Message coordinator","owner":{{"name":"coordinator","provider":"inbox","session":"session","endpoint":""}}}} -->"#
        )
    }

    #[test]
    fn display_hides_only_active_composer_metadata_without_changing_source() {
        let marker = message_marker("compose");
        let source = format!("Before\n{marker}\nAfter\n");
        let original = source.clone();
        let display = display_markdown(&source);
        assert!(matches!(display, std::borrow::Cow::Owned(_)));
        assert_eq!(display, "Before\n\nAfter\n");
        assert_eq!(source, original);
        assert_eq!(controls(&source).len(), 1);
    }

    #[test]
    fn display_preserves_examples_unrecognized_comments_and_invalid_controls() {
        let marker = message_marker("compose");
        for source in [
            format!("```md\n{marker}\n```\n"),
            format!("<!-- outer\n{marker}\n-->\n"),
            format!("<!-- board-input: response -->\n> {marker}\n<!-- /board-input -->\n"),
            "<!-- ordinary comment -->\n".to_owned(),
            "<!-- board-message: {broken} -->\n".to_owned(),
            format!("{marker}\n{marker}\n"),
        ] {
            assert!(
                matches!(display_markdown(&source), std::borrow::Cow::Borrowed(_)),
                "{source}"
            );
            assert_eq!(display_markdown(&source), source);
        }
        let example = message_marker("example");
        let source = format!("```md\n{example}\n```\n\n<!-- ordinary comment -->\n\n{marker}\n");
        let displayed = display_markdown(&source);
        assert!(displayed.contains(&example));
        assert!(displayed.contains("<!-- ordinary comment -->"));
        assert!(!displayed.contains(&marker));
    }
}
