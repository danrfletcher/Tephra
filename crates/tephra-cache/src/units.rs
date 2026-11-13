//! `units`/`links`/`tags`/`blocks` row types and insert/query helpers
//! (plus the delete cascade over `headings`/`symbols`/`block_fts`).
//!
//! Every table carries a foreign key back toward `sources`: `units`
//! directly via `source_id`, and the others indirectly —
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
    /// One of `prose`, `code`, `document`, `text-other`, `binary`
    /// (enforced by a CHECK constraint).
    pub kind: String,
    /// File size in bytes. `None` for a row restored from an offline
    /// source across a rebuild, until its next online rescan.
    pub size: Option<i64>,
    /// The file was over the size cap and was indexed by name only.
    pub too_large: bool,
    /// A provisional row restored across a schema rebuild: `kind` was
    /// guessed from the extension and `size` is unknown. Cleared by
    /// the source's next online rescan.
    pub needs_rescan: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct NewUnit<'a> {
    pub source_id: i64,
    pub rel_path: &'a str,
    pub mtime: Option<i64>,
    pub frontmatter_json: Option<&'a str>,
    /// One of `prose`, `code`, `document`, `text-other`, `binary`.
    pub kind: &'a str,
    pub size: Option<i64>,
    pub too_large: bool,
}

/// The `units` columns [`unit_from_row`] reads, in order.
const UNIT_COLUMNS: &str =
    "id, source_id, rel_path, mtime, frontmatter_json, kind, size, too_large, needs_rescan";

fn unit_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UnitRow> {
    Ok(UnitRow {
        id: row.get(0)?,
        source_id: row.get(1)?,
        rel_path: row.get(2)?,
        mtime: row.get(3)?,
        frontmatter_json: row.get(4)?,
        kind: row.get(5)?,
        size: row.get(6)?,
        too_large: row.get::<_, i64>(7)? != 0,
        needs_rescan: row.get::<_, i64>(8)? != 0,
    })
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkRow {
    pub id: i64,
    pub from_unit_id: i64,
    pub to_raw: String,
    pub to_unit_id: Option<i64>,
    pub kind: String,
    /// 1-based line of the link in its file. `None` for a link in a
    /// document (a docx, pptx or PDF has pages and slides, not lines)
    /// and for a row carried over from a cache written before this
    /// column existed.
    pub line: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewLink<'a> {
    pub from_unit_id: i64,
    pub to_raw: &'a str,
    pub to_unit_id: Option<i64>,
    pub kind: &'a str,
    /// 1-based line of the link in its file; `None` for a link in a
    /// document, which has no lines.
    pub line: Option<i64>,
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
    /// The block's `^id` (without the caret), or `None` for a
    /// paragraph that has none. Not unique per `(unit_id, block_id)`:
    /// every occurrence of an ID is stored.
    pub block_id: Option<String>,
    /// First line of the block.
    pub line: i64,
    /// Last line of the block, when known.
    pub end_line: Option<i64>,
    /// The `symbols` row that owns this block ID (G13), for a block ID
    /// in a code file. `None` for a prose block, and for a code ID
    /// comment that has no symbol to own it.
    pub symbol_id: Option<i64>,
}

#[derive(Debug, Clone, Copy)]
pub struct NewBlock<'a> {
    pub unit_id: i64,
    /// `None` for a block (a prose paragraph) with no `^id`.
    pub block_id: Option<&'a str>,
    pub line: i64,
    pub end_line: Option<i64>,
    /// The owning `symbols` row (a code block ID, G13); `None` for a
    /// prose block or an ID comment with no symbol. Must be a row that
    /// already exists (foreign key), so a caller inserts the unit's
    /// symbols first.
    pub symbol_id: Option<i64>,
}

/// One `blocks` row with the unit it belongs to: one occurrence of a
/// block ID, as [`crate::Cache::blocks_with_id`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockOccurrence {
    pub block_id: String,
    pub unit_id: i64,
    /// The unit's path relative to its source root.
    pub rel_path: String,
    /// 1-based line the block is stored on (a symbol's first line, for
    /// a code symbol).
    pub line: i64,
    pub end_line: Option<i64>,
    pub symbol_id: Option<i64>,
}

/// One `symbols` row read back: see [`NewSymbol`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SymbolRow {
    pub id: i64,
    pub unit_id: i64,
    pub kind: String,
    pub name: String,
    pub parent: Option<String>,
    /// The definition's first line (the line that carries a block ID).
    pub line: i64,
    /// First line of the symbol's range, which takes in the
    /// attributes, decorators and doc comments directly above it.
    pub start_line: i64,
    /// Last line of the symbol's range.
    pub end_line: i64,
}

