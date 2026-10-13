//! SQL DDL and the single drop-and-recreate rebuild path (F2).
//!
//! Two groups of tables:
//! - Bootstrap tables (`sources`, `schema_version`) are created once
//!   with `CREATE TABLE IF NOT EXISTS` and are never dropped, not even
//!   by a schema-version rebuild.
//! - Core tables (`units`, `links`, `tags`, `blocks`) hold derived,
//!   re-parseable data. On a schema-version mismatch they are
//!   unconditionally dropped and recreated — this crate has no
//!   migration-file mechanism, version ladder, or incremental
//!   `ALTER TABLE` path anywhere (F2): the only way core tables ever
//!   change shape is this one drop/recreate pair, gated by
//!   [`crate::CURRENT_SCHEMA_VERSION`].
//!
//! `sources` itself is never touched by a rebuild (F2). Rows in the
//! core tables that belong to a currently-`offline` source are an
//! exception to the "unconditionally" above: [`snapshot_offline_rows`]
//! and [`restore_offline_rows`] wrap the drop/recreate so an offline
//! source's cached index comes back byte-for-byte rather than being
//! wiped (E2) — the tables are still genuinely dropped and recreated
//! (satisfying F2's "no incremental migration path"), the offline
//! rows are just carried across that drop/recreate rather than lost.

use rusqlite::Connection;

use crate::units::{BlockRow, LinkRow, TagRow, UnitRow};
use crate::CacheError;

const CREATE_SOURCES: &str = "\
    CREATE TABLE IF NOT EXISTS sources (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        name TEXT NOT NULL UNIQUE, \
        path TEXT NOT NULL, \
        status TEXT NOT NULL CHECK (status IN ('active', 'stale', 'offline')), \
        last_scanned_at INTEGER \
    )";

const CREATE_SCHEMA_VERSION: &str = "\
    CREATE TABLE IF NOT EXISTS schema_version (\
        version INTEGER NOT NULL \
    )";

const CREATE_UNITS: &str = "\
    CREATE TABLE units (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        source_id INTEGER NOT NULL REFERENCES sources(id), \
        rel_path TEXT NOT NULL, \
        mtime INTEGER, \
        frontmatter_json TEXT, \
        UNIQUE(source_id, rel_path) \
    )";

const CREATE_LINKS: &str = "\
    CREATE TABLE links (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        from_unit_id INTEGER NOT NULL REFERENCES units(id), \
        to_raw TEXT NOT NULL, \
        to_unit_id INTEGER REFERENCES units(id), \
        kind TEXT NOT NULL \
    )";

const CREATE_TAGS: &str = "\
    CREATE TABLE tags (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        unit_id INTEGER NOT NULL REFERENCES units(id), \
        tag TEXT NOT NULL \
    )";

const CREATE_BLOCKS: &str = "\
    CREATE TABLE blocks (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        unit_id INTEGER NOT NULL REFERENCES units(id), \
        block_id TEXT NOT NULL, \
        line INTEGER NOT NULL \
    )";

/// Creates `sources` and `schema_version` if absent. Idempotent and
/// safe to call on every open: never drops or truncates either table.
pub(crate) fn create_bootstrap_tables(conn: &Connection) -> Result<(), CacheError> {
    conn.execute(CREATE_SOURCES, [])?;
    conn.execute(CREATE_SCHEMA_VERSION, [])?;
    Ok(())
}

/// Creates `units`/`links`/`tags`/`blocks` only if they don't already
/// exist. Used on first run (after a fresh `schema_version` insert) and
/// defensively on a version-match open, where it is a guaranteed no-op
/// against an already-populated schema.
pub(crate) fn create_core_tables_if_not_exists(conn: &Connection) -> Result<(), CacheError> {
    for create in core_table_ddls_if_not_exists() {
        conn.execute(&create, [])?;
    }
    Ok(())
}

fn core_table_ddls_if_not_exists() -> [String; 4] {
    [
        CREATE_UNITS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_LINKS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_TAGS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_BLOCKS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
    ]
}

/// Drops `units`/`links`/`tags`/`blocks` (children first, parents
/// last, though `DROP TABLE` in SQLite does not itself enforce foreign
/// keys). Never touches `sources` or `schema_version`.
pub(crate) fn drop_core_tables(conn: &Connection) -> Result<(), CacheError> {
    conn.execute("DROP TABLE IF EXISTS blocks", [])?;
    conn.execute("DROP TABLE IF EXISTS tags", [])?;
    conn.execute("DROP TABLE IF EXISTS links", [])?;
    conn.execute("DROP TABLE IF EXISTS units", [])?;
    Ok(())
}

/// Recreates `units`/`links`/`tags`/`blocks` from the current schema,
/// unconditionally. Only ever called immediately after
/// [`drop_core_tables`] within the same transaction.
pub(crate) fn create_core_tables(conn: &Connection) -> Result<(), CacheError> {
    conn.execute(CREATE_UNITS, [])?;
    conn.execute(CREATE_LINKS, [])?;
    conn.execute(CREATE_TAGS, [])?;
    conn.execute(CREATE_BLOCKS, [])?;
    Ok(())
}

/// Every row from `units`/`links`/`tags`/`blocks` that belongs,
/// directly or transitively, to a currently-`offline` source. Read
/// before [`drop_core_tables`] runs, and fed back to
/// [`restore_offline_rows`] after [`create_core_tables`], so an
/// offline source's cached index survives a schema-version rebuild
/// byte-for-byte (E2) even though the tables themselves are still
/// unconditionally dropped and recreated (F2) — the rebuild still has
/// exactly one code path; this is a snapshot/restore wrapped around
/// it, not a second mutation path.
#[derive(Debug, Default)]
pub(crate) struct OfflineRows {
    units: Vec<UnitRow>,
    links: Vec<LinkRow>,
    tags: Vec<TagRow>,
    blocks: Vec<BlockRow>,
}

