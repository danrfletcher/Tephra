//! What the daemon knows of a GitHub source's git clone (atlas-27 PR-9): the
//! tip it last saw on the tracked branch, how far the clone is behind it,
//! how many files differ from it, and the item states that follow.
//!
//! The numbers live in `remote_clone_state` (one row per source, durable
//! like the remote tables and kept out of [`crate::REMOTE_TABLES`]), so a
//! source that cannot be reached still shows the last values, and a restart
//! does not lose the tip a force-push is detected against.

use std::collections::HashSet;

use rusqlite::{params, OptionalExtension};

use crate::remote_fetch::{
    ITEM_STATE_CACHED, ITEM_STATE_NOT_PUSHED, ITEM_STATE_PINNED, ITEM_STATE_REMOTE,
};
use crate::remote_sync::ITEM_STATE_NEEDS_ATTENTION;
use crate::{Cache, CacheError};

/// The reason of a file changed on both sides since the merge base. Dan
/// brings the two together himself; Tephra never merges or rebases.
pub const REASON_CHANGED_BOTH_SIDES: &str = "changed locally and on GitHub; run `git pull --rebase` in the clone yourself to bring the two together";

/// How [`REASON_CHANGED_BOTH_SIDES`] starts: what a file's reason is told by.
const REASON_CHANGED_BOTH_SIDES_START: &str = "changed locally and on GitHub";

/// A source's status reason when the remote's history was rewritten (E12).
/// Nothing was changed in the clone; what to do about it is Dan's call.
pub const CLONE_REASON_REWRITTEN: &str = "history rewritten on GitHub; nothing in the clone was changed, run `git pull --rebase` yourself to bring it to the new history";

/// How [`CLONE_REASON_REWRITTEN`] starts.
const CLONE_REASON_REWRITTEN_START: &str = "history rewritten on GitHub";

/// A source's note when the clone and GitHub each have commits the other
/// lacks (F14). The source stays `active`: nothing is wrong, and nothing is
/// merged. Tephra only fetched.
pub const CLONE_REASON_DIVERGED: &str = "the clone and GitHub have diverged; nothing was changed, run `git pull --rebase` in the clone yourself";

/// How [`CLONE_REASON_DIVERGED`] starts.
const CLONE_REASON_DIVERGED_START: &str = "the clone and GitHub have diverged";

/// The start of the status reason when the clone is not on the tracked
/// branch.
const CLONE_REASON_NOT_ON_BRANCH_START: &str = "clone is not on `";

/// The start of the status reason when the clone's `git fetch` failed.
pub const CLONE_FETCH_REASON_PREFIX: &str = "git fetch failed: ";

/// `clone is not on `main`: ...`: the clone is on another branch (or a
/// detached HEAD), so nothing is fast-forwarded.
pub fn clone_reason_not_on_branch(branch: &str, current: &str) -> String {
    let now = if current == "HEAD" {
        "a detached HEAD".to_string()
    } else {
        format!("`{current}`")
    };
    format!(
        "{CLONE_REASON_NOT_ON_BRANCH_START}{branch}`: it is on {now}; nothing was changed, check out `{branch}` yourself to have it fast-forwarded"
    )
}

/// `true` when `reason` is one of the status reasons the clone's sync
/// writes, so the poller leaves it alone and the sync clears it.
pub fn is_clone_reason(reason: &str) -> bool {
    reason.starts_with(CLONE_REASON_REWRITTEN_START)
        || reason.starts_with(CLONE_REASON_DIVERGED_START)
        || reason.starts_with(CLONE_REASON_NOT_ON_BRANCH_START)
        || reason.starts_with(CLONE_FETCH_REASON_PREFIX)
}

/// What is kept of a clone between passes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloneState {
    /// The tip of the tracked branch the last good pass recorded: what a
    /// rewritten history is told from. Kept at the OLD tip while the history
    /// stays rewritten.
    pub tip: Option<String>,
    /// Commits on the tracked branch the clone does not have.
    pub behind: u64,
    /// Files that differ from the remote tip.
    pub not_pushed: u64,
}

/// The files of one pass, by repository path.
#[derive(Debug, Default)]
pub struct CloneChanges {
    /// Files Dan changed since the merge base (edits, new, deleted, and
    /// files only in unpushed commits).
    pub changed: HashSet<String>,
    /// The files among them the remote changed too.
    pub both: HashSet<String>,
}

