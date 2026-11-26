//! `tephra-cache`: the SQLite persistent cache layer for `tephrad`.
//!
//! Owns `cache.db` (WAL mode), its schema (`sources`, `units`, `links`,
//! `tags`, `blocks`, `headings`, `symbols`, the `block_fts` FTS5 index,
//! `scan_state`, `schema_version`, the durable `fix_log`,
//! `pending_fixes` and `promoted_roots`, and the durable remote tables
//! `remote_sources`, `remote_items`, `upload_queue`, `remote_pins` and
//! `remote_cursors`), the single drop-and-rebuild
//! path triggered by a schema-version mismatch (F2 — no migration
//! scripts), and per-source status bookkeeping
//! (`active`/`stale`/`offline`) so an unmounted or offline source's
//! cached index is preserved rather than wiped (E2).
//!
//! Out of scope for this crate: AST parsing, filesystem watching, RPC
//! transport, and CLI — those are later crates/PRs that call into the
//! typed helpers exposed here.

mod cache;
mod evict;
mod fix_log;
mod fts;
mod in_file;
mod listing;
mod pending_fixes;
mod promote;
mod remote;
mod remote_clone;
mod remote_evict;
mod remote_fetch;
mod remote_ops;
mod remote_pins;
mod remote_sync;
mod remote_upload;
mod schema;
mod search;
mod source_rename;
mod sources;
mod units;

use std::fmt;

pub use cache::{Cache, FaultInjector};
pub use evict::{
    plan_eviction, protected_bytes, EvictItem, EvictPick, EvictReason, EvictionPlan,
    DEFAULT_IDLE_SECS,
};
pub use fix_log::{
    FixLogChange, FixLogRow, FixLogSummary, NewFixLog, DEFAULT_LIST_LIMIT, MAX_LIST_LIMIT,
};
pub use fts::{
    make_snippet, BlockHit, NewFtsRow, FTS_ROWS_PER_UNIT, MAX_RANK_CANDIDATES, MIN_TRIGRAM_CHARS,
};
pub use in_file::{InFileCandidate, InFileScope};
pub use listing::{
    compare_entries, compare_names, FolderEntry, FolderListing, FolderState, FOLDER_KIND,
};
pub use pending_fixes::{
    NewPendingFix, PendingFixRow, PendingOp, STATE_NEEDS_MANUAL_FIX, STATE_PENDING,
};
pub use promote::{NewPromote, NewUnpromote, Promoted, PromotedRootRow, Unpromoted};
pub use remote::{REMOTE_ITEM_STATES, REMOTE_SOURCE_KINDS, REMOTE_TABLES};
pub use remote_clone::{
    clone_reason_not_on_branch, is_clone_reason, CloneChanges, CloneState,
    CLONE_FETCH_REASON_PREFIX, CLONE_REASON_DIVERGED, CLONE_REASON_REWRITTEN,
    REASON_CHANGED_BOTH_SIDES,
};
pub use remote_fetch::{
    item_cache_bytes, state_has_copy, RemoteFetchRow, FETCH_REASON_PREFIX, ITEM_STATE_CACHED,
    ITEM_STATE_DOWNLOADING, ITEM_STATE_MISSING, ITEM_STATE_NOT_PUSHED, ITEM_STATE_PENDING_UPLOAD,
    ITEM_STATE_PINNED, ITEM_STATE_REMOTE,
};
pub use remote_ops::{
    is_drive_item_kind, is_text_item_kind, remote_root, IndexProgress, NewRemoteItem,
    NewRemoteSource, RemoteCursor, RemoteItemRow, RemoteSourceRow, ITEM_KIND_GDOC,
    ITEM_KIND_GDRAWING, ITEM_KIND_GFORM, ITEM_KIND_GSHEET, ITEM_KIND_GSHORTCUT, ITEM_KIND_GSLIDES,
    ITEM_KIND_SUBMODULE, ITEM_KIND_SYMLINK, LISTING_MARK, REASON_LFS_POINTER, REASON_LISTING,
    REMOTE_ROOT_PREFIX, STATUS_ACTIVE, STATUS_INDEXING,
};
pub use remote_pins::{pin_covers, PinWrite, RemotePin};
pub use remote_sync::{
    decode_cursor, encode_cursor, is_kept_deleted, PollState, ITEM_STATE_NEEDS_ATTENTION,
    REASON_DELETED_ON_REMOTE, REASON_READDED_AFTER_DELETE, STATUS_AUTH_REQUIRED,
    STATUS_NEEDS_ATTENTION, STATUS_OFFLINE, STATUS_SYNCING,
};
pub use remote_upload::{
    upload_backoff_secs, DriveConflict, DriveHolds, QueuedEdit, UploadEntry, UploadLanded,
    HELD_ITEMS_LISTED, REASON_CHANGED_ON_DRIVE, UPLOAD_BACKOFF_CAP_SECS, UPLOAD_REFUSED_PREFIX,
};
pub use schema::KindInferrer;
pub use search::{compare_hits, file_stem, resolve_title, NameCandidate, NameHit, NAME_SEARCH_SQL};
pub use source_rename::{NewSourceRename, SourceRenamed};
pub use sources::{validate_transition, InvalidTransition, SourceRow, SourceStatus};
pub use units::{
    BlockOccurrence, BlockRow, HeadingRow, IdBlockText, LinkRow, NewBlock, NewHeading, NewLink,
    NewSymbol, NewTag, NewUnit, SymbolRow, TagRow, UnitRow, UnitWithSourceStatus,
};

/// The schema version this build of the crate expects. Bumping this
/// constant is the *only* way the on-disk schema ever changes shape:
/// on next open, [`Cache::open`] compares the stored `schema_version`
/// row against this value and, on mismatch, drops and recreates
/// every core table (never `sources`, `schema_version`,
/// `scan_state`, `fix_log`, `pending_fixes`, `promoted_roots` or any of
/// the five remote tables) — see [`Cache::rebuild`]. There is no
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
///
/// Version 3 added `units.hash` (SHA-256 of the file's bytes),
/// `units.inode` and `units.device` (the file's identity on disk), and
/// the durable `fix_log`, `pending_fixes` and `promoted_roots` tables,
/// which a rebuild never drops. The first start after the upgrade
/// rebuilds the core tables and rescans every source, filling the hashes
/// and identities.
///
/// The remote tables did not bump the version: they are created `IF NOT
/// EXISTS` on every open, so a v3 cache that lacks them gets them on its
/// next open and a rebuild never drops them.
pub const CURRENT_SCHEMA_VERSION: i64 = 3;

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
    /// [`Cache::begin_unpromote`] (or its simulation) named a source that
    /// has no `promoted_roots` row for the given parent and folder: it
    /// was never promoted out of that folder.
    NotPromoted(String),
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
            CacheError::NotPromoted(name) => {
                write!(f, "the source \"{name}\" was not promoted out of a folder")
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
