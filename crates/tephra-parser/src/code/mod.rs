//! Links from code files (G4): `[[wikilinks]]` and `[text](target)`
//! links read **only** inside comments and docstrings.
//!
//! A code file is parsed with its bundled tree-sitter grammar
//! ([`Language`]); [`comments::find_ranges`] finds the comment and
//! docstring byte ranges; each range is then scanned with the same
//! wikilink grammar ([`crate::wikilinks::scan`]) and the same Markdown
//! pass ([`crate::markdown_pass::run`]) that prose uses, so the
//! `@source:` prefix, `#anchor`, `|alias` and same-line-close rules
//! stay defined in one place. The links come out as ordinary
//! [`ParsedUnit`] `wikilinks` / `markdown_links`, so
//! `tephra-graph` resolves them exactly like a note's.
//!
//! Why each range is scanned on its own, rather than scanning the
//! whole file with the complement of the ranges excluded: the
//! wikilink scan closes `[[` at the first `]]` *on the same line*, and
//! on a line such as `/* [[a */ b]]` that `]]` is code, outside the
//! comment. A per-range scan cannot cross a comment boundary.
//!
//! The same parse gives the file's **symbols** and **block IDs**
//! ([`symbols`]): functions, classes, methods and the like with their
//! kind, name, parent, first line and line range, and every `^id`
//! comment attached to the symbol that owns it (G13). They come out as
//! [`ParsedUnit::symbols`] and [`ParsedUnit::blocks`].
//!
//! The same comment ranges are grouped into **comment blocks**
//! ([`CommentBlock`]: a run of consecutive line comments, or one block
//! comment) for the `^^` full-text index (G10), and [`split_identifier`]
//! gives a symbol name's words for it.
//!
//! What is *not* done here, on purpose: no Markdown fence exclusion
//! around a wikilink (the comment boundary is the only filter), no
//! headings, tags or paragraphs, no name-based `#anchor` resolution to
//! a symbol (F3), no writing of an ID into a file (PR-12.F1) and no
//! I/O.
//!
//! A half-written file still gives what parses (E12), and a parse is
//! bounded in time and in memory:
//!
//! - a file with syntax errors still gives every symbol and link
//!   tree-sitter recovered, plus a [`DiagnosticKind::SyntaxErrors`]
//!   diagnostic;
//! - past [`CODE_PARSE_BUDGET`] the parse is abandoned and the unit
//!   carries no links or symbols and a [`DiagnosticKind::ParseTimeout`]
//!   diagnostic for the caller to log;
//! - a file larger than [`MAX_CODE_PARSE_BYTES`] is never handed to
//!   tree-sitter at all (its tree costs roughly 100 times the file's
//!   size in memory, which a time budget does not bound), and the unit
//!   carries no links or symbols and a
//!   [`DiagnosticKind::ParseSkippedTooLarge`] diagnostic.

mod comments;
mod index;
mod languages;
mod symbols;

use std::ops::ControlFlow;
use std::path::Path;
use std::time::{Duration, Instant};

pub use index::{split_identifier, CommentBlock};
pub use languages::{Language, EXTENSIONS};
pub use symbols::Symbol;

use crate::diagnostics::{Diagnostic, DiagnosticKind};
use crate::line_index::LineIndex;
use crate::{hash, markdown_pass, prose, wikilinks, ParsedUnit};

/// How long one file's parse may run before it is abandoned.
pub const CODE_PARSE_BUDGET: Duration = Duration::from_secs(5);

/// The largest code file (in bytes) that is parsed. tree-sitter builds
/// the whole syntax tree in memory, measured at about 75 to 95 times
/// the file's size (a 15 MB `.py` peaked at 1.4 GB), and a time budget
/// does not bound that. Above this size the parse is skipped before it
/// starts: the unit has no links and a
/// [`DiagnosticKind::ParseSkippedTooLarge`] diagnostic. 2 MiB is far
/// above hand-written source (a few hundred KB at the very most) and
/// keeps one parse's peak near 200 MB.
pub const MAX_CODE_PARSE_BYTES: usize = 2 * 1024 * 1024;

/// Parses the code file `content` at `path`, written in `lang`, into a
/// [`ParsedUnit`] holding its comment/docstring links, symbols and
/// block IDs. Pure,
/// `Send + Sync`, never panics on malformed input.
pub fn parse_code_unit(path: &Path, lang: Language, content: &str) -> ParsedUnit {
    parse_code_unit_within(path, lang, content, CODE_PARSE_BUDGET)
}

