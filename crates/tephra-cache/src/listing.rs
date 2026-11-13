//! Folder listing over the cached `units` rows (`unit.list`, G1).
//!
//! The cache stores files only: a folder exists here when some unit's
//! `rel_path` has it as a prefix. [`list_folder`] derives the folder
//! entries from those prefixes, so it works for an offline source (whose
//! root can't be read) exactly as for an online one. An online caller
//! that wants empty folders, folder mtimes or folders hidden by the
//! ignore rules reads those from disk and keeps only this listing's file
//! entries (see `tephrad::unit_list`).
//!
//! Read-only: nothing here writes, so it is safe on a
//! [`crate::Cache::open_read_only`] connection and never blocks the
//! scan's writer (WAL).

use std::cmp::Ordering;
use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension};

use crate::sources::{self, SourceStatus};
use crate::CacheError;

/// The `kind` value of a folder entry. A unit's own kinds are the five
/// the `units.kind` CHECK allows; folders are never stored.
pub const FOLDER_KIND: &str = "folder";

/// One row of a folder listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEntry {
    /// Path relative to the source root, no leading or trailing slash.
    pub rel_path: String,
    /// The last component of `rel_path`.
    pub name: String,
    /// `folder`, or the unit's `prose`/`code`/`document`/`text-other`/
    /// `binary`.
    pub kind: String,
    /// A file's size in bytes (`None` for a row restored from a v1
    /// cache); a folder's number of distinct direct children.
    pub size: Option<i64>,
    /// A file's mtime in Unix seconds. Always `None` for a folder: the
    /// cache doesn't store folder mtimes.
    pub mtime: Option<i64>,
    /// Set only for a `prose` file: its frontmatter `title`, else its
    /// first cached heading.
    pub title: Option<String>,
    /// The file's row is provisional (restored from a v1 cache).
    pub needs_rescan: bool,
}

impl FolderEntry {
    pub fn is_folder(&self) -> bool {
        self.kind == FOLDER_KIND
    }
}

/// What the requested `folder` path is, according to the cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderState {
    /// The source root, or a prefix of at least one cached unit.
    Folder,
    /// A cached unit is exactly this path.
    File,
    /// Neither. For an online source this can still be an empty folder
    /// on disk: the cache can't tell.
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderListing {
    pub source_status: SourceStatus,
    pub state: FolderState,
    /// Empty unless `state` is [`FolderState::Folder`].
    pub entries: Vec<FolderEntry>,
    /// Some listed file is a provisional row from a v1 cache.
    pub needs_rescan: bool,
}

/// The order every listing uses: folders before files; within each
/// group, names compared case-insensitively (Unicode lowercase), then
/// by bytes to break ties. Total and deterministic.
pub fn compare_entries(a: &FolderEntry, b: &FolderEntry) -> Ordering {
    compare_names(a.is_folder(), &a.name, b.is_folder(), &b.name)
}

/// [`compare_entries`] on the parts it uses, for callers that haven't
/// built a [`FolderEntry`].
pub fn compare_names(a_folder: bool, a_name: &str, b_folder: bool, b_name: &str) -> Ordering {
    b_folder
        .cmp(&a_folder)
        .then_with(|| a_name.to_lowercase().cmp(&b_name.to_lowercase()))
        .then_with(|| a_name.as_bytes().cmp(b_name.as_bytes()))
}

#[derive(Default)]
struct Node {
    file: Option<FileRow>,
    children: BTreeMap<String, Node>,
}

impl Node {
    /// A node is a folder unless it is a leaf that some unit's path
    /// ends at.
    fn is_folder(&self) -> bool {
        self.file.is_none() || !self.children.is_empty()
    }
}

struct FileRow {
    id: i64,
    mtime: Option<i64>,
    kind: String,
    size: Option<i64>,
    needs_rescan: bool,
}

/// How many path levels below the listed folder a non-recursive listing
/// keeps: the direct children, and their own children (to count a
/// folder's size).
const NON_RECURSIVE_DEPTH: usize = 2;

