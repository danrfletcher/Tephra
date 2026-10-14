//! Integration tests exercising `SourceRegistry::list`/`rescan`/
//! `unregister` against a real `Cache` and a real filesystem fixture
//! (spec: `tests/multi_source.rs`'s sibling coverage for lifecycle
//! operations the registry.rs unit tests can't reach, since those are
//! deliberately pure in-memory `Source` fixtures — see this crate's
//! `registry.rs` module doc comment).
//!
//! R7(b)/(c) review findings: `registry.rs`'s own `#[cfg(test)]`
//! module only ever asserted on local arrays
//! (`list_returns_every_registered_source_unmodified`) or on
//! `tephra_cache::validate_transition` directly (the "state
//! transition" tests), never on `SourceRegistry::register`/`rescan`/
//! `unregister` themselves, and nothing called `unregister` twice to
//! prove its documented idempotency. These tests close that gap.

mod fixtures;

use tephra_cache::{Cache, SourceStatus};
use tephrad::registry::SourceRegistry;

#[test]
fn list_reflects_real_registered_sources_not_a_fixture_array() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    assert!(
        registry.list().expect("list").is_empty(),
        "a fresh registry starts with no sources"
    );

    registry
        .register("notes", fixture.root())
        .expect("register");

    let sources = registry.list().expect("list");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].name, "notes");
    assert_eq!(
        sources[0].path,
        fixture
            .root()
            .canonicalize()
            .unwrap()
            .to_string_lossy()
            .to_string()
    );
    assert_eq!(sources[0].status, SourceStatus::Active);

    registry.unregister("notes").expect("unregister");
    assert!(
        registry.list().expect("list").is_empty(),
        "list must reflect the real unregister, not a stale snapshot"
    );
}

#[test]
fn rescan_transitions_through_stale_back_to_active_and_rewrites_rows() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    // Add a new file directly, bypassing the watcher (no wait/sleep
    // needed): `rescan` must pick it up on its own forced re-walk.
    fixture.write("three.md", "# Three\n\nNo links.\n");

    let stats = registry.rescan("notes").expect("rescan");
    assert_eq!(
        stats.units_written, 3,
        "one.md, two.md, and the newly-added three.md"
    );

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let row = verify_cache
        .get_source("notes")
        .expect("get_source")
        .expect("source row");
    assert_eq!(
        row.status,
        SourceStatus::Active,
        "rescan must land back on Active, not stay Stale"
    );
    assert!(
        verify_cache
            .get_unit_by_rel_path(row.id, "three.md")
            .expect("get_unit_by_rel_path")
            .is_some(),
        "rescan's real re-walk must index the file added since the initial scan"
    );

    registry.unregister("notes").expect("unregister");
}

#[test]
fn rescan_on_a_missing_root_marks_the_source_offline_without_erroring() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    std::fs::remove_dir_all(fixture.root()).expect("remove_dir_all");

    let stats = registry
        .rescan("notes")
        .expect("rescan on a missing root must not error");
    assert_eq!(stats.files_seen, 0);

    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let row = verify_cache
        .get_source("notes")
        .expect("get_source")
        .expect("source row");
    assert_eq!(row.status, SourceStatus::Offline);

    registry.unregister("notes").expect("unregister");
}

#[test]
fn unregister_is_idempotent_when_called_twice() {
    // R7(c): the spec's idempotency criterion, exercised against the
    // real `SourceRegistry`, not just `Cache::delete_source` directly.
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    registry.unregister("notes").expect("first unregister");
    registry
        .unregister("notes")
        .expect("second unregister on an already-removed source must still succeed");

    assert!(registry.list().expect("list").is_empty());
}

#[test]
fn unregister_on_a_name_that_was_never_registered_is_a_no_op() {
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache = Cache::open(cache_dir.path().join("cache.db")).expect("open cache");
    let registry = SourceRegistry::new(cache).expect("new registry");

    registry
        .unregister("never-registered")
        .expect("unregister on an unknown name must not error");
}
