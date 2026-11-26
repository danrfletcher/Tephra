//! Pins (atlas-27 PR-12): a remote file or folder marked "keep downloaded".
//!
//! A pin is one row of `remote_pins`, a durable table that names its source
//! and path as text (no foreign key). A file pin names the file's path; a
//! folder pin names the folder's path without a trailing slash and covers
//! every file under it, now and in the future: a file that appears under
//! the folder later is covered with no new row.
//!
//! What a pin does to an item is in `remote_items.state`: a covered file
//! that has a copy is `pinned` (not `cached`), and eviction never drops it
//! ([`crate::remote_evict`]). Every step here that changes an item is
//! conditional on the state the caller saw.

use rusqlite::{params, OptionalExtension};

use crate::remote_evict::PINNED;
use crate::remote_fetch::{ITEM_STATE_CACHED, ITEM_STATE_PINNED, ITEM_STATE_REMOTE};
use crate::{Cache, CacheError, RemoteItemRow};

/// A `remote_pins` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemotePin {
    pub id: i64,
    pub source: String,
    /// The file's path, or the folder's path with no trailing slash.
    pub path: String,
    pub is_folder: bool,
    pub created_at: i64,
}

/// What [`Cache::add_remote_pin`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinWrite {
    /// A row was added.
    New,
    /// The same pin (path and kind) was already there; nothing changed.
    Existing,
    /// The path is pinned, but as the other kind (a file where a folder was
    /// asked for, or the other way round); nothing changed.
    OtherKind,
}

/// Whether a pin on `pin_path` (a folder when `is_folder`) covers the file
/// at `path`. A folder covers only paths below it: `docs` covers
/// `docs/a.md` and not `docs-old/a.md` or `docs` itself. The empty folder
/// path covers the whole source. This is the same rule as the SQL eviction
/// uses, in Rust, for the planner.
pub fn pin_covers(pin_path: &str, is_folder: bool, path: &str) -> bool {
    if pin_path == path {
        return true;
    }
    if !is_folder {
        return false;
    }
    pin_path.is_empty()
        || path
            .strip_prefix(pin_path)
            .is_some_and(|rest| rest.starts_with('/'))
}

impl Cache {
    /// Adds a pin. A pin that is already there is not written twice.
    pub fn add_remote_pin(
        &self,
        source: &str,
        path: &str,
        is_folder: bool,
        now: i64,
    ) -> Result<PinWrite, CacheError> {
        let n = self.connection().execute(
            "INSERT OR IGNORE INTO remote_pins (source, path, is_folder, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![source, path, is_folder, now],
        )?;
        if n > 0 {
            return Ok(PinWrite::New);
        }
        let existing: Option<bool> = self
            .connection()
            .query_row(
                "SELECT is_folder FROM remote_pins WHERE source = ?1 AND path = ?2",
                params![source, path],
                |r| r.get::<_, i64>(0).map(|v| v != 0),
            )
            .optional()?;
        Ok(match existing {
            Some(kind) if kind != is_folder => PinWrite::OtherKind,
            _ => PinWrite::Existing,
        })
    }

