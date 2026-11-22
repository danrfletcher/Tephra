//! The row-level steps of `unit.fetch` (PR-7): reading what a fetch needs
//! of an item, touching `last_used`, and moving the item through
//! `remote -> downloading -> cached` (or back to `remote` with the reason
//! the download failed).
//!
//! Every state change is conditional on the state the caller saw, so a
//! step that lost a race (a poll that deleted the row, a second fetch)
//! changes nothing and says so.

use rusqlite::{params, OptionalExtension};

use crate::{Cache, CacheError};

/// `remote_items.state` of a file that is not on disk.
pub const ITEM_STATE_REMOTE: &str = "remote";
/// A fetch is writing the file.
pub const ITEM_STATE_DOWNLOADING: &str = "downloading";
/// The file is on disk, as the remote has it.
pub const ITEM_STATE_CACHED: &str = "cached";
/// Cached and kept: never evicted.
pub const ITEM_STATE_PINNED: &str = "pinned";
/// Edited, waiting to be sent.
pub const ITEM_STATE_PENDING_UPLOAD: &str = "pending_upload";
/// Edited, sent and refused or not yet pushed.
pub const ITEM_STATE_NOT_PUSHED: &str = "not_pushed";
/// The remote deleted the file (the row is kept).
pub const ITEM_STATE_MISSING: &str = "missing";

/// The start of a `remote_items.reason` a failed fetch leaves; the rest
/// says why, in words. A reason that does not start with it (an LFS
/// pointer, a delete on the remote) is never written or cleared by a
/// fetch.
pub const FETCH_REASON_PREFIX: &str = "fetch failed: ";

/// What a fetch needs of one `remote_items` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFetchRow {
    pub source: String,
    pub path: String,
    pub kind: Option<String>,
    /// The remote's id for the file (a Drive file id); `None` for GitHub.
    pub file_id: Option<String>,
    /// The git blob sha (GitHub).
    pub sha: Option<String>,
    pub size: Option<i64>,
    pub state: String,
    pub reason: Option<String>,
    pub last_used: Option<i64>,
}

/// Whether a file in `state` has a copy on disk in Tephra's cache.
pub fn state_has_copy(state: &str) -> bool {
    matches!(
        state,
        ITEM_STATE_CACHED | ITEM_STATE_PINNED | ITEM_STATE_PENDING_UPLOAD | ITEM_STATE_NOT_PUSHED
    )
}

/// What a file takes in the cache: its listed size when it has a copy, else 0.
pub fn item_cache_bytes(state: &str, size: Option<i64>) -> u64 {
    if state_has_copy(state) {
        size.unwrap_or(0).max(0) as u64
    } else {
        0
    }
}

