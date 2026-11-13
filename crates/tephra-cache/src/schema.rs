//! SQL DDL and the single drop-and-recreate rebuild path (F2).
//!
//! Two groups of tables:
//! - Bootstrap tables (`sources`, `schema_version`, `scan_state`) are
//!   created once with `CREATE TABLE IF NOT EXISTS` and are never
//!   dropped, not even by a schema-version rebuild.
//! - Core tables (`units`, `links`, `tags`, `blocks`, `headings`,
//!   `symbols`, the `block_fts` FTS5 index and its `block_names`
//!   exact-name table) hold derived,
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
//! The snapshot reads only the column set every schema version has
//! had (the v1 columns): anything a newer schema added (`kind`,
//! `size`, `too_large`, `needs_rescan`, `blocks.end_line`, the
//! `headings`/`symbols`/`block_fts` tables) is not read, because an
//! offline source cannot be re-read from disk. Restored `units` rows
//! get a `kind` inferred from the extension alone (via the
//! [`KindInferrer`] the [`crate::Cache`] was opened with), a `NULL`
//! `size`, and `needs_rescan = 1`, so the next online rescan knows to
//! fill them in (E10).
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

use crate::units::{BlockRow, LinkRow, TagRow};
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

/// Per-source scan bookkeeping (E11). Bootstrap table like `sources`:
/// created `IF NOT EXISTS` and never dropped. A row with a NULL
/// `finished_at` is a scan that started and never finished, which
/// `tephrad` rescans from the top on its next start. There is no
/// resume logic.
const CREATE_SCAN_STATE: &str = "\
    CREATE TABLE IF NOT EXISTS scan_state (\
        source_id INTEGER PRIMARY KEY REFERENCES sources(id), \
        started_at INTEGER NOT NULL, \
        finished_at INTEGER \
    )";

const CREATE_UNITS: &str = "\
    CREATE TABLE units (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        source_id INTEGER NOT NULL REFERENCES sources(id), \
        rel_path TEXT NOT NULL, \
        mtime INTEGER, \
        frontmatter_json TEXT, \
        kind TEXT NOT NULL CHECK (kind IN ('prose','code','document','text-other','binary')), \
        size INTEGER, \
        too_large INTEGER NOT NULL DEFAULT 0, \
        needs_rescan INTEGER NOT NULL DEFAULT 0, \
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

/// One row per block of a unit: a prose paragraph (`line` is its first
/// line, `end_line` its last) or a bare `^id` occurrence. `block_id`
/// is NULL for a paragraph with no `^id`. There is no uniqueness on
/// `(unit_id, block_id)`: every occurrence of an ID is stored.
///
/// A block ID in a code file (G13) is attached to the symbol that owns
/// it: `symbol_id` points at that `symbols` row and `line` is the
/// symbol's first line. An ID comment with no symbol to own it is a
/// block on its own line with `symbol_id` NULL. Prose rows have no
/// symbol. `blocks` references `symbols`, so `symbols` is created
/// first and dropped last of the two.
const CREATE_BLOCKS: &str = "\
    CREATE TABLE blocks (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        unit_id INTEGER NOT NULL REFERENCES units(id), \
        block_id TEXT, \
        line INTEGER NOT NULL, \
        end_line INTEGER, \
        symbol_id INTEGER REFERENCES symbols(id) \
    )";

const CREATE_HEADINGS: &str = "\
    CREATE TABLE headings (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        unit_id INTEGER NOT NULL REFERENCES units(id), \
        level INTEGER NOT NULL, \
        text TEXT NOT NULL, \
        line INTEGER NOT NULL \
    )";

