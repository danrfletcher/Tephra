//! Name search over the cached `units` rows (`unit.search` with
//! `scope: "names"`, G8).
//!
//! The cache supplies the candidates and keeps the best `limit` of them;
//! the caller supplies the matching rule as a scoring closure (the fuzzy
//! matcher lives in `tephrad::search`). One call is one pass over `units`
//! with no per-row query beyond a first-heading lookup on the `headings`
//! index, and it holds only the `limit` best hits in memory, so it
//! costs the same on a 100k-unit cache as on a small one.
//!
//! Read-only: nothing here writes, so it is safe on a
//! [`crate::Cache::open_read_only`] connection and, under WAL, never
//! blocks a scan's writer or waits on the writer gate. The whole listing
//! is one `SELECT`, so it sees one consistent snapshot: a unit is
//! never returned twice, even while a rescan rewrites its row.
//!
//! Every indexed kind is a candidate (`prose`, `code`, `document`,
//! `text-other`, `binary`, including a `too_large` file). Folders are
//! derived, never stored, so they are never candidates.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, Row};

use crate::listing::compare_names;
use crate::sources::SourceStatus;
use crate::CacheError;

/// The one statement a name search runs. A frontmatter title comes from
/// `json_extract` guarded the same way `unit.list` does (nested `CASE`,
/// since SQLite does not promise `AND` short-circuits and `json_type` on
/// malformed JSON is an error); the heading is the file's first by line,
/// found through `idx_headings_unit_id`. Neither is read for a unit that
/// is not `prose`. Exposed so a test can check its query plan.
pub const NAME_SEARCH_SQL: &str = "\
    SELECT s.name, s.status, u.rel_path, u.kind, \
           CASE WHEN u.kind = 'prose' \
                 AND u.frontmatter_json IS NOT NULL \
                 AND json_valid(u.frontmatter_json) \
                THEN CASE WHEN json_type(u.frontmatter_json, '$.title') = 'text' \
                          THEN json_extract(u.frontmatter_json, '$.title') END \
           END, \
           CASE WHEN u.kind = 'prose' \
                THEN (SELECT h.text FROM headings h WHERE h.unit_id = u.id \
                      ORDER BY h.line, h.id LIMIT 1) \
           END \
    FROM units u JOIN sources s ON s.id = u.source_id";

/// One unit offered to the scoring closure.
#[derive(Debug, Clone, Copy)]
pub struct NameCandidate<'a> {
    /// The source's name.
    pub source: &'a str,
    /// Path relative to the source root.
    pub rel_path: &'a str,
    /// The last component of `rel_path`, extension included.
    pub name: &'a str,
    /// `name` without its last extension (see [`file_stem`]).
    pub stem: &'a str,
    /// `prose`, `code`, `document`, `text-other` or `binary`.
    pub kind: &'a str,
    /// The title the result will carry; see [`resolve_title`].
    pub title: &'a str,
    /// The source's stored status is `offline`. (The daemon may also
    /// find the root missing before the watcher has recorded that.)
    pub offline: bool,
}

/// One kept result of [`crate::Cache::search_names`].
#[derive(Debug, Clone, PartialEq)]
pub struct NameHit {
    pub source: String,
    pub rel_path: String,
    pub name: String,
    pub kind: String,
    pub title: String,
    /// The source's stored status is `offline`.
    pub offline: bool,
    /// The source's stored status is `stale`.
    pub stale: bool,
    /// What the scoring closure returned, 0 to 1.
    pub score: f64,
}

/// The order of results: higher score first; on equal scores the shorter
/// path (in characters), then the name (case-insensitive, then by bytes),
/// then the source name and the path's bytes, which make it total (a
/// source and path identify one unit), so repeated calls agree.
/// `Less` means "ranks first".
pub fn compare_hits(a: &NameHit, b: &NameHit) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then_with(|| a.rel_path.chars().count().cmp(&b.rel_path.chars().count()))
        .then_with(|| compare_names(false, &a.name, false, &b.name))
        .then_with(|| a.source.as_bytes().cmp(b.source.as_bytes()))
        .then_with(|| a.rel_path.as_bytes().cmp(b.rel_path.as_bytes()))
}

/// `name` without its last extension: `main.rs` is `main`,
/// `archive.tar.gz` is `archive.tar`. A name with no dot, or whose only
/// dot is the first character (`.gitignore`), is its own stem.
pub fn file_stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(0) | None => name,
        Some(i) => &name[..i],
    }
}

/// A unit's result title (G8):
///
/// - `prose`: the frontmatter `title`, else the first heading, else the
///   file name as it is (extension included). A blank title or heading
///   counts as absent.
/// - Every other kind: the file name without its extension.
///
/// A row kept from a v1 cache has neither a frontmatter title nor
/// headings until it is rescanned, so it falls back to the name.
pub fn resolve_title(
    kind: &str,
    name: &str,
    frontmatter_title: Option<&str>,
    first_heading: Option<&str>,
) -> String {
    if kind != "prose" {
        return file_stem(name).to_string();
    }
    frontmatter_title
        .filter(|t| !t.trim().is_empty())
        .or_else(|| first_heading.filter(|t| !t.trim().is_empty()))
        .unwrap_or(name)
        .to_string()
}

