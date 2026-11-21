//! The `fix_log` table: one row per automatic fix (G10), a record and
//! nothing more. A row is written when a fix starts and updated in
//! place as its retries finish (`partial` to `applied`); it is never
//! deleted, replayed or undone here (F9).
//!
//! The table is durable (see `schema`): a schema-version rebuild leaves
//! it alone, and it holds no foreign key into `sources` or `units`.
//! Sources and paths are stored as text.
//!
//! The functions take a [`Connection`] (or a transaction, which derefs
//! to one) so the caller can put a log write and its `pending_fixes`
//! rows in one transaction (E9); [`crate::Cache`] wraps them.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::CacheError;

/// Entries [`list`] returns when the caller names no limit.
pub const DEFAULT_LIST_LIMIT: usize = 50;

/// The most entries [`list`] ever returns: a larger `limit` is clamped.
pub const MAX_LIST_LIMIT: usize = 1000;

/// A `fix_log` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixLogRow {
    pub id: i64,
    /// Epoch seconds, set when the fix was logged. Never changed.
    pub created_at: i64,
    /// `rename`, or `uncertain` for a vanished file with no certain
    /// pair.
    pub kind: String,
    /// `applied`, `partial`, `needs_manual_fix` or `uncertain`.
    pub status: String,
    pub old_source: Option<String>,
    pub old_path: Option<String>,
    pub new_source: Option<String>,
    pub new_path: Option<String>,
    /// How many linking files were rewritten.
    pub files_changed: i64,
    /// Per-file results, as JSON the caller owns.
    pub detail_json: Option<String>,
}

/// What [`insert`] stores. The id is assigned by the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFixLog {
    pub created_at: i64,
    pub kind: String,
    pub status: String,
    pub old_source: Option<String>,
    pub old_path: Option<String>,
    pub new_source: Option<String>,
    pub new_path: Option<String>,
    pub files_changed: i64,
    pub detail_json: Option<String>,
}

/// What [`update`] changes: the outcome, never the identity or the
/// time of the entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixLogChange {
    pub status: String,
    pub files_changed: i64,
    pub detail_json: Option<String>,
}

const COLUMNS: &str = "id, created_at, kind, status, old_source, old_path, \
                       new_source, new_path, files_changed, detail_json";

fn from_row(row: &Row<'_>) -> rusqlite::Result<FixLogRow> {
    Ok(FixLogRow {
        id: row.get(0)?,
        created_at: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
        old_source: row.get(4)?,
        old_path: row.get(5)?,
        new_source: row.get(6)?,
        new_path: row.get(7)?,
        files_changed: row.get(8)?,
        detail_json: row.get(9)?,
    })
}

pub(crate) fn insert(conn: &Connection, new: &NewFixLog) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO fix_log (created_at, kind, status, old_source, old_path, \
         new_source, new_path, files_changed, detail_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            new.created_at,
            new.kind,
            new.status,
            new.old_source,
            new.old_path,
            new.new_source,
            new.new_path,
            new.files_changed,
            new.detail_json,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Updates the entry in place. `false` when there is no entry `id`.
pub(crate) fn update(
    conn: &Connection,
    id: i64,
    change: &FixLogChange,
) -> Result<bool, CacheError> {
    let n = conn.execute(
        "UPDATE fix_log SET status = ?1, files_changed = ?2, detail_json = ?3 WHERE id = ?4",
        params![change.status, change.files_changed, change.detail_json, id],
    )?;
    Ok(n > 0)
}

pub(crate) fn get(conn: &Connection, id: i64) -> Result<Option<FixLogRow>, CacheError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM fix_log WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
}