/// One `symbols` row to insert: a function, class, struct, enum,
/// trait, interface, impl, mod, method or top-level variable, const,
/// static or key of a code unit (G4). Lines are 1-based.
#[derive(Debug, Clone, Copy)]
pub struct NewSymbol<'a> {
    pub unit_id: i64,
    pub kind: &'a str,
    pub name: &'a str,
    /// The enclosing symbol's name; `None` at the top level.
    pub parent: Option<&'a str>,
    pub line: i64,
    pub start_line: i64,
    pub end_line: i64,
}

/// One `headings` row read back: see [`NewHeading`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadingRow {
    pub id: i64,
    pub unit_id: i64,
    pub level: i64,
    pub text: String,
    pub line: i64,
}

/// One `headings` row to insert: a heading of a prose unit (Markdown,
/// org, rst or adoc).
#[derive(Debug, Clone, Copy)]
pub struct NewHeading<'a> {
    pub unit_id: i64,
    /// 1 through 6 for Markdown and adoc; org and rst have no cap.
    pub level: i64,
    pub text: &'a str,
    /// 1-indexed line within the file.
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
        "INSERT INTO units (source_id, rel_path, mtime, frontmatter_json, kind, size, too_large) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            new.source_id,
            new.rel_path,
            new.mtime,
            new.frontmatter_json,
            new.kind,
            new.size,
            new.too_large as i64
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn insert_link(conn: &Connection, new: &NewLink) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO links (from_unit_id, to_raw, to_unit_id, kind, line) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            new.from_unit_id,
            new.to_raw,
            new.to_unit_id,
            new.kind,
            new.line
        ],
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
        "INSERT INTO blocks (unit_id, block_id, line, end_line, symbol_id) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            new.unit_id,
            new.block_id,
            new.line,
            new.end_line,
            new.symbol_id
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn insert_symbol(conn: &Connection, new: &NewSymbol) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO symbols (unit_id, kind, name, parent, line, start_line, end_line) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            new.unit_id,
            new.kind,
            new.name,
            new.parent,
            new.line,
            new.start_line,
            new.end_line
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn insert_heading(conn: &Connection, new: &NewHeading) -> Result<i64, CacheError> {
    conn.execute(
        "INSERT INTO headings (unit_id, level, text, line) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![new.unit_id, new.level, new.text, new.line],
    )?;
    Ok(conn.last_insert_rowid())
}

pub(crate) fn get_unit(conn: &Connection, id: i64) -> Result<Option<UnitRow>, CacheError> {
    conn.query_row(
        &format!("SELECT {UNIT_COLUMNS} FROM units WHERE id = ?1"),
        [id],
        unit_from_row,
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
        &format!("SELECT {UNIT_COLUMNS} FROM units WHERE source_id = ?1 AND rel_path = ?2"),
        rusqlite::params![source_id, rel_path],
        unit_from_row,
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
    conn.execute("DELETE FROM headings WHERE unit_id = ?1", [unit_id])?;
    conn.execute("DELETE FROM symbols WHERE unit_id = ?1", [unit_id])?;
    crate::fts::delete_fts_for_unit(conn, unit_id)?;
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
    conn.execute(
        "DELETE FROM headings WHERE unit_id IN (SELECT id FROM units WHERE source_id = ?1)",
        [source_id],
    )?;
    conn.execute(
        "DELETE FROM symbols WHERE unit_id IN (SELECT id FROM units WHERE source_id = ?1)",
        [source_id],
    )?;
    // A unit's rows are the rowids `unit_id << 24 ..` (see `crate::fts`),
    // so each unit's rows go by a rowid range, never a scan of the index.
    let ids = unit_ids(
        conn,
        "SELECT id FROM units WHERE source_id = ?1",
        [source_id],
    )?;
    for id in ids {
        crate::fts::delete_fts_for_unit(conn, id)?;
    }
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
    for child in ["headings", "symbols"] {
        conn.execute(
            &format!(
                "DELETE FROM {child} WHERE unit_id IN \
                 (SELECT id FROM units WHERE source_id = ?1 AND rel_path >= ?2 AND rel_path < ?3)"
            ),
            rusqlite::params![source_id, lower, upper],
        )?;
    }
    // The index rows are found by rowid, as in `delete_units_for_source`.
    let ids = unit_ids(
        conn,
        "SELECT id FROM units WHERE source_id = ?1 AND rel_path >= ?2 AND rel_path < ?3",
        rusqlite::params![source_id, lower, upper],
    )?;
    for id in ids {
        crate::fts::delete_fts_for_unit(conn, id)?;
    }
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
        "SELECT u.id, u.source_id, u.rel_path, u.mtime, u.frontmatter_json, \
                u.kind, u.size, u.too_large, u.needs_rescan, s.status \
         FROM units u JOIN sources s ON s.id = u.source_id \
         WHERE s.name = ?1 ORDER BY u.id",
    )?;
    let mut rows = stmt.query([source_name])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let status_raw: String = row.get(9)?;
        let source_status = SourceStatus::parse(&status_raw)
            .ok_or_else(|| CacheError::CorruptStatus(status_raw.clone()))?;
        out.push(UnitWithSourceStatus {
            unit: unit_from_row(row)?,
            source_status,
        });
    }
    Ok(out)
}

