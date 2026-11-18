//! Making a folder of one source its own source (FR22): the
//! `promoted_roots` table and the three steps on the cache that a
//! promote is made of.
//!
//! 1. [`Cache::begin_promote`]: ONE transaction writes the new source's
//!    row, the parent's scan exclusion (`promoted_roots`), the new
//!    source's open scan record, the fix-log entry and the pending list
//!    of every link that has to change. A crash after it leaves
//!    everything the later steps need.
//! 2. [`Cache::reown_units`]: the parent's cached rows under the folder
//!    become rows of the new source, in place (one `UPDATE`), so no file
//!    is ever in two sources and no unit id changes. Safe to run again.
//! 3. The link rewrites, which are the fix engine's work (`pending_fixes`).
//!
//! [`Cache::simulate_promote`] runs steps 1's row write and 2 inside a
//! savepoint it always rolls back: what the graph will look like once the
//! promote is done, read without changing anything.

use rusqlite::params;

use crate::fix_log::{self, NewFixLog};
use crate::pending_fixes::{self, NewPendingFix};
use crate::sources::SourceStatus;
use crate::{Cache, CacheError};

/// One `promoted_roots` row: `parent` skips `rel_path` (relative to its
/// root, forward slashes) because `source` owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromotedRootRow {
    pub id: i64,
    pub parent: String,
    pub rel_path: String,
    pub source: String,
    pub created_at: i64,
}

/// What [`Cache::begin_promote`] writes.
#[derive(Debug, Clone)]
pub struct NewPromote<'a> {
    /// The new source's name and canonical root.
    pub name: &'a str,
    pub root: &'a str,
    /// The source the folder is in, and the folder's path in it.
    pub parent: &'a str,
    pub rel_path: &'a str,
    pub created_at: i64,
    pub log: &'a NewFixLog,
    pub pending: &'a [NewPendingFix],
}

/// What [`Cache::begin_promote`] created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Promoted {
    pub source_id: i64,
    pub fix_id: i64,
}

const COLUMNS: &str = "id, parent, rel_path, source, created_at";

fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PromotedRootRow> {
    Ok(PromotedRootRow {
        id: row.get(0)?,
        parent: row.get(1)?,
        rel_path: row.get(2)?,
        source: row.get(3)?,
        created_at: row.get(4)?,
    })
}

impl Cache {
    /// Every promoted root, oldest first.
    pub fn promoted_roots(&self) -> Result<Vec<PromotedRootRow>, CacheError> {
        let mut stmt = self
            .connection()
            .prepare_cached(&format!("SELECT {COLUMNS} FROM promoted_roots ORDER BY id"))?;
        let rows = stmt
            .query_map([], from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The promoted roots `parent` skips, oldest first.
    pub fn promoted_roots_of(&self, parent: &str) -> Result<Vec<PromotedRootRow>, CacheError> {
        let mut stmt = self.connection().prepare_cached(&format!(
            "SELECT {COLUMNS} FROM promoted_roots WHERE parent = ?1 ORDER BY id"
        ))?;
        let rows = stmt
            .query_map([parent], from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Step 1 of a promote, in ONE transaction (see the module doc). The
    /// new source starts `active` with a scan that has started and not
    /// finished, so a crash before the caller marks it finished has the
    /// next start scan it again. The parent must be a registered source
    /// and the name unused; either failing leaves nothing written. A source
    /// already promoted out of the folder keeps its exclusion, now as a
    /// child of the new source.
    pub fn begin_promote(&self, new: &NewPromote<'_>) -> Result<Promoted, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let parent_exists: bool = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM sources WHERE name = ?1)",
            [new.parent],
            |r| r.get(0),
        )?;
        if !parent_exists {
            return Err(CacheError::SourceNotFound(new.parent.to_string()));
        }
        let inserted = tx.execute(
            "INSERT INTO sources (name, path, status, last_scanned_at) VALUES (?1, ?2, ?3, NULL)",
            params![new.name, new.root, SourceStatus::Active.as_str()],
        );
        match inserted {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(CacheError::DuplicateSource(new.name.to_string()));
            }
            Err(e) => return Err(CacheError::from(e)),
        }
        let source_id = tx.last_insert_rowid();
        // A source already promoted out of this folder (E14) is now a
        // child of the new source: its exclusion moves with it, its path
        // made relative to the folder, so the new source does not read
        // what the inner one owns.
        tx.execute(
            "UPDATE promoted_roots SET parent = ?1, rel_path = substr(rel_path, ?2) \
             WHERE parent = ?3 AND rel_path >= ?4 AND rel_path < ?5",
            params![
                new.name,
                new.rel_path.chars().count() as i64 + 2,
                new.parent,
                format!("{}/", new.rel_path),
                format!("{}0", new.rel_path),
            ],
        )?;
        tx.execute(
            "INSERT INTO promoted_roots (parent, rel_path, source, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![new.parent, new.rel_path, new.name, new.created_at],
        )?;
        tx.execute(
            "INSERT INTO scan_state (source_id, started_at, finished_at) \
             VALUES (?1, CAST(strftime('%s','now') AS INTEGER), NULL) \
             ON CONFLICT(source_id) DO UPDATE SET \
                 started_at = excluded.started_at, finished_at = NULL",
            [source_id],
        )?;
        let fix_id = fix_log::insert(&tx, new.log)?;
        for p in new.pending {
            pending_fixes::insert(&tx, fix_id, p, new.created_at)?;
        }
        tx.commit()?;
        Ok(Promoted { source_id, fix_id })
    }

    /// Step 2: the rows of `from_source_id` nested under the folder
    /// `rel_prefix` become rows of `to_source_id`, their paths made
    /// relative to the folder. One statement, so all or nothing; running
    /// it again finds nothing left to move. Returns how many rows moved.
    pub fn reown_units(
        &self,
        from_source_id: i64,
        to_source_id: i64,
        rel_prefix: &str,
    ) -> Result<usize, CacheError> {
        let lower = format!("{rel_prefix}/");
        let upper = format!("{rel_prefix}0");
        // `substr` counts characters, 1-based: the first one after
        // `<prefix>/`.
        let keep_from = lower.chars().count() as i64 + 1;
        let moved = self.connection().execute(
            "UPDATE units SET source_id = ?1, rel_path = substr(rel_path, ?2) \
             WHERE source_id = ?3 AND rel_path >= ?4 AND rel_path < ?5",
            params![to_source_id, keep_from, from_source_id, lower, upper],
        )?;
        Ok(moved)
    }

    /// Runs `read` against this cache as it will be once a promote of
    /// `rel_prefix` of `parent` into a source `name` is done (the row
    /// written, the units re-owned), then puts everything back: nothing
    /// is left behind, whatever `read` returns.
    pub fn simulate_promote<T>(
        &self,
        parent_id: i64,
        name: &str,
        root: &str,
        rel_prefix: &str,
        read: impl FnOnce(&Cache) -> T,
    ) -> Result<T, CacheError> {
        let conn = self.connection();
        conn.execute_batch("SAVEPOINT simulate_promote")?;
        let result = (|| {
            conn.execute(
                "INSERT INTO sources (name, path, status, last_scanned_at) \
                 VALUES (?1, ?2, 'active', NULL)",
                params![name, root],
            )?;
            let source_id = conn.last_insert_rowid();
            self.reown_units(parent_id, source_id, rel_prefix)?;
            Ok::<T, CacheError>(read(self))
        })();
        let undone = conn.execute_batch("ROLLBACK TO simulate_promote; RELEASE simulate_promote");
        let value = result?;
        undone?;
        Ok(value)
    }
}
