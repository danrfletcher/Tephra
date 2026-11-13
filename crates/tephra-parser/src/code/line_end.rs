//! What the end of one line of a code file is (PR-12.F1, E5).
//!
//! `block.ensure_id` writes a comment at the end of a symbol's
//! definition line. Whether that is safe is a question for the syntax
//! tree, not for the text: a `#` at the end of `key: 'a#b` or inside a
//! docstring that opens on the `def` line would be part of a string, and
//! a `//` after `/* ...` would be part of a block comment. This module
//! answers it with the same tree-sitter parse the symbols come from.
//!
//! The answer is a [`LineEnd`]:
//!
//! - [`LineEnd::Code`]: the line ends in code (or after a block comment
//!   that closes on the same line): a line comment can follow;
//! - [`LineEnd::LineComment`]: the line already ends with a line
//!   comment: the ID goes at the end of that comment;
//! - [`LineEnd::InString`] / [`LineEnd::InBlockComment`]: the line break
//!   is inside a string-like token or a block comment that goes on to
//!   the next line;
//! - [`LineEnd::ParseError`]: an `ERROR` or `MISSING` node starts or ends
//!   on the line, so the tree there cannot be trusted (E12);
//! - [`LineEnd::Unparsed`]: the file was not parsed (too large, or past
//!   the time budget).
//!
//! Only the line's own errors matter: a syntax error elsewhere in the
//! file does not stop an ID on a line that parses cleanly.

use tree_sitter::{Node, Tree};

use super::languages::Language;
use super::{parse_tree, CODE_PARSE_BUDGET, MAX_CODE_PARSE_BYTES};

/// What the end of a line is. See the [module docs](self).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnd {
    Code,
    LineComment,
    InString,
    InBlockComment,
    ParseError,
    Unparsed,
}

/// Node kinds that are a string, or hold string text, in any bundled
/// grammar. A line break inside one of these is part of the string.
/// (`concatenated_string`, Python's and C's run of adjacent literals, is
/// deliberately not here: the break between two literals is code.)
const STRING_KINDS: &[&str] = &[
    "string",
    "string_literal",
    "raw_string_literal",
    "interpreted_string_literal",
    "template_string",
    "raw_string",
    "ansi_c_string",
    "translated_string",
    "heredoc_body",
    "multi_line_string_literal",
    "line_string_literal",
    "double_quote_scalar",
    "single_quote_scalar",
    "plain_scalar",
    "string_scalar",
    "block_scalar",
];

/// How the end of line `line` (1-based) of `content`, a file in `lang`,
/// reads. `None` when the file has no such line. A line break is the
/// `\n` or `\r\n` that ends the line; the last line of a file with no
/// final newline has none, so nothing can continue past it.
pub fn classify_line_end(lang: Language, content: &str, line: usize) -> Option<LineEnd> {
    let (start, end) = line_bounds(content, line)?;
    if content.len() > MAX_CODE_PARSE_BYTES {
        return Some(LineEnd::Unparsed);
    }
    let Ok(tree) = parse_tree(lang, content, CODE_PARSE_BUDGET) else {
        return Some(LineEnd::Unparsed);
    };
    Some(classify(lang, &tree, content, line, start, end))
}

/// `(start, end)` byte offsets of line `line` (1-based): `end` is before
/// its `\n` or `\r\n`.
fn line_bounds(content: &str, line: usize) -> Option<(usize, usize)> {
    let mut start = 0usize;
    let mut number = 1usize;
    let bytes = content.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'\n' {
            if number == line {
                let end = if i > start && bytes[i - 1] == b'\r' {
                    i - 1
                } else {
                    i
                };
                return Some((start, end));
            }
            number += 1;
            start = i + 1;
        }
    }
    (number == line && start < bytes.len()).then_some((start, bytes.len()))
}

