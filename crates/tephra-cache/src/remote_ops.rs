//! The typed helpers of the remote tables the first index of a GitHub
//! source uses: `remote_sources`, `remote_items` and `remote_cursors`
//! (the upload buffer and the pins come in later PRs). The vocabulary the
//! tables' `CHECK` constraints enforce is in [`crate::remote`].
//!
//! A remote source is also a row of `sources`, whose `path` is not a
//! folder but the marker [`remote_root`] (`remote:github:<name>`): the
//! `units`, `links` and graph code all hang off a `sources` row, so a
//! remote source's files are ordinary `units` rows and need no second path
//! through them. [`SourceRow::is_remote`] tells the two apart; every
//! caller that treats `path` as a folder on disk must skip a remote row.
//! The per-file remote facts (state, size, the git blob sha) live in
//! `remote_items`, which survives a rebuild; a rebuild drops the `units`
//! rows, and [`Cache::restore_remote_units`] makes them again from
//! `remote_items` with no remote call.

use rusqlite::{params, OptionalExtension};

use crate::sources::SourceStatus;
use crate::units::NewUnit;
use crate::{Cache, CacheError, SourceRow};

/// What a remote source's `sources.path` starts with: a marker, never a
/// folder.
pub const REMOTE_ROOT_PREFIX: &str = "remote:";

/// The `sources.path` of the remote source `name` of `kind`.
pub fn remote_root(kind: &str, name: &str) -> String {
    format!("{REMOTE_ROOT_PREFIX}{kind}:{name}")
}

/// The `remote_sources.status` of a source whose text is still being read.
pub const STATUS_INDEXING: &str = "indexing";
/// The `remote_sources.status` of a source whose first index ran to the end.
pub const STATUS_ACTIVE: &str = "active";

/// `remote_items.kind` of a symlink (never followed). A `binary` unit,
/// listed by name only.
pub const ITEM_KIND_SYMLINK: &str = "symlink";
/// `remote_items.kind` of a submodule (never fetched). A `binary` unit,
/// listed by name only.
pub const ITEM_KIND_SUBMODULE: &str = "submodule";

/// `remote_items.reason` of a file the index found to be a Git LFS
/// pointer: kept by name only, its object never fetched.
pub const REASON_LFS_POINTER: &str = "lfs_pointer";

impl SourceRow {
    /// Whether this is a remote source (its `path` is [`remote_root`], not
    /// a folder).
    pub fn is_remote(&self) -> bool {
        self.path.starts_with(REMOTE_ROOT_PREFIX)
    }
}

/// A `remote_sources` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSourceRow {
    pub name: String,
    pub kind: String,
    /// `owner/repo` for GitHub.
    pub target: String,
    pub branch: Option<String>,
    pub cache: bool,
    pub status: String,
    pub status_reason: Option<String>,
    pub created_at: i64,
    pub last_sync_at: Option<i64>,
}

/// A `remote_sources` row to add.
#[derive(Debug, Clone, Copy)]
pub struct NewRemoteSource<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub target: &'a str,
    pub branch: Option<&'a str>,
    /// The commit sha the listing was taken at: the source's first cursor.
    pub cursor: &'a str,
    pub now: i64,
}

/// One file of a remote listing.
#[derive(Debug, Clone, Copy)]
pub struct NewRemoteItem<'a> {
    pub path: &'a str,
    /// `prose`, `code`, `document`, `text-other`, `binary`, or one of
    /// [`ITEM_KIND_SYMLINK`] / [`ITEM_KIND_SUBMODULE`]: what the listing
    /// knew of it from its name. Fixed at listing, even if reading the
    /// file later finds it is binary (the unit row then says so).
    pub kind: &'a str,
    /// The git blob sha (the commit sha for a submodule).
    pub sha: &'a str,
    pub size: Option<i64>,
}

/// A `remote_items` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteItemRow {
    pub path: String,
    pub kind: Option<String>,
    pub sha: Option<String>,
    pub size: Option<i64>,
    pub state: String,
    pub reason: Option<String>,
}

/// A `remote_cursors` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCursor {
    pub cursor: Option<String>,
    pub index_complete: bool,
}

/// How far the first index of a source has got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexProgress {
    pub done: usize,
    pub total: usize,
}

/// Whether the index reads a file of this listing kind: a text kind, not
/// a symlink, a submodule or a known binary.
pub fn is_text_item_kind(kind: &str) -> bool {
    matches!(kind, "prose" | "code" | "document" | "text-other")
}

/// The `units.kind` a listing kind is stored as.
fn unit_kind_of_item(kind: &str) -> &str {
    match kind {
        ITEM_KIND_SYMLINK | ITEM_KIND_SUBMODULE => "binary",
        other => other,
    }
}

