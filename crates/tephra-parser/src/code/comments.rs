//! Finding the comment and docstring byte ranges of a parsed code
//! file (G4).
//!
//! Only these ranges are ever scanned for links. Everything else in
//! the file (code, string literals, template literals, raw strings,
//! heredocs, YAML/TOML values, Lua long strings) never gives a link
//! (E1, F5).
//!
//! What counts:
//! - every node the grammar calls a comment ([`Language::is_comment_kind`]),
//!   wherever it sits in the tree, **including inside a tree-sitter
//!   `ERROR` node** (E12: a half-written file still gives its comments).
//!   JSDoc `/** */`, Rust `///` and `//!` are ordinary comment nodes.
//! - for Python, a string that is the first statement of a module,
//!   class or function (a docstring). An f-string or a bytes string is
//!   not a docstring, and neither is a string anywhere else.
//!
//! The walk is iterative (a [`tree_sitter::TreeCursor`], no recursion),
//! so a deeply nested file cannot overflow the stack.

use tree_sitter::{Node, Tree};

use super::languages::Language;

/// A byte range `[start, end)` of the file's content.
pub(crate) type ByteRange = (usize, usize);

/// Every comment/docstring range of `tree`, sorted by start, never
/// empty, never overlapping. `src` is the exact text `tree` was
/// parsed from.
pub(crate) fn find_ranges(lang: Language, tree: &Tree, src: &str) -> Vec<ByteRange> {
    let mut ranges: Vec<ByteRange> = Vec::new();
    let mut cursor = tree.walk();
    let python = lang == Language::Python;

    'walk: loop {
        let node = cursor.node();
        let mut descend = true;
        if lang.is_comment_kind(node.kind()) {
            if !is_unterminated_heredoc_text(lang, node) {
                if let Some(range) = comment_range(lang, src, node.start_byte(), node.end_byte()) {
                    ranges.push(range);
                }
            }
            // A comment has nothing inside it worth reading.
            descend = false;
        } else if python {
            if let Some(range) = python_docstring_of(node, src) {
                ranges.push(range);
            }
        }

        if descend && cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                continue 'walk;
            }
            if !cursor.goto_parent() {
                break 'walk;
            }
        }
    }

    // A docstring is recorded when its parent is visited, i.e. before
    // a comment that sits between the parent's header and the string.
    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

/// A `#` line of a shell heredoc whose terminator is missing (a file
/// cut off mid-heredoc). The grammar's error recovery reads such a
/// line as a comment, but the shell would read it as heredoc text,
/// which is a string and never gives a link. A comment that follows a
/// `heredoc_body` inside its `heredoc_redirect` can only be that.
fn is_unterminated_heredoc_text(lang: Language, comment: Node<'_>) -> bool {
    if lang != Language::Shell || comment.parent().map(|p| p.kind()) != Some("heredoc_redirect") {
        return false;
    }
    let mut prev = comment.prev_sibling();
    while let Some(sibling) = prev {
        if sibling.kind() == "heredoc_body" {
            return true;
        }
        prev = sibling.prev_sibling();
    }
    false
}

/// The part of a comment node that is scanned for links, or `None`
/// when nothing is left.
///
/// Lua's block comment `--[==[ ... ]==]` keeps its `--[==[` opener out
/// of the range: the opener is `--[[`, which would otherwise read as
/// the start of a `[[` wikilink (E1). The closer stays in: Lua ends a
/// long bracket at the first `]]`, so in `--[[ see [[A]] ]]` that `]]`
/// closes both the comment and the link, and the comment node ends
/// right there.
fn comment_range(lang: Language, src: &str, start: usize, end: usize) -> Option<ByteRange> {
    let (mut start, end) = (start, end.min(src.len()));
    if lang == Language::Lua {
        if let Some(opener) = src.get(start..end).and_then(lua_long_bracket_len) {
            start += opener;
        }
    }
    (start < end && src.is_char_boundary(start) && src.is_char_boundary(end))
        .then_some((start, end))
}

/// Length in bytes of a Lua block comment opener (`--[[`, `--[=[`,
/// `--[==[` ...) at the start of `text`, or `None` when `text` is a
/// line comment.
fn lua_long_bracket_len(text: &str) -> Option<usize> {
    let rest = text.strip_prefix("--[")?;
    let equals = rest.bytes().take_while(|&b| b == b'=').count();
    // `--[` + the `=`s + the second `[`.
    (rest.as_bytes().get(equals) == Some(&b'[')).then_some(3 + equals + 1)
}

/// If `node` is a Python module, class or function definition whose
/// first statement is a (non-f, non-bytes) string, that string's range.
fn python_docstring_of(node: Node<'_>, src: &str) -> Option<ByteRange> {
    let body = match node.kind() {
        "module" => node,
        "class_definition" | "function_definition" => node.child_by_field_name("body")?,
        _ => return None,
    };
    // The first statement, skipping comments (they are extras that
    // can sit between a header and its first statement).
    let mut first = None;
    for i in 0..body.named_child_count() {
        let child = body.named_child(i as u32)?;
        if child.kind() != "comment" {
            first = Some(child);
            break;
        }
    }
    let statement = first?;
    if statement.kind() != "expression_statement" || statement.named_child_count() != 1 {
        return None;
    }
    let string = statement.named_child(0)?;
    if string.kind() != "string" {
        return None;
    }
    let text = src.get(string.start_byte()..string.end_byte())?;
    let prefix: String = text
        .chars()
        .take_while(|c| *c != '"' && *c != '\'')
        .collect();
    // Only `r` and `u` prefixes keep a string a docstring: `f` makes
    // it a formatted string and `b` a bytes literal.
    if !prefix.chars().all(|c| matches!(c, 'r' | 'R' | 'u' | 'U')) {
        return None;
    }
    Some((string.start_byte(), string.end_byte()))
}
