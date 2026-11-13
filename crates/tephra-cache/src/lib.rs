//! `tephra-cache`: the SQLite persistent cache layer for `tephrad`.
//!
//! Owns `cache.db` (WAL mode), its schema (`sources`, `units`, `links`,
//! `tags`, `blocks`, `headings`, `symbols`, the `block_fts` FTS5 index,
//! `scan_state`, `schema_version`), the single drop-and-rebuild
//! path triggered by a schema-version mismatch (F2 — no migration
//! scripts), and per-source status bookkeeping
//! (`active`/`stale`/`offline`) so an unmounted or offline source's
//! cached index is preserved rather than wiped (E2).
//!
//! Out of scope for this crate: AST parsing, filesystem watching, RPC
//! transport, and CLI — those are later crates/PRs that call into the
//! typed helpers exposed here.

mod cache;
mod fts;
mod in_file;
mod listing;
mod schema;
mod search;
mod sources;
mod units;

use std::fmt;

pub use cache::{Cache, FaultInjector};
pub use fts::{
    make_snippet, BlockHit, NewFtsRow, FTS_ROWS_PER_UNIT, MAX_RANK_CANDIDATES, MIN_TRIGRAM_CHARS,
};
pub use in_file::{InFileCandidate, InFileScope};
pub use listing::{
    compare_entries, compare_names, FolderEntry, FolderListing, FolderState, FOLDER_KIND,
};
pub use schema::KindInferrer;
pub use search::{compare_hits, file_stem, resolve_title, NameCandidate, NameHit, NAME_SEARCH_SQL};
pub use sources::{validate_transition, InvalidTransition, SourceRow, SourceStatus};
pub use units::{
    BlockOccurrence, BlockRow, HeadingRow, LinkRow, NewBlock, NewHeading, NewLink, NewSymbol,
    NewTag, NewUnit, SymbolRow, TagRow, UnitRow, UnitWithSourceStatus,
};

/// The schema version this build of the crate expects. Bumping this
/// constant is the *only* way the on-disk schema ever changes shape:
/// on next open, [`Cache::open`] compares the stored `schema_version`
/// row against this value and, on mismatch, drops and recreates
/// every core table (never `sources`, `schema_version` or
/// `scan_state`) — see [`Cache::rebuild`]. There is no
/// migration-file mechanism or version-ladder anywhere in this crate
/// (F2).
///
/// Version 2 added `units.kind`/`size`/`too_large`/`needs_rescan`,
/// the `headings` and `symbols` tables (a symbol's first line in
/// `symbols.line`), `blocks.end_line` and `blocks.symbol_id` (the
/// symbol that owns a code block ID) and the `block_fts` trigram FTS5
/// index. Builds of v2 before the last two have another table shape
/// at the same version number; the open path spots that and rebuilds
/// (see `schema::core_shape_is_stale`).
pub const CURRENT_SCHEMA_VERSION: i64 = 2;

#[derive(Debug)]
pub enum CacheError {
    Io(std::io::Error),
    Sqlite(rusqlite::Error),
    /// [`Cache::rebuild`]'s test-only fault-injection seam forced a
    /// failure between dropping and recreating the core tables, to
    /// verify the rebuild transaction rolls back cleanly rather than
    /// leaving a partially-dropped schema (F2).
    FaultInjected,
    /// An attempted source-status transition is not one of the
    /// documented legal edges; see [`validate_transition`].
    InvalidTransition(SourceStatus, SourceStatus),
    /// No `sources` row exists with the given name.
    SourceNotFound(String),
    /// A `sources.status` value on disk is not one of
    /// `active`/`stale`/`offline` — the database was modified outside
    /// this crate's typed API.
    CorruptStatus(String),
    /// `register_source` was called with a `name` that already exists.
    DuplicateSource(String),
    /// [`Cache::table_row_count`] was called with a table name outside
    /// the closed allow-list of real tables this crate owns. Returned
    /// as an error rather than letting the name reach SQL at all — see
    /// `units::count_rows`'s allow-list check.
    InvalidTableName(String),
    /// This SQLite build has no FTS5 with the `trigram` tokenizer, which
    /// the `^^` search needs. Carries a message saying what is missing.
    Fts5Unavailable(String),
}

impl fmt::Display for CacheError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CacheError::Io(e) => write!(f, "{e}"),
            CacheError::Sqlite(e) => write!(f, "{e}"),
            CacheError::FaultInjected => {
                write!(
                    f,
                    "rebuild failed: fault injected after table drop (test-only)"
                )
            }
            CacheError::InvalidTransition(from, to) => {
                write!(f, "illegal source status transition: {from} -> {to}")
            }
            CacheError::SourceNotFound(name) => write!(f, "no source named \"{name}\""),
            CacheError::CorruptStatus(raw) => {
                write!(f, "source status \"{raw}\" is not active/stale/offline")
            }
            CacheError::DuplicateSource(name) => {
                write!(f, "a source named \"{name}\" is already registered")
            }
            CacheError::InvalidTableName(name) => {
                write!(f, "\"{name}\" is not a recognized cache table name")
            }
            CacheError::Fts5Unavailable(message) => write!(f, "{message}"),
        }
    }
}

impl std::error::Error for CacheError {}

impl From<std::io::Error> for CacheError {
    fn from(e: std::io::Error) -> Self {
        CacheError::Io(e)
    }
}

impl From<rusqlite::Error> for CacheError {
    fn from(e: rusqlite::Error) -> Self {
        CacheError::Sqlite(e)
    }
}
