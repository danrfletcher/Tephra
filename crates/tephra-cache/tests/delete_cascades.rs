//! `delete_unit`/`delete_units_for_source`/`delete_source`: the
//! cascading-delete helpers PR-4's registry and watcher pipeline rely
//! on (an unregister, a rescan's scoped clear, and a watcher's
//! delete-then-reinsert on a file edit).

mod common;

use common::{open_temp_cache, seed_source_with_units};
use tephra_cache::SourceStatus;

#[test]
fn delete_unit_removes_the_row_and_its_links_tags_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let unit_id = seeded.unit_ids[0];

    cache.delete_unit(unit_id).expect("delete_unit");

    assert!(cache.get_unit(unit_id).expect("get_unit").is_none());

    // The other unit (and its own links/tags/blocks) survive untouched.
    let other_unit_id = seeded.unit_ids[1];
    assert!(cache.get_unit(other_unit_id).expect("get_unit").is_some());

    let conn = cache.connection();
    let links: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap();
    let tags: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tags WHERE unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap();
    let blocks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM blocks WHERE unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 0);
    assert_eq!(tags, 0);
    assert_eq!(blocks, 0);
}

#[test]
fn delete_unit_is_a_no_op_for_a_nonexistent_id() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    // No seed data at all: id 999 never existed.
    cache
        .delete_unit(999)
        .expect("delete_unit on missing id must not error");
}

#[test]
fn get_unit_by_rel_path_finds_the_right_row() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);

    let found = cache
        .get_unit_by_rel_path(seeded.source_id, "one.md")
        .expect("get_unit_by_rel_path")
        .expect("row should exist");
    assert_eq!(found.id, seeded.unit_ids[0]);
    assert_eq!(found.rel_path, "one.md");

    let missing = cache
        .get_unit_by_rel_path(seeded.source_id, "nonexistent.md")
        .expect("get_unit_by_rel_path");
    assert!(missing.is_none());
}

#[test]
fn delete_units_for_source_clears_only_that_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let notes = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let repo = seed_source_with_units(&cache, "repo", "/code/repo", SourceStatus::Active);

    cache
        .delete_units_for_source(notes.source_id)
        .expect("delete_units_for_source");

    for unit_id in &notes.unit_ids {
        assert!(cache.get_unit(*unit_id).expect("get_unit").is_none());
    }
    // The other source's rows are completely untouched (GP4
    // independence).
    for unit_id in &repo.unit_ids {
        assert!(cache.get_unit(*unit_id).expect("get_unit").is_some());
    }
    // Both `sources` rows themselves survive — only the core-table
    // rows for `notes` were cleared.
    assert!(cache.get_source("notes").unwrap().is_some());
    assert!(cache.get_source("repo").unwrap().is_some());
}

#[test]
fn delete_source_removes_the_source_row_and_cascades() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let notes = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let repo = seed_source_with_units(&cache, "repo", "/code/repo", SourceStatus::Active);

    cache.delete_source("notes").expect("delete_source");

    assert!(cache.get_source("notes").unwrap().is_none());
    for unit_id in &notes.unit_ids {
        assert!(cache.get_unit(*unit_id).expect("get_unit").is_none());
    }
    // `repo` is entirely unaffected.
    assert!(cache.get_source("repo").unwrap().is_some());
    for unit_id in &repo.unit_ids {
        assert!(cache.get_unit(*unit_id).expect("get_unit").is_some());
    }
}

#[test]
fn delete_source_is_idempotent_on_an_already_removed_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);

    cache.delete_source("notes").expect("first delete_source");
    // Calling it again on an already-removed source must not error.
    cache
        .delete_source("notes")
        .expect("second delete_source must be a no-op, not an error");
    // Never-registered name: also not an error.
    cache
        .delete_source("never-existed")
        .expect("delete_source on an unknown name must not error");
}
