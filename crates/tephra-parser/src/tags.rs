//! `#tag` scanning.
//!
//! A tag starts at a `#` that is either the first byte of the body
//! or immediately preceded by whitespace (whitespace includes `\n`,
//! so this also covers "line start"). This also doubles as the
//! heading-vs-tag disambiguation CommonMark requires (G3 edge
//! case): a `pulldown-cmark` ATX heading always has a space (or is
//! empty) after its `#`s, so a bare `#word` at the start of a line
//! is never recognized as a heading by `markdown_pass` and is
//! picked up here instead.
//!
//! A `#` anywhere else never starts a tag: mid-word as in `foo#bar`,
//! a wikilink heading anchor as in `[[Note#Heading]]`, or inside a
//! link/image destination as in `(https://example.com/page#frag)` —
//! none of those `#`s are preceded by whitespace. `[[...]]` spans
//! and link/image destination spans (`wikilink_spans`,
//! `link_destination_ranges`) are also excluded explicitly as a
//! second line of defense.
//!
//! Nesting (`#parent/child`) and unicode/emoji characters are part
//! of a single tag token, never split. A tag made up entirely of
//! ASCII digits (e.g. `#123`) is not a tag — Obsidian requires at
//! least one non-digit character — and is left as plain text.
//! Tag-like text inside a fenced/indented code block, inline code
//! span, or HTML block/inline HTML (`excluded_ranges`) is never
//! parsed as a tag.

use serde::Serialize;

use crate::fences::FenceCursor;
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
    excluded_ranges: &[(usize, usize)],
    wikilink_spans: &[(usize, usize)],
    link_destination_ranges: &[(usize, usize)],
) -> Vec<Tag> {
    // Merge the three independently-sorted exclusion lists into one
    // sorted list once, up front, so the scan below can walk it with
    // a single forward-only cursor instead of three.
    let mut skip: Vec<(usize, usize)> = Vec::with_capacity(
        excluded_ranges.len() + wikilink_spans.len() + link_destination_ranges.len(),
    );
    skip.extend_from_slice(excluded_ranges);
    skip.extend_from_slice(wikilink_spans);
    skip.extend_from_slice(link_destination_ranges);
    skip.sort_unstable_by_key(|r| r.0);

    let mut cursor = FenceCursor::new(&skip);
    let line_index = LineIndex::new(body);
    let mut out = Vec::new();

    // Sentinel: treat the very first byte of the body as if it
    // followed a newline, so a `#` there is a valid tag start too.
    let mut prev_char = '\n';
    let mut chars = body.char_indices().peekable();
    while let Some((byte_offset, c)) = chars.next() {
        if c == '#' && prev_char.is_whitespace() && !cursor.contains(byte_offset) {
            let mut end = byte_offset + c.len_utf8();
            let mut last = c;
            while let Some(&(next_offset, next_char)) = chars.peek() {
                if !is_tag_char(next_char) {
                    break;
                }
                chars.next();
                end = next_offset + next_char.len_utf8();
                last = next_char;
            }
            let tag_text = &body[byte_offset + 1..end];
            if !tag_text.is_empty() && !tag_text.chars().all(|ch| ch.is_ascii_digit()) {
                out.push(Tag {
                    tag: tag_text.to_string(),
                    line: body_start_line - 1 + line_index.line_number(byte_offset),
                });
            }
            prev_char = last;
            continue;
        }
        prev_char = c;
    }
    out
}
