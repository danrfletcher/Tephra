//! `tephra-parser`: a pure, synchronous prose parser.
//!
//! [`parse_unit`] turns a path and a string of file content into a
//! [`ParsedUnit`] in a single pass: YAML frontmatter, a heading tree,
//! `[[wikilinks]]` (including the federated `[[@source:path]]`
//! prefix form), `[markdown](links)`, `#tags`, `^block` anchors and
//! paragraph blocks with their line ranges, plus a stable content
//! hash.
//!
//! The path's extension picks the rules ([`ProseFormat`]): Markdown
//! (`.md`, `.markdown`) gets all of the above; `.txt`, `.org`, `.rst`
//! and `.adoc` get links, `^id` anchors and paragraphs by the same
//! link/anchor code, headings by their own native rules (none for
//! `.txt`), no frontmatter and no tags. See the [`prose`] module.
//!
//! [`parse_code_unit`] is the code-file counterpart (G4): it parses a
//! file in one of the 14 bundled tree-sitter grammars ([`Language`])
//! and reads `[[wikilinks]]` and `[text](target)` links only inside
//! comments and docstrings, through the same wikilink and Markdown
//! scanners. It also records the file's symbols (G4) and the `^id`
//! comments attached to them (G13), and no headings, tags or
//! paragraphs. See the [`code`] module.
//!
//! This crate does no I/O, owns no watcher, and never opens SQLite or
//! resolves a link against a registry (those are PR-4/PR-5). Its row
//! types — [`tephra_cache::NewTag`], [`tephra_cache::NewLink`],
//! [`tephra_cache::NewBlock`], [`tephra_cache::NewUnit`] — are
//! imported directly from `tephra-cache` rather than redeclared here,
//! via the `to_new_*` conversion methods below, so a caller that has
//! since inserted the owning `units` row and knows its `unit_id` can
//! turn a [`ParsedUnit`] straight into `tephra-cache` insert calls
//! without this crate re-describing that schema.
//!
//! Regenerate the golden snapshots under `tests/fixtures/units/` with
//! `UPDATE_SNAPSHOTS=1 cargo test -p tephra-parser` — see this
//! crate's `tests/integration/parse_unit_fixtures.rs`.

mod blocks;
mod code;
mod diagnostics;
mod documents;
mod fences;
mod frontmatter;
mod hash;
mod line_index;
mod markdown_pass;
mod prose;
mod tags;
mod wikilinks;

use std::path::{Path, PathBuf};

use serde::Serialize;

pub use blocks::{BlockAnchor, Paragraph};
pub use code::{
    parse_code_unit, parse_code_unit_within, Language, Symbol, CODE_PARSE_BUDGET, EXTENSIONS,
    MAX_CODE_PARSE_BYTES,
};
pub use diagnostics::{Diagnostic, DiagnosticKind};
pub use documents::{
    parse_document_bytes, DocHeading, DocLocation, DocMarkdownLink, DocParagraph, DocWikiLink,
    DocumentError, DocumentFormat, ParsedDocument, MAX_PART_BYTES,
};
pub use frontmatter::Frontmatter;
pub use markdown_pass::{Heading, MarkdownLink};
pub use prose::ProseFormat;
pub use tags::Tag;
pub use wikilinks::WikiLink;

pub use tephra_cache::{NewBlock, NewHeading, NewLink, NewSymbol, NewTag, NewUnit};

/// `LinkRow`/`NewLink`'s `kind` value for a row derived from a
/// `[[wikilink]]`.
pub const LINK_KIND_WIKILINK: &str = "wikilink";
/// `LinkRow`/`NewLink`'s `kind` value for a row derived from a
/// `[markdown](link)`.
pub const LINK_KIND_MARKDOWN: &str = "markdown";

