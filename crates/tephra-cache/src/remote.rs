//! The vocabulary of the durable remote tables (`remote_sources`,
//! `remote_items`, `upload_queue`, `remote_pins`, `remote_cursors`).
//!
//! The DDL is in [`crate::schema`] and the tables are created with the
//! bootstrap tables on every open (see there for why they survive a
//! rebuild). The typed helpers that read and write them are in
//! `remote_ops` (the listing and its restore) and `remote_sync` (the
//! poll cursor and the steps of a poll); the upload buffer and the pins
//! add theirs in later PRs.
//! What lives here are the closed value sets the tables' `CHECK`
//! constraints enforce, so a writer names them from one place; `tests/remote_tables.rs` checks each list against the
//! constraint it mirrors, in both directions.

/// The five remote tables, in the order a dump lists them. Every one is
/// durable: [`crate::Cache::rebuild`] and a version-mismatch open leave
/// all five alone.
pub const REMOTE_TABLES: [&str; 5] = [
    "remote_sources",
    "remote_items",
    "upload_queue",
    "remote_pins",
    "remote_cursors",
];

/// Every table that holds rows of one remote source, with the column that
/// names the source: the five of [`REMOTE_TABLES`] and the three
/// bookkeeping tables beside them (`remote_copy_stamps`,
/// `remote_copy_revisions`, `remote_clone_state`). [`crate::Cache::delete_remote_source`]
/// deletes a source's rows from every one of them; `tests/remote_source_remove.rs`
/// finds the tables itself (every table with a `source` column, and
/// `remote_sources`) and fails when one is missing here.
pub const REMOTE_SOURCE_TABLES: [(&str, &str); 8] = [
    ("remote_sources", "name"),
    ("remote_items", "source"),
    ("upload_queue", "source"),
    ("remote_pins", "source"),
    ("remote_cursors", "source"),
    ("remote_copy_stamps", "source"),
    ("remote_copy_revisions", "source"),
    ("remote_clone_state", "source"),
];

/// The values `remote_sources.kind` accepts.
pub const REMOTE_SOURCE_KINDS: [&str; 2] = ["github", "drive"];

/// The values `remote_items.state` accepts: the eight item states of a
/// remote file. When more than one applies the precedence is
/// `needs_attention` > `pending_upload` / `not_pushed` > `downloading` >
/// `pinned` > `cached` > `remote`; `missing` is an item whose remote
/// file is gone but whose row is kept.
pub const REMOTE_ITEM_STATES: [&str; 8] = [
    "remote",
    "downloading",
    "cached",
    "pinned",
    "pending_upload",
    "not_pushed",
    "needs_attention",
    "missing",
];
