//! The `pending_fixes` table: the work list of link rewrites still to
//! do, one row per linking file (G9). A row is intent (the file, the
//! target it named and the target it should now name), never a recorded
//! edit: whoever retries re-derives the edit from the file as it is
//! then.
//!
//! Durable like `fix_log` (see `schema`): never dropped by a rebuild,
//! no foreign key. `fix_id` is the `fix_log` entry the row belongs to,
//! held as a plain number; [`crate::Cache::start_fix`] writes the entry
//! and its rows in one transaction so neither exists without the other
//! (E9), and [`crate::Cache::apply_fix_step`] changes a row and its
//! entry in one transaction too.

use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::CacheError;

/// A row not yet written, waiting for its turn (or for its backoff).
pub const STATE_PENDING: &str = "pending";

/// A row that will not be retried by a timer: a permanent failure, or
/// the attempt cap reached. Retried only at startup, on `source.rescan`
/// or when its file changes.
pub const STATE_NEEDS_MANUAL_FIX: &str = "needs_manual_fix";

/// A `pending_fixes` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFixRow {
    pub id: i64,
    pub fix_id: Option<i64>,
    /// The source the linking file is in.
    pub source: String,
    /// The linking file, relative to its source's root.
    pub file_path: String,
    pub old_source: Option<String>,
    pub old_path: Option<String>,
    pub new_source: Option<String>,
    pub new_path: Option<String>,
    pub state: String,
    pub reason: Option<String>,
    /// Rounds of this file that have ended in a transient failure.
    pub attempts: i64,
    pub created_at: i64,
    /// When the row last changed; a retry's backoff counts from here.
    pub updated_at: i64,
}

/// What [`insert`] stores for one linking file. The state starts as
/// [`STATE_PENDING`] with no attempts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPendingFix {
    pub source: String,
    pub file_path: String,
    pub old_source: Option<String>,
    pub old_path: Option<String>,
    pub new_source: Option<String>,
    pub new_path: Option<String>,
}

/// What to do with one row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingOp {
    /// The file is done (written, nothing to write, or dropped).
    Delete,
    /// Keep the row with a new state, reason and attempt count.
    Update {
        state: String,
        reason: Option<String>,
        attempts: i64,
    },
    /// Keep the row, point it at another target (a chain A to B to C
    /// resolved to C), and reset it to [`STATE_PENDING`].
    Repoint {
        new_source: String,
        new_path: String,
    },
}

/// What a fix was about to write into a row's file (see
/// [`crate::Cache::mark_pending_write`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWrite {
    /// Byte length of the new text.
    pub len: i64,
    /// `content_hash_bytes` of the new text.
    pub hash: String,
    /// How many links the write changes.
    pub links: i64,
}

const COLUMNS: &str = "id, fix_id, source, file_path, old_source, old_path, new_source, \
                       new_path, state, reason, attempts, created_at, updated_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<PendingFixRow> {
    Ok(PendingFixRow {
        id: row.get(0)?,
        fix_id: row.get(1)?,
        source: row.get(2)?,
        file_path: row.get(3)?,
        old_source: row.get(4)?,
        old_path: row.get(5)?,
        new_source: row.get(6)?,
        new_path: row.get(7)?,
        state: row.get(8)?,
        reason: row.get(9)?,
        attempts: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
    })
}

pub(crate) fn insert(
    conn: &Connection,
    fix_id: i64,
    new: &NewPendingFix,
    now: i64,
) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO pending_fixes (fix_id, source, file_path, old_source, old_path, \
         new_source, new_path, state, attempts, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, ?9, ?9)",
        params![
            fix_id,
            new.source,
            new.file_path,
            new.old_source,
            new.old_path,
            new.new_source,
            new.new_path,
            STATE_PENDING,
            now,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Records (or replaces) what row `pending_id` is about to write.
pub(crate) fn mark_write(
    conn: &Connection,
    pending_id: i64,
    write: &PendingWrite,
    now: i64,
) -> Result<(), CacheError> {
    conn.execute(
        "INSERT OR REPLACE INTO pending_writes (pending_id, len, hash, links, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![pending_id, write.len, write.hash, write.links, now],
    )?;
    Ok(())
}

pub(crate) fn marked_write(
    conn: &Connection,
    pending_id: i64,
) -> Result<Option<PendingWrite>, CacheError> {
    Ok(conn
        .query_row(
            "SELECT len, hash, links FROM pending_writes WHERE pending_id = ?1",
            [pending_id],
            |r| {
                Ok(PendingWrite {
                    len: r.get(0)?,
                    hash: r.get(1)?,
                    links: r.get(2)?,
                })
            },
        )
        .optional()?)
}

pub(crate) fn get(conn: &Connection, id: i64) -> Result<Option<PendingFixRow>, CacheError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM pending_fixes WHERE id = ?1"),
            [id],
            from_row,
        )
        .optional()?)
}

/// Every row, oldest first; `source` limits it to the linking files of
/// one source and `fix_id` to one fix.
pub(crate) fn list(
    conn: &Connection,
    source: Option<&str>,
    fix_id: Option<i64>,
) -> Result<Vec<PendingFixRow>, CacheError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM pending_fixes \
         WHERE (?1 IS NULL OR source = ?1) AND (?2 IS NULL OR fix_id = ?2) ORDER BY id ASC"
    ))?;
    let rows = stmt
        .query_map(params![source, fix_id], from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Applies `op` to row `id`. `false` when the row is gone.
pub(crate) fn apply(
    conn: &Connection,
    id: i64,
    op: &PendingOp,
    now: i64,
) -> Result<bool, CacheError> {
    // Whatever the row becomes, what it was about to write is settled: it
    // was written (the entry says so), or it was not and is derived again.
    conn.execute("DELETE FROM pending_writes WHERE pending_id = ?1", [id])?;
    let n = match op {
        PendingOp::Delete => conn.execute("DELETE FROM pending_fixes WHERE id = ?1", [id])?,
        PendingOp::Update {
            state,
            reason,
            attempts,
        } => conn.execute(
            "UPDATE pending_fixes SET state = ?1, reason = ?2, attempts = ?3, updated_at = ?4 \
             WHERE id = ?5",
            params![state, reason, attempts, now, id],
        )?,
        PendingOp::Repoint {
            new_source,
            new_path,
        } => conn.execute(
            "UPDATE pending_fixes SET new_source = ?1, new_path = ?2, state = ?3, \
             reason = NULL, updated_at = ?4 WHERE id = ?5",
            params![new_source, new_path, STATE_PENDING, now, id],
        )?,
    };
    Ok(n > 0)
}
