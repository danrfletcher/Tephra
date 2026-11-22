//! The rows eviction reads and writes (atlas-27 PR-8): what is cached, the
//! per-source cache flag, and the one conditional step that sends a copy
//! back to `remote`.
//!
//! Every step that changes a row is conditional on what the caller saw, so
//! a step that lost a race (a fetch that used the file, a pin, an edit)
//! changes nothing and says so.

use rusqlite::{params, OptionalExtension};

use crate::evict::EvictItem;
use crate::remote_fetch::{
    ITEM_STATE_CACHED, ITEM_STATE_NOT_PUSHED, ITEM_STATE_PENDING_UPLOAD, ITEM_STATE_PINNED,
    ITEM_STATE_REMOTE,
};
use crate::{Cache, CacheError};

/// "Pinned", as SQL over `remote_items` aliased `i`: a pin on the file
/// itself or on a folder above it (a folder pin with the empty path covers
/// the whole source).
const PINNED: &str = "EXISTS (SELECT 1 FROM remote_pins p \
     WHERE p.source = i.source AND (p.path = i.path OR (p.is_folder = 1 AND \
       (p.path = '' OR substr(i.path, 1, length(p.path) + 1) = p.path || '/'))))";

/// An edit of the file waits in the upload queue.
const QUEUED_EDIT: &str =
    "EXISTS (SELECT 1 FROM upload_queue q WHERE q.source = i.source AND q.path = i.path)";

