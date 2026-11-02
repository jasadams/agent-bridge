//! Compatibility module name for Claude's supported native CLI directory.
//! Transcript scanning and its background observer were retired in AGB-43.
pub use super::claude_directory::{DirectorySession, DirectorySnapshot, parse, query_blocking};
