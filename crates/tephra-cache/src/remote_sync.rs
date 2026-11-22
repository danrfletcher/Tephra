//! What keeping a remote source current needs from the cache (PR-6): the
//! sync cursor and the ETag it is polled with, the statuses a poll leaves,
//! and the row-level steps of applying one poll's changes.
//!
//! None of the row-level steps opens a transaction of its own: the poller
//! in `tephrad` opens ONE transaction per poll and runs them inside it,
//! together with the writes of the files it re-read, so a poll is applied
//! whole or not at all. [`Cache::reset_remote_index`] is the exception: it
//! is a whole operation of its own.

use rusqlite::params;

use crate::remote_ops::{insert_unit_for_item, remote_root, RemoteItemRow, STATUS_INDEXING};
use crate::sources::SourceStatus;
use crate::{Cache, CacheError, NewRemoteItem};

/// `remote_sources.status` of a source whose last poll could not reach the
/// remote (a connection failure, a DNS failure, a timeout, a 5xx). Its
/// index is kept and still answers.
pub const STATUS_OFFLINE: &str = "offline";
/// A source waiting out a rate limit, or whose last poll failed for a
/// reason that may pass; the reason says when it tries again.
pub const STATUS_SYNCING: &str = "syncing";
/// The token is missing or was rejected. Not `offline`.
pub const STATUS_AUTH_REQUIRED: &str = "auth_required";
/// The remote is not where it was (moved, gone, or no longer readable): a
/// person has to look. Nothing is removed and nothing is followed.
pub const STATUS_NEEDS_ATTENTION: &str = "needs_attention";

/// `remote_items.state` of a file that needs a person.
pub const ITEM_STATE_NEEDS_ATTENTION: &str = "needs_attention";

/// `remote_items.reason` of a file the remote deleted while its cached copy
/// had edits. The copy is kept; the file is no longer in the index (it has
/// no `units` row) and is not read again.
pub const REASON_DELETED_ON_REMOTE: &str = "deleted on GitHub";
/// `remote_items.reason` of a file that was deleted on the remote with an
/// edited local copy, and has since been added there again.
pub const REASON_READDED_AFTER_DELETE: &str =
    "deleted on GitHub and added again there; the local copy has edits";

/// Whether the item is one the remote deleted but whose edited copy was
/// kept: it has a row here and no `units` row.
pub fn is_kept_deleted(item: &RemoteItemRow) -> bool {
    item.reason.as_deref() == Some(REASON_DELETED_ON_REMOTE)
}

/// The cursor text stored in `remote_cursors.cursor`: the commit the index
/// is at and, when there is one, the ETag the remote gave for that commit.
/// `<commit>` alone is what a cursor was before the ETag was kept.
///
/// Stored as `<commit>` or `<commit>\n<etag>`: a header value cannot hold a
/// newline, so the first one is the separator.
pub fn encode_cursor(commit: &str, etag: Option<&str>) -> String {
    match etag.filter(|e| usable_etag(e)) {
        Some(etag) => format!("{commit}\n{etag}"),
        None => commit.to_string(),
    }
}

/// The commit and ETag of a stored cursor; each is `None` when absent.
pub fn decode_cursor(text: Option<&str>) -> (Option<String>, Option<String>) {
    let Some(text) = text else {
        return (None, None);
    };
    let (commit, etag) = match text.split_once('\n') {
        Some((c, e)) => (c, Some(e)),
        None => (text, None),
    };
    (
        Some(commit.to_string()).filter(|c| !c.is_empty()),
        etag.filter(|e| usable_etag(e)).map(str::to_string),
    )
}