/// Parses a wikilink's raw inner text — the content between `[[` and
/// `]]`, exactly the string stored in `WikiLink::raw` / a wikilink-kind
/// `LinkRow::to_raw` — into its structured [`WikiLink`] form, with no
/// surrounding file or line context. The returned `line` is always
/// `0`: there is no real file position to report, and a caller
/// resolving an already-stored link (`tephra-graph`'s resolver, PR-5)
/// never needs one. This is the one supported way to re-derive a
/// stored link's `@source` prefix / target / anchor / alias outside
/// this crate — the grammar lives here (G3) and must not be
/// redefined a second time elsewhere, where it could drift out of
/// sync with [`parse_unit`]'s own wikilink scan.
pub fn parse_wikilink_inner(raw: &str) -> WikiLink {
    wikilinks::parse_inner_standalone(raw, 0)
}

/// Practical size cap this parser is bounded-time/bounded-memory
/// tested against (see `tests/unit/large_file.rs`). `parse_unit` does
/// not refuse to parse a larger file — there is no hard limit — but
/// above this size parse time and memory grow roughly linearly with
/// input size, so a caller indexing an initial scan (PR-4) may want
/// to warn, or deprioritize, a file above this threshold rather than
/// block its scan on it.
pub const RECOMMENDED_MAX_FILE_BYTES: usize = 10 * 1024 * 1024;

/// A single note or repo file's structured parse result.
///
/// Every field is a pure function of `path` + the file's content: two
/// calls with byte-identical content (regardless of `path`, for
/// `content_hash`) always produce byte-identical results, including
/// the hash — see `tests/unit/determinism.rs`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParsedUnit {
    pub path: PathBuf,
    /// Hex-encoded SHA-256 of the raw content, independent of `path`.
    pub content_hash: String,
    pub frontmatter: Option<Frontmatter>,
    pub headings: Vec<Heading>,
    pub wikilinks: Vec<WikiLink>,
    pub markdown_links: Vec<MarkdownLink>,
    pub tags: Vec<Tag>,
    /// Every `^id` occurrence, in file order, none merged or dropped
    /// (a repeated ID is one entry per occurrence, each with its own
    /// line, plus a [`DiagnosticKind::DuplicateBlockId`] diagnostic).
    pub blocks: Vec<BlockAnchor>,
    /// Every paragraph block, in file order, with its line range and
    /// the `^id` ending its last line, if any.
    pub paragraphs: Vec<Paragraph>,
    /// The symbols of a code file (G4), in file order, parents before
    /// their children. Always empty for prose.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub symbols: Vec<Symbol>,
    /// Structured, non-fatal problems found while parsing (malformed
    /// frontmatter, empty/unterminated links, duplicate block ids).
    /// Diagnostics never replace or suppress whatever else could
    /// still be extracted from the rest of the file.
    pub diagnostics: Vec<Diagnostic>,
}

