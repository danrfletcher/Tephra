//! The `block_fts` index (G10): writing a unit's rows and the `^^`
//! full-text search over every source.
//!
//! ## Writing
//!
//! A unit's rows are written in one call ([`insert_fts_rows`]), by the
//! same code path and in the same transaction as the unit's `blocks`,
//! `symbols` and `headings` rows. They get the rowids
//! `unit_id << 24 ..` ([`FTS_ROWS_PER_UNIT`] of them), so a unit's rows
//! are one contiguous rowid range: deleting a changed file's rows is a
//! range delete, not a scan of the whole index (see [`crate::schema`]).
//!
//! ## Searching
//!
//! [`search_all_blocks`] is read-only: it works on a
//! [`crate::Cache::open_read_only`] connection, never waits on the
//! writer gate and, under WAL, sees one consistent snapshot, so a unit
//! being rescanned is either fully its old rows or fully its new ones.
//!
//! The query is **never** passed to FTS5 as syntax. It is trimmed and
//! sent whole as one quoted phrase (`"` doubled), which under the trigram
//! tokenizer is a case-insensitive substring match. `NEAR`, `AND`, `OR`,
//! `*`, `-`, `:`, `(`, `^` and every other FTS5 operator are plain text
//! inside the quotes, so no user input can make a malformed `MATCH`.
//!
//! A query of fewer than [`MIN_TRIGRAM_CHARS`] characters has no
//! trigram, so FTS5 would match nothing; it falls back to a `LIKE` scan
//! of the same table (`%`, `_` and `\` escaped), with the same ordering
//! and limit. (`LIKE` is case-insensitive for ASCII only.)
//!
//! Ranking: a symbol or heading whose whole name equals the query
//! (ignoring ASCII case) first, then FTS5's `bm25` (the fallback has
//! none, so all its rows tie there), then the shorter `rel_path`, then
//! the path, the source, the line (or place in a document) and the
//! rowid, so the order is total and repeatable.
//!
//! ## Bounded work (E9)
//!
//! `bm25` costs about 2 to 4 microseconds for every row that matches,
//! and a trigram of a common word matches most of a large index, so
//! ranking every match would take seconds on 100k files. A search does
//! two small things instead:
//!
//! - the exact-name rows come from `block_names`, an ordinary table with
//!   an index on the lower-cased name (at most [`MAX_RANK_CANDIDATES`]
//!   of them), not from scanning matches;
//! - the rest are ranked among the first [`MAX_RANK_CANDIDATES`] rows
//!   FTS5 matches, in index (rowid) order. A query that matches no more
//!   than that is ranked exactly; a more common one has no meaningful
//!   best hit, and gets the best of the first that match.

use rusqlite::{params, Connection};

use crate::CacheError;

/// How many `block_fts` rows one unit may have, which is also the gap
/// between two units' first rowids: a unit's row `n` has the rowid
/// `(unit_id << 24) + n`. A unit with more rows than this keeps its
/// first [`FTS_ROWS_PER_UNIT`] ones (a 20 MiB file cannot reach it).
pub const FTS_ROWS_PER_UNIT: i64 = 1 << 24;

/// How many matching rows a search ranks (and how many exact-name rows it
/// takes). See the module docs on bounded work.
pub const MAX_RANK_CANDIDATES: usize = 10_000;

/// The shortest query (in characters, not bytes) the trigram index can
/// answer. A shorter one is searched with `LIKE`.
pub const MIN_TRIGRAM_CHARS: usize = 3;

/// The longest snippet [`make_snippet`] returns, in characters.
const SNIPPET_CHARS: usize = 120;
/// How many characters of context a snippet shows before the match.
const SNIPPET_LEAD_CHARS: usize = 30;

/// One row to index, borrowing from the caller. See `CREATE_BLOCK_FTS`
/// in [`crate::schema`] for what each column means.
#[derive(Debug, Clone, Copy, Default)]
pub struct NewFtsRow<'a> {
    /// `paragraph`, `comment`, `symbol` or `heading`.
    pub kind: &'a str,
    /// 1-based line of a file's row; `None` for a document's.
    pub line: Option<i64>,
    pub block_id: Option<&'a str>,
    /// A symbol's or heading's bare name.
    pub name: Option<&'a str>,
    /// What a symbol is shown as (`enum ConflictReason`).
    pub label: Option<&'a str>,
    /// `page`, `slide` or `paragraph` for a document's row.
    pub place: Option<&'a str>,
    pub place_n: Option<i64>,
    /// A document paragraph's 1-based position within its page or slide.
    pub idx: Option<i64>,
    /// What is searched. A symbol's carries its name split into words.
    pub text: &'a str,
}

