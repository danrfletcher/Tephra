//! `Cache`: the connection wrapper that opens `cache.db`, applies WAL
//! pragmas, runs the schema-version check/rebuild, and exposes typed
//! read/write helpers over `sources`/`units`/`links`/`tags`/`blocks`.

use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use crate::schema;
use crate::sources::{self, SourceRow, SourceStatus};
use crate::units::{
    self, BlockRow, LinkRow, NewBlock, NewLink, NewTag, NewUnit, TagRow, UnitRow,
    UnitWithSourceStatus,
};
use crate::{CacheError, CURRENT_SCHEMA_VERSION};

/// A connection to `cache.db`, plus the path it was opened against.
pub struct Cache {
    conn: Connection,
    path: PathBuf,
}

/// Test-only seam for forcing [`Cache::rebuild`] to fail partway
/// through, after the old core tables are dropped but before they are
/// recreated — used by `tests/rebuild.rs` to verify F2's atomicity
/// guarantee (a crash mid-rebuild leaves the previous schema intact).
/// `FaultInjector::none()` (the default used by every production call
/// site) never injects a failure.
#[derive(Debug, Clone, Copy, Default)]
pub struct FaultInjector {
    pub fail_after_drop: bool,
}

impl FaultInjector {
    pub fn none() -> Self {
        Self::default()
    }
}

impl Cache {
    /// Opens (creating if absent) the SQLite database at `path`,
    /// creating its parent directory if missing (G5), applies the WAL
    /// pragmas, and runs the schema-version check: a fresh/matching
    /// database is left as-is (beyond ensuring the bootstrap tables
    /// exist); a mismatched one is rebuilt via the single drop/recreate
    /// path in [`schema`] (F2).
    ///
    /// `path` is caller-supplied rather than hardcoded: in production
    /// the daemon passes `<data_dir>/cache.db`, where `data_dir` is the
    /// same directory PR-1's `paths::data_dir()` resolves (honoring
    /// `XDG_DATA_HOME`, falling back to `~/.local/share/tephra`); tests
    /// in this crate pass a temp directory instead.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        let conn = Connection::open(&path)?;
        apply_pragmas(&conn)?;
        ensure_schema(&conn)?;

