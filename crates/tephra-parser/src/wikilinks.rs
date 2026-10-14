//! `[[wikilink]]` scanning.
//!
//! Wikilinks are not part of CommonMark, so this is a hand-rolled
//! scan over the raw body text rather than a `pulldown-cmark` pass.
//! A wikilink is always resolved within a single line: `[[` opens it
//! and the first `]]` on the *same* line closes it; an unclosed `[[`
//! at end-of-line (or end-of-file) is flagged rather than left to
//! swallow the rest of the document.
//!
//! Inner grammar, left-to-right: an optional `@source-prefix:` before
//! the target, then the target itself, then an optional `#anchor`
//! (an anchor starting with `^` denotes a block anchor rather than a
//! heading — callers can tell the two apart by that leading `^`),
//! then an optional `|alias`. None of these fields are resolved
//! against any registry here (G3) — they are recorded verbatim.

use serde::Serialize;

use crate::diagnostics::{Diagnostic, DiagnosticKind};
use crate::fences::FenceCursor;
use crate::line_index::LineIndex;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WikiLink {
    /// The `@source` prefix, without the leading `@` or trailing `:`,
    /// e.g. `"atlas-repo"` for `[[@atlas-repo:README]]`.
    pub source_prefix: Option<String>,
    /// The link target, e.g. `"README"`. Empty when the link is a
    /// same-file anchor-only link such as `[[#Heading]]`.
    pub target: String,
    /// Text after `#`, verbatim (so `"^block-id"` denotes a block
    /// anchor and anything else denotes a heading anchor).
    pub anchor: Option<String>,
    /// Text after `|`.
    pub alias: Option<String>,
    /// The full raw text between `[[` and `]]`.
    pub raw: String,
    /// 1-indexed line number, absolute within the original file.
    pub line: usize,
    /// Absolute byte range `[start, end)` within the original file's
    /// content — the exact bytes `"[[" + raw + "]]"` occupies (R1
    /// review finding): `tephrad::refactor` rewrites a link by this
    /// exact span rather than a textual search-and-replace, so a
    /// textually-identical occurrence elsewhere (e.g. the same raw
    /// text inside an inline code span, or on a different line) is
    /// never mistaken for this one. `(0, 0)` for a [`WikiLink`]
    /// produced by [`parse_inner_standalone`] (no real file context
    /// — mirrors `line`'s `0` convention there).
    pub span: (usize, usize),
}

/// Parses a wikilink's raw inner text (the content between `[[` and
/// `]]`) into its structured form, standalone — no surrounding file,
/// no line number. Exposed via [`crate::parse_wikilink_inner`] for a
/// caller (`tephra-graph`'s resolver, PR-5) that has only a stored
/// `WikiLink::raw` / `LinkRow::to_raw` string and needs to re-derive
/// the `@source` prefix / target / anchor / alias it encodes, without
/// this crate's wikilink grammar (G3) being redefined a second time
/// elsewhere and risking drift. `line` on the result is always
/// whatever the caller passes (see [`crate::parse_wikilink_inner`]'s
/// doc comment: `0`, since there is no real file context).
pub(crate) fn parse_inner_standalone(inner: &str, line: usize) -> WikiLink {
    parse_inner(inner, line, (0, 0))
}

/// Scans `body` for `[[wikilinks]]`. Returns the parsed links
/// alongside the full raw byte span (including both pairs of
/// brackets) of every successfully-closed `[[...]]` occurrence, so
/// callers such as the tag scanner can skip over them too (a `#` in
/// `[[Note#Heading]]`'s heading anchor is never a `#tag`).
pub(crate) fn scan(
    body: &str,
    body_start_line: usize,
    body_start_byte: usize,
    excluded_ranges: &[(usize, usize)],
    diagnostics: &mut Vec<Diagnostic>,
) -> (Vec<WikiLink>, Vec<(usize, usize)>) {
    let mut out = Vec::new();
    let mut spans = Vec::new();
    let bytes = body.as_bytes();
    let line_index = LineIndex::new(body);
    let mut cursor = FenceCursor::new(excluded_ranges);
    let mut i = 0usize;
    while i + 1 < bytes.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' && !cursor.contains(i) {
            let open_at = i;
            let line = body_start_line - 1 + line_index.line_number(open_at);
            let content_start = i + 2;
            // Search for "]]" without crossing a newline.
            let mut j = content_start;
            let mut close: Option<usize> = None;
            while j < bytes.len() {
                if bytes[j] == b'\n' {
                    break;
                }
                if j + 1 < bytes.len() && bytes[j] == b']' && bytes[j + 1] == b']' {
                    close = Some(j);
                    break;
                }
                j += 1;
            }
            match close {
                Some(close_at) => {
                    let inner = &body[content_start..close_at];
                    if inner.is_empty() {
                        diagnostics.push(Diagnostic::new(
                            DiagnosticKind::EmptyWikilink,
                            line,
                            "empty wikilink [[]]",
                        ));
                    } else {
                        let span = (body_start_byte + open_at, body_start_byte + close_at + 2);
                        out.push(parse_inner(inner, line, span));
                    }
                    spans.push((open_at, close_at + 2));
                    i = close_at + 2;
                    continue;
                }
                None => {
                    diagnostics.push(Diagnostic::new(
                        DiagnosticKind::UnterminatedWikilink,
                        line,
                        "unterminated wikilink: no closing ]] before end of line",
                    ));
                    i = content_start;
                    continue;
                }
            }
        }
        i += 1;
    }
    (out, spans)
}

fn parse_inner(inner: &str, line: usize, span: (usize, usize)) -> WikiLink {
    let (before_alias, alias) = match inner.split_once('|') {
        Some((a, b)) => (a, Some(b.to_string())),
        None => (inner, None),
    };
    let (before_anchor, anchor) = match before_alias.split_once('#') {
        Some((a, b)) => (a, Some(b.to_string())),
        None => (before_alias, None),
    };
    let (source_prefix, target) = match before_anchor.strip_prefix('@') {
        Some(rest) => match rest.split_once(':') {
            Some((prefix, target)) if !prefix.is_empty() => {
                (Some(prefix.to_string()), target.to_string())
            }
            _ => (None, before_anchor.to_string()),
        },
        None => (None, before_anchor.to_string()),
    };

    WikiLink {
        source_prefix,
        target,
        anchor,
        alias,
        raw: inner.to_string(),
        line,
        span,
    }
}