impl Cache {
    /// The row of `source`'s file `path`, if there is one.
    pub fn remote_fetch_row(
        &self,
        source: &str,
        path: &str,
    ) -> Result<Option<RemoteFetchRow>, CacheError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT source, path, kind, file_id, sha, size, state, reason, last_used \
                 FROM remote_items WHERE source = ?1 AND path = ?2",
                params![source, path],
                |r| {
                    Ok(RemoteFetchRow {
                        source: r.get(0)?,
                        path: r.get(1)?,
                        kind: r.get(2)?,
                        file_id: r.get(3)?,
                        sha: r.get(4)?,
                        size: r.get(5)?,
                        state: r.get(6)?,
                        reason: r.get(7)?,
                        last_used: r.get(8)?,
                    })
                },
            )
            .optional()?)
    }

    /// Whether another file of `source` has a path that differs from
    /// `path` only by letter case: on a case-insensitive disk the two
    /// would be one file.
    pub fn remote_has_case_sibling(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        // SQLite's `lower()` folds ASCII letters only, so two names that
        // differ only in the case of a non-ASCII letter are not found
        // (a known gap: such names are rare in a code or notes tree).
        let mut stmt = self.connection().prepare(
            "SELECT path FROM remote_items \
             WHERE source = ?1 AND path <> ?2 AND lower(path) = lower(?2)",
        )?;
        let found = stmt
            .query_map(params![source, path], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(!found.is_empty())
    }

    /// Sets `last_used` of the file to `now`. `false` when there is no such
    /// row.
    pub fn touch_remote_last_used(
        &self,
        source: &str,
        path: &str,
        now: i64,
    ) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE remote_items SET last_used = ?3 WHERE source = ?1 AND path = ?2",
            params![source, path, now],
        )?;
        Ok(n > 0)
    }

    /// `remote -> downloading`, clearing a reason a failed fetch left. A
    /// file that already says `downloading` is taken too: only one fetch
    /// of a file runs in a process, so such a row is a download that died
    /// with its process. `false` when the file is in another state (an
    /// edit, a pin, a copy) or the row is gone.
    pub fn begin_remote_download(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE remote_items SET state = ?3, \
               reason = CASE WHEN reason LIKE ?5 THEN NULL ELSE reason END \
             WHERE source = ?1 AND path = ?2 AND state IN (?4, ?3)",
            params![
                source,
                path,
                ITEM_STATE_DOWNLOADING,
                ITEM_STATE_REMOTE,
                format!("{FETCH_REASON_PREFIX}%")
            ],
        )?;
        Ok(n > 0)
    }

    /// `downloading -> cached`, provided the row still has the `sha` and
    /// `size` the download was checked against (a poll that re-listed the
    /// file meanwhile makes the bytes stale). `false` when it does not.
    ///
    /// `last_used` is set to `now` in the same statement: a download that
    /// took longer than the cache's idle window must not be dropped by an
    /// eviction pass in the instant before the fetch notes its use.
    pub fn finish_remote_download(
        &self,
        source: &str,
        path: &str,
        sha: Option<&str>,
        size: Option<i64>,
        now: i64,
    ) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE remote_items SET state = ?3, last_used = ?7 \
             WHERE source = ?1 AND path = ?2 AND state = ?4 AND sha IS ?5 AND size IS ?6",
            params![
                source,
                path,
                ITEM_STATE_CACHED,
                ITEM_STATE_DOWNLOADING,
                sha,
                size,
                now
            ],
        )?;
        Ok(n > 0)
    }

    /// `downloading -> remote` with `reason` (a failed or abandoned
    /// download). `false` when the file was not downloading.
    pub fn fail_remote_download(
        &self,
        source: &str,
        path: &str,
        reason: &str,
    ) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE remote_items SET state = ?3, reason = ?5 \
             WHERE source = ?1 AND path = ?2 AND state = ?4",
            params![
                source,
                path,
                ITEM_STATE_REMOTE,
                ITEM_STATE_DOWNLOADING,
                format!("{FETCH_REASON_PREFIX}{reason}")
            ],
        )?;
        Ok(n > 0)
    }

    /// A file that says `cached` but is not on disk is `remote` again, so
    /// the next fetch downloads it. Only from `cached`: a file with edits
    /// or a pin is never touched.
    pub fn forget_remote_copy(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE remote_items SET state = ?3 \
             WHERE source = ?1 AND path = ?2 AND state = ?4",
            params![source, path, ITEM_STATE_REMOTE, ITEM_STATE_CACHED],
        )?;
        Ok(n > 0)
    }

    /// A file is at the cache path that is not the remote's and that Tephra
    /// did not write: it is left as it is and the item needs attention,
    /// with `reason`. From `remote` or `downloading` only.
    pub fn mark_foreign_copy(
        &self,
        source: &str,
        path: &str,
        reason: &str,
    ) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE remote_items SET state = 'needs_attention', reason = ?3 \
             WHERE source = ?1 AND path = ?2 AND state IN (?4, ?5)",
            params![
                source,
                path,
                reason,
                ITEM_STATE_REMOTE,
                ITEM_STATE_DOWNLOADING
            ],
        )?;
        Ok(n > 0)
    }

    /// What `source`'s files take in Tephra's cache: the listed size of
    /// every file that has a copy on disk (`cached`, `pinned`, or edited).
    /// A file being downloaded has none yet. An edited copy is counted at
    /// the size the remote listed, not the size of the edit.
    pub fn remote_cache_bytes(&self, source: &str) -> Result<u64, CacheError> {
        let n: i64 = self.connection().query_row(
            "SELECT COALESCE(SUM(size), 0) FROM remote_items \
             WHERE source = ?1 AND state IN (?2, ?3, ?4, ?5)",
            params![
                source,
                ITEM_STATE_CACHED,
                ITEM_STATE_PINNED,
                ITEM_STATE_PENDING_UPLOAD,
                ITEM_STATE_NOT_PUSHED
            ],
            |r| r.get(0),
        )?;
        Ok(n.max(0) as u64)
    }

    /// Start-up: no download survives a restart, so every item that says
    /// `downloading` is `remote` again. Returns how many were reset.
    pub fn reset_remote_downloads(&self) -> Result<usize, CacheError> {
        Ok(self.connection().execute(
            "UPDATE remote_items SET state = ?1 WHERE state = ?2",
            params![ITEM_STATE_REMOTE, ITEM_STATE_DOWNLOADING],
        )?)
    }
}
