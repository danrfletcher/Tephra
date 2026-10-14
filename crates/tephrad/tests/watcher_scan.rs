//! Integration test: a real `notify` watcher against a temp-directory
//! fixture (spec: "create, modify, and delete files, then assert the
//! resulting cache rows ... match expected links/tags/blocks").
//!
//! Uses the real [`tephrad::registry::SourceRegistry`] end to end —
//! `register` (which runs the real initial scan through the real
//! `tephra-parser`/`tephra-cache`), then real filesystem edits
//! picked up by the real watcher and debounce window, polled for via
//! `tephrad::registry`'s cache-backed state rather than a fixed-time
//! sleep (the project has learned fixed-timeout waits on real
//! watchers flake).

mod fixtures;

use std::time::{Duration, Instant};

use tephra_cache::Cache;
use tephrad::registry::SourceRegistry;

/// Polls `check` every 20ms until it returns `true` or `timeout`
/// elapses, instead of a single fixed-time sleep — bounded retry
/// rather than a flaky guess at exactly how long the real 250ms
/// debounce plus a real scan will take.
fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn registering_a_vault_source_runs_the_initial_scan_with_expected_rows() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    let stats = registry
        .register("notes", fixture.root())
        .expect("register");

    // Exactly `one.md` and `two.md` are seen and parsed; the
    // `.git`/`node_modules` subtrees' `.md` files are ignored before
    // ever reaching the parser (G2/E1).
    assert_eq!(stats.files_seen, 2, "expected one.md and two.md only");
    assert_eq!(stats.units_written, 2);
    assert_eq!(stats.links_written, 1, "one.md's [[Two]] wikilink");
    assert_eq!(stats.tags_written, 1, "one.md's #project tag");
    assert_eq!(stats.blocks_written, 1, "one.md's ^block1 anchor");

    let sources = registry.list().expect("list");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].name, "notes");

    registry.unregister("notes").expect("unregister");
}

#[test]
fn editing_a_tracked_file_debounces_and_updates_only_that_files_row() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    let mut events = registry.subscribe();
    registry
        .register("notes", fixture.root())
        .expect("register");

    // Edit one.md: drop its wikilink and tag, add a new one instead,
    // so the row's post-edit content is unambiguously distinguishable
    // from its pre-edit content.
    fixture.write("one.md", "# One\n\nNow links to [[Three]] only.\n");

    // Wait for the `source_changed` event (GP6) rather than sleeping
    // a fixed guess at the debounce window.
    let fired = wait_until(Duration::from_secs(5), || {
        matches!(
            events.try_recv(),
            Ok(tephrad::registry::DaemonEvent::SourceChanged { .. })
        )
    });
    assert!(
        fired,
        "expected a source_changed event after editing one.md"
    );

    // Re-open a fresh handle on the same on-disk cache file to assert
    // on the row through the real storage API, independent of the
    // registry's own in-process `Cache` handle.
    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let source = verify_cache
        .get_source("notes")
        .expect("get_source")
        .expect("source row");
    let unit = verify_cache
        .get_unit_by_rel_path(source.id, "one.md")
        .expect("get_unit_by_rel_path")
        .expect("one.md's unit row must still exist after the edit");

    let conn = verify_cache.connection();
    let link_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
            [unit.id],
            |r| r.get(0),
        )
        .unwrap();
    let tag_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tags WHERE unit_id = ?1",
            [unit.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        link_count, 1,
        "exactly the new [[Three]] link, not the old one re-added on top"
    );
    assert_eq!(
        tag_count, 0,
        "the old #project tag must be gone after the edit"
    );

    // R7(a): assert the stored link's actual target, not just a
    // count — a naive implementation could pass `link_count == 1`
    // while still storing stale data if deletion and reinsertion
    // were miswired.
    let link_to_raw: String = conn
        .query_row(
            "SELECT to_raw FROM links WHERE from_unit_id = ?1",
            [unit.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        link_to_raw, "Three",
        "the stored link must point at the new [[Three]] wikilink"
    );

    // R7(a): the old `^block1` anchor must be gone too, not just
    // uncounted in a stale aggregate.
    let block_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM blocks WHERE unit_id = ?1",
            [unit.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        block_count, 0,
        "the old ^block1 anchor must be gone after the edit"
    );

    // two.md's own row must be completely untouched by one.md's edit.
    let other_unit = verify_cache
        .get_unit_by_rel_path(source.id, "two.md")
        .expect("get_unit_by_rel_path")
        .expect("two.md's unit row must be untouched");
    assert_eq!(other_unit.rel_path, "two.md");

    registry.unregister("notes").expect("unregister");
}

