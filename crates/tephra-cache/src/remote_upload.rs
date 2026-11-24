//! The upload buffer of a Drive source (atlas-27 PR-11): the rows that say a
//! saved edit is waiting to go to Drive, how long to wait before the next
//! try, and the conditional steps that move an item through
//! `pending_upload -> cached` or `pending_upload -> needs_attention`.
//!
//! The queue holds no bytes: the latest bytes are read from the content
//! cache when they are sent, so a second save of a queued file needs no
//! queue change at all. There is one row per file (`UNIQUE(source, path)`).
//! `base_revision` is the Drive head revision the edit was made on and never
//! moves while edits pile up; it is the value the worker compares with
//! Drive's head right before it sends. It is the revision the *local bytes
//! came from* (`remote_copy_revisions`: set by a fetch and by a landed
//! upload), not the item's `revision`, which every Drive poll moves to
//! Drive's head: a copy that went stale under a poll is queued on the
//! revision it was read at, and so is refused.
//!
//! Every step that changes a row is conditional on what the caller saw, so a
//! step that lost a race (a second `resolve`, a watcher batch, a poll) changes
//! nothing and says so. The tables are durable and foreign-key free, so a
//! step that touches the item and its queue row does both in one transaction.

use rusqlite::{params, OptionalExtension};

use crate::remote_fetch::{
    ITEM_STATE_CACHED, ITEM_STATE_DOWNLOADING, ITEM_STATE_PENDING_UPLOAD, ITEM_STATE_PINNED,
    ITEM_STATE_REMOTE,
};
use crate::remote_sync::ITEM_STATE_NEEDS_ATTENTION;
use crate::{Cache, CacheError};

/// The `needs_attention` reason of an upload Drive's head moved under.
pub const REASON_CHANGED_ON_DRIVE: &str = "changed on Drive";

/// The start of the `needs_attention` reason of an upload Drive turned down
/// for good (the file is gone, or Tephra may not write it). The rest is the
/// reason Drive gave. Saving the file again queues it again.
pub const UPLOAD_REFUSED_PREFIX: &str = "Drive refused the upload: ";

/// What the queue row's `last_error` starts with for a conflict; the rest is
/// the Drive head revision that was seen when it was raised. `resolve
/// --mine` only ever overwrites that revision: a head that moved again is a
/// new conflict.
const CONFLICT_MARK: &str = "changed on Drive @ ";

/// What a queue row's `last_error` starts with once the worker has begun to
/// send: `sending:<md5 of the bytes>`. It survives a failed try (the error
/// is added after it) and a kill, so that a retry which finds Drive's head
/// moved can tell its own upload that landed from somebody else's change.
const SENDING_MARK: &str = "sending:";

/// The longest wait between two tries, in seconds.
pub const UPLOAD_BACKOFF_CAP_SECS: u64 = 60;

/// How long to wait after a failed try, given the number of tries that
/// failed before it: 1, 2, 4, 8, 16, 32, then 60 seconds for ever (there is
/// no retry limit).
pub fn upload_backoff_secs(attempts: u32) -> u64 {
    (1u64 << attempts.min(6)).min(UPLOAD_BACKOFF_CAP_SECS)
}

/// One `upload_queue` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadEntry {
    pub id: i64,
    pub source: String,
    pub path: String,
    /// The Drive head revision the edit was based on.
    pub base_revision: Option<String>,
    /// How many tries have failed since the last success or conflict.
    pub attempts: u32,
    /// Unix time before which the worker does not try again.
    pub next_retry_at: Option<i64>,
    pub last_error: Option<String>,
    pub created_at: i64,
}

/// What a saved edit did to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueuedEdit {
    /// The item is `pending_upload` and a row was made (or reopened).
    Queued,
    /// The item was already `pending_upload`: the newest bytes simply
    /// replace the queued ones, the row and its base are untouched.
    AlreadyQueued,
    /// The item is held (`needs_attention`): nothing changes until the
    /// person resolves it.
    Held,
    /// The item has no copy to send (not a Drive file row, being fetched,
    /// gone): nothing was queued.
    Ignored,
}

