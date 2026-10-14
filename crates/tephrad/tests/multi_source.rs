//! Integration test: two sources registered concurrently (GP4),
//! exercising independent scan/watch lifecycles — including a
//! crash/removal of one leaving the other's watcher and cache rows
//! untouched (spec: `tests/multi_source.rs`).

mod fixtures;

use std::time::{Duration, Instant};

use tephra_cache::Cache;
use tephrad::registry::SourceRegistry;

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
fn two_sources_register_and_scan_fully_independently() {
    let notes_fixture = fixtures::vault_fixture();
    let repo_fixture = fixtures::repo_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");

    let notes_stats = registry
        .register("notes", notes_fixture.root())
        .expect("register notes");
    let repo_stats = registry
        .register("atlas-repo", repo_fixture.root())
        .expect("register atlas-repo");

    // Each source's own scan sees only its own files — no shared
    // mutable state leaking file counts across sources (GP4).
    assert_eq!(notes_stats.files_seen, 2);
    // README.md, Cargo.toml, and src/main.rs are all walked and
    // counted, but only README.md's `.md` extension qualifies it to
    // actually be parsed (R3).
    assert_eq!(repo_stats.files_seen, 3);
    assert_eq!(
        repo_stats.units_written, 1,
        "only README.md becomes a unit; Cargo.toml and main.rs must not"
    );

    let sources = registry.list().expect("list");
    assert_eq!(sources.len(), 2);
    let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"notes"));
    assert!(names.contains(&"atlas-repo"));
    // Distinct ids (Checkpoint 1 in the spec: "two distinct
    // source_registered log lines with different source ids").
    assert_ne!(sources[0].id, sources[1].id);

    // R3: Cargo.toml and main.rs must never have become unit rows.
    let repo_row = sources
        .iter()
        .find(|s| s.name == "atlas-repo")
        .expect("atlas-repo row");
    let verify_cache = Cache::open(cache_dir.path().join("cache.db")).expect("reopen cache");
    assert!(
        verify_cache
            .get_unit_by_rel_path(repo_row.id, "Cargo.toml")
            .expect("get_unit_by_rel_path")
            .is_none(),
        "Cargo.toml must never be parsed as a unit"
    );
    assert!(
        verify_cache
            .get_unit_by_rel_path(repo_row.id, "src/main.rs")
            .expect("get_unit_by_rel_path")
            .is_none(),
        "src/main.rs must never be parsed as a unit"
    );

    registry.unregister("notes").expect("unregister notes");
    registry
        .unregister("atlas-repo")
        .expect("unregister atlas-repo");
}

#[test]
fn two_sources_register_concurrently_from_separate_threads() {
    // R7(d): a real concurrent registration, not two sequential
    // calls from the same thread — proves `register` is actually
    // safe to call from multiple threads at once, as PR-6's eventual
    // JSON-RPC dispatch will.
    let notes_fixture = fixtures::vault_fixture();
    let repo_fixture = fixtures::repo_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let registry = std::sync::Arc::new(SourceRegistry::new(cache).expect("new registry"));

    let notes_root = notes_fixture.root().to_path_buf();
    let repo_root = repo_fixture.root().to_path_buf();

    let registry_for_notes = std::sync::Arc::clone(&registry);
    let notes_handle = std::thread::spawn(move || registry_for_notes.register("notes", notes_root));
    let registry_for_repo = std::sync::Arc::clone(&registry);
    let repo_handle =
        std::thread::spawn(move || registry_for_repo.register("atlas-repo", repo_root));

    let notes_result = notes_handle.join().expect("notes thread panicked");
    let repo_result = repo_handle.join().expect("repo thread panicked");

    assert!(notes_result.is_ok(), "{:?}", notes_result);
    assert!(repo_result.is_ok(), "{:?}", repo_result);

    let sources = registry.list().expect("list");
    assert_eq!(sources.len(), 2, "both concurrent registrations must land");
    let names: Vec<&str> = sources.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"notes"));
    assert!(names.contains(&"atlas-repo"));

    registry.unregister("notes").expect("unregister notes");
    registry
        .unregister("atlas-repo")
        .expect("unregister atlas-repo");
}