/// The entries logged at or after `since` (epoch seconds; none means
/// all), newest first, at most `limit` of them (clamped to
/// [`MAX_LIST_LIMIT`]). Entries logged in the same second come
/// newest-id first.
pub(crate) fn list(
    conn: &Connection,
    since: Option<i64>,
    limit: usize,
) -> Result<Vec<FixLogRow>, CacheError> {
    let limit = limit.min(MAX_LIST_LIMIT) as i64;
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM fix_log WHERE created_at >= ?1 \
         ORDER BY created_at DESC, id DESC LIMIT ?2"
    ))?;
    let rows = stmt
        .query_map(params![since.unwrap_or(i64::MIN), limit], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Renames logged after entry `after_id` whose old side is `(source,
/// path)`, oldest first. What a pending fix needs to follow a target
/// that moved again, or to see its own target was corrected.
pub(crate) fn renames_from(
    conn: &Connection,
    source: &str,
    path: &str,
    after_id: i64,
) -> Result<Vec<FixLogRow>, CacheError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM fix_log WHERE kind = 'rename' AND id > ?1 \
         AND old_source = ?2 AND old_path = ?3 ORDER BY id ASC"
    ))?;
    let rows = stmt
        .query_map(params![after_id, source, path], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// What an exact rename event needs of one log entry to tell whether it
/// was already fixed: who, what and when, with none of the per-file
/// detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixLogSummary {
    pub id: i64,
    pub created_at: i64,
    pub kind: String,
    pub old_source: Option<String>,
    pub old_path: Option<String>,
    pub new_source: Option<String>,
    pub new_path: Option<String>,
}