/// [`parse_code_unit`] with an explicit time budget. A parse that
/// exceeds `budget` returns a unit with no links and a
/// [`DiagnosticKind::ParseTimeout`] diagnostic.
pub fn parse_code_unit_within(
    path: &Path,
    lang: Language,
    content: &str,
    budget: Duration,
) -> ParsedUnit {
    let mut diagnostics = Vec::new();
    let mut wikilinks_out = Vec::new();
    let mut markdown_links = Vec::new();
    let mut symbols = Vec::new();
    let mut blocks = Vec::new();
    let mut comment_blocks = Vec::new();

    let parsed = if content.len() > MAX_CODE_PARSE_BYTES {
        Err(ParseFailure::TooLarge)
    } else {
        parse_tree(lang, content, budget)
    };
    match parsed {
        Ok(tree) => {
            let found = symbols::extract(lang, &tree, content, &mut diagnostics);
            symbols = found.symbols;
            blocks = found.blocks;
            let ranges = comments::find_ranges(lang, &tree, content);
            let lines = LineIndex::new(content);
            comment_blocks = index::group_comments(content, &ranges, &lines, &blocks);
            for (start, end) in ranges {
                let Some(text) = content.get(start..end) else {
                    continue;
                };
                let first_line = lines.line_number(start);
                let (links, _spans) =
                    wikilinks::scan(text, first_line, start, &[], &mut diagnostics);
                wikilinks_out.extend(links);
                scan_markdown_links(
                    text,
                    first_line,
                    start,
                    &mut markdown_links,
                    &mut diagnostics,
                );
            }
        }
        Err(ParseFailure::TimedOut) => diagnostics.push(Diagnostic::new(
            DiagnosticKind::ParseTimeout,
            1,
            format!(
                "{} parse abandoned after {} ms; no links recorded",
                lang.name(),
                budget.as_millis()
            ),
        )),
        Err(ParseFailure::TooLarge) => diagnostics.push(Diagnostic::new(
            DiagnosticKind::ParseSkippedTooLarge,
            1,
            format!(
                "{} file of {} bytes is over the {} byte parse limit; not parsed, no links recorded",
                lang.name(),
                content.len(),
                MAX_CODE_PARSE_BYTES
            ),
        )),
        Err(ParseFailure::Failed) => diagnostics.push(Diagnostic::new(
            DiagnosticKind::ParseFailed,
            1,
            format!("{} parser produced no tree; no links recorded", lang.name()),
        )),
    }

    ParsedUnit {
        path: path.to_path_buf(),
        content_hash: hash::content_hash(content),
        frontmatter: None,
        headings: Vec::new(),
        wikilinks: wikilinks_out,
        markdown_links,
        tags: Vec::new(),
        blocks,
        paragraphs: Vec::new(),
        symbols,
        comment_blocks,
        diagnostics,
    }
}

/// `[text](target)` links of one comment, found the way a prose file's
/// are ([`crate::parse_unit`] on a `.txt`): the Markdown pass runs on
/// each run of consecutive non-blank lines, starting at its first
/// non-blank character so the comment's own indentation never reads
/// as an indented code block.
pub(crate) fn scan_markdown_links(
    text: &str,
    first_line: usize,
    start_byte: usize,
    out: &mut Vec<crate::MarkdownLink>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    if !text.contains('[') && !text.contains('<') {
        return;
    }
    let lines = prose::split_lines(text);
    let mut i = 0usize;
    while i < lines.len() {
        if lines[i].is_blank() {
            i += 1;
            continue;
        }
        let first = i;
        while i + 1 < lines.len() && !lines[i + 1].is_blank() {
            i += 1;
        }
        let last = i;
        i += 1;
        let chunk_start =
            lines[first].start + (lines[first].text.len() - lines[first].text.trim_start().len());
        let chunk = &text[chunk_start..lines[last].end];
        if !chunk.contains('[') && !chunk.contains('<') {
            continue;
        }
        let pass = markdown_pass::run(
            chunk,
            first_line + first,
            start_byte + chunk_start,
            diagnostics,
        );
        out.extend(pass.links);
    }
}

enum ParseFailure {
    TooLarge,
    TimedOut,
    Failed,
}

/// Parses `content` with `lang`'s grammar, abandoning the parse once
/// `budget` has elapsed.
fn parse_tree(
    lang: Language,
    content: &str,
    budget: Duration,
) -> Result<tree_sitter::Tree, ParseFailure> {
    let mut parser = tree_sitter::Parser::new();
    parser
        .set_language(&lang.grammar())
        .map_err(|_| ParseFailure::Failed)?;

    let bytes = content.as_bytes();
    let deadline = Instant::now() + budget;
    let timed_out = std::cell::Cell::new(false);
    let mut progress = |_: &tree_sitter::ParseState| {
        if Instant::now() >= deadline {
            timed_out.set(true);
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    };
    let options = tree_sitter::ParseOptions::new().progress_callback(&mut progress);
    let tree = parser.parse_with_options(
        &mut |offset, _| bytes.get(offset..).unwrap_or(&[]),
        None,
        Some(options),
    );
    match tree {
        Some(tree) => Ok(tree),
        None if timed_out.get() => Err(ParseFailure::TimedOut),
        None => Err(ParseFailure::Failed),
    }
}

// Compile-time assertion that `parse_code_unit` and the types it
// shares with the scan are `Send + Sync`, for the parallel scan.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ParsedUnit>();
    assert_send_sync::<Language>();
    let _ = parse_code_unit;
};