/// The facts of a Drive conflict: the queue row and the head Drive had when
/// it was raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveConflict {
    pub entry: UploadEntry,
    /// The Drive head revision seen when the upload was refused.
    pub head: Option<String>,
}

/// How many held items `source.list` names one by one; the count is always
/// the whole number.
pub const HELD_ITEMS_LISTED: usize = 20;

/// The unsent work of a Drive source, as `source.list` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DriveHolds {
    /// Items that are `pending_upload`.
    pub pending_upload: u64,
    /// Items that are `needs_attention`.
    pub needs_attention: u64,
    /// `(path, reason)` of the first [`HELD_ITEMS_LISTED`] of them, by path.
    pub attention: Vec<(String, String)>,
}

/// What a landed upload changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadLanded<'a> {
    pub source: &'a str,
    pub path: &'a str,
    /// The base the row had when the upload was started.
    pub base_seen: Option<&'a str>,
    /// Drive's head revision now (the new revision the upload made, or the
    /// head that already held these bytes).
    pub new_revision: &'a str,
    pub size: i64,
    /// The git blob sha of the bytes Drive now has, when they are known:
    /// the stamp later saves are compared with.
    pub stamp: Option<&'a str>,
    /// The file on disk still holds exactly those bytes. When it does not,
    /// a save came in while the upload was in flight: the row stays, based
    /// on the new revision.
    pub file_unchanged: bool,
}

impl UploadEntry {
    /// The MD5 of the bytes the last try was sending, if one began.
    pub fn sending_md5(&self) -> Option<&str> {
        let rest = self.last_error.as_deref()?.strip_prefix(SENDING_MARK)?;
        let md5 = rest.split(' ').next()?;
        (!md5.is_empty()).then_some(md5)
    }
}

fn entry_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<UploadEntry> {
    Ok(UploadEntry {
        id: r.get(0)?,
        source: r.get(1)?,
        path: r.get(2)?,
        base_revision: r.get(3)?,
        attempts: r.get::<_, i64>(4)?.clamp(0, i64::from(u32::MAX)) as u32,
        next_retry_at: r.get(5)?,
        last_error: r.get(6)?,
        created_at: r.get(7)?,
    })
}

const ENTRY_COLUMNS: &str =
    "q.id, q.source, q.path, q.base_revision, q.attempts, q.next_retry_at, q.last_error, q.created_at";