/// A [`NameHit`] ordered by [`compare_hits`], so a max-heap pops the
/// worst kept hit first.
struct Ranked(NameHit);

impl PartialEq for Ranked {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for Ranked {}
impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Ranked {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_hits(&self.0, &other.0)
    }
}

fn text<'a>(row: &'a Row<'_>, index: usize) -> Result<Option<&'a str>, rusqlite::Error> {
    match row.get_ref(index)? {
        ValueRef::Null => Ok(None),
        ValueRef::Text(bytes) => std::str::from_utf8(bytes)
            .map(Some)
            .map_err(|e| rusqlite::Error::FromSqlConversionFailure(index, Type::Text, Box::new(e))),
        other => Err(rusqlite::Error::InvalidColumnType(
            index,
            "text".to_string(),
            other.data_type(),
        )),
    }
}

pub(crate) fn search_names<F>(
    conn: &Connection,
    limit: usize,
    mut score: F,
) -> Result<Vec<NameHit>, CacheError>
where
    F: FnMut(&NameCandidate<'_>) -> Option<f64>,
{
    if limit == 0 {
        return Ok(Vec::new());
    }
    let mut kept: BinaryHeap<Ranked> = BinaryHeap::new();
    let mut stmt = conn.prepare(NAME_SEARCH_SQL)?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let (Some(source), Some(status), Some(rel_path), Some(kind)) =
            (text(row, 0)?, text(row, 1)?, text(row, 2)?, text(row, 3)?)
        else {
            continue;
        };
        let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
        let title = resolve_title(kind, name, text(row, 4)?, text(row, 5)?);
        let status =
            SourceStatus::parse(status).ok_or_else(|| CacheError::CorruptStatus(status.into()))?;
        let candidate = NameCandidate {
            source,
            rel_path,
            name,
            stem: file_stem(name),
            kind,
            title: &title,
            offline: status == SourceStatus::Offline,
        };
        let offline = candidate.offline;
        let Some(score) = score(&candidate) else {
            continue;
        };
        let hit = NameHit {
            source: source.to_string(),
            rel_path: rel_path.to_string(),
            name: name.to_string(),
            kind: kind.to_string(),
            title,
            offline,
            stale: status == SourceStatus::Stale,
            score,
        };
        if kept.len() < limit {
            kept.push(Ranked(hit));
        } else if kept
            .peek()
            .is_some_and(|worst| compare_hits(&hit, &worst.0) == Ordering::Less)
        {
            kept.pop();
            kept.push(Ranked(hit));
        }
    }
    Ok(kept.into_sorted_vec().into_iter().map(|r| r.0).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_stem_drops_only_the_last_extension() {
        assert_eq!(file_stem("main.rs"), "main");
        assert_eq!(file_stem("archive.tar.gz"), "archive.tar");
        assert_eq!(file_stem("Makefile"), "Makefile");
        assert_eq!(file_stem(".gitignore"), ".gitignore");
        assert_eq!(file_stem("trailing."), "trailing");
        assert_eq!(file_stem("café notes.md"), "café notes");
    }

    #[test]
    fn title_rule_for_prose_is_frontmatter_then_heading_then_the_file_name() {
        assert_eq!(
            resolve_title("prose", "a.md", Some("FM"), Some("Head")),
            "FM"
        );
        assert_eq!(resolve_title("prose", "a.md", None, Some("Head")), "Head");
        assert_eq!(
            resolve_title("prose", "a.md", Some("  "), Some("Head")),
            "Head"
        );
        assert_eq!(resolve_title("prose", "a.md", None, Some("")), "a.md");
        assert_eq!(resolve_title("prose", "a.md", None, None), "a.md");
    }

    #[test]
    fn title_rule_for_every_other_kind_is_the_name_without_extension() {
        for kind in ["code", "document", "text-other", "binary"] {
            // Frontmatter and headings never apply outside prose.
            assert_eq!(
                resolve_title(kind, "main.rs", Some("FM"), Some("Head")),
                "main"
            );
        }
    }

    fn hit(source: &str, rel_path: &str, score: f64) -> NameHit {
        NameHit {
            source: source.into(),
            rel_path: rel_path.into(),
            name: rel_path.rsplit('/').next().unwrap().into(),
            kind: "prose".into(),
            title: String::new(),
            offline: false,
            stale: false,
            score,
        }
    }

    #[test]
    fn hits_order_by_score_then_shorter_path_then_name_then_source() {
        let mut hits = [
            hit("b", "x/long/path/a.md", 0.5),
            hit("b", "z.md", 0.5),
            hit("a", "z.md", 0.5),
            hit("a", "y.md", 0.5),
            hit("a", "deep/q.md", 0.9),
        ];
        hits.sort_by(compare_hits);
        let order: Vec<_> = hits
            .iter()
            .map(|h| format!("{}:{}", h.source, h.rel_path))
            .collect();
        assert_eq!(
            order,
            [
                "a:deep/q.md",
                "a:y.md",
                "a:z.md",
                "b:z.md",
                "b:x/long/path/a.md"
            ]
        );
    }
}