fn source_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RemoteSourceRow> {
    Ok(RemoteSourceRow {
        name: r.get(0)?,
        kind: r.get(1)?,
        target: r.get(2)?,
        branch: r.get(3)?,
        cache: r.get::<_, i64>(4)? != 0,
        status: r.get(5)?,
        status_reason: r.get(6)?,
        created_at: r.get(7)?,
        last_sync_at: r.get(8)?,
    })
}

const SOURCE_COLUMNS: &str =
    "name, kind, target, branch, cache, status, status_reason, created_at, last_sync_at";

fn insert_unit_for_item(
    cache: &Cache,
    source_id: i64,
    item: &NewRemoteItem<'_>,
    max_indexed_bytes: u64,
) -> Result<(), CacheError> {
    let too_large = item.size.is_some_and(|s| s as u64 > max_indexed_bytes);
    cache.insert_unit(&NewUnit {
        source_id,
        rel_path: item.path,
        mtime: None,
        frontmatter_json: None,
        kind: unit_kind_of_item(item.kind),
        size: item.size,
        too_large,
        hash: None,
        inode: None,
        device: None,
    })?;
    Ok(())
}

impl Cache {
    /// Adds a remote source with its whole listing in one transaction: the
    /// `sources` row (`active`, path [`remote_root`]), one `units` row and
    /// one `remote_items` row (state `remote`) per file, the
    /// `remote_sources` row (status `indexing`) and its cursor (index not
    /// complete). Nothing is left behind if any step fails. A name already
    /// in `sources` is [`CacheError::DuplicateSource`].
    pub fn add_remote_source(
        &self,
        new: &NewRemoteSource<'_>,
        items: &[NewRemoteItem<'_>],
        max_indexed_bytes: u64,
    ) -> Result<i64, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let source_id = self.register_source(
            new.name,
            &remote_root(new.kind, new.name),
            SourceStatus::Active,
        )?;
        self.connection().execute(
            "INSERT INTO remote_sources \
             (name, kind, target, branch, cache, status, status_reason, created_at, last_sync_at) \
             VALUES (?1, ?2, ?3, ?4, 1, ?5, NULL, ?6, ?6)",
            params![
                new.name,
                new.kind,
                new.target,
                new.branch,
                STATUS_INDEXING,
                new.now
            ],
        )?;
        self.connection().execute(
            "INSERT INTO remote_cursors (source, cursor, index_complete, updated_at) \
             VALUES (?1, ?2, 0, ?3)",
            params![new.name, new.cursor, new.now],
        )?;
        {
            let mut stmt = self.connection().prepare(
                "INSERT INTO remote_items (source, path, kind, sha, size, state) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'remote')",
            )?;
            for item in items {
                stmt.execute(params![new.name, item.path, item.kind, item.sha, item.size])?;
                insert_unit_for_item(self, source_id, item, max_indexed_bytes)?;
            }
        }
        tx.commit()?;
        Ok(source_id)
    }

    /// The remote source `name`, if it is one.
    pub fn remote_source(&self, name: &str) -> Result<Option<RemoteSourceRow>, CacheError> {
        Ok(self
            .connection()
            .query_row(
                &format!("SELECT {SOURCE_COLUMNS} FROM remote_sources WHERE name = ?1"),
                [name],
                source_from_row,
            )
            .optional()?)
    }