/// The head revision a conflict's `last_error` carries.
fn conflict_head(last_error: Option<&str>) -> Option<String> {
    last_error
        .and_then(|e| e.strip_prefix(CONFLICT_MARK))
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

impl Cache {
    /// Notes the Drive revision the bytes of the local copy of `path` came
    /// from (`None` forgets it). Replaces an earlier note.
    pub fn record_remote_copy_revision(
        &self,
        source: &str,
        path: &str,
        revision: Option<&str>,
    ) -> Result<(), CacheError> {
        match revision {
            Some(revision) => self.connection().execute(
                "INSERT OR REPLACE INTO remote_copy_revisions (source, path, revision) \
                 VALUES (?1, ?2, ?3)",
                params![source, path, revision],
            )?,
            None => self.connection().execute(
                "DELETE FROM remote_copy_revisions WHERE source = ?1 AND path = ?2",
                params![source, path],
            )?,
        };
        Ok(())
    }

    /// The revision [`Cache::record_remote_copy_revision`] noted, if any.
    pub fn remote_copy_revision(
        &self,
        source: &str,
        path: &str,
    ) -> Result<Option<String>, CacheError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT revision FROM remote_copy_revisions WHERE source = ?1 AND path = ?2",
                params![source, path],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// A saved edit of the Drive file `path` (a cached copy, or one saved
    /// again after the cache dropped it): the item becomes `pending_upload`
    /// and an `upload_queue` row is made with the revision the local bytes
    /// came from as its base (see [`Cache::record_remote_copy_revision`];
    /// the item's own revision when none was noted, as it is moved by every
    /// poll). A second save changes nothing (the row is kept, so the base
    /// is the original one).
    pub fn queue_drive_edit(
        &self,
        source: &str,
        path: &str,
        now: i64,
    ) -> Result<QueuedEdit, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let item: Option<(String, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT state, revision, reason FROM remote_items \
                 WHERE source = ?1 AND path = ?2",
                params![source, path],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((state, item_revision, reason)) = item else {
            return Ok(QueuedEdit::Ignored);
        };
        let copy_revision: Option<String> = tx
            .query_row(
                "SELECT revision FROM remote_copy_revisions WHERE source = ?1 AND path = ?2",
                params![source, path],
                |r| r.get(0),
            )
            .optional()?;
        let revision = copy_revision.or(item_revision);
        let reopened = state == ITEM_STATE_NEEDS_ATTENTION
            && reason
                .as_deref()
                .is_some_and(|r| r.starts_with(UPLOAD_REFUSED_PREFIX));
        let outcome = match state.as_str() {
            ITEM_STATE_CACHED | ITEM_STATE_PINNED | ITEM_STATE_REMOTE => QueuedEdit::Queued,
            ITEM_STATE_PENDING_UPLOAD => QueuedEdit::AlreadyQueued,
            _ if reopened => QueuedEdit::Queued,
            ITEM_STATE_NEEDS_ATTENTION => return Ok(QueuedEdit::Held),
            _ => return Ok(QueuedEdit::Ignored),
        };
        tx.execute(
            "INSERT OR IGNORE INTO upload_queue (source, path, base_revision, created_at) \
             VALUES (?1, ?2, ?3, ?4)",
            params![source, path, revision, now],
        )?;
        if outcome == QueuedEdit::Queued {
            if reopened {
                tx.execute(
                    "UPDATE upload_queue SET attempts = 0, next_retry_at = NULL, \
                     last_error = NULL WHERE source = ?1 AND path = ?2",
                    params![source, path],
                )?;
            }
            let n = tx.execute(
                "UPDATE remote_items SET state = ?3, reason = NULL \
                 WHERE source = ?1 AND path = ?2 AND state = ?4",
                params![source, path, ITEM_STATE_PENDING_UPLOAD, state],
            )?;
            if n == 0 {
                // Lost a race: the row changed under the read. Nothing is
                // kept of this call.
                return Ok(QueuedEdit::Ignored);
            }
        }
        tx.commit()?;
        Ok(outcome)
    }

    /// The queue row of `path`, if there is one.
    pub fn upload_entry(
        &self,
        source: &str,
        path: &str,
    ) -> Result<Option<UploadEntry>, CacheError> {
        Ok(self
            .connection()
            .query_row(
                &format!(
                    "SELECT {ENTRY_COLUMNS} FROM upload_queue q \
                     WHERE q.source = ?1 AND q.path = ?2"
                ),
                params![source, path],
                entry_of,
            )
            .optional()?)
    }

    /// The rows of `source` whose item is `pending_upload` and whose wait is
    /// over at `now`, oldest first.
    pub fn due_uploads(&self, source: &str, now: i64) -> Result<Vec<UploadEntry>, CacheError> {
        let mut stmt = self.connection().prepare(&format!(
            "SELECT {ENTRY_COLUMNS} FROM upload_queue q \
             JOIN remote_items i ON i.source = q.source AND i.path = q.path \
             WHERE q.source = ?1 AND i.state = ?2 \
               AND (q.next_retry_at IS NULL OR q.next_retry_at <= ?3) \
             ORDER BY q.id"
        ))?;
        let rows = stmt
            .query_map(params![source, ITEM_STATE_PENDING_UPLOAD, now], entry_of)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every queue row of `source`, oldest first.
    pub fn upload_entries(&self, source: &str) -> Result<Vec<UploadEntry>, CacheError> {
        let mut stmt = self.connection().prepare(&format!(
            "SELECT {ENTRY_COLUMNS} FROM upload_queue q WHERE q.source = ?1 ORDER BY q.id"
        ))?;
        let rows = stmt
            .query_map(params![source], entry_of)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A try failed for a reason that may pass (network, 5xx, 429, sign-in):
    /// the wait doubles. `attempts_seen` is the count the caller read; a row
    /// that moved on (a conflict, a landed upload) is left as it is.
    /// `not_before` raises the next try to a time Drive named (a rate limit).
    /// Returns the time of the next try, or `None` when the row moved.
    pub fn fail_upload_attempt(
        &self,
        id: i64,
        attempts_seen: u32,
        now: i64,
        not_before: Option<i64>,
        error: &str,
    ) -> Result<Option<i64>, CacheError> {
        let wait = i64::try_from(upload_backoff_secs(attempts_seen)).unwrap_or(i64::MAX);
        let at = now.saturating_add(wait).max(not_before.unwrap_or(0));
        let tx = self.connection().unchecked_transaction()?;
        let seen: Option<Option<String>> = tx
            .query_row(
                "SELECT last_error FROM upload_queue WHERE id = ?1 AND attempts = ?2",
                params![id, i64::from(attempts_seen)],
                |r| r.get(0),
            )
            .optional()?;
        let Some(seen) = seen else {
            return Ok(None);
        };
        // The marker of a send that may have landed outlives the failure.
        let kept = seen
            .as_deref()
            .and_then(|e| e.strip_prefix(SENDING_MARK))
            .and_then(|rest| rest.split(' ').next())
            .filter(|md5| !md5.is_empty());
        let last_error = match kept {
            Some(md5) => format!("{SENDING_MARK}{md5} {error}"),
            None => error.to_string(),
        };
        tx.execute(
            "UPDATE upload_queue SET attempts = ?3, next_retry_at = ?4, last_error = ?5 \
             WHERE id = ?1 AND attempts = ?2",
            params![
                id,
                i64::from(attempts_seen),
                i64::from(attempts_seen) + 1,
                at,
                last_error
            ],
        )?;
        tx.commit()?;
        Ok(Some(at))
    }

    /// The worker is about to send the bytes whose MD5 is `md5`: noted in
    /// the row first (see `UploadEntry::sending_md5`). Conditional on the
    /// row being the one the caller read. `false` when it moved on.
    pub fn mark_upload_sending(
        &self,
        id: i64,
        attempts_seen: u32,
        md5: &str,
    ) -> Result<bool, CacheError> {
        let n = self.connection().execute(
            "UPDATE upload_queue SET last_error = ?3 WHERE id = ?1 AND attempts = ?2",
            params![id, i64::from(attempts_seen), format!("{SENDING_MARK}{md5}")],
        )?;
        Ok(n > 0)
    }

    /// The file of a queued edit is gone from the cache (somebody deleted
    /// it): there is nothing to send, and Drive still has its own copy. The
    /// row ends and the item is `remote` again. Conditional on the item
    /// being `pending_upload`.
    pub fn abandon_queued_edit(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let n = tx.execute(
            "UPDATE remote_items SET state = ?3, reason = NULL \
             WHERE source = ?1 AND path = ?2 AND state = ?4",
            params![source, path, ITEM_STATE_REMOTE, ITEM_STATE_PENDING_UPLOAD],
        )?;
        if n == 0 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM upload_queue WHERE source = ?1 AND path = ?2",
            params![source, path],
        )?;
        tx.execute(
            "DELETE FROM remote_copy_stamps WHERE source = ?1 AND path = ?2",
            params![source, path],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The upload was refused: no bytes were sent. The item becomes
    /// `needs_attention` with `reason`, and the queue row stays (it holds
    /// the base, and the edit is still on disk). `head` is the Drive head
    /// that was seen when the refusal is a conflict; it is kept for
    /// `resolve`. Conditional on the item still being `pending_upload` and
    /// the row still having `base_seen`. `false` when it was not.
    pub fn hold_upload(
        &self,
        source: &str,
        path: &str,
        base_seen: Option<&str>,
        reason: &str,
        head: Option<&str>,
    ) -> Result<bool, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let last_error = match head {
            Some(head) => format!("{CONFLICT_MARK}{head}"),
            None => reason.to_string(),
        };
        let n = tx.execute(
            "UPDATE upload_queue SET last_error = ?4, next_retry_at = NULL \
             WHERE source = ?1 AND path = ?2 AND base_revision IS ?3",
            params![source, path, base_seen, last_error],
        )?;
        let m = tx.execute(
            "UPDATE remote_items SET state = ?3, reason = ?4 \
             WHERE source = ?1 AND path = ?2 AND state = ?5",
            params![
                source,
                path,
                ITEM_STATE_NEEDS_ATTENTION,
                reason,
                ITEM_STATE_PENDING_UPLOAD
            ],
        )?;
        if n == 0 || m == 0 {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    /// An upload landed (or Drive already had the bytes). See
    /// [`UploadLanded`]. `false` when the row or the item had moved on; then
    /// nothing was changed.
    pub fn complete_upload(&self, landed: &UploadLanded<'_>, now: i64) -> Result<bool, CacheError> {
        let UploadLanded {
            source,
            path,
            base_seen,
            new_revision,
            size,
            stamp,
            file_unchanged,
        } = *landed;
        let tx = self.connection().unchecked_transaction()?;
        let row: Option<i64> = tx
            .query_row(
                "SELECT id FROM upload_queue \
                 WHERE source = ?1 AND path = ?2 AND base_revision IS ?3",
                params![source, path, base_seen],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = row else {
            return Ok(false);
        };
        let n = tx.execute(
            "UPDATE remote_items SET revision = ?3, size = ?4 \
             WHERE source = ?1 AND path = ?2 AND state = ?5",
            params![source, path, new_revision, size, ITEM_STATE_PENDING_UPLOAD],
        )?;
        if n == 0 {
            return Ok(false);
        }
        // Drive's bytes are now `new_revision`, and the file on disk is
        // either those bytes or a newer save made on them.
        tx.execute(
            "INSERT OR REPLACE INTO remote_copy_revisions (source, path, revision) \
             VALUES (?1, ?2, ?3)",
            params![source, path, new_revision],
        )?;
        match stamp {
            Some(hash) => tx.execute(
                "INSERT OR REPLACE INTO remote_copy_stamps (source, path, hash) \
                 VALUES (?1, ?2, ?3)",
                params![source, path, hash],
            )?,
            None => tx.execute(
                "DELETE FROM remote_copy_stamps WHERE source = ?1 AND path = ?2",
                params![source, path],
            )?,
        };
        if file_unchanged {
            tx.execute("DELETE FROM upload_queue WHERE id = ?1", params![id])?;
            tx.execute(
                &format!(
                    "UPDATE remote_items AS i SET state = CASE WHEN {} THEN ?3 ELSE ?4 END, \
                         reason = NULL, last_used = ?5 \
                     WHERE i.source = ?1 AND i.path = ?2 AND i.state = ?6",
                    crate::remote_evict::PINNED
                ),
                params![
                    source,
                    path,
                    ITEM_STATE_PINNED,
                    ITEM_STATE_CACHED,
                    now,
                    ITEM_STATE_PENDING_UPLOAD
                ],
            )?;
        } else {
            tx.execute(
                "UPDATE upload_queue SET base_revision = ?2, attempts = 0, \
                 next_retry_at = NULL, last_error = NULL WHERE id = ?1",
                params![id, new_revision],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    /// The file went back to the bytes Drive has: the queued edit is
    /// dropped and the item is `cached` (or `pinned`) again. Conditional on
    /// the item being `pending_upload`.
    pub fn cancel_queued_edit(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let tx = self.connection().unchecked_transaction()?;
        let n = tx.execute(
            &format!(
                "UPDATE remote_items AS i SET state = CASE WHEN {} THEN ?3 ELSE ?4 END, \
                     reason = NULL \
                 WHERE i.source = ?1 AND i.path = ?2 AND i.state = ?5",
                crate::remote_evict::PINNED
            ),
            params![
                source,
                path,
                ITEM_STATE_PINNED,
                ITEM_STATE_CACHED,
                ITEM_STATE_PENDING_UPLOAD
            ],
        )?;
        if n == 0 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM upload_queue WHERE source = ?1 AND path = ?2",
            params![source, path],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// The Drive conflict of `path`, when the item is held with
    /// [`REASON_CHANGED_ON_DRIVE`] and has its queue row. `None` for any
    /// other item: `resolve` has nothing to do with it.
    pub fn drive_conflict(
        &self,
        source: &str,
        path: &str,
    ) -> Result<Option<DriveConflict>, CacheError> {
        let held: Option<(String, Option<String>)> = self
            .connection()
            .query_row(
                "SELECT state, reason FROM remote_items WHERE source = ?1 AND path = ?2",
                params![source, path],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match held {
            Some((state, Some(reason)))
                if state == ITEM_STATE_NEEDS_ATTENTION && reason == REASON_CHANGED_ON_DRIVE => {}
            _ => return Ok(None),
        }
        Ok(self.upload_entry(source, path)?.map(|entry| DriveConflict {
            head: conflict_head(entry.last_error.as_deref()),
            entry,
        }))
    }

    /// `resolve --mine`: the queued edit is based on the head the conflict
    /// saw, and the item is `pending_upload` again. Conditional on the
    /// conflict being the one the caller read (the row's `last_error`), so
    /// of two `resolve` calls at once only one gets it. `false` when it was
    /// not.
    pub fn claim_conflict_for_mine(&self, conflict: &DriveConflict) -> Result<bool, CacheError> {
        let e = &conflict.entry;
        let tx = self.connection().unchecked_transaction()?;
        let n = tx.execute(
            "UPDATE upload_queue SET base_revision = ?3, attempts = 0, \
             next_retry_at = NULL, last_error = NULL \
             WHERE id = ?1 AND last_error IS ?2",
            params![e.id, e.last_error, conflict.head],
        )?;
        let m = tx.execute(
            "UPDATE remote_items SET state = ?3, reason = NULL \
             WHERE source = ?1 AND path = ?2 AND state = ?4 AND reason = ?5",
            params![
                e.source,
                e.path,
                ITEM_STATE_PENDING_UPLOAD,
                ITEM_STATE_NEEDS_ATTENTION,
                REASON_CHANGED_ON_DRIVE
            ],
        )?;
        if n == 0 || m == 0 {
            return Ok(false);
        }
        tx.commit()?;
        Ok(true)
    }

    /// `resolve --theirs`: the queued edit is dropped, and the item is
    /// `remote` again with Drive's `revision` and `size`, no stamp and no
    /// reason: the file is fetched anew. Conditional like
    /// [`Cache::claim_conflict_for_mine`].
    pub fn claim_conflict_for_theirs(
        &self,
        conflict: &DriveConflict,
        revision: Option<&str>,
        size: Option<i64>,
    ) -> Result<bool, CacheError> {
        let e = &conflict.entry;
        let tx = self.connection().unchecked_transaction()?;
        let n = tx.execute(
            "DELETE FROM upload_queue WHERE id = ?1 AND last_error IS ?2",
            params![e.id, e.last_error],
        )?;
        let m = tx.execute(
            "UPDATE remote_items SET state = ?3, reason = NULL, \
             revision = COALESCE(?6, revision), size = COALESCE(?7, size) \
             WHERE source = ?1 AND path = ?2 AND state = ?4 AND reason = ?5",
            params![
                e.source,
                e.path,
                ITEM_STATE_REMOTE,
                ITEM_STATE_NEEDS_ATTENTION,
                REASON_CHANGED_ON_DRIVE,
                revision,
                size
            ],
        )?;
        if n == 0 || m == 0 {
            return Ok(false);
        }
        tx.execute(
            "DELETE FROM remote_copy_stamps WHERE source = ?1 AND path = ?2",
            params![e.source, e.path],
        )?;
        // The edited file is set aside and the file is fetched anew, at
        // Drive's head (the fetch notes it again).
        match revision {
            Some(revision) => tx.execute(
                "INSERT OR REPLACE INTO remote_copy_revisions (source, path, revision) \
                 VALUES (?1, ?2, ?3)",
                params![e.source, e.path, revision],
            )?,
            None => tx.execute(
                "DELETE FROM remote_copy_revisions WHERE source = ?1 AND path = ?2",
                params![e.source, e.path],
            )?,
        };
        tx.commit()?;
        Ok(true)
    }

    /// The unsent work of `source`: how many items wait to be uploaded, and
    /// which ones are held and why. Reads the item rows only: nothing is
    /// downloaded to say so.
    pub fn drive_holds(&self, source: &str) -> Result<DriveHolds, CacheError> {
        let pending_upload: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM remote_items WHERE source = ?1 AND state = ?2",
            params![source, ITEM_STATE_PENDING_UPLOAD],
            |r| r.get(0),
        )?;
        let needs_attention: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM remote_items WHERE source = ?1 AND state = ?2",
            params![source, ITEM_STATE_NEEDS_ATTENTION],
            |r| r.get(0),
        )?;
        let mut stmt = self.connection().prepare(
            "SELECT path, reason FROM remote_items WHERE source = ?1 AND state = ?2 \
             ORDER BY path LIMIT ?3",
        )?;
        let attention = stmt
            .query_map(
                params![source, ITEM_STATE_NEEDS_ATTENTION, HELD_ITEMS_LISTED as i64],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DriveHolds {
            pending_upload: pending_upload.max(0) as u64,
            needs_attention: needs_attention.max(0) as u64,
            attention,
        })
    }

    /// The Drive items of `source` that a watcher batch may have edited: the
    /// ones with a copy or that could have been re-created (every state but
    /// `downloading` and `missing`, and a held one counts too: its bytes
    /// are the newest). Native Google files have no bytes to send and are
    /// left out. Returns `(path, state, stamp)`, the stamp being the hash
    /// of the bytes Drive has (what [`Cache::record_remote_copy_stamp`]
    /// noted).
    pub fn drive_edit_candidates(
        &self,
        source: &str,
    ) -> Result<Vec<(String, String, Option<String>)>, CacheError> {
        let mut stmt = self.connection().prepare(
            "SELECT i.path, i.state, s.hash FROM remote_items i \
             JOIN remote_sources r ON r.name = i.source AND r.kind = 'drive' \
             LEFT JOIN remote_copy_stamps s ON s.source = i.source AND s.path = i.path \
             WHERE i.source = ?1 AND i.state NOT IN (?2, 'missing', 'not_pushed') \
               AND i.file_id IS NOT NULL AND (i.kind IS NULL OR i.kind NOT IN \
                 ('folder', 'gdoc', 'gsheet', 'gslides', 'gform', 'gdrawing', 'gshortcut'))",
        )?;
        let rows = stmt
            .query_map(params![source, ITEM_STATE_DOWNLOADING], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}
