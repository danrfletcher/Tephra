//! `units`/`links`/`tags`/`blocks` row types and insert/query helpers.
//!
//! All four tables carry a foreign key back toward `sources`: `units`
//! directly via `source_id`, and `links`/`tags`/`blocks` indirectly —
//! every `unit_id` (or `from_unit_id`/`to_unit_id`) they reference must
//! itself belong to a real `units` row, which in turn must belong to a
//! real `sources` row. `PRAGMA foreign_keys = ON` (set on every
//! connection open, see `cache.rs`) enforces every link in that chain.

use rusqlite::{Connection, OptionalExtension};

use crate::sources::SourceStatus;
use crate::CacheError;

#[derive(Debug, Clone, PartialEq)]
pub struct UnitRow {
    pub id: i64,
    pub source_id: i64,
    pub rel_path: String,
    pub mtime: Option<i64>,
    pub frontmatter_json: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewUnit<'a> {
    pub source_id: i64,
    pub rel_path: &'a str,
    pub mtime: Option<i64>,
    pub frontmatter_json: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkRow {
    pub id: i64,
    pub from_unit_id: i64,
    pub to_raw: String,
    pub to_unit_id: Option<i64>,
    pub kind: String,
}

#[derive(Debug, Clone, Copy)]
pub struct NewLink<'a> {
    pub from_unit_id: i64,
    pub to_raw: &'a str,
    pub to_unit_id: Option<i64>,
    pub kind: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagRow {
    pub id: i64,
    pub unit_id: i64,
    pub tag: String,
}

#[derive(Debug, Clone, Copy)]
pub struct NewTag<'a> {
    pub unit_id: i64,
    pub tag: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BlockRow {
    pub id: i64,
    pub unit_id: i64,
    pub block_id: String,
    pub line: i64,
}

#[derive(Debug, Clone, Copy)]
pub struct NewBlock<'a> {
    pub unit_id: i64,
    pub block_id: &'a str,
    pub line: i64,
}

/// A `units` row paired with its owning source's current status, so a
/// caller querying across an offline source can tell the rows are
/// cached-but-stale (E2) rather than current.
#[derive(Debug, Clone, PartialEq)]
pub struct UnitWithSourceStatus {
    pub unit: UnitRow,
    pub source_status: SourceStatus,
}