/// An ETag is sent back as a header value: visible ASCII and space only.
fn usable_etag(etag: &str) -> bool {
    !etag.is_empty() && etag.len() <= 256 && etag.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Where a poll resumes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollState {
    /// The commit the index is at; `None` when none is known.
    pub commit: Option<String>,
    /// The ETag to poll with; `None` makes the next poll unconditional.
    pub etag: Option<String>,
    pub index_complete: bool,
}

impl Cache {
    /// The poll state of `name`; `None` when it has no cursor row.
    pub fn remote_poll_state(&self, name: &str) -> Result<Option<PollState>, CacheError> {
        Ok(self.remote_cursor(name)?.map(|c| {
            let (commit, etag) = decode_cursor(c.cursor.as_deref());
            PollState {
                commit,
                etag,
                index_complete: c.index_complete,
            }
        }))
    }

    /// Moves the cursor of `name` to `commit` and its `etag`, at `now`. The
    /// cursor row must exist.
    pub fn set_remote_poll_state(
        &self,
        name: &str,
        commit: &str,
        etag: Option<&str>,
        now: i64,
    ) -> Result<(), CacheError> {
        self.connection().execute(
            "UPDATE remote_cursors SET cursor = ?2, updated_at = ?3 WHERE source = ?1",
            params![name, encode_cursor(commit, etag), now],
        )?;
        Ok(())
    }

    /// Advances `last_sync_at` and nothing else: what a poll the remote
    /// answered "not modified" to writes.
    pub fn set_remote_last_sync(&self, name: &str, now: i64) -> Result<(), CacheError> {
        self.connection().execute(
            "UPDATE remote_sources SET last_sync_at = ?2 WHERE name = ?1",
            params![name, now],
        )?;
        Ok(())
    }

    /// Whether an edit of `path` is waiting in the upload queue.
    pub fn has_queued_upload(&self, source: &str, path: &str) -> Result<bool, CacheError> {
        let n: i64 = self.connection().query_row(
            "SELECT COUNT(*) FROM upload_queue WHERE source = ?1 AND path = ?2",
            params![source, path],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Adds the item, or brings the row of that path to this listing (kind,
    /// sha, size). A reason from an earlier read (an LFS pointer, a blob
    /// that was gone) is cleared: the file is read again. An item whose
    /// edited copy was kept when the remote deleted the file stays
    /// `needs_attention`, with [`REASON_READDED_AFTER_DELETE`]: an edited
    /// copy is never overwritten or hidden.
    pub fn upsert_remote_item(
        &self,
        source: &str,
        item: &NewRemoteItem<'_>,
    ) -> Result<(), CacheError> {
        self.connection().execute(
            "INSERT INTO remote_items (source, path, kind, sha, size, state) \
             VALUES (?1, ?2, ?3, ?4, ?5, 'remote') \
             ON CONFLICT(source, path) DO UPDATE SET \
                kind = excluded.kind, sha = excluded.sha, size = excluded.size, \
                state = CASE WHEN remote_items.reason = ?6 THEN ?7 ELSE remote_items.state END, \
                reason = CASE WHEN remote_items.reason = ?6 THEN ?8 ELSE NULL END",
            params![
                source,
                item.path,
                item.kind,
                item.sha,
                item.size,
                REASON_DELETED_ON_REMOTE,
                ITEM_STATE_NEEDS_ATTENTION,
                REASON_READDED_AFTER_DELETE,
            ],
        )?;
        Ok(())
    }

    /// Removes the item `path` of `source` and the pin on that file. A
    /// folder pin stays (it covers files that come later), and the upload
    /// queue is not touched.
    pub fn delete_remote_item(&self, source: &str, path: &str) -> Result<(), CacheError> {
        self.connection().execute(
            "DELETE FROM remote_items WHERE source = ?1 AND path = ?2",
            params![source, path],
        )?;
        self.connection().execute(
            "DELETE FROM remote_pins WHERE source = ?1 AND path = ?2 AND is_folder = 0",
            params![source, path],
        )?;
        Ok(())
    }

    /// The remote deleted the file but its cached copy has edits and stays:
    /// the item stays too, `needs_attention` with [`REASON_DELETED_ON_REMOTE`].
    /// Its `units` row is the caller's to delete (the file is out of the
    /// index). The file's pin goes with it.
    pub fn keep_deleted_remote_item(&self, source: &str, path: &str) -> Result<(), CacheError> {
        self.connection().execute(
            "UPDATE remote_items SET state = ?3, reason = ?4 WHERE source = ?1 AND path = ?2",
            params![
                source,
                path,
                ITEM_STATE_NEEDS_ATTENTION,
                REASON_DELETED_ON_REMOTE
            ],
        )?;
        self.connection().execute(
            "DELETE FROM remote_pins WHERE source = ?1 AND path = ?2 AND is_folder = 0",
            params![source, path],
        )?;
        Ok(())
    }

    /// Makes the `units` row of this item what the listing alone says: the
    /// row for the path (with its links, tags, blocks, headings and index
    /// rows) is deleted and a bare one, with no content hash, is inserted.
    /// For a file that is not read now; a file that is read is written by
    /// the caller's own writer instead.
    pub fn replace_remote_unit_listing(
        &self,
        source_id: i64,
        item: &NewRemoteItem<'_>,
        max_indexed_bytes: u64,
    ) -> Result<(), CacheError> {
        if let Some(existing) = self.get_unit_by_rel_path(source_id, item.path)? {
            self.delete_unit(existing.id)?;
        }
        insert_unit_for_item(self, source_id, item, max_indexed_bytes)
    }

    /// A source whose cursor row is lost: nothing it has read is trusted to
    /// be current, so it is read again from scratch. Every `units` row of
    /// the source is deleted and made again from `remote_items` with no
    /// content, the reasons of the files that were read are cleared (not the
    /// marks of a deleted file whose copy was kept), a fresh cursor row (no
    /// commit, no ETag, index not complete) is written and the source is
    /// `indexing`. One transaction; no remote call. `false` for a name that
    /// is not a remote source.
    pub fn reset_remote_index(
        &self,
        name: &str,
        now: i64,
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
        self.delete_units_for_source(source_id)?;
        for item in self
            .remote_items(name)?
            .iter()
            .filter(|i| !is_kept_deleted(i))
        {
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
            "UPDATE remote_items SET reason = NULL WHERE source = ?1 \
             AND (reason IS NULL OR reason NOT IN (?2, ?3))",
            params![name, REASON_DELETED_ON_REMOTE, REASON_READDED_AFTER_DELETE],
        )?;
        self.connection().execute(
            "INSERT INTO remote_cursors (source, cursor, index_complete, updated_at) \
             VALUES (?1, NULL, 0, ?2) \
             ON CONFLICT(source) DO UPDATE SET cursor = NULL, index_complete = 0, updated_at = ?2",
            params![name, now],
        )?;
        self.set_remote_status(name, STATUS_INDEXING, None, None)?;
        tx.commit()?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cursor_is_the_commit_alone_or_the_commit_and_the_etag() {
        assert_eq!(encode_cursor("abc", None), "abc");
        assert_eq!(encode_cursor("abc", Some("\"e1\"")), "abc\n\"e1\"");
        assert_eq!(decode_cursor(Some("abc")), (Some("abc".into()), None));
        assert_eq!(
            decode_cursor(Some("abc\nW/\"e1\"")),
            (Some("abc".into()), Some("W/\"e1\"".into()))
        );
        assert_eq!(decode_cursor(Some("")), (None, None));
        assert_eq!(decode_cursor(None), (None, None));
    }

    #[test]
    fn an_etag_that_could_not_be_a_header_value_is_not_stored_or_sent() {
        assert_eq!(encode_cursor("abc", Some("bad\r\nx: y")), "abc");
        assert_eq!(encode_cursor("abc", Some("")), "abc");
        assert_eq!(
            decode_cursor(Some("abc\nbad\ttab")),
            (Some("abc".into()), None)
        );
    }
}
