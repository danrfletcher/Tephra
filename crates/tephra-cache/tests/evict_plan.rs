//! `plan_eviction` (atlas-27 PR-8): the rules of G12, G20, E4, E5 and E22 as
//! a table over synthetic rows: no disk, no database.

use tephra_cache::{plan_eviction, protected_bytes, EvictItem, EvictReason, DEFAULT_IDLE_SECS};

const NOW: i64 = 1_795_132_800;
const IDLE: i64 = DEFAULT_IDLE_SECS;
/// A time well outside the idle window.
const OLD: i64 = NOW - 3 * IDLE;

fn item(path: &str, state: &str, size: i64, last_used: Option<i64>) -> EvictItem {
    EvictItem {
        source: "g".to_string(),
        path: path.to_string(),
        state: state.to_string(),
        size: Some(size),
        last_used,
        pinned: false,
        edit_queued: false,
        source_cache: true,
    }
}

fn cached(path: &str, size: i64, age: i64) -> EvictItem {
    item(path, "cached", size, Some(NOW - age))
}

fn picked(items: &[EvictItem], limit: u64) -> Vec<String> {
    plan_eviction(items, limit, NOW, IDLE)
        .picks
        .into_iter()
        .map(|p| format!("{}:{}", p.source, p.path))
        .collect()
}

#[test]
fn the_oldest_go_first_until_the_total_is_at_or_under_the_cap() {
    let items = [
        cached("new.md", 1_000, 700),
        cached("old.md", 1_000, 5_000),
        cached("mid.md", 1_000, 2_000),
    ];
    // 3000 bytes, cap 1000: two go, oldest first, and it stops there.
    assert_eq!(picked(&items, 1_000), ["g:old.md", "g:mid.md"]);
    // Exactly at the cap is under it: nothing goes.
    assert!(picked(&items, 3_000).is_empty());
    // One byte over the cap takes the oldest only.
    assert_eq!(picked(&items, 2_999), ["g:old.md"]);
}

#[test]
fn ties_in_last_used_break_by_path() {
    let items = [
        cached("b.md", 10, 5_000),
        cached("a.md", 10, 5_000),
        cached("c.md", 10, 5_000),
    ];
    assert_eq!(picked(&items, 10), ["g:a.md", "g:b.md"]);
}

#[test]
fn a_file_never_used_is_older_than_any_that_was() {
    let items = [
        item("never.md", "cached", 10, None),
        cached("used.md", 10, 5_000),
    ];
    assert_eq!(picked(&items, 10), ["g:never.md"]);
}

#[test]
fn a_zero_limit_drops_everything_that_may_go() {
    let items = [cached("a.md", 10, 5_000), cached("b.md", 10, 4_000)];
    let plan = plan_eviction(&items, 0, NOW, IDLE);
    assert_eq!(plan.picks.len(), 2);
    assert_eq!(plan.bytes_before, 20);
    assert_eq!(plan.bytes_after, 0);
    assert!(!plan.over_limit_after(0));
}

#[test]
fn no_protected_state_is_ever_picked() {
    let items = [
        item("pinned.md", "pinned", 100, Some(OLD)),
        item("edit.md", "pending_upload", 100, Some(OLD)),
        item("unpushed.md", "not_pushed", 100, Some(OLD)),
        item("busy.md", "downloading", 100, Some(OLD)),
        item("attn.md", "needs_attention", 100, Some(OLD)),
        item("gone.md", "missing", 100, Some(OLD)),
        item("plain.md", "remote", 100, Some(OLD)),
        cached("ok.md", 100, 5_000),
    ];
    let plan = plan_eviction(&items, 0, NOW, IDLE);
    assert_eq!(
        plan.picks
            .iter()
            .map(|p| p.path.as_str())
            .collect::<Vec<_>>(),
        ["ok.md"]
    );
}

#[test]
fn a_pinned_or_queued_cached_file_is_never_picked() {
    let mut pinned = cached("pinned.md", 100, 5_000);
    pinned.pinned = true;
    let mut queued = cached("queued.md", 100, 5_000);
    queued.edit_queued = true;
    let items = [pinned, queued, cached("free.md", 100, 5_000)];
    assert_eq!(picked(&items, 0), ["g:free.md"]);
}

#[test]
fn a_file_used_inside_the_window_is_never_picked_and_the_edge_is_the_window() {
    let items = [
        cached("just.md", 100, 0),
        cached("almost.md", 100, IDLE - 1),
        cached("edge.md", 100, IDLE),
        cached("past.md", 100, IDLE + 1),
    ];
    // The cap is soft for ten minutes: only a file idle at least that long
    // can go, whatever the cap.
    assert_eq!(picked(&items, 0), ["g:past.md", "g:edge.md"]);
}

#[test]
fn a_cache_over_the_cap_with_only_recent_files_picks_nothing() {
    let items = [cached("a.md", 1_000, 1), cached("b.md", 1_000, 2)];
    let plan = plan_eviction(&items, 10, NOW, IDLE);
    assert!(plan.picks.is_empty());
    assert!(plan.over_limit_after(10), "still over, and said so");
}