pub(crate) fn insert_unit(conn: &Connection, new: &NewUnit) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO units (source_id, rel_path, mtime, frontmatter_json) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![new.source_id, new.rel_path, new.mtime, new.frontmatter_json],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn insert_link(conn: &Connection, new: &NewLink) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO links (from_unit_id, to_raw, to_unit_id, kind) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![new.from_unit_id, new.to_raw, new.to_unit_id, new.kind],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn insert_tag(conn: &Connection, new: &NewTag) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO tags (unit_id, tag) VALUES (?1, ?2)",
        rusqlite::params![new.unit_id, new.tag],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn insert_block(conn: &Connection, new: &NewBlock) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO blocks (unit_id, block_id, line) VALUES (?1, ?2, ?3)",
        rusqlite::params![new.unit_id, new.block_id, new.line],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn get_unit(conn: &Connection, id: i64) -> Result<Option<UnitRow>, CacheError> {
    conn.query_row(
        "SELECT id, source_id, rel_path, mtime, frontmatter_json FROM units WHERE id = ?1",
        [id],
        |row| {
            Ok(UnitRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                rel_path: row.get(2)?,
                mtime: row.get(3)?,
                frontmatter_json: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(CacheError::from)
}

/// The one `units` row at `(source_id, rel_path)`, if any — the
/// `UNIQUE(source_id, rel_path)` constraint guarantees at most one.
/// Used by a caller (e.g. the watcher's debounced change-apply
/// pipeline, PR-4) that knows a file's path but not its row id, to
/// find the existing row it needs to replace on an edit.
pub(crate) fn get_unit_by_rel_path(
    conn: &Connection,
    source_id: i64,
    rel_path: &str,
) -> Result<Option<UnitRow>, CacheError> {
    conn.query_row(
        "SELECT id, source_id, rel_path, mtime, frontmatter_json \
         FROM units WHERE source_id = ?1 AND rel_path = ?2",
        rusqlite::params![source_id, rel_path],
        |row| {
            Ok(UnitRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                rel_path: row.get(2)?,
                mtime: row.get(3)?,
                frontmatter_json: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(CacheError::from)
}

/// Deletes one `units` row and everything that cascades from it:
/// `links` rows it authored (`from_unit_id = unit_id`), its `tags`
/// and `blocks` rows, and — defensively, since nothing in this
/// workspace resolves `to_unit_id` yet (that's PR-5's job) — any
/// `links.to_unit_id` elsewhere that happened to point at it is first
/// nulled out rather than left dangling or rejected by the foreign
/// key. A no-op (not an error) if `unit_id` doesn't exist.
pub(crate) fn delete_unit(conn: &Connection, unit_id: i64) -> Result<(), CacheError> {
    conn.execute(
        "UPDATE links SET to_unit_id = NULL WHERE to_unit_id = ?1",
        [unit_id],
    )?;
    conn.execute("DELETE FROM links WHERE from_unit_id = ?1", [unit_id])?;
    conn.execute("DELETE FROM tags WHERE unit_id = ?1", [unit_id])?;
    conn.execute("DELETE FROM blocks WHERE unit_id = ?1", [unit_id])?;
    conn.execute("DELETE FROM units WHERE id = ?1", [unit_id])?;
    Ok(())
}

/// Deletes every `units`/`links`/`tags`/`blocks` row belonging to
/// `source_id`, leaving every other source's rows (and the `sources`
/// row itself) untouched — the scoped clear a rescan uses before
/// re-walking just this one source (never a global rebuild; that's
/// [`crate::Cache::rebuild`]'s job, gated by schema version, F2).
pub(crate) fn delete_units_for_source(conn: &Connection, source_id: i64) -> Result<(), CacheError> {
    conn.execute(
        "UPDATE links SET to_unit_id = NULL \
         WHERE to_unit_id IN (SELECT id FROM units WHERE source_id = ?1)",
        [source_id],
    )?;
    conn.execute(
        "DELETE FROM links WHERE from_unit_id IN (SELECT id FROM units WHERE source_id = ?1)",
        [source_id],
    )?;
    conn.execute(
        "DELETE FROM tags WHERE unit_id IN (SELECT id FROM units WHERE source_id = ?1)",
        [source_id],
    )?;
    conn.execute(
        "DELETE FROM blocks WHERE unit_id IN (SELECT id FROM units WHERE source_id = ?1)",
        [source_id],
    )?;
    conn.execute("DELETE FROM units WHERE source_id = ?1", [source_id])?;
    Ok(())
}

/// Deletes every `units` row (and cascading `links`/`tags`/`blocks`
/// rows) whose `rel_path` is nested under `rel_prefix` — i.e. starts
/// with `"<rel_prefix>/"` — scoped to `source_id`. Leaves a row at
/// `rel_prefix` itself (an exact match, not a nested one) untouched;
/// callers that also need that exact row gone call
/// [`delete_unit`]/[`get_unit_by_rel_path`] separately.
///
/// Used by the watcher's batch-apply path (PR-4) when a changed path
/// no longer exists on disk and was a directory rather than a file: a
/// shell `mv` of a directory out of a source arrives as exactly one
/// such event, naming only the directory itself, so every unit row
/// that used to live under it must be cleaned up explicitly or it
/// would be left behind as a stale row forever. Returns the number of
/// `units` rows removed, so a caller can tell whether anything was
/// actually there to delete.
///
/// Matches via a plain binary range comparison, not `LIKE` (R10
/// review finding): SQLite's `LIKE` is case-insensitive for ASCII by
/// default (so a sibling directory differing only in case, e.g.
/// `Docs` vs `docs`, would also match) and treats a bare `_` or `%` in
/// the pattern as a wildcard rather than a literal character (so a
/// real directory named `a_b` or `a%` would delete unrelated rows
/// under `acb`/`aQQ` or any single-character variant) — neither
/// requires an actual nested path, just an unlucky name. Every
/// `rel_path` nested under `rel_prefix` starts with
/// `"<rel_prefix>/"`; in a byte-wise (`BINARY`, SQLite's default
/// collation) comparison, every such string sorts at or after
/// `"<rel_prefix>/"` and strictly before `"<rel_prefix>0"`, because
/// `/` (0x2F) sorts immediately before `0` (0x30) and nothing can sort
/// between two adjacent byte values — so this range exactly (and only
/// ever) captures paths nested under `rel_prefix`, with no escaping
/// and no case-folding surprises.
pub(crate) fn delete_units_under_prefix(
    conn: &Connection,
    source_id: i64,
    rel_prefix: &str,
) -> Result<usize, CacheError> {
    let lower = format!("{rel_prefix}/");
    let upper = format!("{rel_prefix}0");
    conn.execute(
        "UPDATE links SET to_unit_id = NULL WHERE to_unit_id IN \
         (SELECT id FROM units WHERE source_id = ?1 AND rel_path >= ?2 AND rel_path < ?3)",
        rusqlite::params![source_id, lower, upper],
    )?;
    conn.execute(
        "DELETE FROM links WHERE from_unit_id IN \
         (SELECT id FROM units WHERE source_id = ?1 AND rel_path >= ?2 AND rel_path < ?3)",
        rusqlite::params![source_id, lower, upper],
    )?;
    conn.execute(
        "DELETE FROM tags WHERE unit_id IN \
         (SELECT id FROM units WHERE source_id = ?1 AND rel_path >= ?2 AND rel_path < ?3)",
        rusqlite::params![source_id, lower, upper],
    )?;
    conn.execute(
        "DELETE FROM blocks WHERE unit_id IN \
         (SELECT id FROM units WHERE source_id = ?1 AND rel_path >= ?2 AND rel_path < ?3)",
        rusqlite::params![source_id, lower, upper],
    )?;
    let deleted = conn.execute(
        "DELETE FROM units WHERE source_id = ?1 AND rel_path >= ?2 AND rel_path < ?3",
        rusqlite::params![source_id, lower, upper],
    )?;
    Ok(deleted)
}

/// Every `units` row belonging to the source named `source_name`,
/// each paired with that source's current status. Returns cached rows
/// for an offline source too (E2) rather than an empty result — the
/// WHERE clause filters only by source name, never by status.
pub(crate) fn units_with_status_for_source(
    conn: &Connection,
    source_name: &str,
) -> Result<Vec<UnitWithSourceStatus>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT u.id, u.source_id, u.rel_path, u.mtime, u.frontmatter_json, s.status \
         FROM units u JOIN sources s ON s.id = u.source_id \
         WHERE s.name = ?1 ORDER BY u.id",
    )?;
    let mut rows = stmt.query([source_name])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let status_raw: String = row.get(5)?;
        let source_status = SourceStatus::parse(&status_raw)
            .ok_or_else(|| CacheError::CorruptStatus(status_raw.clone()))?;
        out.push(UnitWithSourceStatus {
            unit: UnitRow {
                id: row.get(0)?,
                source_id: row.get(1)?,
                rel_path: row.get(2)?,
                mtime: row.get(3)?,
                frontmatter_json: row.get(4)?,
            },
            source_status,
        });
    }
    Ok(out)
}

/// The closed set of real table names this crate ever counts rows in.
/// [`count_rows`] checks `table` against this allow-list with a real,
/// always-enforced `if` (not a `debug_assert!`, which compiles out of
/// release builds) before it ever reaches `format!`'d SQL: `table` is
/// documented as always being one of these fixed internal names, but
/// [`crate::Cache::table_row_count`] is `pub` and reachable from a
/// future RPC layer, so an unexpected value must be rejected rather
/// than merely asserted against in debug builds.
const ALLOWED_TABLE_NAMES: [&str; 6] = [
    "sources",
    "units",
    "links",
    "tags",
    "blocks",
    "schema_version",
];

pub(crate) fn count_rows(conn: &Connection, table: &str) -> Result<i64, CacheError> {
    if !ALLOWED_TABLE_NAMES.contains(&table) {
        return Err(CacheError::InvalidTableName(table.to_string()));
    }
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(count)
}
