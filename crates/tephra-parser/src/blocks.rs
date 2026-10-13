//! `^block-id` anchor scanning.
//!
//! A block anchor is a trailing `^id` token at the end of a line
//! (optionally preceded by whitespace), where `id` is one or more
//! ASCII letters/digits/hyphens. Anchors inside fenced code blocks
//! are skipped.

use serde::Serialize;

use crate::diagnostics::{Diagnostic, DiagnosticKind};
use crate::fences::in_any_range;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockAnchor {
    /// Without the leading `^`.
    pub block_id: String,
    /// 1-indexed line number, absolute within the original file.
    pub line: usize,
}

fn is_block_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-'
}

pub(crate) fn scan(
    body: &str,
    body_start_line: usize,
    fenced_ranges: &[(usize, usize)],
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<BlockAnchor> {
    let mut out = Vec::new();
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

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
                && !in_any_range(fenced_ranges, line_start)
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
                });
            }
        }
        line_start += raw_line.len();
    }
    out
}
