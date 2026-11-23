//! What the cache keeps of a git clone (atlas-27 PR-9): the numbers, and the
//! item states a pass moves between `cached`, `not_pushed` and
//! `needs_attention` without touching a state it does not own.

mod common;

use std::collections::HashSet;

use tephra_cache::{
    is_clone_reason, Cache, CloneChanges, CloneState, CLONE_REASON_DIVERGED,
    CLONE_REASON_REWRITTEN, REASON_CHANGED_BOTH_SIDES,
};

fn seeded(dir: &tempfile::TempDir) -> Cache {
    let cache = common::open_temp_cache(dir);
    cache
        .connection()
        .execute_batch(
            "INSERT INTO remote_sources (name, kind, target, created_at, cache) \
                VALUES ('g', 'github', 'o/r', 1, 1), ('h', 'github', 'o/s', 1, 1);
             INSERT INTO remote_items (source, path, size, state, reason) VALUES
                ('g', 'a.md', 1, 'cached', NULL),
                ('g', 'b.md', 1, 'cached', NULL),
                ('g', 'c.md', 1, 'remote', NULL),
                ('g', 'd.md', 1, 'pending_upload', NULL),
                ('g', 'e.md', 1, 'downloading', NULL),
                ('g', 'f.md', 1, 'needs_attention', 'deleted on GitHub'),
                ('g', 'docs/p.md', 1, 'cached', NULL),
                ('h', 'a.md', 1, 'cached', NULL);
             INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('g', 'docs', 1, 1);",
        )
        .unwrap();
    cache
}

fn state(cache: &Cache, path: &str) -> (String, Option<String>) {
    cache
        .remote_items("g")
        .unwrap()
        .into_iter()
        .find(|r| r.path == path)
        .map(|r| (r.state, r.reason))
        .unwrap_or_else(|| panic!("{path}"))
}

fn set(names: &[&str]) -> HashSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

#[test]
fn the_numbers_are_kept_per_source_and_start_absent() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    assert_eq!(cache.remote_clone_state("g").unwrap(), None);
    let st = CloneState {
        tip: Some("abc".into()),
        behind: 2,
        not_pushed: 5,
    };
    cache.set_remote_clone_state("g", &st).unwrap();
    assert_eq!(cache.remote_clone_state("g").unwrap(), Some(st));
    assert_eq!(cache.remote_clone_state("h").unwrap(), None);
    let st2 = CloneState {
        tip: None,
        behind: 0,
        not_pushed: 0,
    };
    cache.set_remote_clone_state("g", &st2).unwrap();
    assert_eq!(cache.remote_clone_state("g").unwrap(), Some(st2));
}

#[test]
fn a_changed_file_is_not_pushed_and_a_file_changed_on_both_sides_needs_attention() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    let changes = CloneChanges {
        changed: set(&["a.md", "b.md", "c.md"]),
        both: set(&["b.md"]),
    };
    let rows = cache.apply_clone_changes("g", &changes, &|_| true).unwrap();
    assert_eq!(rows, 3);
    assert_eq!(state(&cache, "a.md"), ("not_pushed".into(), None));
    assert_eq!(
        state(&cache, "b.md"),
        (
            "needs_attention".into(),
            Some(REASON_CHANGED_BOTH_SIDES.into())
        )
    );
    // The exact start is kept; the guidance after it points at Dan's own command.
    assert!(REASON_CHANGED_BOTH_SIDES.starts_with("changed locally and on GitHub"));
    assert!(REASON_CHANGED_BOTH_SIDES.contains("git pull --rebase"));
    // A file with no copy (`remote`) that differs is still counted.
    assert_eq!(state(&cache, "c.md"), ("not_pushed".into(), None));
    // Another source is untouched.
    let other = cache
        .remote_items("h")
        .unwrap()
        .into_iter()
        .find(|r| r.path == "a.md")
        .unwrap();
    assert_eq!(other.state, "cached");
    // Applying the same thing again changes nothing.
    assert_eq!(
        cache.apply_clone_changes("g", &changes, &|_| true).unwrap(),
        0
    );
}

#[test]
fn a_file_that_no_longer_differs_goes_back_to_pinned_cached_or_remote() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    let all = CloneChanges {
        changed: set(&["a.md", "b.md", "c.md", "docs/p.md"]),
        both: set(&["b.md"]),
    };
    cache.apply_clone_changes("g", &all, &|_| true).unwrap();
    assert_eq!(state(&cache, "docs/p.md").0, "not_pushed");

    // Nothing differs now. `c.md` has no copy on disk.
    cache
        .apply_clone_changes("g", &CloneChanges::default(), &|p| p != "c.md")
        .unwrap();
    assert_eq!(state(&cache, "a.md"), ("cached".into(), None));
    assert_eq!(
        state(&cache, "b.md"),
        ("cached".into(), None),
        "attention clears too"
    );
    assert_eq!(state(&cache, "c.md"), ("remote".into(), None));
    assert_eq!(
        state(&cache, "docs/p.md"),
        ("pinned".into(), None),
        "under a folder pin"
    );
}

#[test]
fn a_state_the_pass_does_not_own_is_never_touched() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir);
    let all = CloneChanges {
        changed: set(&["d.md", "e.md", "f.md"]),
        both: set(&["f.md"]),
    };
    assert_eq!(cache.apply_clone_changes("g", &all, &|_| true).unwrap(), 0);
    assert_eq!(state(&cache, "d.md").0, "pending_upload");
    assert_eq!(state(&cache, "e.md").0, "downloading");
    assert_eq!(
        state(&cache, "f.md"),
        ("needs_attention".into(), Some("deleted on GitHub".into())),
        "another reason keeps its own"
    );
    // And when nothing differs any more, they still keep their states.
    cache
        .apply_clone_changes("g", &CloneChanges::default(), &|_| true)
        .unwrap();
    assert_eq!(state(&cache, "d.md").0, "pending_upload");
    assert_eq!(state(&cache, "f.md").0, "needs_attention");
}

#[test]
fn the_reasons_the_clone_pass_writes_are_known_to_it_alone() {
    assert!(is_clone_reason(CLONE_REASON_REWRITTEN));
    assert!(is_clone_reason(
        "git fetch failed: the remote cannot be reached"
    ));
    assert!(is_clone_reason(&tephra_cache::clone_reason_not_on_branch(
        "main", "feature"
    )));
    assert!(!is_clone_reason("cannot reach GitHub (connection failed)"));
    assert!(!is_clone_reason("deleted on GitHub"));
    assert!(is_clone_reason(CLONE_REASON_DIVERGED));
    // The exact starts are kept; the guidance after them points at Dan's own command.
    assert!(CLONE_REASON_REWRITTEN.starts_with("history rewritten on GitHub"));
    assert!(CLONE_REASON_REWRITTEN.contains("git pull --rebase"));
    assert!(CLONE_REASON_DIVERGED.contains("git pull --rebase"));
}