/// One row per symbol of a `code` unit (G4): `kind`, `name`, the
/// enclosing symbol's name (`parent`, NULL at the top level), the
/// definition's first line (`line`, the line that carries a block ID)
/// and the symbol's whole line range `start_line..=end_line`, which
/// also takes in the attributes, decorators and doc comments directly
/// above the definition (so `start_line <= line <= end_line`). All
/// lines are 1-based. There is no uniqueness on `(unit_id, name)`: an
/// `enum X` and an `impl X`, or overloaded methods, each keep a row.
const CREATE_SYMBOLS: &str = "\
    CREATE TABLE symbols (\
        id INTEGER PRIMARY KEY AUTOINCREMENT, \
        unit_id INTEGER NOT NULL REFERENCES units(id), \
        kind TEXT NOT NULL, \
        name TEXT NOT NULL, \
        parent TEXT, \
        line INTEGER NOT NULL, \
        start_line INTEGER NOT NULL, \
        end_line INTEGER NOT NULL \
    )";

/// Trigram FTS5 index for the `^^` search (G10): one row per paragraph,
/// comment block, symbol name, heading and document paragraph of a unit.
///
/// Only `text` is tokenized (the trigram tokenizer, so `conflict` finds
/// `ConflictReason`; a symbol's text also carries its name split on
/// camelCase/snake_case). Every other column is stored and never
/// searched (`UNINDEXED`): `unit_id` and `line` place the row, `kind`
/// is `paragraph`, `comment`, `symbol` or `heading`, `block_id` is the
/// `^id` the block carries, `name` is the bare symbol or heading name
/// (what an exact-name match boosts),
/// `label` is the text a symbol is shown as (`enum ConflictReason`),
/// and a document row has `place` (`page`, `slide` or `paragraph`),
/// `place_n` and `idx` (its place within a page or slide) instead of a
/// `line`.
///
/// A virtual table cannot carry a foreign key or a regular index, so a
/// unit's rows are found by `rowid` instead: [`crate::fts`] gives a
/// unit's rows the rowids `unit_id << 24 ..= (unit_id << 24) + 2^24 - 1`,
/// and every delete helper in `units.rs` removes that range (a plain
/// rowid range scan, where a `WHERE unit_id = ?` over an UNINDEXED
/// column would read the whole table once per file).
const CREATE_BLOCK_FTS: &str = "\
    CREATE VIRTUAL TABLE block_fts USING fts5(\
        unit_id UNINDEXED, \
        line UNINDEXED, \
        kind UNINDEXED, \
        block_id UNINDEXED, \
        name UNINDEXED, \
        label UNINDEXED, \
        place UNINDEXED, \
        place_n UNINDEXED, \
        idx UNINDEXED, \
        text, \
        tokenize = 'trigram'\
    )";

/// The lower-cased (ASCII) name of every `block_fts` row that has one (a
/// symbol or a heading), keyed by that row's rowid, so the `^^` search
/// finds the rows named exactly like the query from an index instead of
/// by scanning every match. Written and deleted with the `block_fts`
/// rows, by the same functions.
const CREATE_BLOCK_NAMES: &str = "\
    CREATE TABLE block_names (\
        fts_rowid INTEGER PRIMARY KEY, \
        name_lower TEXT NOT NULL\
    )";