#[test]
fn protected_bytes_count_toward_the_total_the_cap_is_judged_against() {
    let mut pinned = cached("pinned.md", 800, 5_000);
    pinned.pinned = true;
    let items = [
        pinned,
        cached("a.md", 100, 5_000),
        cached("b.md", 100, 4_000),
    ];
    // 1000 total, cap 900: dropping the oldest unpinned file is enough.
    assert_eq!(picked(&items, 900), ["g:a.md"]);
    // Cap 850: both go, 800 stay.
    let plan = plan_eviction(&items, 850, NOW, IDLE);
    assert_eq!(plan.picks.len(), 2);
    assert_eq!(plan.bytes_after, 800);
    assert!(!plan.over_limit_after(850));
}

/// E4: pins alone exceed the cap. Nothing pinned is picked, the unpinned
/// files go, and the plan says it is still over.
#[test]
fn pins_over_the_cap_evict_the_unpinned_first_and_never_the_pinned() {
    let mut a = cached("pin-a.md", 600, 9_000);
    a.pinned = true;
    let b = item("pin-b.md", "pinned", 600, Some(OLD));
    let items = [a, b, cached("free.md", 50, 100_000)];
    assert_eq!(protected_bytes(&items), 1_200);
    let plan = plan_eviction(&items, 1_000, NOW, IDLE);
    assert_eq!(plan.picks.len(), 1);
    assert_eq!(plan.picks[0].path, "free.md");
    assert_eq!(plan.bytes_after, 1_200);
    assert!(plan.over_limit_after(1_000));
}

#[test]
fn protected_bytes_is_every_copy_no_age_can_free() {
    let mut pinned = cached("p.md", 10, 9_000);
    pinned.pinned = true;
    let items = [
        pinned,
        item("e.md", "pending_upload", 20, Some(OLD)),
        item("n.md", "not_pushed", 30, Some(OLD)),
        item("x.md", "pinned", 40, None),
        cached("free.md", 1_000, 9_000),
        cached("recent.md", 2_000, 1),
    ];
    assert_eq!(protected_bytes(&items), 100);
}

// ---- the per-source cache flag (G20, E22) --------------------------------------

#[test]
fn a_source_with_cache_off_drops_what_is_idle_even_under_the_cap() {
    let mut off = cached("a.md", 10, 5_000);
    off.source = "off".to_string();
    off.source_cache = false;
    let mut recent = cached("b.md", 10, 5);
    recent.source = "off".to_string();
    recent.source_cache = false;
    let kept = cached("c.md", 10, 5_000);
    let plan = plan_eviction(&[off, recent, kept], u64::MAX, NOW, IDLE);
    assert_eq!(plan.picks.len(), 1);
    assert_eq!(plan.picks[0].source, "off");
    assert_eq!(plan.picks[0].path, "a.md");
    assert_eq!(plan.picks[0].reason, EvictReason::CacheOff);
    assert_eq!(plan.bytes_after, 20);
}

#[test]
fn pins_win_over_cache_off_and_so_do_edits() {
    let mut pinned = cached("pinned.md", 10, 5_000);
    pinned.source_cache = false;
    pinned.pinned = true;
    let mut edited = item("edit.md", "pending_upload", 10, Some(OLD));
    edited.source_cache = false;
    let mut unpushed = item("push.md", "not_pushed", 10, Some(OLD));
    unpushed.source_cache = false;
    let mut free = cached("free.md", 10, 5_000);
    free.source_cache = false;
    assert_eq!(
        picked(&[pinned, edited, unpushed, free], u64::MAX),
        ["g:free.md"]
    );
}

#[test]
fn cache_off_drops_count_toward_the_cap_before_the_oldest_are_chosen() {
    // 'off' holds 100 bytes of the 130 total; once it is gone the cache is
    // at 30, under the cap of 50, so the oldest file of the 'on' source
    // stays.
    let mut off = cached("big.md", 100, 5_000);
    off.source = "off".to_string();
    off.source_cache = false;
    let a = cached("a.md", 10, 9_000);
    let b = cached("b.md", 20, 8_000);
    let plan = plan_eviction(&[off, a, b], 50, NOW, IDLE);
    assert_eq!(plan.picks.len(), 1);
    assert_eq!(plan.picks[0].reason, EvictReason::CacheOff);
    assert_eq!(plan.bytes_after, 30);
}

#[test]
fn a_file_of_zero_bytes_is_not_dropped_for_the_cap() {
    let items = [cached("empty.md", 0, 9_000), cached("full.md", 10, 8_000)];
    assert_eq!(picked(&items, 5), ["g:full.md"]);
}

#[test]
fn the_cap_is_across_every_source() {
    let mut other = cached("z.md", 100, 9_000);
    other.source = "gdrive".to_string();
    let items = [cached("a.md", 100, 8_000), other];
    // Oldest across both sources first.
    assert_eq!(picked(&items, 100), ["gdrive:z.md"]);
}

#[test]
fn an_empty_cache_plans_nothing() {
    let plan = plan_eviction(&[], 0, NOW, IDLE);
    assert!(plan.picks.is_empty());
    assert_eq!((plan.bytes_before, plan.bytes_after), (0, 0));
}

#[test]
fn a_size_the_row_does_not_have_counts_as_zero() {
    let mut unknown = cached("x.md", 0, 9_000);
    unknown.size = None;
    let plan = plan_eviction(&[unknown], 0, NOW, IDLE);
    assert!(plan.picks.is_empty());
    assert_eq!(plan.bytes_before, 0);
}
