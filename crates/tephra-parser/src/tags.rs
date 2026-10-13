//! `#tag` scanning.
//!
//! A tag is a `#` immediately followed (no space) by at least one
//! valid tag character; this also doubles as the heading-vs-tag
//! disambiguation CommonMark requires (G3 edge case): a `pulldown-cmark`
//! ATX heading always has a space (or is empty) after its `#`s, so a
//! bare `#word` at the start of a line is never recognized as a
//! heading by `markdown_pass` and is picked up here instead. Nesting
//! (`#parent/child`) and unicode/emoji characters are part of a
//! single tag token, never split. Tag-like text inside a fenced code
//! block is skipped entirely.

use serde::Serialize;

use crate::fences::in_any_range;
use crate::line_index::LineIndex;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tag {
    /// Without the leading `#`.
    pub tag: String,
    /// 1-indexed line number, absolute within the original file.
    pub line: usize,
}

/// Characters that end a tag token. Everything else — unicode
/// letters/digits, `_`, `-`, `/` (nesting), and symbol/emoji
/// characters not in this list — continues the tag.
const TAG_TERMINATORS: &[char] = &[
    '#', '[', ']', '(', ')', '{', '}', '<', '>', ',', '.', ';', ':', '!', '?', '\'', '"', '`', '|',
    '\\', '*', '~', '^', '=', '+', '$', '%', '&', '@',
];

fn is_tag_char(c: char) -> bool {
    !c.is_whitespace() && !TAG_TERMINATORS.contains(&c)
}

pub(crate) fn scan(
    body: &str,
    body_start_line: usize,
    fenced_ranges: &[(usize, usize)],
) -> Vec<Tag> {
    let mut out = Vec::new();
    let line_index = LineIndex::new(body);
    let chars: Vec<(usize, char)> = body.char_indices().collect();
    let mut idx = 0;
    while idx < chars.len() {
        let (byte_offset, c) = chars[idx];
        if c == '#' && !in_any_range(fenced_ranges, byte_offset) {
            let mut end = idx + 1;
            while end < chars.len() && is_tag_char(chars[end].1) {
                end += 1;
            }
            if end > idx + 1 {
                let tag_start_byte = chars[idx + 1].0;
                let tag_end_byte = if end < chars.len() {
                    chars[end].0
                } else {
                    body.len()
                };
                out.push(Tag {
                    tag: body[tag_start_byte..tag_end_byte].to_string(),
                    line: body_start_line - 1 + line_index.line_number(byte_offset),
                });
                idx = end;
                continue;
            }
        }
        idx += 1;
    }
    out
}