/// Indexes backing the per-unit cascading deletes in `units.rs`
/// (`delete_unit`, `delete_units_for_source`,
/// `delete_units_under_prefix`), each of which walks `links` by both
/// `from_unit_id` and `to_unit_id`, `tags` by `unit_id`, and `blocks`
/// by `unit_id` (R17 review finding). Without these, every one of
/// those lookups was a full table scan: deleting a changed file's
/// stale row (PR-4's debounced watcher path, run once per edited
/// file) cost O(the *entire* cache's links/tags/blocks row count),
/// not O(that one file's own rows) — the review's probe measured 100
/// modified files at 0.86s and 1000 at 8.2s against a 100k-file
/// source, long enough to hold SQLite's single writer lock past the
/// 5-second `busy_timeout` for an ordinary `git checkout`-sized burst
/// (E5), stalling every other source's watcher and the registry's own
/// `list`/`register`/`rescan` calls.
///
/// `IF NOT EXISTS`: created both by [`create_core_tables`] (right
/// after a schema-version rebuild's drop+recreate, since `DROP TABLE`
/// also drops its indexes) and by [`create_core_tables_if_not_exists`]
/// (so an already-up-to-date database gets these indexes backfilled
/// the next time it's opened, with no schema-version bump and no
/// separate migration script — consistent with F2's "no incremental
/// migration logic" fence: this is the same idempotent `IF NOT
/// EXISTS` DDL pattern already used for the core tables themselves on
/// that same code path).
const CREATE_IDX_LINKS_FROM_UNIT_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_links_from_unit_id ON links(from_unit_id)";
const CREATE_IDX_LINKS_TO_UNIT_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_links_to_unit_id ON links(to_unit_id)";
const CREATE_IDX_TAGS_UNIT_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_tags_unit_id ON tags(unit_id)";
const CREATE_IDX_BLOCKS_UNIT_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_blocks_unit_id ON blocks(unit_id)";
const CREATE_IDX_HEADINGS_UNIT_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_headings_unit_id ON headings(unit_id)";
const CREATE_IDX_SYMBOLS_UNIT_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_symbols_unit_id ON symbols(unit_id)";
/// Backs the exact-name lookup of the `^^` search.
const CREATE_IDX_BLOCK_NAMES_NAME: &str =
    "CREATE INDEX IF NOT EXISTS idx_block_names_name ON block_names(name_lower)";
/// Backs `blocks_with_id` (E8): every occurrence of one block ID.
const CREATE_IDX_BLOCKS_BLOCK_ID: &str =
    "CREATE INDEX IF NOT EXISTS idx_blocks_block_id ON blocks(block_id)";

/// Creates (or confirms, if already present) every index in this
/// module. Idempotent: safe to call on every open, whether or not the
/// tables were just freshly created.
pub(crate) fn create_core_indexes(conn: &Connection) -> Result<(), CacheError> {
    conn.execute(CREATE_IDX_LINKS_FROM_UNIT_ID, [])?;
    conn.execute(CREATE_IDX_LINKS_TO_UNIT_ID, [])?;
    conn.execute(CREATE_IDX_TAGS_UNIT_ID, [])?;
    conn.execute(CREATE_IDX_BLOCKS_UNIT_ID, [])?;
    conn.execute(CREATE_IDX_HEADINGS_UNIT_ID, [])?;
    conn.execute(CREATE_IDX_SYMBOLS_UNIT_ID, [])?;
    conn.execute(CREATE_IDX_BLOCKS_BLOCK_ID, [])?;
    conn.execute(CREATE_IDX_BLOCK_NAMES_NAME, [])?;
    Ok(())
}

/// Creates `sources`, `schema_version` and `scan_state` if absent.
/// Idempotent and safe to call on every open: never drops or
/// truncates any of them.
pub(crate) fn create_bootstrap_tables(conn: &Connection) -> Result<(), CacheError> {
    conn.execute(CREATE_SOURCES, [])?;
    conn.execute(CREATE_SCHEMA_VERSION, [])?;
    conn.execute(CREATE_SCAN_STATE, [])?;
    Ok(())
}

/// Creates every core table only if it doesn't already exist. Used on first run (after a fresh `schema_version` insert) and
/// defensively on a version-match open, where it is a guaranteed no-op
/// against an already-populated schema.
pub(crate) fn create_core_tables_if_not_exists(conn: &Connection) -> Result<(), CacheError> {
    for create in core_table_ddls_if_not_exists() {
        conn.execute(&create, [])?;
    }
    create_core_indexes(conn)?;
    Ok(())
}

fn core_table_ddls_if_not_exists() -> [String; 8] {
    [
        CREATE_UNITS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_LINKS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_TAGS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_SYMBOLS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_BLOCKS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_HEADINGS.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
        CREATE_BLOCK_FTS.replacen(
            "CREATE VIRTUAL TABLE",
            "CREATE VIRTUAL TABLE IF NOT EXISTS",
            1,
        ),
        CREATE_BLOCK_NAMES.replacen("CREATE TABLE", "CREATE TABLE IF NOT EXISTS", 1),
    ]
}