    /// Every remote source, oldest first.
    pub fn remote_sources(&self) -> Result<Vec<RemoteSourceRow>, CacheError> {
        let mut stmt = self.connection().prepare(&format!(
            "SELECT {SOURCE_COLUMNS} FROM remote_sources ORDER BY created_at, name"
        ))?;
        let rows = stmt
            .query_map([], source_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every item of the remote source `name`, in path order. Read-only.
    pub fn remote_items(&self, name: &str) -> Result<Vec<RemoteItemRow>, CacheError> {
        let mut stmt = self.connection().prepare(
            "SELECT path, kind, sha, size, state, reason FROM remote_items \
             WHERE source = ?1 ORDER BY path",
        )?;
        let rows = stmt
            .query_map([name], |r| {
                Ok(RemoteItemRow {
                    path: r.get(0)?,
                    kind: r.get(1)?,
                    sha: r.get(2)?,
                    size: r.get(3)?,
                    state: r.get(4)?,
                    reason: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The paths of `name`'s units that have a content hash: the files the
    /// index has read.
    fn hashed_paths(&self, name: &str) -> Result<std::collections::HashSet<String>, CacheError> {
        let mut stmt = self.connection().prepare(
            "SELECT u.rel_path FROM units u JOIN sources s ON s.id = u.source_id \
             WHERE s.name = ?1 AND u.hash IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([name], |r| r.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// The items of `name` the index still has to read: a text item (see
    /// [`is_text_item_kind`]) within the size cap, not known to be an LFS
    /// pointer, whose unit has no content hash yet. In path order.
    pub fn remote_pending_items(
        &self,
        name: &str,
        max_indexed_bytes: u64,
    ) -> Result<Vec<RemoteItemRow>, CacheError> {
        let hashed = self.hashed_paths(name)?;
        Ok(self
            .remote_items(name)?
            .into_iter()
            .filter(|i| i.kind.as_deref().is_some_and(is_text_item_kind))
            .filter(|i| !i.size.is_some_and(|s| s as u64 > max_indexed_bytes))
            .filter(|i| i.reason.is_none() && !hashed.contains(&i.path))
            .collect())
    }

    /// `done` of `total` text items of `name` read so far. An LFS pointer
    /// counts as read.
    pub fn remote_index_progress(
        &self,
        name: &str,
        max_indexed_bytes: u64,
    ) -> Result<IndexProgress, CacheError> {
        let hashed = self.hashed_paths(name)?;
        let items = self.remote_items(name)?;
        let candidates = items
            .iter()
            .filter(|i| i.kind.as_deref().is_some_and(is_text_item_kind))
            .filter(|i| !i.size.is_some_and(|s| s as u64 > max_indexed_bytes));
        let (mut done, mut total) = (0, 0);
        for i in candidates {
            total += 1;
            if i.reason.is_some() || hashed.contains(&i.path) {
                done += 1;
            }
        }
        Ok(IndexProgress { done, total })
    }

    /// Sets (or clears) the reason of the item `path` of `source`.
    pub fn set_remote_item_reason(
        &self,
        source: &str,
        path: &str,
        reason: Option<&str>,
    ) -> Result<(), CacheError> {
        self.connection().execute(
            "UPDATE remote_items SET reason = ?3 WHERE source = ?1 AND path = ?2",
            params![source, path, reason],
        )?;
        Ok(())
    }

    /// Sets a remote source's `status` and `status_reason`, and, when
    /// given, `last_sync_at`.
    pub fn set_remote_status(
        &self,
        name: &str,
        status: &str,
        reason: Option<&str>,
        last_sync_at: Option<i64>,
    ) -> Result<(), CacheError> {
        self.connection().execute(
            "UPDATE remote_sources SET status = ?2, status_reason = ?3, \
             last_sync_at = COALESCE(?4, last_sync_at) WHERE name = ?1",
            params![name, status, reason, last_sync_at],
        )?;
        Ok(())
    }

    /// The cursor row of `name`.
    pub fn remote_cursor(&self, name: &str) -> Result<Option<RemoteCursor>, CacheError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT cursor, index_complete FROM remote_cursors WHERE source = ?1",
                [name],
                |r| {
                    Ok(RemoteCursor {
                        cursor: r.get(0)?,
                        index_complete: r.get::<_, i64>(1)? != 0,
                    })
                },
            )
            .optional()?)
    }

    /// Marks the first index of `name` finished (or not).
    pub fn set_remote_index_complete(
        &self,
        name: &str,
        complete: bool,
        now: i64,
    ) -> Result<(), CacheError> {
        self.connection().execute(
            "UPDATE remote_cursors SET index_complete = ?2, updated_at = ?3 WHERE source = ?1",
            params![name, complete as i64, now],
        )?;
        Ok(())
    }

    /// Makes the `units` rows of the remote source `name` again from its
    /// `remote_items` when it has none (a rebuild drops them), with no
    /// remote call, and marks its first index unfinished so the files are
    /// read again. Returns whether it did anything. A `sources` row that is
    /// missing is made too.
    pub fn restore_remote_units(
        &self,
        name: &str,
        max_indexed_bytes: u64,
    ) -> Result<bool, CacheError> {
        let Some(remote) = self.remote_source(name)? else {
            return Ok(false);
        };
        let tx = self.connection().unchecked_transaction()?;
        let source_id = match self.get_source(name)? {
            Some(row) => row.id,
            None => {
                self.register_source(name, &remote_root(&remote.kind, name), SourceStatus::Active)?
            }
        };
        let have: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM units WHERE source_id = ?1",
            [source_id],
            |r| r.get(0),
        )?;
        let items = self.remote_items(name)?;
        if have > 0 || items.is_empty() {
            return Ok(false);
        }
        for item in &items {
            insert_unit_for_item(
                self,
                source_id,
                &NewRemoteItem {
                    path: &item.path,
                    kind: item.kind.as_deref().unwrap_or("text-other"),
                    sha: item.sha.as_deref().unwrap_or(""),
                    size: item.size,
                },
                max_indexed_bytes,
            )?;
        }
        self.connection().execute(
            "UPDATE remote_items SET reason = NULL WHERE source = ?1",
            [name],
        )?;
        self.set_remote_index_complete(name, false, 0)?;
        self.set_remote_status(name, STATUS_INDEXING, None, None)?;
        tx.commit()?;
        Ok(true)
    }
}