#[test]
fn creating_a_new_file_in_a_tracked_source_adds_its_row() {
    // R7(a): the spec requires "create, modify, and delete" coverage;
    // only modify and delete existed before this test.
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    fixture.write("three.md", "# Three\n\nBrand new file with a #fresh tag.\n");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let source = verify_cache
        .get_source("notes")
        .expect("get_source")
        .expect("source row");

    let created = wait_until(Duration::from_secs(5), || {
        verify_cache
            .get_unit_by_rel_path(source.id, "three.md")
            .expect("get_unit_by_rel_path")
            .is_some()
    });
    assert!(
        created,
        "three.md's row must be created after the file is added"
    );

    let unit = verify_cache
        .get_unit_by_rel_path(source.id, "three.md")
        .expect("get_unit_by_rel_path")
        .expect("three.md's unit row");
    let tag_count: i64 = verify_cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM tags WHERE unit_id = ?1 AND tag = ?2",
            rusqlite::params![unit.id, "fresh"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tag_count, 1, "the new file's #fresh tag must be recorded");

    // one.md and two.md are untouched by the new file's arrival.
    let one = verify_cache
        .get_unit_by_rel_path(source.id, "one.md")
        .expect("get_unit_by_rel_path");
    assert!(one.is_some(), "one.md's row must still exist");

    registry.unregister("notes").expect("unregister");
}

#[test]
fn deleting_a_tracked_file_removes_its_row_without_touching_others() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    fixture.remove("two.md");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let source = verify_cache
        .get_source("notes")
        .expect("get_source")
        .expect("source row");

    let deleted = wait_until(Duration::from_secs(5), || {
        verify_cache
            .get_unit_by_rel_path(source.id, "two.md")
            .expect("get_unit_by_rel_path")
            .is_none()
    });
    assert!(
        deleted,
        "two.md's row must be removed after the file is deleted"
    );

    // one.md survives untouched.
    let one = verify_cache
        .get_unit_by_rel_path(source.id, "one.md")
        .expect("get_unit_by_rel_path");
    assert!(one.is_some(), "one.md's row must still exist");

    registry.unregister("notes").expect("unregister");
}

#[test]
fn rescan_after_a_tephraignore_edit_picks_up_the_new_rules_not_a_cached_file_list() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    let initial = registry
        .register("notes", fixture.root())
        .expect("register");
    assert_eq!(
        initial.files_seen, 2,
        "one.md and two.md, before any .tephraignore exists"
    );

    // Add a `.tephraignore` that excludes two.md's bare name, then
    // force a rescan: the new rule must be re-evaluated fresh from
    // disk on this walk, not skipped because a previous walk's file
    // list was cached anywhere.
    fixture.write(".tephraignore", "two.md\n");
    let rescanned = registry.rescan("notes").expect("rescan");

    // `.tephraignore` itself is now on disk too and is always
    // ignored as a content unit, so the walk now sees 3 files total
    // (one.md, two.md, .tephraignore) but only parses one.md.
    assert_eq!(
        rescanned.files_seen, 3,
        "one.md, two.md, and .tephraignore are all walked"
    );
    assert_eq!(
        rescanned.files_ignored, 2,
        "two.md (new rule) and .tephraignore (always ignored) are skipped"
    );
    assert_eq!(rescanned.units_written, 1);

    registry.unregister("notes").expect("unregister");
}