impl Cache {
    /// What was kept of `source`'s clone; `None` before the first pass.
    pub fn remote_clone_state(&self, source: &str) -> Result<Option<CloneState>, CacheError> {
        Ok(self
            .connection()
            .query_row(
                "SELECT tip, behind, not_pushed FROM remote_clone_state WHERE source = ?1",
                [source],
                |r| {
                    Ok(CloneState {
                        tip: r.get(0)?,
                        behind: r.get::<_, i64>(1)?.max(0) as u64,
                        not_pushed: r.get::<_, i64>(2)?.max(0) as u64,
                    })
                },
            )
            .optional()?)
    }

    /// Keeps `state` for `source`.
    pub fn set_remote_clone_state(
        &self,
        source: &str,
        state: &CloneState,
    ) -> Result<(), CacheError> {
        self.connection().execute(
            "INSERT INTO remote_clone_state (source, tip, behind, not_pushed) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(source) DO UPDATE SET tip = excluded.tip, \
                behind = excluded.behind, not_pushed = excluded.not_pushed",
            params![
                source,
                state.tip,
                i64::try_from(state.behind).unwrap_or(i64::MAX),
                i64::try_from(state.not_pushed).unwrap_or(i64::MAX),
            ],
        )?;
        Ok(())
    }

    /// Moves the items of `source` to the states `changes` call for, in one
    /// transaction, and says how many rows changed:
    ///
    /// - a file changed on both sides is `needs_attention` with
    ///   [`REASON_CHANGED_BOTH_SIDES`];
    /// - any other changed file is `not_pushed`;
    /// - a file that no longer differs goes back to `pinned` (it is
    ///   pinned), `cached` (`has_copy` says it is on disk) or `remote`.
    ///
    /// Only the states a pass owns are touched: `remote`, `cached`,
    /// `pinned`, `not_pushed`, and a `needs_attention` that carries
    /// [`REASON_CHANGED_BOTH_SIDES`]. A file waiting for an upload, being
    /// downloaded, or needing attention for another reason keeps its state.
    pub fn apply_clone_changes(
        &self,
        source: &str,
        changes: &CloneChanges,
        has_copy: &dyn Fn(&str) -> bool,
    ) -> Result<usize, CacheError> {
        let rows = self.remote_items(source)?;
        let pinned: HashSet<String> = {
            let mut stmt = self.connection().prepare(&format!(
                "SELECT i.path FROM remote_items i WHERE i.source = ?1 AND {}",
                PINNED_SQL
            ))?;
            let found = stmt
                .query_map([source], |r| r.get::<_, String>(0))?
                .collect::<Result<HashSet<_>, _>>()?;
            found
        };
        let tx = self.connection().unchecked_transaction()?;
        let mut changed_rows = 0;
        for row in &rows {
            let mine_attention = row.state == ITEM_STATE_NEEDS_ATTENTION
                && row
                    .reason
                    .as_deref()
                    .is_some_and(|r| r.starts_with(REASON_CHANGED_BOTH_SIDES_START));
            let owned = matches!(
                row.state.as_str(),
                ITEM_STATE_REMOTE | ITEM_STATE_CACHED | ITEM_STATE_PINNED | ITEM_STATE_NOT_PUSHED
            ) || mine_attention;
            if !owned {
                continue;
            }
            let (state, reason): (&str, Option<&str>) = if changes.both.contains(&row.path) {
                (ITEM_STATE_NEEDS_ATTENTION, Some(REASON_CHANGED_BOTH_SIDES))
            } else if changes.changed.contains(&row.path) {
                (ITEM_STATE_NOT_PUSHED, None)
            } else if row.state == ITEM_STATE_NOT_PUSHED || mine_attention {
                let back = if pinned.contains(&row.path) {
                    ITEM_STATE_PINNED
                } else if has_copy(&row.path) {
                    ITEM_STATE_CACHED
                } else {
                    ITEM_STATE_REMOTE
                };
                (back, None)
            } else {
                continue;
            };
            if row.state == state && row.reason.as_deref() == reason {
                continue;
            }
            tx.execute(
                "UPDATE remote_items SET state = ?3, reason = ?4 \
                 WHERE source = ?1 AND path = ?2 AND state = ?5",
                params![source, row.path, state, reason, row.state],
            )?;
            changed_rows += 1;
        }
        tx.commit()?;
        Ok(changed_rows)
    }
}

/// A pin on the file itself or on a folder above it, over `remote_items`
/// aliased `i` (the same rule eviction uses).
const PINNED_SQL: &str = "EXISTS (SELECT 1 FROM remote_pins p \
     WHERE p.source = i.source AND (p.path = i.path OR (p.is_folder = 1 AND \
       (p.path = '' OR substr(i.path, 1, length(p.path) + 1) = p.path || '/'))))";
