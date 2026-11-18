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

/// What [`Cache::begin_unpromote`] writes: the reverse of a promote.
#[derive(Debug, Clone)]
pub struct NewUnpromote<'a> {
    /// The promoted source that folds back, and the source it was
    /// promoted out of.
    pub name: &'a str,
    pub parent: &'a str,
    /// The folder of `parent` that `name` was promoted from (forward
    /// slashes, no trailing slash).
    pub rel_path: &'a str,
    pub created_at: i64,
    pub log: &'a NewFixLog,
    pub pending: &'a [NewPendingFix],
}

/// What [`Cache::begin_unpromote`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unpromoted {
    pub fix_id: i64,
    /// How many cached rows went back to the parent.
    pub units_moved: usize,
}

const COLUMNS: &str = "id, parent, rel_path, source, created_at";

/// The same columns, for a query that names the table `r`.
const COLUMNS_R: &str = "r.id, r.parent, r.rel_path, r.source, r.created_at";

/// What makes a `promoted_roots` row LIVE rather than history (`r` is the
/// row, `p` the source named by its `parent`, `c` the one named by its
/// `source`): the child's root is the parent's root plus the row's folder.
/// Names alone prove nothing. The row is kept as history after its parent
/// is unregistered, and a source registered later under the parent's old
/// name, elsewhere, is not the parent: a stale row must never be taken for
/// a promote of that other source.
const LIVE: &str = "c.path = rtrim(p.path, '/') || '/' || r.rel_path";

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

    /// The promoted roots `parent` skips, oldest first: only the live
    /// rows (see [`Cache::live_promoted_roots`]), so a stale row left by
    /// an earlier source of the same name excludes nothing.
    pub fn promoted_roots_of(&self, parent: &str) -> Result<Vec<PromotedRootRow>, CacheError> {
        let mut stmt = self.connection().prepare_cached(&format!(
            "SELECT {COLUMNS_R} FROM promoted_roots r \
             JOIN sources p ON p.name = r.parent \
             JOIN sources c ON c.name = r.source \
             WHERE r.parent = ?1 AND {LIVE} ORDER BY r.id"
        ))?;
        let rows = stmt
            .query_map([parent], from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The promoted roots that are in force, oldest first: both sources
    /// are registered and the child's root is the parent's root plus
    /// `rel_path`. [`Cache::promoted_roots`] also returns the rows kept as
    /// history (the parent was unregistered, or its name now belongs to a
    /// different source); everything that acts on a promote (the scan
    /// exclusion, `is_promoted`, unpromote, re-owning) reads this one.
    pub fn live_promoted_roots(&self) -> Result<Vec<PromotedRootRow>, CacheError> {
        let mut stmt = self.connection().prepare_cached(&format!(
            "SELECT {COLUMNS_R} FROM promoted_roots r \
             JOIN sources p ON p.name = r.parent \
             JOIN sources c ON c.name = r.source \
             WHERE {LIVE} ORDER BY r.id"
        ))?;
        let rows = stmt
            .query_map([], from_row)?
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
        // what the inner one owns. Only a live row moves: a stale one
        // (history of an unregistered source of the parent's name) is no
        // child of this parent.
        tx.execute(
            "UPDATE promoted_roots SET parent = ?1, rel_path = substr(rel_path, ?2) \
             WHERE parent = ?3 AND rel_path >= ?4 AND rel_path < ?5 \
             AND EXISTS (SELECT 1 FROM sources p, sources c \
                         WHERE p.name = promoted_roots.parent \
                         AND c.name = promoted_roots.source \
                         AND c.path = rtrim(p.path, '/') || '/' || promoted_roots.rel_path)",
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

    /// The reverse of a promote, in ONE transaction: the units of the
    /// promoted source `new.name` go back to `new.parent` under the folder
    /// `new.rel_path`, the folder's exclusion and the source's registry
    /// row and scan record are removed, a source promoted out of the
    /// promoted one becomes a child of the parent (its exclusion moves up
    /// with its path made relative to the parent's root), and the log
    /// entry and the pending list are written. The units move inside the
    /// transaction, not after it, because a unit's `source_id` is a
    /// foreign key: the row of a source that still owns units cannot go.
    /// A crash therefore leaves either nothing or all of it, and only the
    /// link rewrites are left for the next start.
    ///
    /// Fails with [`CacheError::SourceNotFound`] for a missing source or
    /// parent and with [`CacheError::NotPromoted`] when no `promoted_roots`
    /// row says `new.name` was promoted out of `new.rel_path` of
    /// `new.parent`; either leaves nothing written.
    pub fn begin_unpromote(&self, new: &NewUnpromote<'_>) -> Result<Unpromoted, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let units_moved = fold_back(&tx, new.name, new.parent, new.rel_path)?;
        let fix_id = fix_log::insert(&tx, new.log)?;
        for p in new.pending {
            pending_fixes::insert(&tx, fix_id, p, new.created_at)?;
        }
        tx.commit()?;
        Ok(Unpromoted {
            fix_id,
            units_moved,
        })
    }

    /// Runs `read` against this cache as it will be once the promoted
    /// source `name` is folded back into `parent` (see
    /// [`Cache::begin_unpromote`]), then puts everything back: nothing is
    /// left behind, whatever `read` returns.
    pub fn simulate_unpromote<T>(
        &self,
        name: &str,
        parent: &str,
        rel_path: &str,
        read: impl FnOnce(&Cache) -> T,
    ) -> Result<T, CacheError> {
        let conn = self.connection();
        conn.execute_batch("SAVEPOINT simulate_unpromote")?;
        let result = fold_back(conn, name, parent, rel_path).map(|_| read(self));
        let undone =
            conn.execute_batch("ROLLBACK TO simulate_unpromote; RELEASE simulate_unpromote");
        let value = result?;
        undone?;
        Ok(value)
    }
}

/// The row changes of an unpromote, shared by the real transaction and
/// its simulation. Returns how many units went back to the parent.
fn fold_back(
    conn: &rusqlite::Connection,
    name: &str,
    parent: &str,
    rel_path: &str,
) -> Result<usize, CacheError> {
    let id_of = |source: &str| -> Result<i64, CacheError> {
        conn.query_row("SELECT id FROM sources WHERE name = ?1", [source], |r| {
            r.get(0)
        })
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => CacheError::SourceNotFound(source.to_string()),
            e => CacheError::from(e),
        })
    };
    let child_id = id_of(name)?;
    let parent_id = id_of(parent)?;
    // The row must be live: the promoted source's root is this parent's
    // root plus the folder. A row of the same names that fails this is
    // history of another source and nothing here is promoted.
    let live: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM promoted_roots r \
                        JOIN sources p ON p.name = r.parent \
                        JOIN sources c ON c.name = r.source \
                        WHERE r.parent = ?1 AND r.rel_path = ?2 AND r.source = ?3 \
                        AND c.path = rtrim(p.path, '/') || '/' || r.rel_path)",
        params![parent, rel_path, name],
        |r| r.get(0),
    )?;
    if !live {
        return Err(CacheError::NotPromoted(name.to_string()));
    }
    conn.execute(
        "DELETE FROM promoted_roots WHERE parent = ?1 AND rel_path = ?2 AND source = ?3",
        params![parent, rel_path, name],
    )?;
    // A source promoted out of the promoted one stays promoted: now a
    // child of the parent, its path under the folder the parent gets back.
    // Only a live row (its root is under the promoted source's root).
    conn.execute(
        "UPDATE promoted_roots SET parent = ?1, rel_path = ?2 || '/' || rel_path \
         WHERE parent = ?3 \
         AND EXISTS (SELECT 1 FROM sources p, sources c \
                     WHERE p.name = ?3 AND c.name = promoted_roots.source \
                     AND c.path = rtrim(p.path, '/') || '/' || promoted_roots.rel_path)",
        params![parent, rel_path, name],
    )?;
    let moved = conn.execute(
        "UPDATE units SET source_id = ?1, rel_path = ?2 || '/' || rel_path WHERE source_id = ?3",
        params![parent_id, rel_path, child_id],
    )?;
    conn.execute("DELETE FROM scan_state WHERE source_id = ?1", [child_id])?;
    conn.execute("DELETE FROM sources WHERE id = ?1", [child_id])?;
    Ok(moved)
}
