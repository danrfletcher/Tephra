//! Which cached remote files go back to `remote` (atlas-27 PR-8, G12, G20).
//!
//! [`plan_eviction`] is pure: it takes the rows that have a copy on disk
//! (with what the daemon knows of each: pinned, an edit waiting in the
//! upload queue, the source's cache flag), the limit, the time and the idle
//! window, and answers which items to drop. It reads no disk and no
//! database; the daemon runs the plan, re-checking every item under the
//! cache lock right before it deletes anything (`remote_evict.rs`).
//!
//! The rules:
//!
//! 1. **Only a `cached` item can be dropped.** `pinned`, `pending_upload`,
//!    `not_pushed`, `downloading`, `needs_attention`, `missing` and `remote`
//!    never are. A `cached` item is also kept while it is pinned (a file
//!    pin, or a folder pin above it) or has a row in the upload queue (an
//!    edit made before its state caught up).
//! 2. **A file used in the last `idle` seconds is never dropped.** The cap
//!    is soft for that window. `last_used` unset counts as never used.
//! 3. **A source with its cache flag off** drops every droppable file,
//!    under the cap or not (G20). Pins win over off (rule 1).
//! 4. **Over the cap**, the droppable files go oldest `last_used` first
//!    (ties by path, then source) until the total is at or under the cap or
//!    none is left. A file of 0 bytes is never picked for the cap: it
//!    would not lower the total.
//!
//! What counts toward the total is what [`item_cache_bytes`] says (the
//! listed size of every item with a copy), summed over every source. The
//! `.git` folder of a GitHub clone is not counted and not touched.

use crate::remote_fetch::{item_cache_bytes, ITEM_STATE_CACHED};

/// The window after its last use in which a file is never dropped: ten
/// minutes.
pub const DEFAULT_IDLE_SECS: i64 = 600;

/// One item that has a copy on disk, as the planner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictItem {
    pub source: String,
    pub path: String,
    /// `remote_items.state`.
    pub state: String,
    /// The listed size, as `remote_items.size` has it.
    pub size: Option<i64>,
    /// Unix time of the last fetch; `None` for a file never fetched.
    pub last_used: Option<i64>,
    /// A file pin on this path, or a folder pin above it.
    pub pinned: bool,
    /// A row in the upload queue: an edit is waiting to be sent.
    pub edit_queued: bool,
    /// The source's cache flag (G20).
    pub source_cache: bool,
}

impl EvictItem {
    /// What the item takes in the cache.
    pub fn bytes(&self) -> u64 {
        item_cache_bytes(&self.state, self.size)
    }

    /// Whether the item may never be dropped, whatever its age: a state
    /// that is not plain `cached`, a pin, or a queued edit.
    pub fn is_protected(&self) -> bool {
        self.state != ITEM_STATE_CACHED || self.pinned || self.edit_queued
    }

    /// Whether the item may be dropped at `now` (not protected, and idle
    /// for at least `idle` seconds).
    pub fn is_droppable(&self, now: i64, idle: i64) -> bool {
        !self.is_protected() && self.last_used.is_none_or(|used| now - used >= idle)
    }
}

/// Why an item is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictReason {
    /// The cache is over its limit and this is among the oldest.
    OverLimit,
    /// The source's cache flag is off.
    CacheOff,
}

/// One item to drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictPick {
    pub source: String,
    pub path: String,
    pub reason: EvictReason,
}

/// What [`plan_eviction`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionPlan {
    /// The items to drop, in the order to drop them: first those of a
    /// source with its cache off (by path), then the oldest over the cap.
    pub picks: Vec<EvictPick>,
    /// What the items take now.
    pub bytes_before: u64,
    /// What they would take once every pick is gone.
    pub bytes_after: u64,
}

impl EvictionPlan {
    /// Whether the cache is still over `limit` after the plan.
    pub fn over_limit_after(&self, limit: u64) -> bool {
        self.bytes_after > limit
    }
}

/// The bytes of items that no age can make droppable: everything with a
/// copy that is not plain unpinned `cached`. When this alone exceeds the
/// limit, the cache can never get under it (E4).
pub fn protected_bytes(items: &[EvictItem]) -> u64 {
    items
        .iter()
        .filter(|i| i.is_protected())
        .map(EvictItem::bytes)
        .sum()
}

/// Decides what to drop: see the module documentation for the rules.
/// `now` and `idle` are seconds.
pub fn plan_eviction(items: &[EvictItem], limit: u64, now: i64, idle: i64) -> EvictionPlan {
    let bytes_before: u64 = items.iter().map(EvictItem::bytes).sum();
    let mut remaining = bytes_before;
    let mut picks = Vec::new();

    // Rule 3: a source with its cache off.
    let mut off: Vec<&EvictItem> = items
        .iter()
        .filter(|i| !i.source_cache && i.is_droppable(now, idle))
        .collect();
    off.sort_by(|a, b| (&a.source, &a.path).cmp(&(&b.source, &b.path)));
    let mut dropped_off = std::collections::HashSet::new();
    for item in off {
        remaining = remaining.saturating_sub(item.bytes());
        dropped_off.insert((item.source.as_str(), item.path.as_str()));
        picks.push(EvictPick {
            source: item.source.clone(),
            path: item.path.clone(),
            reason: EvictReason::CacheOff,
        });
    }

    // Rule 4: over the cap, oldest first.
    if remaining > limit {
        let mut aged: Vec<&EvictItem> = items
            .iter()
            .filter(|i| {
                i.is_droppable(now, idle)
                    && i.bytes() > 0
                    && !dropped_off.contains(&(i.source.as_str(), i.path.as_str()))
            })
            .collect();
        // Never used sorts before any use.
        aged.sort_by(|a, b| {
            (a.last_used.unwrap_or(i64::MIN), &a.path, &a.source).cmp(&(
                b.last_used.unwrap_or(i64::MIN),
                &b.path,
                &b.source,
            ))
        });
        for item in aged {
            if remaining <= limit {
                break;
            }
            remaining = remaining.saturating_sub(item.bytes());
            picks.push(EvictPick {
                source: item.source.clone(),
                path: item.path.clone(),
                reason: EvictReason::OverLimit,
            });
        }
    }

    EvictionPlan {
        picks,
        bytes_before,
        bytes_after: remaining,
    }
}
