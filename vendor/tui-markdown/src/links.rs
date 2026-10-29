//! Optional link provenance. Destinations are data, never terminal escapes.
use std::collections::BTreeMap;

use ratatui_core::text::{Line, Span, Text};

/// Index into a rendered document's destination table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinkId(pub usize);

/// Maximum number of attributed spans retained for one rendered document.
pub const MAX_LINK_SPANS: usize = 16 * 1024;

/// Ordinary terminal text and the link belonging to each attributed span.
/// Padding, borders and other generated text are not attributed.
#[derive(Debug)]
pub struct LinkedText<'a> {
    pub text: Text<'a>,
    pub destinations: Vec<String>,
    pub spans: BTreeMap<(usize, usize), LinkId>,
    /// Attribution overflow disables the whole map; ordinary text is retained.
    pub metadata_limited: bool,
}

#[derive(Clone)]
pub(crate) struct LinkedSpan<'a> {
    pub span: Span<'a>,
    pub link: Option<LinkId>,
}
impl<'a> From<Span<'a>> for LinkedSpan<'a> {
    fn from(span: Span<'a>) -> Self {
        Self { span, link: None }
    }
}
impl std::fmt::Debug for LinkedSpan<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.span.fmt(formatter)
    }
}

#[derive(Clone)]
pub(crate) struct LinkedLine<'a> {
    pub spans: Vec<LinkedSpan<'a>>,
}
impl<'a> From<Line<'a>> for LinkedLine<'a> {
    fn from(line: Line<'a>) -> Self {
        Self {
            spans: line.spans.into_iter().map(Into::into).collect(),
        }
    }
}