/// One result of [`search_all_blocks`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockHit {
    pub source: String,
    pub rel_path: String,
    pub kind: String,
    pub line: Option<i64>,
    pub block_id: Option<String>,
    pub place: Option<String>,
    pub place_n: Option<i64>,
    pub idx: Option<i64>,
    /// A symbol's label, else the text around the match; see
    /// [`make_snippet`].
    pub snippet: String,
    /// The source's stored status is `offline`.
    pub offline: bool,
}

/// Writes `rows` as `unit_id`'s index rows and returns how many were
/// written (at most [`FTS_ROWS_PER_UNIT`] in total for the unit).
/// Expects the unit to have none yet, which holds for the fresh unit
/// row every write path inserts; if it has some, the new rows follow
/// them.
pub(crate) fn insert_fts_rows(
    conn: &Connection,
    unit_id: i64,
    rows: &[NewFtsRow<'_>],
) -> Result<usize, CacheError> {
    if rows.is_empty() {
        return Ok(0);
    }
    let base = unit_id << 24;
    let last: Option<i64> = conn.query_row(
        "SELECT MAX(rowid) FROM block_fts WHERE rowid >= ?1 AND rowid < ?2",
        params![base, base + FTS_ROWS_PER_UNIT],
        |r| r.get(0),
    )?;
    let first = last.map_or(base, |n| n + 1);
    let mut stmt = conn.prepare_cached(
        "INSERT INTO block_fts \
         (rowid, unit_id, line, kind, block_id, name, label, place, place_n, idx, text) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    let mut names = conn.prepare_cached(
        "INSERT OR REPLACE INTO block_names (fts_rowid, name_lower) VALUES (?1, ?2)",
    )?;
    let mut written = 0;
    for (rowid, row) in (first..base + FTS_ROWS_PER_UNIT).zip(rows) {
        if let Some(name) = row.name {
            names.execute(params![rowid, name.to_ascii_lowercase()])?;
        }
        stmt.execute(params![
            rowid,
            unit_id,
            row.line,
            row.kind,
            row.block_id,
            row.name,
            row.label,
            row.place,
            row.place_n,
            row.idx,
            row.text
        ])?;
        written += 1;
    }
    Ok(written)
}

/// Removes every index row of `unit_id` (and its `block_names` rows): one
/// range delete on each rowid.
pub(crate) fn delete_fts_for_unit(conn: &Connection, unit_id: i64) -> Result<(), CacheError> {
    let base = unit_id << 24;
    conn.execute(
        "DELETE FROM block_fts WHERE rowid >= ?1 AND rowid < ?2",
        params![base, base + FTS_ROWS_PER_UNIT],
    )?;
    conn.execute(
        "DELETE FROM block_names WHERE fts_rowid >= ?1 AND fts_rowid < ?2",
        params![base, base + FTS_ROWS_PER_UNIT],
    )?;
    Ok(())
}

/// `query` as one FTS5 phrase: wrapped in `"` with each inner `"`
/// doubled. Inside a phrase nothing is an operator.
pub(crate) fn quote_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// `query` as a `LIKE` pattern matching it anywhere, with `%`, `_` and
/// the escape character `\` taken literally.
fn like_pattern(query: &str) -> String {
    let mut out = String::with_capacity(query.len() + 2);
    out.push('%');
    for c in query.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// The columns and joins every search statement shares: `{candidates}`
/// is a subquery giving `rowid`, `score`, `unit_id`, `kind`, `line`,
/// `block_id`, `label`, `place`, `place_n` and `idx` for the rows to
/// rank.
fn hits_sql(candidates: &str) -> String {
    format!(
        "SELECT c.rowid, s.name, s.status, u.rel_path, c.kind, c.line, c.block_id, c.label, \
                c.place, c.place_n, c.idx \
         FROM ({candidates}) c \
         JOIN units u ON u.id = c.unit_id \
         JOIN sources s ON s.id = u.source_id \
         ORDER BY c.score, length(u.rel_path), u.rel_path, s.name, c.line, c.place_n, \
                  c.idx, c.rowid \
         LIMIT ?3"
    )
}

/// The candidates of a query long enough for the trigram index: the
/// first `?4` rows FTS5 matches `?1`, scored by `bm25` (`rank`).
const ALL_BLOCKS_CANDIDATES_SQL: &str = "\
    SELECT block_fts.rank AS score, block_fts.rowid, block_fts.unit_id, block_fts.kind, \
           block_fts.line, block_fts.block_id, block_fts.label, block_fts.place, \
           block_fts.place_n, block_fts.idx \
    FROM block_fts WHERE block_fts MATCH ?1 LIMIT ?4";

/// The first `?4` rows whose text has `?1` (a `LIKE` pattern) in it, all
/// with the same score.
const LIKE_CANDIDATES_SQL: &str = "\
    SELECT 0 AS score, block_fts.rowid, block_fts.unit_id, block_fts.kind, \
           block_fts.line, block_fts.block_id, block_fts.label, block_fts.place, \
           block_fts.place_n, block_fts.idx \
    FROM block_fts WHERE block_fts.text LIKE ?1 ESCAPE '\\' LIMIT ?4";

/// The rows whose symbol or heading name is `?2` (lower case), the
/// first `?4` of them: those rank before every other match.
const EXACT_NAME_CANDIDATES_SQL: &str = "\
    SELECT 0 AS score, block_fts.rowid, block_fts.unit_id, block_fts.kind, \
           block_fts.line, block_fts.block_id, block_fts.label, block_fts.place, \
           block_fts.place_n, block_fts.idx \
    FROM (SELECT fts_rowid FROM block_names WHERE name_lower = ?2 LIMIT ?4) n \
    JOIN block_fts ON block_fts.rowid = n.fts_rowid";

/// `^^` search: the best `limit` rows of every source (offline ones
/// included, flagged) for `query`; see the module doc comment. A blank
/// `query` runs no SQL and returns nothing: the caller refuses it first.
/// Read-only.
pub(crate) fn search_all_blocks(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<BlockHit>, CacheError> {
    // The hits and their texts are read by separate statements, so they
    // are read in one transaction: under WAL that is one snapshot, and a
    // scan that deletes and rewrites a unit in between can neither make
    // a text vanish nor show a unit's old and new rows together.
    let tx = conn.unchecked_transaction()?;
    let hits = search_all_blocks_in(&tx, query, limit)?;
    tx.commit()?;
    Ok(hits)
}

fn search_all_blocks_in(
    conn: &Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<BlockHit>, CacheError> {
    // A NUL byte ends a bound string early inside FTS5 ("unterminated
    // string"), and no indexed text has one, so it is dropped.
    let query = query.replace('\0', "");
    let query = query.trim();
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let lowered = query.to_ascii_lowercase();
    let limit_sql = i64::try_from(limit).unwrap_or(i64::MAX);

    let mut hits: Vec<BlockHit> = Vec::new();
    let mut texts: Vec<i64> = Vec::new();
    let mut collect = |row: &rusqlite::Row<'_>| -> rusqlite::Result<bool> {
        let rowid: i64 = row.get(0)?;
        // A row can come from both statements; it is a hit once.
        if texts.contains(&rowid) {
            return Ok(false);
        }
        let status: String = row.get(2)?;
        let kind: String = row.get(4)?;
        let label: Option<String> = row.get(7)?;
        texts.push(rowid);
        hits.push(BlockHit {
            source: row.get(1)?,
            rel_path: row.get(3)?,
            kind,
            line: row.get(5)?,
            block_id: row.get(6)?,
            place: row.get(8)?,
            place_n: row.get(9)?,
            idx: row.get(10)?,
            snippet: label.unwrap_or_default(),
            offline: status == "offline",
        });
        Ok(true)
    };

    let candidates = i64::try_from(MAX_RANK_CANDIDATES).unwrap_or(i64::MAX);
    // 1. The symbols and headings named exactly like the query.
    let mut stmt = conn.prepare_cached(&hits_sql(EXACT_NAME_CANDIDATES_SQL))?;
    let mut rows = stmt.query(params![
        Option::<String>::None,
        lowered,
        limit_sql,
        candidates
    ])?;
    let mut exact = 0usize;
    while let Some(row) = rows.next()? {
        if collect(row)? {
            exact += 1;
        }
    }
    drop(rows);
    // 2. Everything else, best first. Asking for the exact-name rows'
    // worth more means the limit is still met after a repeat is dropped.
    let more = limit_sql.saturating_add(i64::try_from(exact).unwrap_or(0));
    let (sql, pattern) = if query.chars().count() >= MIN_TRIGRAM_CHARS {
        (hits_sql(ALL_BLOCKS_CANDIDATES_SQL), quote_phrase(query))
    } else {
        (hits_sql(LIKE_CANDIDATES_SQL), like_pattern(query))
    };
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut rows = stmt.query(params![pattern, lowered, more, candidates])?;
    while let Some(row) = rows.next()? {
        collect(row)?;
    }
    drop(rows);
    hits.truncate(limit);
    texts.truncate(limit);

    // The text itself is read only for the rows that were kept.
    let mut text_stmt = conn.prepare_cached("SELECT text FROM block_fts WHERE rowid = ?1")?;
    for (hit, rowid) in hits.iter_mut().zip(texts) {
        if hit.kind == "symbol" && !hit.snippet.is_empty() {
            continue;
        }
        let text: String = text_stmt.query_row([rowid], |r| r.get(0))?;
        hit.snippet = make_snippet(&text, query);
    }
    Ok(hits)
}

/// A one-line excerpt of `text` for showing a hit: whitespace runs
/// become one space, and a text longer than 120 characters is cut to a
/// window that starts a little before the first (case-insensitive)
/// occurrence of `query`, or at the start if there is none, with `...`
/// where text was left out.
pub fn make_snippet(text: &str, query: &str) -> String {
    let flat: Vec<char> = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    if flat.len() <= SNIPPET_CHARS {
        return flat.into_iter().collect();
    }
    let fold = |c: char| c.to_lowercase().next().unwrap_or(c);
    let hay: Vec<char> = flat.iter().map(|&c| fold(c)).collect();
    let needle: Vec<char> = query.trim().chars().map(fold).collect();
    let at = if needle.is_empty() {
        None
    } else {
        hay.windows(needle.len())
            .position(|w| w == needle.as_slice())
    };
    let start = at.map_or(0, |i| i.saturating_sub(SNIPPET_LEAD_CHARS));
    let end = (start + SNIPPET_CHARS).min(flat.len());
    let start = end.saturating_sub(SNIPPET_CHARS).min(start);
    let mut out = String::new();
    if start > 0 {
        out.push_str("...");
    }
    out.extend(&flat[start..end]);
    if end < flat.len() {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_phrase_doubles_quotes_and_wraps() {
        assert_eq!(quote_phrase("foo"), "\"foo\"");
        assert_eq!(quote_phrase("foo\"bar"), "\"foo\"\"bar\"");
        assert_eq!(quote_phrase("NEAR(a b)"), "\"NEAR(a b)\"");
    }

    #[test]
    fn like_pattern_escapes_wildcards() {
        assert_eq!(like_pattern("a%"), "%a\\%%");
        assert_eq!(like_pattern("_\\"), "%\\_\\\\%");
    }

    #[test]
    fn snippet_of_short_text_is_the_whole_text_on_one_line() {
        assert_eq!(make_snippet("a  b\n\tc", "b"), "a b c");
    }

    #[test]
    fn snippet_of_long_text_is_a_window_around_the_match() {
        let text = format!("{} needle {}", "x".repeat(300), "y".repeat(300));
        let snippet = make_snippet(&text, "NEEDLE");
        assert!(snippet.contains("needle"), "{snippet}");
        assert!(snippet.starts_with("...") && snippet.ends_with("..."));
        assert!(snippet.chars().count() <= SNIPPET_CHARS + 6);
    }

    #[test]
    fn snippet_without_a_match_starts_at_the_beginning() {
        let text = "z".repeat(500);
        let snippet = make_snippet(&text, "nope");
        assert!(!snippet.starts_with("..."));
        assert!(snippet.ends_with("..."));
    }

    #[test]
    fn snippet_handles_multibyte_text() {
        let text = format!("{}café{}", "日".repeat(200), "本".repeat(200));
        let snippet = make_snippet(&text, "café");
        assert!(snippet.contains("café"));
    }
}