pub(crate) fn snapshot_offline_rows(conn: &Connection) -> Result<OfflineRows, CacheError> {
    let mut out = OfflineRows::default();

    let mut units_stmt = conn.prepare(
        "SELECT u.id, u.source_id, u.rel_path, u.mtime, u.frontmatter_json \
         FROM units u JOIN sources s ON s.id = u.source_id \
         WHERE s.status = 'offline'",
    )?;
    let mut rows = units_stmt.query([])?;
    while let Some(row) = rows.next()? {
        out.units.push(UnitRow {
            id: row.get(0)?,
            source_id: row.get(1)?,
            rel_path: row.get(2)?,
            mtime: row.get(3)?,
            frontmatter_json: row.get(4)?,
        });
    }

    let mut links_stmt = conn.prepare(
        "SELECT l.id, l.from_unit_id, l.to_raw, l.to_unit_id, l.kind \
         FROM links l \
         JOIN units u ON u.id = l.from_unit_id \
         JOIN sources s ON s.id = u.source_id \
         WHERE s.status = 'offline'",
    )?;
    let mut rows = links_stmt.query([])?;
    while let Some(row) = rows.next()? {
        out.links.push(LinkRow {
            id: row.get(0)?,
            from_unit_id: row.get(1)?,
            to_raw: row.get(2)?,
            to_unit_id: row.get(3)?,
            kind: row.get(4)?,
        });
    }

    let mut tags_stmt = conn.prepare(
        "SELECT t.id, t.unit_id, t.tag \
         FROM tags t \
         JOIN units u ON u.id = t.unit_id \
         JOIN sources s ON s.id = u.source_id \
         WHERE s.status = 'offline'",
    )?;
    let mut rows = tags_stmt.query([])?;
    while let Some(row) = rows.next()? {
        out.tags.push(TagRow {
            id: row.get(0)?,
            unit_id: row.get(1)?,
            tag: row.get(2)?,
        });
    }

    let mut blocks_stmt = conn.prepare(
        "SELECT b.id, b.unit_id, b.block_id, b.line \
         FROM blocks b \
         JOIN units u ON u.id = b.unit_id \
         JOIN sources s ON s.id = u.source_id \
         WHERE s.status = 'offline'",
    )?;
    let mut rows = blocks_stmt.query([])?;
    while let Some(row) = rows.next()? {
        out.blocks.push(BlockRow {
            id: row.get(0)?,
            unit_id: row.get(1)?,
            block_id: row.get(2)?,
            line: row.get(3)?,
        });
    }

    Ok(out)
}

/// Reinserts exactly the rows [`snapshot_offline_rows`] captured,
/// preserving their original ids so any link that targeted one of
/// these units (`to_unit_id`) still resolves after the rebuild. A link
/// whose original `to_unit_id` pointed at a unit that was *not*
/// preserved (i.e. belonged to a source that was not offline, and so
/// was genuinely wiped by this rebuild) is restored with `to_unit_id`
/// set to `NULL` instead — it becomes a dangling/unresolved link
/// rather than a foreign-key violation, consistent with how an
/// unresolved link is represented elsewhere in this schema.
pub(crate) fn restore_offline_rows(
    conn: &Connection,
    rows: &OfflineRows,
) -> Result<(), CacheError> {
    for u in &rows.units {
        conn.execute(
            "INSERT INTO units (id, source_id, rel_path, mtime, frontmatter_json) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![u.id, u.source_id, u.rel_path, u.mtime, u.frontmatter_json],
        )?;
    }

    let preserved_unit_ids: std::collections::HashSet<i64> =
        rows.units.iter().map(|u| u.id).collect();

    for l in &rows.links {
        let to_unit_id = l.to_unit_id.filter(|id| preserved_unit_ids.contains(id));
        conn.execute(
            "INSERT INTO links (id, from_unit_id, to_raw, to_unit_id, kind) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![l.id, l.from_unit_id, l.to_raw, to_unit_id, l.kind],
        )?;
    }

    for t in &rows.tags {
        conn.execute(
            "INSERT INTO tags (id, unit_id, tag) VALUES (?1, ?2, ?3)",
            rusqlite::params![t.id, t.unit_id, t.tag],
        )?;
    }

    for b in &rows.blocks {
        conn.execute(
            "INSERT INTO blocks (id, unit_id, block_id, line) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![b.id, b.unit_id, b.block_id, b.line],
        )?;
    }

    Ok(())
}

/// Reads the single `schema_version` row, if one exists yet. `None`
/// means the table is present but empty — i.e. this is the very first
/// run against this database file.
pub(crate) fn read_schema_version(conn: &Connection) -> Result<Option<i64>, CacheError> {
    let version: Option<i64> = conn
        .query_row("SELECT version FROM schema_version LIMIT 1", [], |row| {
            row.get(0)
        })
        .map(Some)
        .or_else(|e| {
            if matches!(e, rusqlite::Error::QueryReturnedNoRows) {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
    Ok(version)
}

/// Overwrites the single `schema_version` row with `version`, deleting
/// any prior row first so there is always exactly one.
pub(crate) fn write_schema_version(conn: &Connection, version: i64) -> Result<(), CacheError> {
    conn.execute("DELETE FROM schema_version", [])?;
    conn.execute(
        "INSERT INTO schema_version (version) VALUES (?1)",
        [version],
    )?;
    Ok(())
}