        Ok(Cache { conn, path })
    }

    /// The path this cache was opened against.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Current `PRAGMA journal_mode` value (expected `"wal"`).
    pub fn journal_mode(&self) -> Result<String, CacheError> {
        let mode: String = self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
        Ok(mode)
    }

    /// Current `PRAGMA synchronous` value (SQLite's integer encoding:
    /// `0` = OFF, `1` = NORMAL, `2` = FULL; NORMAL is expected here).
    pub fn synchronous_mode(&self) -> Result<i64, CacheError> {
        let mode: i64 = self
            .conn
            .query_row("PRAGMA synchronous", [], |row| row.get(0))?;
        Ok(mode)
    }

    /// Whether `PRAGMA foreign_keys` is on for this connection.
    pub fn foreign_keys_enabled(&self) -> Result<bool, CacheError> {
        let v: i64 = self
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))?;
        Ok(v != 0)
    }

    /// The current value of the single `schema_version` row, or `None`
    /// if the table is present but empty (should not happen once
    /// [`Cache::open`] has run at least once).
    pub fn schema_version(&self) -> Result<Option<i64>, CacheError> {
        schema::read_schema_version(&self.conn)
    }

    /// F2's single drop-and-recreate rebuild path: drops and recreates
    /// `units`/`links`/`tags`/`blocks` inside one transaction, then
    /// rewrites `schema_version` to [`CURRENT_SCHEMA_VERSION`]. Never
    /// touches `sources` — row count and contents are preserved across
    /// a rebuild, including a source's `offline`/`stale`/`active`
    /// status (E2). Any core-table row belonging to a currently
    /// `offline` source is carried across the drop/recreate rather
    /// than wiped (E2) — see [`schema::snapshot_offline_rows`].
    ///
    /// If `injector` forces a failure (test-only), or any step returns
    /// an error, the transaction is dropped without being committed,
    /// so SQLite rolls it back: the previous schema and all its rows
    /// are left exactly as they were, as if the rebuild never started.
    /// This is the one and only rebuild code path in the crate; both
    /// [`Cache::open`]'s automatic version-mismatch handling and direct
    /// test calls go through this same function.
    pub fn rebuild(&mut self, injector: &FaultInjector) -> Result<(), CacheError> {
        let mut tx = self.conn.transaction()?;
        perform_rebuild(&mut tx, injector)?;
        tx.commit()?;
        Ok(())
    }

    // ---- sources ----------------------------------------------------

    /// Registers a new source. Fails with [`CacheError::DuplicateSource`]
    /// if a source with this `name` already exists.
    pub fn register_source(
        &self,
        name: &str,
        path: &str,
        status: SourceStatus,
    ) -> Result<i64, CacheError> {
        let result = self.conn.execute(
            "INSERT INTO sources (name, path, status, last_scanned_at) VALUES (?1, ?2, ?3, NULL)",
            rusqlite::params![name, path, status.as_str()],
        );
        match result {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                Err(CacheError::DuplicateSource(name.to_string()))
            }
            Err(e) => Err(CacheError::from(e)),
        }
    }

    pub fn get_source(&self, name: &str) -> Result<Option<SourceRow>, CacheError> {
        sources::get_by_name(&self.conn, name)
    }

    pub fn list_sources(&self) -> Result<Vec<SourceRow>, CacheError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, path, status, last_scanned_at FROM sources ORDER BY id")?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let status_raw: String = row.get(3)?;
            out.push(sources::row_from_query(
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                status_raw,
                row.get(4)?,
            )?);
        }
        Ok(out)
    }

    /// Transitions the named source's status, validating the edge
    /// against [`sources::validate_transition`] first (G5: "documented
    /// transition function, no silent state-skipping"). Only the
    /// `status` column is touched — the source's `units`/`links`/
    /// `tags`/`blocks` rows are never read or written here (E2).
    pub fn transition_source_status(&self, name: &str, to: SourceStatus) -> Result<(), CacheError> {
        let current = self
            .get_source(name)?
            .ok_or_else(|| CacheError::SourceNotFound(name.to_string()))?;

        sources::validate_transition(current.status, to)
            .map_err(|e| CacheError::InvalidTransition(e.from, e.to))?;

        self.conn.execute(
            "UPDATE sources SET status = ?1 WHERE name = ?2",
            rusqlite::params![to.as_str(), name],
        )?;
        Ok(())
    }

    /// Records a completed scan's timestamp without otherwise changing
    /// status (callers typically pair this with
    /// `transition_source_status(name, Active)`).
    pub fn set_last_scanned(&self, name: &str, unix_timestamp: i64) -> Result<(), CacheError> {
        let updated = self.conn.execute(
            "UPDATE sources SET last_scanned_at = ?1 WHERE name = ?2",
            rusqlite::params![unix_timestamp, name],
        )?;
        if updated == 0 {
            return Err(CacheError::SourceNotFound(name.to_string()));
        }
        Ok(())
    }

    pub fn source_row_count(&self) -> Result<i64, CacheError> {
        units::count_rows(&self.conn, "sources")
    }

    /// Row count for one of the fixed internal table names (`sources`,
    /// `units`, `links`, `tags`, `blocks`, `schema_version`). `table`
    /// is checked against that closed allow-list and rejected with
    /// [`CacheError::InvalidTableName`] for anything else — this
    /// method is `pub` (tests and a future RPC layer call it), so the
    /// check is a real, always-enforced guard rather than a
    /// debug-only assertion.
    pub fn table_row_count(&self, table: &str) -> Result<i64, CacheError> {
        units::count_rows(&self.conn, table)
    }

    // ---- units/links/tags/blocks -------------------------------------

    pub fn insert_unit(&self, new: &NewUnit) -> Result<i64, CacheError> {
        units::insert_unit(&self.conn, new)
    }

    pub fn insert_link(&self, new: &NewLink) -> Result<i64, CacheError> {
        units::insert_link(&self.conn, new)
    }

    pub fn insert_tag(&self, new: &NewTag) -> Result<i64, CacheError> {
        units::insert_tag(&self.conn, new)
    }

    pub fn insert_block(&self, new: &NewBlock) -> Result<i64, CacheError> {
        units::insert_block(&self.conn, new)
    }

    pub fn get_unit(&self, id: i64) -> Result<Option<UnitRow>, CacheError> {
        units::get_unit(&self.conn, id)
    }

    /// The one `units` row at `(source_id, rel_path)`, if any.
    pub fn get_unit_by_rel_path(
        &self,
        source_id: i64,
        rel_path: &str,
    ) -> Result<Option<UnitRow>, CacheError> {
        units::get_unit_by_rel_path(&self.conn, source_id, rel_path)
    }

    /// Deletes one `units` row and its cascading `links`/`tags`/
    /// `blocks` rows. A no-op if `unit_id` doesn't exist.
    pub fn delete_unit(&self, unit_id: i64) -> Result<(), CacheError> {
        units::delete_unit(&self.conn, unit_id)
    }

    /// Deletes every `units`/`links`/`tags`/`blocks` row belonging to
    /// `source_id`, scoped to that one source — every other source's
    /// rows, and the `sources` row itself, are untouched. Used by
    /// `source.rescan` to clear a source's rows before re-walking it.
    pub fn delete_units_for_source(&self, source_id: i64) -> Result<(), CacheError> {
        units::delete_units_for_source(&self.conn, source_id)
    }

    /// Deletes every `units` row (and cascading `links`/`tags`/
    /// `blocks` rows) whose `rel_path` is nested under `rel_prefix`,
    /// scoped to `source_id`. See
    /// [`units::delete_units_under_prefix`] for the exact semantics
    /// (used by PR-4's watcher batch-apply path when a directory is
    /// moved/removed out from under a source). Returns the number of
    /// `units` rows removed.
    pub fn delete_units_under_prefix(
        &self,
        source_id: i64,
        rel_prefix: &str,
    ) -> Result<usize, CacheError> {
        units::delete_units_under_prefix(&self.conn, source_id, rel_prefix)
    }

    /// Deletes the named source's row and every `units`/`links`/
    /// `tags`/`blocks` row that belonged to it. Idempotent: a no-op
    /// (not an error) if no source with this name exists, so
    /// `source.unregister` can be called twice on an already-removed
    /// source without erroring.
    pub fn delete_source(&self, name: &str) -> Result<(), CacheError> {
        if let Some(row) = self.get_source(name)? {
            units::delete_units_for_source(&self.conn, row.id)?;
            self.conn
                .execute("DELETE FROM sources WHERE id = ?1", [row.id])?;
        }
        Ok(())
    }

    /// Every `units` row belonging to the named source, each paired
    /// with that source's current status. Returns the last-cached rows
    /// for an offline source (E2), never an empty result just because
    /// the source is offline.
    pub fn units_with_status_for_source(
        &self,
        source_name: &str,
    ) -> Result<Vec<UnitWithSourceStatus>, CacheError> {
        units::units_with_status_for_source(&self.conn, source_name)
    }

    /// Every `tags` row belonging to `unit_id`. Read counterpart to
    /// [`Cache::insert_tag`] — the typed read path a caller (e.g.
    /// `tephra-graph`'s graph-construction code, PR-5) uses instead of
    /// [`Cache::connection`]'s raw-SQL escape hatch.
    pub fn tags_for_unit(&self, unit_id: i64) -> Result<Vec<TagRow>, CacheError> {
        units::tags_for_unit(&self.conn, unit_id)
    }

    /// Every `blocks` row belonging to `unit_id`. Read counterpart to
    /// [`Cache::insert_block`]; see [`Cache::tags_for_unit`]'s doc
    /// comment.
    pub fn blocks_for_unit(&self, unit_id: i64) -> Result<Vec<BlockRow>, CacheError> {
        units::blocks_for_unit(&self.conn, unit_id)
    }

    /// Every `links` row authored by `from_unit_id`. Read counterpart
    /// to [`Cache::insert_link`]; see [`Cache::tags_for_unit`]'s doc
    /// comment. `to_unit_id` on the returned rows is not trustworthy
    /// as a resolved target — resolving a link against the registry
    /// is `tephra-graph`'s job (PR-5), done fresh from `to_raw`.
    pub fn links_from_unit(&self, from_unit_id: i64) -> Result<Vec<LinkRow>, CacheError> {
        units::links_from_unit(&self.conn, from_unit_id)
    }

    /// Direct access to the underlying connection, for tests and
    /// callers (e.g. a future RPC layer) that need SQL this typed API
    /// doesn't yet expose. Production code paths within this crate
    /// never need this: they go through the typed helpers above.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}

