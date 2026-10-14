//! `tephra-parser`: a pure, synchronous Markdown AST parser.
//!
//! [`parse_unit`] turns a path and a string of file content into a
//! [`ParsedUnit`] in a single pass: YAML frontmatter, a heading tree,
//! `[[wikilinks]]` (including the federated `[[@source:path]]`
//! prefix form), `[markdown](links)`, `#tags`, and `^block` anchors,
//! plus a stable content hash.
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
mod diagnostics;
mod fences;
mod frontmatter;
mod hash;
mod line_index;
mod markdown_pass;
mod tags;
mod wikilinks;

use std::path::{Path, PathBuf};

use serde::Serialize;

pub use blocks::BlockAnchor;
pub use diagnostics::{Diagnostic, DiagnosticKind};
pub use frontmatter::Frontmatter;
pub use markdown_pass::{Heading, MarkdownLink};
pub use tags::Tag;
pub use wikilinks::WikiLink;

pub use tephra_cache::{NewBlock, NewLink, NewTag, NewUnit};

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
    pub blocks: Vec<BlockAnchor>,
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

    /// One `tephra-cache` `NewBlock` per extracted block anchor.
    /// `to_unit_id` resolution is out of scope for this crate — every
    /// row here only ever describes *this* unit's own anchors.
    pub fn to_new_blocks(&self, unit_id: i64) -> Vec<NewBlock<'_>> {
        self.blocks
            .iter()
            .map(|b| NewBlock {
                unit_id,
                block_id: &b.block_id,
                line: b.line as i64,
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
    let mut diagnostics = Vec::new();

    let (frontmatter, body, body_start_line) = frontmatter::extract(content, &mut diagnostics);

    let pass = markdown_pass::run(body, body_start_line, &mut diagnostics);
    let (wikilinks, wikilink_spans) = wikilinks::scan(
        body,
        body_start_line,
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

    ParsedUnit {
        path: path.to_path_buf(),
        content_hash: hash::content_hash(content),
        frontmatter,
        headings: pass.headings,
        wikilinks,
        markdown_links: pass.links,
        tags,
        blocks,
        diagnostics,
    }
}

// Compile-time assertion that `ParsedUnit` is `Send + Sync`, as
// required for PR-4's parallel initial scan.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ParsedUnit>();
};