pub(crate) fn list_folder(
    conn: &Connection,
    source_name: &str,
    folder: &str,
    recursive: bool,
) -> Result<FolderListing, CacheError> {
    let source = sources::get_by_name(conn, source_name)?
        .ok_or_else(|| CacheError::SourceNotFound(source_name.to_string()))?;

    let empty = |state| FolderListing {
        source_status: source.status,
        state,
        entries: Vec::new(),
        needs_rescan: false,
    };

    if !folder.is_empty() {
        let is_file = conn
            .query_row(
                "SELECT 1 FROM units WHERE source_id = ?1 AND rel_path = ?2",
                rusqlite::params![source.id, folder],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if is_file {
            return Ok(empty(FolderState::File));
        }
    }

    let prefix = if folder.is_empty() {
        String::new()
    } else {
        format!("{folder}/")
    };

    // Every path under `folder/` sorts between `folder/` and `folder0`
    // ('0' is the byte after '/'), so a range scan finds them on the
    // (source_id, rel_path) unique index with no LIKE-escaping to get
    // wrong for names containing `%` or `_`.
    let mut root = Node::default();
    let mut any = false;
    {
        let (sql, params): (&str, Vec<rusqlite::types::Value>) = if folder.is_empty() {
            (
                "SELECT id, rel_path, mtime, kind, size, needs_rescan FROM units \
                 WHERE source_id = ?1",
                vec![source.id.into()],
            )
        } else {
            (
                "SELECT id, rel_path, mtime, kind, size, needs_rescan FROM units \
                 WHERE source_id = ?1 AND rel_path >= ?2 AND rel_path < ?3",
                vec![
                    source.id.into(),
                    prefix.clone().into(),
                    format!("{folder}0").into(),
                ],
            )
        };
        let mut stmt = conn.prepare(sql)?;
        let mut rows = stmt.query(rusqlite::params_from_iter(params))?;
        let depth_cap = if recursive {
            usize::MAX
        } else {
            NON_RECURSIVE_DEPTH
        };
        while let Some(row) = rows.next()? {
            let rel_path: String = row.get(1)?;
            let Some(rest) = rel_path.strip_prefix(prefix.as_str()) else {
                continue;
            };
            let file = FileRow {
                id: row.get(0)?,
                mtime: row.get(2)?,
                kind: row.get(3)?,
                size: row.get(4)?,
                needs_rescan: row.get::<_, i64>(5)? != 0,
            };
            if insert_path(&mut root, rest, file, depth_cap) {
                any = true;
            }
        }
    }

    if !folder.is_empty() && !any {
        return Ok(empty(FolderState::Missing));
    }

    let mut builder = Builder {
        recursive,
        titles: TitleLookup::prepare(conn)?,
        out: Vec::new(),
        needs_rescan: false,
    };
    builder.emit_level(&root, &prefix)?;
    Ok(FolderListing {
        source_status: source.status,
        state: FolderState::Folder,
        entries: builder.out,
        needs_rescan: builder.needs_rescan,
    })
}

/// Adds `rest` (a path below the listed folder) to the tree, keeping at
/// most `depth_cap` levels. Returns whether anything was added.
fn insert_path(root: &mut Node, rest: &str, file: FileRow, depth_cap: usize) -> bool {
    let segments: Vec<&str> = rest.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() {
        return false;
    }
    let last = segments.len() - 1;
    let mut node = root;
    let mut file = Some(file);
    for (depth, segment) in segments.iter().enumerate() {
        if depth >= depth_cap {
            break;
        }
        node = node.children.entry((*segment).to_string()).or_default();
        if depth == last {
            node.file = file.take();
        }
    }
    true
}

struct Builder<'c> {
    recursive: bool,
    titles: TitleLookup<'c>,
    out: Vec<FolderEntry>,
    needs_rescan: bool,
}

impl Builder<'_> {
    /// Appends `children` of the folder at `parent_prefix` (`""` or
    /// `"a/b/"`) in listing order, descending depth-first when the
    /// listing is recursive.
    fn emit_level(&mut self, parent: &Node, parent_prefix: &str) -> Result<(), CacheError> {
        let mut level: Vec<(&String, &Node)> = parent.children.iter().collect();
        level.sort_by(|(a_name, a), (b_name, b)| {
            compare_names(a.is_folder(), a_name, b.is_folder(), b_name)
        });
        for (name, node) in level {
            let rel_path = format!("{parent_prefix}{name}");
            if node.is_folder() {
                self.out.push(FolderEntry {
                    rel_path: rel_path.clone(),
                    name: name.clone(),
                    kind: FOLDER_KIND.to_string(),
                    size: Some(node.children.len() as i64),
                    mtime: None,
                    title: None,
                    needs_rescan: false,
                });
                if self.recursive {
                    self.emit_level(node, &format!("{rel_path}/"))?;
                }
            } else if let Some(file) = &node.file {
                let title = if file.kind == "prose" {
                    self.titles.title_for(file.id)?
                } else {
                    None
                };
                self.needs_rescan |= file.needs_rescan;
                self.out.push(FolderEntry {
                    rel_path,
                    name: name.clone(),
                    kind: file.kind.clone(),
                    size: file.size,
                    mtime: file.mtime,
                    title,
                    needs_rescan: file.needs_rescan,
                });
            }
        }
        Ok(())
    }
}

/// A prose file's title: its frontmatter `title` (when that is a
/// non-blank string), else its first cached heading (by line), else
/// none.
struct TitleLookup<'c> {
    frontmatter: rusqlite::Statement<'c>,
    heading: rusqlite::Statement<'c>,
}

impl<'c> TitleLookup<'c> {
    fn prepare(conn: &'c Connection) -> Result<Self, CacheError> {
        Ok(TitleLookup {
            // Nested CASE: SQLite doesn't promise AND short-circuits,
            // and `json_type` on malformed JSON is an error.
            frontmatter: conn.prepare(
                "SELECT CASE WHEN frontmatter_json IS NOT NULL AND json_valid(frontmatter_json) \
                        THEN CASE WHEN json_type(frontmatter_json, '$.title') = 'text' \
                                  THEN json_extract(frontmatter_json, '$.title') END \
                   END \
                 FROM units WHERE id = ?1",
            )?,
            heading: conn.prepare(
                "SELECT text FROM headings WHERE unit_id = ?1 ORDER BY line, id LIMIT 1",
            )?,
        })
    }

    fn title_for(&mut self, unit_id: i64) -> Result<Option<String>, CacheError> {
        let from_frontmatter: Option<String> = self
            .frontmatter
            .query_row([unit_id], |row| row.get(0))
            .optional()?
            .flatten();
        if let Some(title) = from_frontmatter.filter(|t| !t.trim().is_empty()) {
            return Ok(Some(title));
        }
        let from_heading: Option<String> = self
            .heading
            .query_row([unit_id], |row| row.get(0))
            .optional()?;
        Ok(from_heading.filter(|t| !t.trim().is_empty()))
    }
}