/// Applies the pragmas required on every new connection (G5): WAL mode
/// and `synchronous = NORMAL` for the cache's durability/perf trade-off,
/// `foreign_keys = ON` so the `units`/`links`/`tags`/`blocks` schema's
/// foreign keys back to `sources` (and to each other) are enforced
/// rather than merely documented, and a short busy timeout so one
/// connection's writer doesn't immediately fail a concurrent reader
/// under WAL with `SQLITE_BUSY`.
fn apply_pragmas(conn: &Connection) -> Result<(), CacheError> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}

/// Drops and recreates the core tables, preserving any row that
/// belongs to a currently-`offline` source across the drop/recreate
/// (E2), then rewrites `schema_version`. Shared by [`Cache::rebuild`]
/// and [`ensure_schema`]'s automatic version-mismatch branch so there
/// is exactly one rebuild code path in the crate (F2). `injector` is
/// the test-only fault-injection seam; every production call site
/// passes [`FaultInjector::none()`].
///
/// Offline-row carry-over is attempted only when
/// [`schema::offline_rows_are_carryable`] confirms the *old* (pre-drop)
/// tables already have every column the current schema's snapshot
/// query expects (R1 review finding). An older cache file opened by a
/// newer build — e.g. a `units` table from before `frontmatter_json`
/// existed — fails that check, so the snapshot is skipped rather than
/// attempted: the rebuild still unconditionally completes (drop,
/// recreate, rewrite `schema_version`), it just degrades to "a
/// currently-offline source's `sources` row and status survive, but
/// its cached `units`/`links`/`tags`/`blocks` rows don't" instead of
/// erroring `Cache::open` out entirely (which, before this fix, left
/// `schema_version` untouched and so failed identically on every
/// subsequent start).
///
/// A column-compatible old schema can still hold *data* the current
/// schema's stricter constraints reject (R4 review finding): a NULL
/// in a column that is now `NOT NULL` (snapshot fails reading the
/// row), or two old rows that collide under a `UNIQUE` the new schema
/// added (restore fails inserting the row). Both are handled the same
/// way the column-shape mismatch above is, so the rebuild still
/// always completes:
/// - If [`schema::snapshot_offline_rows`] itself returns `Err`, that
///   is treated exactly like "not carryable" — the carry-over is
///   skipped (`preserved = None`) rather than propagated.
/// - [`schema::restore_offline_rows`] runs inside its own SAVEPOINT.
///   If it errors, the SAVEPOINT is rolled back (discarding only the
///   partial restore) and the rebuild continues with empty core
///   tables for the degraded source; the outer transaction is
///   unaffected and `schema_version` is still rewritten below.
fn perform_rebuild(
    tx: &mut rusqlite::Transaction,
    injector: &FaultInjector,
) -> Result<(), CacheError> {
    let preserved = if schema::offline_rows_are_carryable(tx)? {
        // `.ok()`, not `?`: a column-compatible old table can still
        // hold data the current schema rejects while reading it back
        // (e.g. a NULL in a now-`NOT NULL` column) — that must not
        // brick the rebuild either, see the doc comment above.
        schema::snapshot_offline_rows(tx).ok()
    } else {
        None
    };
    schema::drop_core_tables(tx)?;
    if injector.fail_after_drop {
        return Err(CacheError::FaultInjected);
    }
    schema::create_core_tables(tx)?;
    if let Some(preserved) = preserved {
        let mut savepoint = tx.savepoint()?;
        match schema::restore_offline_rows(&savepoint, &preserved) {
            Ok(()) => savepoint.commit()?,
            Err(_) => savepoint.rollback()?,
        }
    }
    schema::write_schema_version(tx, CURRENT_SCHEMA_VERSION)?;
    Ok(())
}

