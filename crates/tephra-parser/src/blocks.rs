//! `^block-id` anchor scanning and paragraph blocks.
//!
//! A block anchor is a trailing `^id` token at the end of a line
//! (optionally preceded by whitespace), where `id` is one or more
//! ASCII letters/digits/hyphens (any length: G12's reading rule).
//! Anchors inside fenced/indented code blocks, inline code spans, or
//! HTML blocks/inline HTML (or, in a non-Markdown prose file, a
//! native literal region) are skipped. Every occurrence is kept, in
//! file order: a repeated ID is reported as a diagnostic but never
//! dropped or merged.
//!
//! A [`Paragraph`] is one block of prose with its line range. An
//! anchor on a paragraph's *last* line sets that paragraph's
//! `block_id` ([`attach_ids`]); an anchor anywhere else (a heading
//! line, a table row, the middle of a paragraph) stays a bare
//! anchor.

use serde::Serialize;

use crate::diagnostics::{Diagnostic, DiagnosticKind};
use crate::fences::FenceCursor;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockAnchor {
    /// Without the leading `^`.
    pub block_id: String,
    /// 1-indexed line number, absolute within the original file. For a
    /// `^id` in a code file that belongs to a symbol (G13) this is the
    /// symbol's first line, whichever line the comment sits on.
    pub line: usize,
    /// For a code file: the index in [`crate::ParsedUnit::symbols`] of
    /// the symbol that owns this ID. `None` for prose, and for an ID
    /// comment with no symbol to own it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<usize>,
    /// 1-indexed line the `^id` token itself is on. Equal to `line` for
    /// prose; for a code ID that belongs to a symbol it is the line of
    /// the comment, which can be above `line` (G19 renames the token in
    /// place, so it needs to know where it is). Not serialised.
    #[serde(skip)]
    pub token_line: usize,
}

/// One block of prose: a paragraph, or one list item's own text, with
/// the lines it spans. `block_id` is the `^id` ending its last line,
/// if there is one.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Paragraph {
    /// 1-indexed first line, absolute within the original file.
    pub start_line: usize,
    /// 1-indexed last line (equal to `start_line` for one line).
    pub end_line: usize,
    /// Without the leading `^`.
    pub block_id: Option<String>,
}

/// Sets `block_id` on each paragraph whose last line carries an
/// anchor. `paragraphs` must not overlap (no two share a last line);
/// a paragraph already holding an ID keeps it.
pub(crate) fn attach_ids(paragraphs: &mut [Paragraph], anchors: &[BlockAnchor]) {
    let mut by_end: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (idx, p) in paragraphs.iter().enumerate() {
        by_end.insert(p.end_line, idx);
    }
    for anchor in anchors {
        if let Some(&idx) = by_end.get(&anchor.line) {
            let p = &mut paragraphs[idx];
            if p.block_id.is_none() {
                p.block_id = Some(anchor.block_id.clone());
            }
        }
    }
}

/// The one character rule for a block ID (G12): ASCII letters, digits
/// and hyphens. Prose and code files share it, so `k3f9a2` and
/// `summary-v2` read the same everywhere.
pub(crate) fn is_block_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

/// `line` without a trailing `^id` token (and the whitespace before
/// it), and the ID: the same standalone-token rule [`scan`] reads, for
/// text that is being indexed rather than scanned for anchors. A line
/// with no such token comes back whole with `None`.
pub(crate) fn strip_trailing_block_id(line: &str) -> (&str, Option<&str>) {
    let trimmed = line.trim_end();
    let Some(caret) = trimmed.rfind('^') else {
        return (line, None);
    };
    let id = &trimmed[caret + 1..];
    let standalone = caret == 0
        || trimmed[..caret]
            .chars()
            .last()
            .is_some_and(char::is_whitespace);
    if standalone && !id.is_empty() && id.chars().all(is_block_id_char) {
        (trimmed[..caret].trim_end(), Some(id))
    } else {
        (line, None)
    }
}

pub(crate) fn scan(
    body: &str,
    body_start_line: usize,
    excluded_ranges: &[(usize, usize)],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<BlockAnchor> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut cursor = FenceCursor::new(excluded_ranges);

    let mut line_start = 0usize;
    let mut line_no = 0usize;
    for raw_line in body.split_inclusive('\n') {
        line_no += 1;
        let line = raw_line.trim_end_matches(['\n', '\r']);
        let trimmed = line.trim_end();
        if let Some(caret_rel) = trimmed.rfind('^') {
            // Must be a standalone token: either the whole (trimmed)
            // line, or immediately preceded by whitespace.
            let preceded_ok = caret_rel == 0
                || trimmed[..caret_rel]
                    .chars()
                    .last()
                    .map(|c| c.is_whitespace())
                    .unwrap_or(true);
            let id = &trimmed[caret_rel + 1..];
            if preceded_ok
                && !id.is_empty()
                && id.chars().all(is_block_id_char)
                && !cursor.contains(line_start + caret_rel)
            {
                let absolute_line = body_start_line - 1 + line_no;
                if let Some(&first_line) = seen.get(id) {
                    diagnostics.push(Diagnostic::new(
                        DiagnosticKind::DuplicateBlockId,
                        absolute_line,
                        format!("duplicate block id \"^{id}\" (first seen on line {first_line})"),
                    ));
                } else {
                    seen.insert(id.to_string(), absolute_line);
                }
                out.push(BlockAnchor {
                    block_id: id.to_string(),
                    line: absolute_line,
                    symbol: None,
                    token_line: absolute_line,
                });
            }
        }
        line_start += raw_line.len();
    }
    out
}