fn classify(
    lang: Language,
    tree: &Tree,
    src: &str,
    line: usize,
    start: usize,
    end: usize,
) -> LineEnd {
    let root = tree.root_node();
    if root.has_error() && line_has_error(root, start, end) {
        return LineEnd::ParseError;
    }

    // The last character that is not whitespace: if a comment holds it,
    // the line ends with that comment.
    let text = &src[start..end];
    if let Some(last) = text.trim_end().char_indices().next_back().map(|(i, _)| i) {
        let at = start + last;
        if let Some(comment) = comment_holding(lang, root, at) {
            let body = src
                .get(comment.start_byte()..comment.end_byte())
                .unwrap_or("");
            if is_line_comment(lang, comment.kind(), body) {
                return LineEnd::LineComment;
            }
            if comment.end_byte() > end {
                return LineEnd::InBlockComment;
            }
            // A block comment that closes on this line: code follows it.
        }
    }

    // The line break itself: is it inside a token or a node that goes on?
    if end < src.len() {
        let Some(mut node) = root.descendant_for_byte_range(end, end + 1) else {
            return LineEnd::Code;
        };
        if node.start_byte() > end || node.end_byte() <= end {
            return LineEnd::Code;
        }
        if lang.is_comment_kind(node.kind()) {
            return LineEnd::InBlockComment;
        }
        let is_leaf = node.child_count() == 0;
        if is_leaf
            && !src
                .get(node.start_byte()..node.end_byte())
                .is_some_and(|t| t.trim().is_empty())
        {
            return LineEnd::InString;
        }
        loop {
            if lang.is_comment_kind(node.kind()) {
                return LineEnd::InBlockComment;
            }
            if STRING_KINDS.contains(&node.kind()) {
                // `key: |` ends its line with the block scalar's own
                // indicator; a comment after it is a YAML comment.
                if node.kind() == "block_scalar" && node.start_position().row + 1 == line {
                    return LineEnd::Code;
                }
                return LineEnd::InString;
            }
            match node.parent() {
                Some(parent) => node = parent,
                None => break,
            }
        }
    }
    LineEnd::Code
}

/// Whether an `ERROR` or `MISSING` node starts or ends on the line
/// `[start, end]` (byte offsets, `end` before the line break). A large
/// `ERROR` that merely spans the line does not count: only its edges
/// are where the parse went wrong.
fn line_has_error(root: Node<'_>, start: usize, end: usize) -> bool {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.end_byte() < start || node.start_byte() > end {
            continue;
        }
        if node.is_error() || node.is_missing() {
            let first = node.start_byte();
            let last = node.end_byte().saturating_sub(1).max(first);
            let on_line = |b: usize| start <= b && b <= end;
            if on_line(first) || on_line(last) {
                return true;
            }
        }
        if node.has_error() || node.is_error() {
            let mut cursor = node.walk();
            stack.extend(node.children(&mut cursor));
        }
    }
    false
}

/// The comment node that holds byte `at`, if any: the smallest node
/// there and its ancestors are checked (a comment's own children, like
/// Lua's `comment_content` or Rust's `//`, are inside it).
fn comment_holding<'t>(lang: Language, root: Node<'t>, at: usize) -> Option<Node<'t>> {
    let mut node = root.descendant_for_byte_range(at, at + 1)?;
    loop {
        if lang.is_comment_kind(node.kind()) {
            return Some(node);
        }
        node = node.parent()?;
    }
}

/// Whether a comment node is a line comment (`// ...`, `# ...`,
/// `-- ...`), as opposed to a block comment (`/* ... */`,
/// `--[[ ... ]]`).
fn is_line_comment(lang: Language, kind: &str, body: &str) -> bool {
    match lang {
        Language::Rust | Language::Java => kind == "line_comment",
        Language::Swift => kind == "comment",
        Language::Lua => body.starts_with("--") && !is_lua_long_bracket(body),
        Language::Python | Language::Shell | Language::Yaml | Language::Toml => true,
        _ => body.starts_with("//"),
    }
}

/// `--[[`, `--[=[`, `--[==[` ...
fn is_lua_long_bracket(text: &str) -> bool {
    let Some(rest) = text.strip_prefix("--[") else {
        return false;
    };
    let equals = rest.bytes().take_while(|&b| b == b'=').count();
    rest.as_bytes().get(equals) == Some(&b'[')
}
