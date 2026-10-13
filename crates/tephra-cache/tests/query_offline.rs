//! Queries against an offline source return cached rows with status
//! metadata attached (E2); fixture = two sources, one flipped offline
//! mid-test.

mod common;

use tephra_cache::SourceStatus;

#[test]
fn offline_source_query_returns_cached_rows_annotated_offline() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    common::seed_source_with_units(&cache, "repo", "/code/repo", SourceStatus::Active);

    // Flip only "repo" offline mid-test; "notes" stays active.
    cache
        .transition_source_status("repo", SourceStatus::Offline)
        .expect("repo -> offline");

    let notes_units = cache.units_with_status_for_source("notes").unwrap();
    assert_eq!(notes_units.len(), 2);
    assert!(notes_units
        .iter()
        .all(|u| u.source_status == SourceStatus::Active));

    let repo_units = cache.units_with_status_for_source("repo").unwrap();
    assert_eq!(
        repo_units.len(),
        2,
        "offline source's cached rows must still be returned, not empty"
    );
    assert!(
        repo_units
            .iter()
            .all(|u| u.source_status == SourceStatus::Offline),
        "every row from the offline source must carry status metadata the caller can detect"
    );

    // The two sources' rows never mix.
    let notes_rel_paths: Vec<&str> = notes_units
        .iter()
        .map(|u| u.unit.rel_path.as_str())
        .collect();
    let repo_rel_paths: Vec<&str> = repo_units
        .iter()
        .map(|u| u.unit.rel_path.as_str())
        .collect();
    assert_eq!(
        notes_rel_paths, repo_rel_paths,
        "both fixtures use the same two rel_paths"
    );
    for u in &notes_units {
        assert!(repo_units.iter().all(|r| r.unit.id != u.unit.id));
    }
}

#[test]
fn query_against_unknown_source_name_returns_empty_not_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);

    let result = cache
        .units_with_status_for_source("never-registered")
        .unwrap();
    assert!(result.is_empty());
}