#[test]
fn a_watched_root_removed_out_from_under_the_watcher_marks_the_source_offline() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    // Remove the entire watched root out from under the running
    // watcher (E2/"stale per the registry's offline handling" — the
    // registry's own documented offline state, not a daemon crash).
    std::fs::remove_dir_all(fixture.root()).expect("remove_dir_all");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let went_offline = wait_until(Duration::from_secs(5), || {
        verify_cache
            .get_source("notes")
            .expect("get_source")
            .map(|row| row.status == tephra_cache::SourceStatus::Offline)
            .unwrap_or(false)
    });
    assert!(
        went_offline,
        "the source must transition to offline, not crash the daemon or other sources' watchers"
    );

    registry.unregister("notes").expect("unregister");
}

#[test]
fn moving_a_directory_into_a_tracked_source_indexes_its_files() {
    // R4: a directory moved into a source (a shell `mv`, or an
    // atomic `rename(2)`) arrives as one changed path naming the
    // directory itself — nothing is indexed unless that path is
    // walked and its files upserted.
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    // Build the directory to be moved in *outside* the watched root,
    // then move it in with one atomic rename.
    let staging = tempfile::tempdir().expect("staging tempdir");
    std::fs::create_dir_all(staging.path().join("docs")).expect("mkdir docs");
    std::fs::write(
        staging.path().join("docs/new.md"),
        "# New\n\nHas a #moved-in tag.\n",
    )
    .expect("write docs/new.md");
    std::fs::rename(staging.path().join("docs"), fixture.root().join("docs"))
        .expect("rename docs into the source");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let source = verify_cache
        .get_source("notes")
        .expect("get_source")
        .expect("source row");

    let created = wait_until(Duration::from_secs(5), || {
        verify_cache
            .get_unit_by_rel_path(source.id, "docs/new.md")
            .expect("get_unit_by_rel_path")
            .is_some()
    });
    assert!(
        created,
        "a directory moved into the source must have its files indexed"
    );

    let unit = verify_cache
        .get_unit_by_rel_path(source.id, "docs/new.md")
        .unwrap()
        .unwrap();
    let tag_count: i64 = verify_cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM tags WHERE unit_id = ?1 AND tag = ?2",
            rusqlite::params![unit.id, "moved-in"],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        tag_count, 1,
        "the moved-in file's #moved-in tag must be recorded"
    );

    registry.unregister("notes").expect("unregister");
}

#[test]
fn moving_a_directory_out_of_a_tracked_source_removes_its_files_rows() {
    // R4: a directory moved out of a source must have every one of
    // its tracked files' rows reclaimed — not left behind forever,
    // since the changed-path event names only the directory, never
    // each file that used to live under it.
    let fixture = fixtures::vault_fixture();
    fixture.mkdir("d2");
    fixture.write("d2/x.md", "# X\n\nSome content.\n");

    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let source = verify_cache
        .get_source("notes")
        .expect("get_source")
        .expect("source row");
    assert!(
        verify_cache
            .get_unit_by_rel_path(source.id, "d2/x.md")
            .expect("get_unit_by_rel_path")
            .is_some(),
        "d2/x.md must be indexed by the initial scan"
    );

    let staging = tempfile::tempdir().expect("staging tempdir");
    std::fs::rename(fixture.root().join("d2"), staging.path().join("d2"))
        .expect("rename d2 out of the source");

    let removed = wait_until(Duration::from_secs(5), || {
        verify_cache
            .get_unit_by_rel_path(source.id, "d2/x.md")
            .expect("get_unit_by_rel_path")
            .is_none()
    });
    assert!(
        removed,
        "a directory moved out of the source must have its files' rows removed"
    );

    // one.md/two.md are untouched by d2's removal.
    assert!(verify_cache
        .get_unit_by_rel_path(source.id, "one.md")
        .unwrap()
        .is_some());
    assert!(verify_cache
        .get_unit_by_rel_path(source.id, "two.md")
        .unwrap()
        .is_some());

    registry.unregister("notes").expect("unregister");
}