impl ParsedUnit {
    /// This unit's row for `tephra-cache`'s `units` table, given the
    /// `source_id`, vault/repo-relative path, and mtime a caller
    /// resolves separately (none of which this crate knows).
    pub fn to_new_unit<'a>(
        &'a self,
        source_id: i64,
        rel_path: &'a str,
        mtime: Option<i64>,
    ) -> NewUnit<'a> {
        NewUnit {
            source_id,
            rel_path,
            mtime,
            frontmatter_json: self.frontmatter.as_ref().and_then(|f| f.json.as_deref()),
            // `prose` for a unit from `parse_unit`. The caller
            // overwrites `size` with the file's real length, and
            // `kind` with `code` for a unit from `parse_code_unit`.
            kind: "prose",
            size: None,
            too_large: false,
        }
    }

    /// One `tephra-cache` `NewTag` per extracted tag, borrowing from
    /// `self`, once the caller knows this unit's row id.
    pub fn to_new_tags(&self, unit_id: i64) -> Vec<NewTag<'_>> {
        self.tags
            .iter()
            .map(|t| NewTag {
                unit_id,
                tag: &t.tag,
            })
            .collect()
    }

    /// The `tephra-cache` `blocks` rows of this unit, in line order:
    /// one per paragraph (`line`..`end_line`, `block_id` set when a
    /// `^id` ends its last line) plus one per `^id` occurrence that
    /// is not at the end of a paragraph (a heading line, a table row,
    /// the middle of a paragraph), stored as a one-line block. Every
    /// occurrence of a repeated ID is its own row, never merged.
    /// `to_unit_id` resolution is out of scope for this crate — every
    /// row here only ever describes *this* unit's own blocks.
    ///
    /// Every row has `symbol_id: None`. For a code unit use
    /// [`ParsedUnit::to_new_blocks_with_symbols`], which can name the
    /// owning symbol row.
    pub fn to_new_blocks(&self, unit_id: i64) -> Vec<NewBlock<'_>> {
        self.to_new_blocks_with_symbols(unit_id, &[])
    }

    /// [`ParsedUnit::to_new_blocks`] for a unit whose symbols have
    /// been inserted: `symbol_ids[i]` is the `symbols` row id of
    /// `self.symbols[i]` (see [`ParsedUnit::to_new_symbols`]). A code
    /// `^id` that belongs to symbol `i` (G13) becomes a row on that
    /// symbol's first line, spanning the symbol's range, with
    /// `symbol_id` set. An ID with no symbol is a one-line row with
    /// `symbol_id` `None`. An index `symbol_ids` has no entry for also
    /// gives `None`.
    pub fn to_new_blocks_with_symbols(
        &self,
        unit_id: i64,
        symbol_ids: &[i64],
    ) -> Vec<NewBlock<'_>> {
        let attached: std::collections::HashSet<(&str, usize)> = self
            .paragraphs
            .iter()
            .filter_map(|p| p.block_id.as_deref().map(|id| (id, p.end_line)))
            .collect();
        let mut rows: Vec<NewBlock<'_>> = self
            .paragraphs
            .iter()
            .map(|p| NewBlock {
                unit_id,
                block_id: p.block_id.as_deref(),
                line: p.start_line as i64,
                end_line: Some(p.end_line as i64),
                symbol_id: None,
            })
            .collect();
        rows.extend(
            self.blocks
                .iter()
                .filter(|b| !attached.contains(&(b.block_id.as_str(), b.line)))
                .map(|b| {
                    let owner = b.symbol.and_then(|i| Some((self.symbols.get(i)?, i)));
                    NewBlock {
                        unit_id,
                        block_id: Some(&b.block_id),
                        line: b.line as i64,
                        end_line: Some(owner.map_or(b.line, |(s, _)| s.end_line) as i64),
                        symbol_id: owner.and_then(|(_, i)| symbol_ids.get(i).copied()),
                    }
                }),
        );
        // Stable: paragraphs and loose anchors interleave by line.
        rows.sort_by_key(|r| r.line);
        rows
    }

    /// One `tephra-cache` `NewSymbol` per symbol of a code file, in
    /// file order (`symbols[i]` is `self.symbols[i]`), once the caller
    /// knows this unit's row id. Empty for prose.
    pub fn to_new_symbols(&self, unit_id: i64) -> Vec<NewSymbol<'_>> {
        self.symbols
            .iter()
            .map(|s| NewSymbol {
                unit_id,
                kind: s.kind,
                name: &s.name,
                parent: s.parent.as_deref(),
                line: s.line as i64,
                start_line: s.start_line as i64,
                end_line: s.end_line as i64,
            })
            .collect()
    }

    /// One `tephra-cache` `NewHeading` per heading, in document
    /// order. The listing's prose title falls back to the first one.
    pub fn to_new_headings(&self, unit_id: i64) -> Vec<NewHeading<'_>> {
        self.headings
            .iter()
            .map(|h| NewHeading {
                unit_id,
                level: i64::from(h.level),
                text: &h.text,
                line: h.line as i64,
            })
            .collect()
    }

    /// One `tephra-cache` `NewLink` per wikilink and markdown link
    /// found in this unit, with `to_unit_id` left `None` — resolving
    /// a link against the registry (`to_unit_id`) is PR-5's job, not
    /// this crate's. `to_raw` is the wikilink's full raw inner text
    /// (so source-prefix/anchor/alias survive for later resolution)
    /// or the markdown link's raw destination.
    pub fn to_new_links(&self, from_unit_id: i64) -> Vec<NewLink<'_>> {
        let mut out: Vec<NewLink<'_>> = self
            .wikilinks
            .iter()
            .map(|w| NewLink {
                from_unit_id,
                to_raw: &w.raw,
                to_unit_id: None,
                kind: LINK_KIND_WIKILINK,
            })
            .collect();
        out.extend(self.markdown_links.iter().map(|m| NewLink {
            from_unit_id,
            to_raw: &m.target,
            to_unit_id: None,
            kind: LINK_KIND_MARKDOWN,
        }));
        out
    }
}

