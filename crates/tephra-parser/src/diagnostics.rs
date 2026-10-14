//! Structured parse diagnostics.
//!
//! A diagnostic never aborts the parse: `parse_unit` always returns a
//! complete `ParsedUnit`, with any malformed input recorded here
//! (with a line number) alongside whatever could still be extracted
//! from the rest of the file.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticKind {
    /// The YAML inside a well-delimited frontmatter block failed to
    /// parse (unbalanced quotes, bad indentation, etc.).
    InvalidFrontmatterYaml,
    /// The same top-level frontmatter key appeared more than once.
    DuplicateFrontmatterKey,
    /// `[[]]` — a wikilink with no inner content.
    EmptyWikilink,
    /// A `[[` was opened but never closed before end of line/file.
    UnterminatedWikilink,
    /// `[]()` — a markdown link with no destination.
    EmptyMarkdownLink,
    /// The same `^block-id` anchor appeared more than once in this file.
    DuplicateBlockId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub kind: DiagnosticKind,
    /// 1-indexed line number within the original file content.
    pub line: usize,
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(kind: DiagnosticKind, line: usize, message: impl Into<String>) -> Self {
        Diagnostic {
            kind,
            line,
            message: message.into(),
        }
    }
}