/// Drops every core table (children first, parents last, though
/// `DROP TABLE` in SQLite does not itself enforce foreign keys).
/// Never touches `sources`, `schema_version` or `scan_state`.
pub(crate) fn drop_core_tables(conn: &Connection) -> Result<(), CacheError> {
    conn.execute("DROP TABLE IF EXISTS block_fts", [])?;
    conn.execute("DROP TABLE IF EXISTS block_names", [])?;
    // `blocks` references `symbols`: drop the referencing table first.
    conn.execute("DROP TABLE IF EXISTS blocks", [])?;
    conn.execute("DROP TABLE IF EXISTS symbols", [])?;
    conn.execute("DROP TABLE IF EXISTS headings", [])?;
    conn.execute("DROP TABLE IF EXISTS tags", [])?;
    conn.execute("DROP TABLE IF EXISTS links", [])?;
    conn.execute("DROP TABLE IF EXISTS units", [])?;
    Ok(())
}

/// Recreates every core table from the current schema,
/// unconditionally. Only ever called immediately after
/// [`drop_core_tables`] within the same transaction.
pub(crate) fn create_core_tables(conn: &Connection) -> Result<(), CacheError> {
    conn.execute(CREATE_UNITS, [])?;
    conn.execute(CREATE_LINKS, [])?;
    conn.execute(CREATE_TAGS, [])?;
    conn.execute(CREATE_SYMBOLS, [])?;
    conn.execute(CREATE_BLOCKS, [])?;
    conn.execute(CREATE_HEADINGS, [])?;
    conn.execute(CREATE_BLOCK_FTS, [])?;
    conn.execute(CREATE_BLOCK_NAMES, [])?;
    create_core_indexes(conn)?;
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
/// This function's `SELECT`s name every column in the v1 set (e.g.
/// `u.frontmatter_json`); against a cache older than that, one of
/// those columns may not exist, and a
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
    units: Vec<OfflineUnit>,
    links: Vec<LinkRow>,
    tags: Vec<TagRow>,
    blocks: Vec<BlockRow>,
}

/// A `units` row as the snapshot reads it: only the columns every
/// schema version has had. The newer columns (`kind`, `size`,
/// `too_large`, `needs_rescan`) are filled in by
/// [`restore_offline_rows`].
#[derive(Debug)]
pub(crate) struct OfflineUnit {
    id: i64,
    source_id: i64,
    rel_path: String,
    mtime: Option<i64>,
    frontmatter_json: Option<String>,
}

/// Infers a `units.kind` value from a `rel_path` alone, with no file
/// read. An offline source's files cannot be opened, so a restored
/// row's kind can only come from its name. Must return one of the
/// five values the `units.kind` CHECK allows (`prose`, `code`,
/// `document`, `text-other`, `binary`); the restore runs inside a
/// savepoint, so a value the CHECK rejects discards the whole
/// carry-over rather than failing the rebuild.
pub type KindInferrer = fn(&str) -> &'static str;

/// The [`KindInferrer`] a plain [`crate::Cache::open`] uses: knows no
/// extensions, so every restored offline row is `text-other`. The
/// `tephrad` daemon opens its cache with
/// [`crate::Cache::open_with_kind_inferrer`] and its real extension
/// table; either way `needs_rescan = 1` marks the guess as provisional.
pub(crate) fn default_kind_inferrer(_rel_path: &str) -> &'static str {
    "text-other"
}

/// The exact column names [`snapshot_offline_rows`]'s `SELECT`s read,
/// by table. This is the v1 column set, which every later schema
/// version still contains, so it is what a rebuild can rely on finding
/// in any old cache. It deliberately omits the v2 columns (`units.kind`
/// and friends, `blocks.end_line`): a v1 cache doesn't have them, and
/// requiring them would make the gate below refuse every v1 cache and
/// drop its offline rows (E10). Checked via `PRAGMA table_info`
/// against the *old*, pre-rebuild table shapes before a rebuild ever
/// attempts to snapshot offline rows.
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