/// A non-UTF-8 byte sequence was passed to [`parse_unit_bytes`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidUtf8 {
    /// Byte offset of the first invalid byte.
    pub valid_up_to: usize,
}

impl std::fmt::Display for InvalidUtf8 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "content is not valid UTF-8 (valid up to byte {})",
            self.valid_up_to
        )
    }
}

impl std::error::Error for InvalidUtf8 {}

/// Validates `bytes` as UTF-8 and delegates to [`parse_unit`].
/// [`parse_unit`] itself takes `&str` because Rust's type system
/// already guarantees that is valid UTF-8; this entry point exists so
/// raw bytes read straight off disk (which might not be) produce a
/// typed error rather than a panic or a lossily-mangled parse.
pub fn parse_unit_bytes(path: &Path, bytes: &[u8]) -> Result<ParsedUnit, InvalidUtf8> {
    let content = std::str::from_utf8(bytes).map_err(|e| InvalidUtf8 {
        valid_up_to: e.valid_up_to(),
    })?;
    Ok(parse_unit(path, content))
}

/// Parses `content` (the file at `path`) into a [`ParsedUnit`] in a
/// single pass. Pure and side-effect free: no disk access, no
/// filesystem watching, no SQLite, no link resolution. `Send + Sync`
/// and safe to call concurrently on the same or different inputs from
/// multiple threads (see `tests/unit/determinism.rs`) since it reads
/// only its own arguments and allocates only local state.
pub fn parse_unit(path: &Path, content: &str) -> ParsedUnit {
    let format = ProseFormat::from_path(path);
    if format.is_markdown() {
        parse_markdown(path, content)
    } else {
        parse_native(path, content, format)
    }
}

fn parse_markdown(path: &Path, content: &str) -> ParsedUnit {
    let mut diagnostics = Vec::new();

    let (frontmatter, body, body_start_line, body_start_byte) =
        frontmatter::extract(content, &mut diagnostics);

    let pass = markdown_pass::run(body, body_start_line, body_start_byte, &mut diagnostics);
    let (wikilinks, wikilink_spans) = wikilinks::scan(
        body,
        body_start_line,
        body_start_byte,
        &pass.excluded_ranges,
        &mut diagnostics,
    );
    let tags = tags::scan(
        body,
        body_start_line,
        &pass.excluded_ranges,
        &wikilink_spans,
        &pass.link_destination_ranges,
    );
    let blocks = blocks::scan(
        body,
        body_start_line,
        &pass.excluded_ranges,
        &mut diagnostics,
    );
    let mut paragraphs = pass.paragraphs;
    blocks::attach_ids(&mut paragraphs, &blocks);

    ParsedUnit {
        path: path.to_path_buf(),
        content_hash: hash::content_hash(content),
        frontmatter,
        headings: pass.headings,
        wikilinks,
        markdown_links: pass.links,
        tags,
        blocks,
        paragraphs,
        symbols: Vec::new(),
        diagnostics,
    }
}