#[test]
fn a_sources_root_disappearing_leaves_the_others_watcher_and_rows_untouched() {
    // R7(d): the "crash" is simulated by the root actually
    // disappearing out from under a running watcher (what GP4/E2
    // describe), not by a clean `unregister` call — `unregister` is
    // already covered by
    // `removing_one_source_leaves_the_others_watcher_and_rows_untouched`.
    let notes_fixture = fixtures::vault_fixture();
    let repo_fixture = fixtures::repo_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", notes_fixture.root())
        .expect("register notes");
    registry
        .register("atlas-repo", repo_fixture.root())
        .expect("register atlas-repo");

    // Simulate notes's host crashing/unmounting: its root vanishes
    // without ever calling `unregister`.
    std::fs::remove_dir_all(notes_fixture.root()).expect("remove_dir_all");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let notes_offline = wait_until(Duration::from_secs(5), || {
        verify_cache
            .get_source("notes")
            .expect("get_source")
            .map(|row| row.status == tephra_cache::SourceStatus::Offline)
            .unwrap_or(false)
    });
    assert!(
        notes_offline,
        "notes must transition to offline rather than vanish or crash the daemon"
    );

    // atlas-repo's row, and its watcher, are untouched by notes's
    // disappearance.
    let repo_row = verify_cache
        .get_source("atlas-repo")
        .expect("get_source")
        .expect("atlas-repo must still be registered");
    let readme_unit = verify_cache
        .get_unit_by_rel_path(repo_row.id, "README.md")
        .expect("get_unit_by_rel_path")
        .expect("atlas-repo's README.md row must survive notes's crash");

    repo_fixture.write("README.md", "# atlas-repo\n\nNo links anymore.\n");
    let updated = wait_until(Duration::from_secs(5), || {
        verify_cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
                [readme_unit.id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(1)
            == 0
    });
    assert!(
        updated,
        "atlas-repo's watcher must keep running after notes's crash"
    );

    registry
        .unregister("atlas-repo")
        .expect("unregister atlas-repo");
}

#[test]
fn removing_one_source_leaves_the_others_watcher_and_rows_untouched() {
    let notes_fixture = fixtures::vault_fixture();
    let repo_fixture = fixtures::repo_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", notes_fixture.root())
        .expect("register notes");
    registry
        .register("atlas-repo", repo_fixture.root())
        .expect("register atlas-repo");

    // "Crash/removal" of `notes`.
    registry.unregister("notes").expect("unregister notes");

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    assert!(
        verify_cache
            .get_source("notes")
            .expect("get_source")
            .is_none(),
        "notes's source row must be gone"
    );
    let repo_row = verify_cache
        .get_source("atlas-repo")
        .expect("get_source")
        .expect("atlas-repo must still be registered");
    let readme_unit = verify_cache
        .get_unit_by_rel_path(repo_row.id, "README.md")
        .expect("get_unit_by_rel_path")
        .expect("atlas-repo's README.md row must survive notes's removal");
    assert_eq!(readme_unit.rel_path, "README.md");

    // atlas-repo's watcher is still live and independently debounces
    // its own edits after notes was torn down.
    repo_fixture.write("README.md", "# atlas-repo\n\nNo links anymore.\n");
    let updated = wait_until(Duration::from_secs(5), || {
        verify_cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
                [readme_unit.id],
                |r| r.get::<_, i64>(0),
            )
            .unwrap_or(1)
            == 0
    });
    assert!(
        updated,
        "atlas-repo's watcher must keep running after notes's removal"
    );

    // The re-parsed row for README.md must still exist (a fresh row,
    // from the delete-then-reinsert apply_change_batch does per
    // change) and carry zero links, matching its edited content.
    let readme_after_edit = verify_cache
        .get_unit_by_rel_path(repo_row.id, "README.md")
        .expect("get_unit_by_rel_path")
        .expect("README.md's row must still exist after the edit");
    let link_count_after: i64 = verify_cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
            [readme_after_edit.id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        link_count_after, 0,
        "the edited README.md has no links left"
    );

    registry
        .unregister("atlas-repo")
        .expect("unregister atlas-repo");
}