/// Whether `table` exists and has no column named `column`. `false`
/// for a table that does not exist at all (the matching-version
/// `CREATE ... IF NOT EXISTS` makes it).
fn table_lacks_column(conn: &Connection, table: &str, column: &str) -> Result<bool, CacheError> {
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query([])?;
    let mut any = false;
    while let Some(row) = rows.next()? {
        any = true;
        if row.get::<_, String>(1)? == column {
            return Ok(false);
        }
    }
    Ok(any)
}

/// Whether `block_fts` exists (a cache written by an earlier build of
/// the `^^` search) and `block_names` does not.
fn block_names_missing_beside_block_fts(conn: &Connection) -> Result<bool, CacheError> {
    let exists = |name: &str| -> Result<bool, CacheError> {
        Ok(conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [name],
            |r| r.get(0),
        )?)
    };
    Ok(exists("block_fts")? && !exists("block_names")?)
}

/// Whether the existing `blocks.block_id` column is declared
/// `NOT NULL`. `false` if the table or column is missing.
fn blocks_block_id_is_not_null(conn: &Connection) -> Result<bool, CacheError> {
    let mut stmt = conn.prepare("PRAGMA table_info(blocks)")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == "block_id" {
            let not_null: i64 = row.get(3)?;
            return Ok(not_null != 0);
        }
    }
    Ok(false)
}

/// Whether a cache file at the *current* `schema_version` was written
/// by an earlier build of that version, whose core tables had a shape
/// the current one does not. SQLite can't relax or add a constrained
/// column in place and the version number did not change, so
/// [`crate::cache`]'s open path uses this to send such a file through
/// the one rebuild path, as a version mismatch is. Two earlier shapes
/// are recognised:
///
/// - the first build of v2 declared `blocks.block_id` `NOT NULL`,
///   where a paragraph with no `^id` is a row with a NULL `block_id`
///   (G3);
/// - the build before code symbols (G4) had no `symbols.line` (the
///   definition's first line) and no `blocks.symbol_id` (the symbol
///   that owns a code block ID, G13);
/// - the build before the `^^` search (G10) had a `block_fts` with only
///   `unit_id`, `line` and `text`, and nothing wrote to it;
/// - the build before the bounded `^^` search had a `block_fts` and no
///   `block_names`, so the symbols and headings named like a query could
///   not be found from the index.
///
/// A missing table is not stale (the matching-version `CREATE ... IF
/// NOT EXISTS` makes it).
pub(crate) fn core_shape_is_stale(conn: &Connection) -> Result<bool, CacheError> {
    Ok(blocks_block_id_is_not_null(conn)?
        || table_lacks_column(conn, "symbols", "line")?
        || table_lacks_column(conn, "blocks", "symbol_id")?
        || table_lacks_column(conn, "block_fts", "kind")?
        || block_names_missing_beside_block_fts(conn)?)
}

/// Fails with [`CacheError::Fts5Unavailable`] when this SQLite has no
/// FTS5 or no `trigram` tokenizer, which the `^^` search cannot work
/// without. `rusqlite`'s `bundled` feature compiles both in, so this is
/// a guard against a build that swapped the bundled library for another
/// one, giving a clear message at start instead of a bare "no such
/// tokenizer" from the first `CREATE VIRTUAL TABLE`. Creates and drops
/// one temp table; touches nothing in the cache file itself.
pub(crate) fn assert_fts5_available(conn: &Connection) -> Result<(), CacheError> {
    conn.execute(
        "CREATE VIRTUAL TABLE temp.tephra_fts5_probe USING fts5(x, tokenize = 'trigram')",
        [],
    )
    .map_err(|e| {
        CacheError::Fts5Unavailable(format!(
            "this SQLite build lacks FTS5 with the trigram tokenizer (needs SQLite 3.34 or \
             newer built with SQLITE_ENABLE_FTS5): {e}"
        ))
    })?;
    conn.execute("DROP TABLE temp.tephra_fts5_probe", [])?;
    Ok(())
}