    /// Removes the pin on `path` of that kind. `false` when there was none.
    pub fn remove_remote_pin(
        &self,
        source: &str,
        path: &str,
        is_folder: bool,
    ) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "DELETE FROM remote_pins WHERE source = ?1 AND path = ?2 AND is_folder = ?3",
            params![source, path, is_folder],
        )?;
        Ok(n > 0)
    }

    /// Gives pins of `source` new paths when what they name was renamed or
    /// moved on the remote. Each pin keeps its id, kind and `created_at`.
    /// A pin row that is gone (a sync that moved a file deletes the pin on
    /// the file's old path) is put back at the new path. All the old rows
    /// go first, so two pins may swap names. A pin whose new path already
    /// has another pin is dropped (the other one covers it). Call it inside
    /// the caller's transaction, then [`Cache::reconcile_remote_pins`].
    pub fn move_remote_pins(
        &self,
        source: &str,
        moves: &[(RemotePin, String)],
    ) -> Result<(), CacheError> {
        let conn = self.connection();
        for (pin, _) in moves {
            conn.execute(
                "DELETE FROM remote_pins WHERE source = ?1 AND id = ?2",
                params![source, pin.id],
            )?;
        }
        for (pin, new) in moves {
            conn.execute(
                "INSERT OR IGNORE INTO remote_pins (id, source, path, is_folder, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![pin.id, source, new, pin.is_folder, pin.created_at],
            )?;
        }
        Ok(())
    }

    /// The pins of `source`, by path.
    pub fn remote_pins(&self, source: &str) -> Result<Vec<RemotePin>, CacheError> {
        let mut stmt = self.connection().prepare(
            "SELECT id, source, path, is_folder, created_at FROM remote_pins \
             WHERE source = ?1 ORDER BY path",
        )?;
        let rows = stmt
            .query_map([source], |r| {
                Ok(RemotePin {
                    id: r.get(0)?,
                    source: r.get(1)?,
                    path: r.get(2)?,
                    is_folder: r.get::<_, i64>(3)? != 0,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The sources that have at least one pin.
    pub fn remote_pinned_sources(&self) -> Result<Vec<String>, CacheError> {
        let mut stmt = self
            .connection()
            .prepare("SELECT DISTINCT source FROM remote_pins ORDER BY source")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Whether some pin of `source` covers `path`, whether or not the file
    /// is in the index.
    pub fn remote_pin_covers(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let sql = format!(
            "SELECT EXISTS (SELECT 1 FROM (SELECT ?1 AS source, ?2 AS path) i WHERE {PINNED})"
        );
        let n: i64 = self
            .connection()
            .query_row(&sql, params![source, path], |r| r.get(0))?;
        Ok(n != 0)
    }

    /// Brings the state of `source`'s items in line with its pins: a
    /// covered `cached` file becomes `pinned`, and a `pinned` file that no
    /// pin covers any more becomes `cached` (and so can be evicted again).
    /// An item with unsent edits, or one that is downloading, is not
    /// touched. Returns how many items changed.
    pub fn reconcile_remote_pins(&self, source: &str) -> Result<usize, CacheError> {
        let up = format!(
            "UPDATE remote_items AS i SET state = ?3 \
             WHERE i.source = ?1 AND i.state = ?2 AND {PINNED}"
        );
        let down = format!(
            "UPDATE remote_items AS i SET state = ?3 \
             WHERE i.source = ?1 AND i.state = ?2 AND NOT {PINNED}"
        );
        let conn = self.connection();
        let a = conn.execute(&up, params![source, ITEM_STATE_CACHED, ITEM_STATE_PINNED])?;
        let b = conn.execute(&down, params![source, ITEM_STATE_PINNED, ITEM_STATE_CACHED])?;
        Ok(a + b)
    }

    /// The files of `source` that a pin covers and that have no copy yet
    /// (state `remote`): what the pin downloader fetches. A file that a
    /// failed fetch left in `remote` is among them (the next pass retries
    /// it).
    pub fn remote_pin_downloads(&self, source: &str) -> Result<Vec<RemoteItemRow>, CacheError> {
        let sql = format!(
            "SELECT i.path, i.kind, i.sha, i.size, i.state, i.reason, i.file_id, i.revision \
             FROM remote_items i WHERE i.source = ?1 AND i.state = ?2 AND {PINNED} \
             ORDER BY i.path"
        );
        let mut stmt = self.connection().prepare(&sql)?;
        let rows = stmt
            .query_map(params![source, ITEM_STATE_REMOTE], |r| {
                Ok(RemoteItemRow {
                    path: r.get(0)?,
                    kind: r.get(1)?,
                    sha: r.get(2)?,
                    size: r.get(3)?,
                    state: r.get(4)?,
                    reason: r.get(5)?,
                    file_id: r.get(6)?,
                    revision: r.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The listed bytes of every file that a pin covers, in every source,
    /// whether or not it is downloaded yet: what the pins will take once
    /// they are all down. `cache status` and the pin answer compare it with
    /// the limit.
    pub fn remote_pin_footprint(&self) -> Result<u64, CacheError> {
        let sql = format!(
            "SELECT COALESCE(SUM(i.size), 0) FROM remote_items i \
             JOIN remote_sources s ON s.name = i.source WHERE {PINNED}"
        );
        let n: i64 = self.connection().query_row(&sql, [], |r| r.get(0))?;
        Ok(n.max(0) as u64)
    }
}