impl Cache {
    /// Every item of every source that has a copy on disk, with the facts
    /// the planner needs ([`crate::plan_eviction`]). Read-only.
    pub fn remote_evict_items(&self) -> Result<Vec<EvictItem>, CacheError> {
        let sql = format!(
            "SELECT i.source, i.path, i.state, i.size, i.last_used, s.cache, \
                    {PINNED}, {QUEUED_EDIT} \
             FROM remote_items i JOIN remote_sources s ON s.name = i.source \
             WHERE i.state IN (?1, ?2, ?3, ?4) ORDER BY i.source, i.path"
        );
        let mut stmt = self.connection().prepare(&sql)?;
        let rows = stmt
            .query_map(
                params![
                    ITEM_STATE_CACHED,
                    ITEM_STATE_PINNED,
                    ITEM_STATE_PENDING_UPLOAD,
                    ITEM_STATE_NOT_PUSHED
                ],
                |r| {
                    Ok(EvictItem {
                        source: r.get(0)?,
                        path: r.get(1)?,
                        state: r.get(2)?,
                        size: r.get(3)?,
                        last_used: r.get(4)?,
                        source_cache: r.get::<_, i64>(5)? != 0,
                        pinned: r.get::<_, i64>(6)? != 0,
                        edit_queued: r.get::<_, i64>(7)? != 0,
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// What every source's files take in the cache: the sum of
    /// [`Cache::remote_cache_bytes`] over all sources.
    pub fn remote_total_cache_bytes(&self) -> Result<u64, CacheError> {
        let n: i64 = self.connection().query_row(
            "SELECT COALESCE(SUM(i.size), 0) FROM remote_items i \
             JOIN remote_sources s ON s.name = i.source \
             WHERE i.state IN (?1, ?2, ?3, ?4)",
            params![
                ITEM_STATE_CACHED,
                ITEM_STATE_PINNED,
                ITEM_STATE_PENDING_UPLOAD,
                ITEM_STATE_NOT_PUSHED
            ],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// Sets the cache flag of the remote source `name`. Returns the flag it
    /// had, or `None` when there is no such remote source.
    pub fn set_remote_source_cache(
        &self,
        name: &str,
        on: bool,
    ) -> Result<Option<bool>, CacheError> {
        let before: Option<i64> = self
            .connection()
            .query_row(
                "SELECT cache FROM remote_sources WHERE name = ?1",
                [name],
                |r| r.get(0),
            )
            .optional()?;
        let Some(before) = before else {
            return Ok(None);
        };
        if (before != 0) != on {
            self.connection().execute(
                "UPDATE remote_sources SET cache = ?2 WHERE name = ?1",
                params![name, on as i64],
            )?;
        }
        Ok(Some(before != 0))
    }

    /// `cached -> remote` for an item that still qualifies, in one
    /// statement: `cached`, not used since `cutoff` (its `last_used` is at
    /// or before it), no pin, no queued edit, and, when `needs_cache_off`,
    /// its source's flag is still off. This is the claim an eviction makes
    /// BEFORE it deletes the file: a fetch that touched `last_used` first
    /// wins (the claim changes nothing), and one that comes after sees
    /// `remote` and waits for the eviction (see `remote/evict.rs`).
    /// `false` when the row did not qualify.
    pub fn drop_remote_copy(
        &self,
        source: &str,
        path: &str,
        cutoff: i64,
        needs_cache_off: bool,
    ) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            &format!(
                "UPDATE remote_items AS i SET state = ?3 \
                 WHERE i.source = ?1 AND i.path = ?2 AND i.state = ?4 \
                   AND (i.last_used IS NULL OR i.last_used <= ?5) \
                   AND (?6 = 0 OR EXISTS (SELECT 1 FROM remote_sources s \
                                          WHERE s.name = i.source AND s.cache = 0)) \
                   AND NOT {PINNED} AND NOT {QUEUED_EDIT}"
            ),
            params![
                source,
                path,
                ITEM_STATE_REMOTE,
                ITEM_STATE_CACHED,
                cutoff,
                needs_cache_off as i64
            ],
        )?;
        Ok(n > 0)
    }

    /// Undoes [`Cache::drop_remote_copy`] for a file that could not be
    /// deleted: `remote -> cached`. `false` when the row is no longer
    /// `remote` (a fetch took it, a sync changed it).
    pub fn restore_remote_copy(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE remote_items SET state = ?3 \
             WHERE source = ?1 AND path = ?2 AND state = ?4",
            params![source, path, ITEM_STATE_CACHED, ITEM_STATE_REMOTE],
        )?;
        Ok(n > 0)
    }

    /// Notes what the Drive copy of `path` is as Tephra wrote it: `hash` is
    /// the git blob sha of its bytes. Replaces an earlier stamp.
    pub fn record_remote_copy_stamp(
        &self,
        source: &str,
        path: &str,
        hash: &str,
    ) -> Result<(), CacheError> {
        self.connection().execute(
            "INSERT OR REPLACE INTO remote_copy_stamps (source, path, hash) VALUES (?1, ?2, ?3)",
            params![source, path, hash],
        )?;
        Ok(())
    }

    /// The hash [`Cache::record_remote_copy_stamp`] noted, if any.
    pub fn remote_copy_stamp(
        &self,
        source: &str,
        path: &str,
    ) -> Result<Option<String>, CacheError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT hash FROM remote_copy_stamps WHERE source = ?1 AND path = ?2",
                params![source, path],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Forgets the stamp of a copy that is gone. `false` when there was none.
    pub fn clear_remote_copy_stamp(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "DELETE FROM remote_copy_stamps WHERE source = ?1 AND path = ?2",
            params![source, path],
        )?;
        Ok(n > 0)
    }

    /// The items that say `remote` but have been used before: the only ones
    /// that can have a file left on disk by an eviction that was cut short
    /// (its claim wrote `remote` before the delete). `(source, path)`,
    /// read-only.
    pub fn remote_items_used_before(&self) -> Result<Vec<(String, String)>, CacheError> {
        let mut stmt = self.connection().prepare(
            "SELECT source, path FROM remote_items \
             WHERE state = ?1 AND last_used IS NOT NULL ORDER BY source, path",
        )?;
        let rows = stmt
            .query_map(params![ITEM_STATE_REMOTE], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// `remote -> cached` for an item whose file turned out to be there,
    /// the remote's own bytes (a drop that was cut short between its claim
    /// and its delete): the copy counts again and can be dropped again.
    /// `false` when the row is not `remote` any more.
    pub fn readopt_remote_copy(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE remote_items SET state = ?3 \
             WHERE source = ?1 AND path = ?2 AND state = ?4",
            params![source, path, ITEM_STATE_CACHED, ITEM_STATE_REMOTE],
        )?;
        Ok(n > 0)
    }

    /// The total bytes of pinned items (state `pinned`, or a pin on a
    /// `cached` file or under a folder pin): what `cache status` compares
    /// with the limit for its warning.
    pub fn remote_pinned_bytes(&self) -> Result<u64, CacheError> {
        let sql = format!(
            "SELECT COALESCE(SUM(i.size), 0) FROM remote_items i \
             JOIN remote_sources s ON s.name = i.source \
             WHERE i.state IN (?1, ?2) AND (i.state = ?2 OR {PINNED})"
        );
        let n: i64 = self.connection().query_row(
            &sql,
            params![ITEM_STATE_CACHED, ITEM_STATE_PINNED],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }
}