/// The txt/org/rst/adoc path (G3): no frontmatter and no tags; native
/// headings; links, anchors and paragraphs by the same scanners
/// Markdown uses, with the format's literal regions excluded.
fn parse_native(path: &Path, content: &str, format: ProseFormat) -> ParsedUnit {
    let mut diagnostics = Vec::new();

    // A leading BOM is not text. Offsets stay relative to `content`.
    let bom_len = if content.starts_with('\u{FEFF}') {
        '\u{FEFF}'.len_utf8()
    } else {
        0
    };
    let body = &content[bom_len..];

    let lines = prose::split_lines(body);
    let literal = prose::literal_mask(format, &lines);
    let native = prose::headings::find(format, &lines, &literal);

    // Literal regions as byte ranges, one per run of masked lines.
    let mut excluded_ranges: Vec<(usize, usize)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if literal[i] {
            match excluded_ranges.last_mut() {
                Some(last) if i > 0 && literal[i - 1] => last.1 = line.next,
                _ => excluded_ranges.push((line.start, line.next)),
            }
        }
    }

    let (mut wikilinks, _spans) =
        wikilinks::scan(body, 1, bom_len, &excluded_ranges, &mut diagnostics);
    if format == ProseFormat::Org {
        // Org's own `[[target][label]]` / `[[file:...]]` links share
        // the wikilink brackets; they are not wikilinks.
        wikilinks.retain(|w| !prose::is_native_org_link(&w.raw));
    }

    // Markdown links: the Markdown pass, run over each block of
    // consecutive non-blank, non-literal lines on its own, starting at
    // the first non-blank character (so source indentation never
    // reads as an indented code block).
    let mut markdown_links = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        if literal[i] || lines[i].is_blank() {
            i += 1;
            continue;
        }
        let first = i;
        while i + 1 < lines.len() && !literal[i + 1] && !lines[i + 1].is_blank() {
            i += 1;
        }
        let last = i;
        i += 1;
        let chunk_start =
            lines[first].start + (lines[first].text.len() - lines[first].text.trim_start().len());
        let chunk = &body[chunk_start..lines[last].end];
        if !chunk.contains('[') && !chunk.contains('<') {
            continue;
        }
        let pass = markdown_pass::run(chunk, first + 1, bom_len + chunk_start, &mut diagnostics);
        markdown_links.extend(pass.links);
    }

    let blocks = blocks::scan(body, 1, &excluded_ranges, &mut diagnostics);

    // Paragraphs: runs of lines that are not blank, literal, a
    // heading's own lines or (adoc) a block delimiter.
    let breaks = |idx: usize| {
        literal[idx]
            || native.heading_lines[idx]
            || lines[idx].is_blank()
            || (format == ProseFormat::Adoc && prose::is_adoc_block_delimiter(lines[idx].text))
            || prose::is_transition(format, &lines, idx)
    };
    let mut paragraphs: Vec<Paragraph> = Vec::new();
    let mut idx = 0usize;
    while idx < lines.len() {
        if breaks(idx) {
            idx += 1;
            continue;
        }
        let first = idx;
        while idx + 1 < lines.len() && !breaks(idx + 1) {
            idx += 1;
        }
        paragraphs.push(Paragraph {
            start_line: first + 1,
            end_line: idx + 1,
            block_id: None,
        });
        idx += 1;
    }
    blocks::attach_ids(&mut paragraphs, &blocks);

    ParsedUnit {
        path: path.to_path_buf(),
        content_hash: hash::content_hash(content),
        frontmatter: None,
        headings: native.headings,
        wikilinks,
        markdown_links,
        tags: Vec::new(),
        blocks,
        paragraphs,
        symbols: Vec::new(),
        diagnostics,
    }
}

// Compile-time assertion that `ParsedUnit` is `Send + Sync`, as
// required for PR-4's parallel initial scan.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ParsedUnit>();
};
