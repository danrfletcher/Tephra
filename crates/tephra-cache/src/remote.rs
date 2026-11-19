//! The vocabulary of the durable remote tables (`remote_sources`,
//! `remote_items`, `upload_queue`, `remote_pins`, `remote_cursors`).
//!
//! The DDL is in [`crate::schema`] and the tables are created with the
//! bootstrap tables on every open (see there for why they survive a
//! rebuild). Nothing in this crate reads or writes them yet: the sync
//! engine, the upload buffer and the pins come in later PRs, which add
//! their typed helpers here. What lives here now are the closed value
//! sets the tables' `CHECK` constraints enforce, so a writer names them
//! from one place; `tests/remote_tables.rs` checks each list against the
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