/// Every `tags` row belonging to `unit_id`, in insertion order. Read
/// counterpart to [`insert_tag`] — used by a caller (e.g.
/// `tephra-graph`'s graph-construction path, PR-5) that has already
/// looked up a unit's own row and now needs its full tag set, rather
/// than reaching for `Cache::connection()`'s raw-SQL escape hatch.
pub(crate) fn tags_for_unit(conn: &Connection, unit_id: i64) -> Result<Vec<TagRow>, CacheError> {
    let mut stmt =
        conn.prepare("SELECT id, unit_id, tag FROM tags WHERE unit_id = ?1 ORDER BY id")?;
    let mut rows = stmt.query([unit_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(TagRow {
            id: row.get(0)?,
            unit_id: row.get(1)?,
            tag: row.get(2)?,
        });
    }
    Ok(out)
}

/// Every `blocks` row belonging to `unit_id`, in insertion order. Read
/// counterpart to [`insert_block`]; see [`tags_for_unit`]'s doc
/// comment for why this exists as a typed helper rather than raw SQL.
pub(crate) fn blocks_for_unit(
    conn: &Connection,
    unit_id: i64,
) -> Result<Vec<BlockRow>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT id, unit_id, block_id, line, end_line, symbol_id \
         FROM blocks WHERE unit_id = ?1 ORDER BY id",
    )?;
    let mut rows = stmt.query([unit_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(BlockRow {
            id: row.get(0)?,
            unit_id: row.get(1)?,
            block_id: row.get(2)?,
            line: row.get(3)?,
            end_line: row.get(4)?,
            symbol_id: row.get(5)?,
        });
    }
    Ok(out)
}

/// Every `symbols` row belonging to `unit_id`, in file order (first
/// line, then insertion). Read counterpart to [`insert_symbol`].
pub(crate) fn symbols_for_unit(
    conn: &Connection,
    unit_id: i64,
) -> Result<Vec<SymbolRow>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT id, unit_id, kind, name, parent, line, start_line, end_line \
         FROM symbols WHERE unit_id = ?1 ORDER BY line, id",
    )?;
    let mut rows = stmt.query([unit_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(SymbolRow {
            id: row.get(0)?,
            unit_id: row.get(1)?,
            kind: row.get(2)?,
            name: row.get(3)?,
            parent: row.get(4)?,
            line: row.get(5)?,
            start_line: row.get(6)?,
            end_line: row.get(7)?,
        });
    }
    Ok(out)
}

/// Every occurrence of `block_id` in the units of `source_id`, sorted
/// by path, then line (E8). One `blocks` row is one occurrence, so the
/// same ID in two files (or twice in one) gives that many entries.
/// Scoped to the one source: the same ID in another source's files is
/// never returned. Matches the ID exactly (case-sensitive).
pub(crate) fn blocks_with_id(
    conn: &Connection,
    source_id: i64,
    block_id: &str,
) -> Result<Vec<BlockOccurrence>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT b.block_id, b.unit_id, u.rel_path, b.line, b.end_line, b.symbol_id \
         FROM blocks b JOIN units u ON u.id = b.unit_id \
         WHERE u.source_id = ?1 AND b.block_id = ?2 \
         ORDER BY u.rel_path, b.line, b.id",
    )?;
    let mut rows = stmt.query(rusqlite::params![source_id, block_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(BlockOccurrence {
            block_id: row.get(0)?,
            unit_id: row.get(1)?,
            rel_path: row.get(2)?,
            line: row.get(3)?,
            end_line: row.get(4)?,
            symbol_id: row.get(5)?,
        });
    }
    Ok(out)
}

