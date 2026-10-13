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
//!
//! That carry-over is itself gated by [`offline_rows_are_carryable`]:
//! an older cache file, opened by a newer build after the schema
//! grew a column, may be missing a column the current snapshot query
//! names, which would otherwise fail to even prepare. In that case
//! the carry-over is skipped (not attempted at all) rather than
//! erroring the whole rebuild out — the source's `sources` row and
//! status are still preserved either way; only its cached rows are
//! lost in that degraded case, same as for any non-offline source.
//!
//! A column-compatible old table can still hold *data* the current
//! schema's stricter constraints reject: a NULL in a column that is
//! now `NOT NULL`, or two rows that collide under a `UNIQUE` the new
//! schema added. [`snapshot_offline_rows`] and [`restore_offline_rows`]
//! can therefore still fail even when [`offline_rows_are_carryable`]
//! returned `true`. The caller (`cache::perform_rebuild`) treats a
//! snapshot failure the same as "not carryable" (skips the
//! carry-over), and runs the restore inside its own SAVEPOINT so a
//! restore failure only discards that partial restore rather than the
//! whole rebuild — see that function's doc comment for the full
//! picture.

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
///
/// Callers must first confirm [`offline_rows_are_carryable`] returns
/// `true` for the connection's *current* (pre-drop) table shapes.
/// This function's `SELECT`s name every column the current schema
/// expects (e.g. `u.frontmatter_json`); against an older cache opened
/// by a newer build, one of those columns may not exist yet, and a
/// statement naming a nonexistent column fails to even *prepare* —
/// before the `WHERE status = 'offline'` filter could matter, even
/// when nothing is offline. [`offline_rows_are_carryable`] checks
/// column presence first so that case is skipped (no snapshot
/// attempted, and so no error) rather than bricking the rebuild.
///
/// Even when every expected column is present, a row's *value* can
/// still be incompatible with the current schema (e.g. a NULL in
/// `blocks.line`, which is `NOT NULL` in the current schema and so
/// fails `row.get::<i64>`). The caller treats any `Err` from this
/// function identically to a failed [`offline_rows_are_carryable`]
/// check: the carry-over is skipped and the rebuild proceeds with
/// empty core tables for the affected source rather than failing.
#[derive(Debug, Default)]
pub(crate) struct OfflineRows {
    units: Vec<UnitRow>,
    links: Vec<LinkRow>,
    tags: Vec<TagRow>,
    blocks: Vec<BlockRow>,
}

/// The exact column names [`snapshot_offline_rows`]'s `SELECT`s and
/// [`restore_offline_rows`]'s `INSERT`s reference, by table, in the
/// current schema (see the `CREATE_*` constants above). Checked via
/// `PRAGMA table_info` against the *old*, pre-rebuild table shapes
/// before a rebuild ever attempts to snapshot offline rows.
const UNITS_SNAPSHOT_COLUMNS: [&str; 5] =
    ["id", "source_id", "rel_path", "mtime", "frontmatter_json"];
const LINKS_SNAPSHOT_COLUMNS: [&str; 5] = ["id", "from_unit_id", "to_raw", "to_unit_id", "kind"];
const TAGS_SNAPSHOT_COLUMNS: [&str; 3] = ["id", "unit_id", "tag"];
const BLOCKS_SNAPSHOT_COLUMNS: [&str; 4] = ["id", "unit_id", "block_id", "line"];

/// Whether `table` exists and has every column named in `columns`,
/// checked via `PRAGMA table_info` rather than assumed. `table` is
/// always one of this module's own fixed internal table names, never
/// caller- or network-supplied input.
fn table_has_columns(conn: &Connection, table: &str, columns: &[&str]) -> Result<bool, CacheError> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let mut existing: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        existing.insert(row.get::<_, String>(1)?);
    }
    if existing.is_empty() {
        // `PRAGMA table_info` returns zero rows both when the table
        // genuinely doesn't exist and when it exists but has no
        // columns (impossible for a real table) — either way, this
        // old table's rows can't be carried across.
        return Ok(false);
    }
    Ok(columns.iter().all(|c| existing.contains(*c)))
}

/// Gate for [`snapshot_offline_rows`]: `true` only if the *current*
/// (pre-drop) `units`/`links`/`tags`/`blocks` tables already have
/// every column this crate's current schema expects. `false` means an
/// older cache file (opened by a newer build after the schema grew a
/// column, as in the R1 review repro: a `units` table with no
/// `frontmatter_json`) is being rebuilt — in that case the rebuild
/// still proceeds (drop, recreate, rewrite `schema_version`; F2's
/// single code path is unaffected), it just skips the offline-row
/// carry-over rather than failing outright. A source's `sources` row
/// and its `offline`/`stale`/`active` status are preserved either way
/// (`perform_rebuild` never touches `sources`); only that source's
/// `units`/`links`/`tags`/`blocks` rows are lost in this degraded
/// path, same as for a non-offline source today.
pub(crate) fn offline_rows_are_carryable(conn: &Connection) -> Result<bool, CacheError> {
    Ok(table_has_columns(conn, "units", &UNITS_SNAPSHOT_COLUMNS)?
        && table_has_columns(conn, "links", &LINKS_SNAPSHOT_COLUMNS)?
        && table_has_columns(conn, "tags", &TAGS_SNAPSHOT_COLUMNS)?
        && table_has_columns(conn, "blocks", &BLOCKS_SNAPSHOT_COLUMNS)?)
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
///
/// A row that was successfully snapshotted can still fail to insert
/// here if it violates a constraint the *current* schema added that
/// the old one didn't have (e.g. two old `units` rows sharing a
/// `(source_id, rel_path)` that only the current schema's `UNIQUE`
/// rejects). The caller (`cache::perform_rebuild`) runs this function
/// inside a SAVEPOINT and rolls back to it on error, so such a
/// failure discards only this partial restore rather than the whole
/// rebuild; this function itself has no partial-success behavior — it
/// either inserts everything in `rows` or returns the first error.
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