/// The `rename`, `move` and `uncertain` entries that name `source` on either
/// side, oldest first (id order).
pub(crate) fn summaries_for_source(
    conn: &Connection,
    source: &str,
) -> Result<Vec<FixLogSummary>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, kind, old_source, old_path, new_source, new_path \
         FROM fix_log WHERE kind IN ('rename', 'move', 'uncertain') \
         AND (old_source = ?1 OR new_source = ?1) ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map([source], |row| {
            Ok(FixLogSummary {
                id: row.get(0)?,
                created_at: row.get(1)?,
                kind: row.get(2)?,
                old_source: row.get(3)?,
                old_path: row.get(4)?,
                new_source: row.get(5)?,
                new_path: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ---------------------------------------------------------------
// On the cache
// ---------------------------------------------------------------

use crate::pending_fixes::{self, NewPendingFix, PendingFixRow, PendingOp, PendingWrite};
use crate::Cache;

impl Cache {
    /// Logs a fix and the work list it leaves, in ONE transaction: there
    /// is never a pending row without its log entry, or the reverse
    /// (E9). Returns the entry's id and the ids of the pending rows, in
    /// the order given.
    pub fn start_fix(
        &self,
        log: &NewFixLog,
        pending: &[NewPendingFix],
    ) -> Result<(i64, Vec<i64>), CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let fix_id = insert(&tx, log)?;
        let mut ids = Vec::with_capacity(pending.len());
        for p in pending {
            ids.push(pending_fixes::insert(&tx, fix_id, p, log.created_at)?);
        }
        tx.commit()?;
        Ok((fix_id, ids))
    }

    /// Logs one entry that has no work list (an `uncertain` one).
    pub fn insert_fix_log(&self, new: &NewFixLog) -> Result<i64, CacheError> {
        insert(self.connection(), new)
    }

    /// Updates a log entry in place; `false` when there is none.
    pub fn update_fix_log(&self, id: i64, change: &FixLogChange) -> Result<bool, CacheError> {
        update(self.connection(), id, change)
    }

    pub fn get_fix_log(&self, id: i64) -> Result<Option<FixLogRow>, CacheError> {
        get(self.connection(), id)
    }

    /// Entries newest first; see [`list`](fn@list) for the filter and
    /// the clamp on `limit`.
    pub fn list_fix_log(
        &self,
        since: Option<i64>,
        limit: usize,
    ) -> Result<Vec<FixLogRow>, CacheError> {
        list(self.connection(), since, limit)
    }

    /// The `rename` and `uncertain` entries that name `source`, oldest
    /// first, without their detail.
    pub fn fix_log_summaries(&self, source: &str) -> Result<Vec<FixLogSummary>, CacheError> {
        summaries_for_source(self.connection(), source)
    }

    /// The `move` and `rename` entries that matter to the move of `old`
    /// to `new` (each a `(source, path)`), oldest first: that exact move,
    /// and any entry that took a file away from `new` or brought one to
    /// `old`. What decides whether the move is already logged and still
    /// stands, read by the database instead of the whole log.
    pub fn fix_log_moves_between(
        &self,
        old: (&str, &str),
        new: (&str, &str),
    ) -> Result<Vec<FixLogSummary>, CacheError> {
        let mut stmt = self.connection().prepare(
            "SELECT id, created_at, kind, old_source, old_path, new_source, new_path \
             FROM fix_log WHERE kind IN ('move', 'rename') AND ( \
               (old_source = ?1 AND old_path = ?2 AND new_source = ?3 AND new_path = ?4) \
               OR (old_source = ?3 AND old_path = ?4) \
               OR (new_source = ?1 AND new_path = ?2)) ORDER BY id ASC",
        )?;
        let rows = stmt
            .query_map(params![old.0, old.1, new.0, new.1], |row| {
                Ok(FixLogSummary {
                    id: row.get(0)?,
                    created_at: row.get(1)?,
                    kind: row.get(2)?,
                    old_source: row.get(3)?,
                    old_path: row.get(4)?,
                    new_source: row.get(5)?,
                    new_path: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Whether the log holds a `source_rename` entry from the source
    /// `old` to the source `new`. The entry is written in the same
    /// transaction as the rename itself, so it is there exactly when the
    /// rename committed: how a repeated `source.rename` tells a retry of
    /// one that already happened from a name that was never a source.
    pub fn source_rename_logged(&self, old: &str, new: &str) -> Result<bool, CacheError> {
        Ok(self.connection().query_row(
            "SELECT EXISTS (SELECT 1 FROM fix_log WHERE kind = 'source_rename' \
             AND old_source = ?1 COLLATE NOCASE AND new_source = ?2 COLLATE NOCASE)",
            params![old, new],
            |r| r.get(0),
        )?)
    }

    /// Renames logged after entry `after_id` that start at `(source,
    /// path)`, oldest first.
    pub fn fix_renames_from(
        &self,
        source: &str,
        path: &str,
        after_id: i64,
    ) -> Result<Vec<FixLogRow>, CacheError> {
        renames_from(self.connection(), source, path, after_id)
    }

    /// The pending rows, oldest first, optionally of one source or one
    /// fix.
    pub fn pending_fixes(
        &self,
        source: Option<&str>,
        fix_id: Option<i64>,
    ) -> Result<Vec<PendingFixRow>, CacheError> {
        pending_fixes::list(self.connection(), source, fix_id)
    }

    /// Records the files the rename detector holds (see
    /// [`crate::HeldFile`]), in one transaction.
    pub fn hold_renames(&self, files: &[crate::HeldFile], now: i64) -> Result<(), CacheError> {
        crate::held_renames::hold(self.connection(), files, now)
    }

    /// Removes held files the detector has settled.
    pub fn release_held_renames(&self, files: &[crate::HeldFile]) -> Result<(), CacheError> {
        crate::held_renames::release(self.connection(), files)
    }

    /// Every held file, oldest first.
    pub fn held_renames(&self) -> Result<Vec<crate::HeldFile>, CacheError> {
        crate::held_renames::all(self.connection())
    }

    /// Records, durably, what the fix of row `pending_id` is about to put
    /// in its file: called just before the rename. The row's next change
    /// ([`Cache::apply_fix_step`]) removes it.
    pub fn mark_pending_write(
        &self,
        pending_id: i64,
        write: &PendingWrite,
        now: i64,
    ) -> Result<(), CacheError> {
        pending_fixes::mark_write(self.connection(), pending_id, write, now)
    }

    /// What row `pending_id` was about to write, if a run stopped before
    /// the row was finished.
    pub fn pending_write(&self, pending_id: i64) -> Result<Option<PendingWrite>, CacheError> {
        pending_fixes::marked_write(self.connection(), pending_id)
    }

    pub fn get_pending_fix(&self, id: i64) -> Result<Option<PendingFixRow>, CacheError> {
        pending_fixes::get(self.connection(), id)
    }

    /// One step of a fix, in ONE transaction: `op` on pending row
    /// `pending_id` and, when `log_fix_id` is given, the log entry
    /// `change` derives from the entry as it is stored. A crash leaves
    /// either both changes or neither. `false` when the row is gone (the
    /// log is then left alone).
    pub fn apply_fix_step(
        &self,
        pending_id: i64,
        op: &PendingOp,
        now: i64,
        log_fix_id: Option<i64>,
        change: impl FnOnce(&FixLogRow) -> Option<FixLogChange>,
    ) -> Result<bool, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        if !pending_fixes::apply(&tx, pending_id, op, now)? {
            return Ok(false);
        }
        if let Some(id) = log_fix_id {
            if let Some(row) = get(&tx, id)? {
                if let Some(c) = change(&row) {
                    update(&tx, id, &c)?;
                }
            }
        }
        tx.commit()?;
        Ok(true)
    }
}
