//! When pending work is retried (G9): the triggers, and which rows each
//! one picks up.
//!
//! - [`Trigger::Startup`]: every row, of every source, at start.
//! - [`Trigger::Rescan`]: every row of one source, after `source.rescan`.
//! - [`Trigger::Batch`]: after a watcher batch in one source. A row that
//!   still waits (`pending`) goes only when its backoff has passed; a
//!   row that needs a person (`needs_manual_fix`) goes only when the
//!   batch changed that very file.
//!
//! There is no timer. A row that is waiting is picked up by the next
//! trigger, never by itself, so a file that keeps failing cannot start
//! a retry loop; the per-row attempt count and [`MAX_ITEM_ROUNDS`] end
//! it for good (until startup, a rescan or a change of that file).
//!
//! [`MAX_ITEM_ROUNDS`]: crate::refactor::fix_write::MAX_ITEM_ROUNDS

use std::collections::HashSet;

use tephra_cache::{PendingFixRow, STATE_NEEDS_MANUAL_FIX, STATE_PENDING};

use crate::refactor::fix_write::item_backoff;

/// What started a pass over the pending rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Startup,
    Rescan {
        source: String,
    },
    /// `changed` are the files the batch changed, relative to the
    /// source's root.
    Batch {
        source: String,
        changed: HashSet<String>,
    },
}

/// A row to try and the rounds it has already had: a row that needs a
/// person starts over from none, as a new trigger gives it a fresh try.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub row: PendingFixRow,
    pub prior_rounds: u32,
}

/// Whether a waiting row's backoff has passed at `now` (epoch
/// seconds). A row never tried is always due.
pub fn is_due(row: &PendingFixRow, now: i64) -> bool {
    if row.attempts <= 0 {
        return true;
    }
    let wait = item_backoff(row.attempts as u32).as_secs() as i64;
    now >= row.updated_at.saturating_add(wait)
}

/// The rows `trigger` picks up out of `rows` (oldest first), at `now`.
pub fn select(rows: Vec<PendingFixRow>, trigger: &Trigger, now: i64) -> Vec<Pick> {
    rows.into_iter()
        .filter_map(|row| {
            let manual = row.state == STATE_NEEDS_MANUAL_FIX;
            let waiting = row.state == STATE_PENDING;
            if !manual && !waiting {
                return None;
            }
            let fresh = |row: PendingFixRow| Pick {
                prior_rounds: if manual {
                    0
                } else {
                    row.attempts.max(0) as u32
                },
                row,
            };
            match trigger {
                Trigger::Startup => Some(fresh(row)),
                Trigger::Rescan { source } => (row.source == *source).then(|| fresh(row)),
                Trigger::Batch { source, changed } => {
                    if row.source != *source {
                        return None;
                    }
                    let go = if manual {
                        changed.contains(&row.file_path)
                    } else {
                        is_due(&row, now)
                    };
                    go.then(|| fresh(row))
                }
            }
        })
        .collect()
}