/// One stored block ID occurrence with the text the `^^` index holds for
/// it: what [`for_each_id_block`] hands its callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdBlockText {
    pub rel_path: String,
    pub block_id: String,
    /// The `blocks` row's line (a code symbol's first line).
    pub line: i64,
    /// The text of the first `block_fts` row of the same unit that
    /// carries this ID (the paragraph without its `^id`, a symbol's name,
    /// a comment block's text); `None` when the index has no such row.
    pub text: Option<String>,
}

/// Calls `f` for every `blocks` row with a block ID in the units of
/// `source_id` (only the unit at `rel_path`, when given), in path, line
/// order, one at a time so a caller can fold each into something small
/// instead of holding every text. The text comes from the unit's own
/// `block_fts` rowid range, so it is one bounded range scan per row.
pub(crate) fn for_each_id_block(
    conn: &Connection,
    source_id: i64,
    rel_path: Option<&str>,
    mut f: impl FnMut(IdBlockText),
) -> Result<(), CacheError> {
    let mut stmt = conn.prepare(
        "SELECT u.rel_path, b.block_id, b.line, \
                (SELECT f.text FROM block_fts f \
                  WHERE f.rowid >= (u.id << 24) AND f.rowid < ((u.id + 1) << 24) \
                    AND f.block_id = b.block_id \
                  ORDER BY f.rowid LIMIT 1) \
         FROM units u JOIN blocks b ON b.unit_id = u.id \
         WHERE u.source_id = ?1 AND b.block_id IS NOT NULL \
           AND (?2 IS NULL OR u.rel_path = ?2) \
         ORDER BY u.rel_path, b.line, b.id",
    )?;
    let mut rows = stmt.query(rusqlite::params![source_id, rel_path])?;
    while let Some(row) = rows.next()? {
        f(IdBlockText {
            rel_path: row.get(0)?,
            block_id: row.get(1)?,
            line: row.get(2)?,
            text: row.get(3)?,
        });
    }
    Ok(())
}

/// Every `headings` row belonging to `unit_id`, in file order (line,
/// then insertion). Read counterpart to [`insert_heading`].
pub(crate) fn headings_for_unit(
    conn: &Connection,
    unit_id: i64,
) -> Result<Vec<HeadingRow>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT id, unit_id, level, text, line FROM headings WHERE unit_id = ?1 ORDER BY line, id",
    )?;
    let mut rows = stmt.query([unit_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(HeadingRow {
            id: row.get(0)?,
            unit_id: row.get(1)?,
            level: row.get(2)?,
            text: row.get(3)?,
            line: row.get(4)?,
        });
    }
    Ok(out)
}

/// Every `links` row authored by `from_unit_id` (i.e. the outgoing
/// wikilinks/markdown-links this unit's own content contains), in
/// insertion order. Read counterpart to [`insert_link`]; see
/// [`tags_for_unit`]'s doc comment for why this exists as a typed
/// helper rather than raw SQL. `to_unit_id` on the returned rows is
/// whatever this crate's writers last stored for it — always `NULL`
/// from every production write path today, since resolving a link
/// against the registry is `tephra-graph`'s job (PR-5), not this
/// crate's or `tephra-parser`'s; a caller resolving links itself
/// (`tephra-graph`) re-derives the target fresh from `to_raw` rather
/// than trusting this column.
pub(crate) fn links_from_unit(
    conn: &Connection,
    from_unit_id: i64,
) -> Result<Vec<LinkRow>, CacheError> {
    let mut stmt = conn.prepare(
        "SELECT id, from_unit_id, to_raw, to_unit_id, kind, line FROM links \
         WHERE from_unit_id = ?1 ORDER BY id",
    )?;
    let mut rows = stmt.query([from_unit_id])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(LinkRow {
            id: row.get(0)?,
            from_unit_id: row.get(1)?,
            to_raw: row.get(2)?,
            to_unit_id: row.get(3)?,
            kind: row.get(4)?,
            line: row.get(5)?,
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
const ALLOWED_TABLE_NAMES: [&str; 11] = [
    "sources",
    "units",
    "links",
    "tags",
    "blocks",
    "schema_version",
    "headings",
    "symbols",
    "block_fts",
    "block_names",
    "scan_state",
];

pub(crate) fn count_rows(conn: &Connection, table: &str) -> Result<i64, CacheError> {
    if !ALLOWED_TABLE_NAMES.contains(&table) {
        return Err(CacheError::InvalidTableName(table.to_string()));
    }
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(count)
}

/// The ids a `SELECT id FROM units ...` statement gives.
fn unit_ids(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<i64>, CacheError> {
    let mut stmt = conn.prepare_cached(sql)?;
    let ids = stmt
        .query_map(params, |r| r.get(0))?
        .collect::<Result<Vec<i64>, _>>()?;
    Ok(ids)
}