/// Whether a cache at the current version holds files that should have
/// `block_fts` rows but the index is empty: a file written by a build
/// that did not populate it, or one whose index was lost. `true` means
/// the open path flags every online source for a full rescan (the same
/// bookkeeping a rebuild uses), so the index is filled on the next
/// start instead of `^^` silently finding nothing.
///
/// "Should have rows" is a `prose`, `code` or `text-other` unit of an
/// online source that is not `too_large`, not a provisional offline
/// carry-over (`needs_rescan`) and not empty (`size` NULL counts as not
/// known to be empty). A `document` is left out on purpose: an
/// image-only PDF or a corrupt docx is a unit with no rows however
/// often it is scanned. A cache with none of the former, or an empty
/// cache, is left alone, so a source that legitimately has no text does
/// not rescan on every start.
pub(crate) fn fts_needs_backfill(conn: &Connection) -> Result<bool, CacheError> {
    let any_fts: bool =
        conn.query_row("SELECT EXISTS (SELECT 1 FROM block_fts)", [], |r| r.get(0))?;
    if any_fts {
        return Ok(false);
    }
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM units u JOIN sources s ON s.id = u.source_id \
         WHERE s.status != 'offline' AND u.kind IN ('prose', 'code', 'text-other') \
           AND u.too_large = 0 \
           AND u.needs_rescan = 0 AND COALESCE(u.size, 1) > 0)",
        [],
        |r| r.get(0),
    )?)
}

/// Gate for [`snapshot_offline_rows`]: `true` only if the *current*
/// (pre-drop) `units`/`links`/`tags`/`blocks` tables already have
/// every column the snapshot reads (the v1 set above). `false` means
/// an older cache file than that (as in the R1 review repro: a
/// `units` table with no `frontmatter_json`) is being rebuilt — in that case the rebuild
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
        out.units.push(OfflineUnit {
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
            end_line: None,
            symbol_id: None,
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
    infer_kind: KindInferrer,
) -> Result<(), CacheError> {
    for u in &rows.units {
        // `size` is NULL (the file can't be stat'ed while the source is
        // offline) and `needs_rescan` is 1, so the next online rescan
        // of this source knows the row is provisional (E10).
        conn.execute(
            "INSERT INTO units \
             (id, source_id, rel_path, mtime, frontmatter_json, kind, size, too_large, needs_rescan) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, 0, 1)",
            rusqlite::params![
                u.id,
                u.source_id,
                u.rel_path,
                u.mtime,
                u.frontmatter_json,
                infer_kind(&u.rel_path)
            ],
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
            "INSERT INTO blocks (id, unit_id, block_id, line, end_line) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![b.id, b.unit_id, b.block_id, b.line, b.end_line],
        )?;
    }

    Ok(())
}

/// Marks every non-offline source as "scan started, never finished"
/// (E11), so the daemon rescans it from the top on its next start. A
/// rebuild empties every online source's core rows, and `scan_state`
/// is never dropped, so without this an online source would keep its
/// old "finished" record and silently stay empty. Offline sources are
/// skipped: their rows were carried across and they can't be scanned.
pub(crate) fn flag_online_sources_for_rescan(conn: &Connection) -> Result<(), CacheError> {
    conn.execute(
        "INSERT INTO scan_state (source_id, started_at, finished_at) \
         SELECT id, CAST(strftime('%s','now') AS INTEGER), NULL FROM sources \
         WHERE status != 'offline' \
         ON CONFLICT(source_id) DO UPDATE SET \
             started_at = excluded.started_at, finished_at = NULL",
        [],
    )?;
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
