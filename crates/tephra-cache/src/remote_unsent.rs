//! The work a remote source holds that has not reached the remote (atlas-27
//! PR-13): what `source remove` refuses to throw away without `--force`.
//!
//! It is read from the same rows `source.list` shows, by the same helpers:
//! the clone's `not_pushed` count ([`Cache::remote_clone_state`], the line
//! `N changes not pushed`) for a GitHub source, and the Drive holds
//! ([`Cache::drive_holds`], the lines `N pending upload` and `needs
//! attention`) for a Drive one. Nothing is recomputed and nothing is read
//! from the disk or the network.

use rusqlite::params;

use crate::remote_fetch::ITEM_STATE_PENDING_UPLOAD;
use crate::remote_sync::ITEM_STATE_NEEDS_ATTENTION;
use crate::{Cache, CacheError};

/// What a remote source holds that the remote does not have, by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnsentWork {
    /// A GitHub clone's files that differ from the remote tip, edits and
    /// unpushed commits alike: the number `source list` shows as
    /// `N changes not pushed`. 0 for a Drive source and for a source with
    /// no clone.
    pub not_pushed: u64,
    /// A Drive source's files waiting to upload: every `pending_upload`
    /// item, and every queued upload besides (a re-created evicted file,
    /// an upload in flight, one in backoff). 0 for a GitHub source.
    pub pending_upload: u64,
    /// A Drive source's held files (`needs_attention`): each holds a local
    /// version Drive does not have (a conflict, a refused upload, a file
    /// Drive deleted that kept its edits). 0 for a GitHub source (its
    /// held files are in `not_pushed`).
    pub needs_attention: u64,
}

impl UnsentWork {
    /// `true` when nothing is unsent.
    pub fn is_empty(&self) -> bool {
        self.not_pushed == 0 && self.pending_upload == 0 && self.needs_attention == 0
    }
}

impl Cache {
    /// The unsent work of the remote source `source`; empty for a name that
    /// is not a remote source.
    pub fn remote_unsent_work(&self, source: &str) -> Result<UnsentWork, CacheError> {
        let Some(row) = self.remote_source(source)? else {
            return Ok(UnsentWork::default());
        };
        if row.kind == "drive" {
            let holds = self.drive_holds(source)?;
            // A queue row whose item is neither waiting nor held: the file
            // was evicted and re-created, or the item moved on while an
            // upload was in flight. The row is still an edit to send.
            let queued_apart: i64 = self.connection().query_row(
                "SELECT COUNT(*) FROM upload_queue q WHERE q.source = ?1 AND NOT EXISTS (\
                     SELECT 1 FROM remote_items i WHERE i.source = q.source \
                       AND i.path = q.path AND i.state IN (?2, ?3))",
                params![
                    source,
                    ITEM_STATE_PENDING_UPLOAD,
                    ITEM_STATE_NEEDS_ATTENTION
                ],
                |r| r.get(0),
            )?;
            return Ok(UnsentWork {
                not_pushed: 0,
                pending_upload: holds.pending_upload + queued_apart.max(0) as u64,
                needs_attention: holds.needs_attention,
            });
        }
        let clone = self.remote_clone_state(source)?;
        Ok(UnsentWork {
            not_pushed: clone.map_or(0, |c| c.not_pushed),
            pending_upload: 0,
            needs_attention: 0,
        })
    }
}