/// Runs once per [`Cache::open`] call: ensures the bootstrap tables
/// exist, then either leaves an up-to-date schema untouched, bootstraps
/// a brand-new database, or rebuilds core tables on a version mismatch
/// (F2) — via [`perform_rebuild`], the same function [`Cache::rebuild`]
/// uses, so there is exactly one rebuild code path in the crate.
fn ensure_schema(conn: &Connection) -> Result<(), CacheError> {
    schema::create_bootstrap_tables(conn)?;

    match schema::read_schema_version(conn)? {
        None => {
            // First run against this database file: no schema_version
            // row yet. Create the core tables (idempotent) and write
            // the current version — not a "rebuild" since there is
            // nothing to drop.
            schema::create_core_tables_if_not_exists(conn)?;
            schema::write_schema_version(conn, CURRENT_SCHEMA_VERSION)?;
        }
        Some(v) if v == CURRENT_SCHEMA_VERSION => {
            // Version matches: proceed without touching existing
            // tables' data. `IF NOT EXISTS` here is purely defensive
            // against a prior partial failure; against a healthy
            // database it is a guaranteed no-op.
            schema::create_core_tables_if_not_exists(conn)?;
        }
        Some(_) => {
            let mut tx = conn.unchecked_transaction()?;
            perform_rebuild(&mut tx, &FaultInjector::none())?;
            tx.commit()?;
        }
    }

    Ok(())
}
