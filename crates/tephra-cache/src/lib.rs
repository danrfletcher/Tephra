//! `tephra-cache`: the SQLite persistent cache layer for `tephrad`.
//!
//! Owns `cache.db` (WAL mode), its schema (`sources`, `units`, `links`,
//! `tags`, `blocks`, `schema_version`), the single drop-and-rebuild
//! path triggered by a schema-version mismatch (F2 — no migration
//! scripts), and per-source status bookkeeping
//! (`active`/`stale`/`offline`) so an unmounted or offline source's
//! cached index is preserved rather than wiped (E2).
//!
//! Out of scope for this crate: AST parsing, filesystem watching, RPC
//! transport, and CLI — those are later crates/PRs that call into the
//! typed helpers exposed here.

mod cache;
mod schema;
mod sources;
mod units;

use std::fmt;

pub use cache::{Cache, FaultInjector};
pub use sources::{validate_transition, InvalidTransition, SourceRow, SourceStatus};
pub use units::{
    BlockRow, LinkRow, NewBlock, NewLink, NewTag, NewUnit, TagRow, UnitRow, UnitWithSourceStatus,
};

/// The schema version this build of the crate expects. Bumping this
/// constant is the *only* way the on-disk schema ever changes shape:
/// on next open, [`Cache::open`] compares the stored `schema_version`
/// row against this value and, on mismatch, drops and recreates
/// `units`/`links`/`tags`/`blocks` (never `sources`) — see
/// [`Cache::rebuild`]. There is no migration-file mechanism or
/// version-ladder anywhere in this crate (F2).
pub const CURRENT_SCHEMA_VERSION: i64 = 1;

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
