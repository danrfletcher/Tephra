//! Table-driven test covering every status transition
//! (`active<->stale<->offline`) including invalid/skipped transitions
//! returning an error (G5: "documented transition function, no silent
//! state-skipping").

use tephra_cache::{validate_transition, SourceStatus};

mod common;

fn all_statuses() -> [SourceStatus; 3] {
    [
        SourceStatus::Active,
        SourceStatus::Stale,
        SourceStatus::Offline,
    ]
}

/// Every legal edge, called out individually so a failure names the
/// exact pair instead of just "some case failed".
#[test]
fn every_legal_edge_is_accepted() {
    let legal = [
        (SourceStatus::Active, SourceStatus::Active),
        (SourceStatus::Stale, SourceStatus::Stale),
        (SourceStatus::Offline, SourceStatus::Offline),
        (SourceStatus::Active, SourceStatus::Stale),
        (SourceStatus::Stale, SourceStatus::Active),
        (SourceStatus::Active, SourceStatus::Offline),
        (SourceStatus::Stale, SourceStatus::Offline),
        (SourceStatus::Offline, SourceStatus::Stale),
    ];
    for (from, to) in legal {
        assert!(
            validate_transition(from, to).is_ok(),
            "{from} -> {to} should be legal"
        );
    }
}

/// The one deliberately-disallowed skip (E2): recovering from offline
/// must go through `stale` first.
#[test]
fn offline_to_active_is_rejected_as_a_skip() {
    let err = validate_transition(SourceStatus::Offline, SourceStatus::Active)
        .expect_err("offline -> active must be rejected");
    assert_eq!(err.from, SourceStatus::Offline);
    assert_eq!(err.to, SourceStatus::Active);
}

/// Exhaustively checks every (from, to) pair in the 3x3 status matrix
/// against the documented table, so a future status addition or a typo
/// in the match arms is caught by a changed count rather than passing
/// silently.
#[test]
fn full_transition_matrix_matches_the_documented_table() {
    let mut legal_count = 0;
    for from in all_statuses() {
        for to in all_statuses() {
            let is_offline_to_active = from == SourceStatus::Offline && to == SourceStatus::Active;
            let expected_legal = !is_offline_to_active;
            let actual_legal = validate_transition(from, to).is_ok();
            assert_eq!(
                actual_legal, expected_legal,
                "transition {from} -> {to}: expected legal={expected_legal}, got {actual_legal}"
            );
            if actual_legal {
                legal_count += 1;
            }
        }
    }
    // 9 total pairs in a 3x3 matrix, minus the one disallowed skip.
    assert_eq!(legal_count, 8);
}

#[test]
fn cache_transition_source_status_applies_a_legal_transition() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    cache
        .register_source("notes", "/vault/notes", SourceStatus::Active)
        .expect("register");

    cache
        .transition_source_status("notes", SourceStatus::Offline)
        .expect("active -> offline should succeed");

    let row = cache.get_source("notes").unwrap().expect("source exists");
    assert_eq!(row.status, SourceStatus::Offline);
}

#[test]
fn cache_transition_source_status_rejects_the_disallowed_skip() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    cache
        .register_source("notes", "/vault/notes", SourceStatus::Offline)
        .expect("register");

    let result = cache.transition_source_status("notes", SourceStatus::Active);
    assert!(
        result.is_err(),
        "offline -> active must be rejected through the Cache API too"
    );

    // The row must be left exactly as it was: still offline.
    let row = cache.get_source("notes").unwrap().expect("source exists");
    assert_eq!(row.status, SourceStatus::Offline);
}

#[test]
fn cache_transition_source_status_on_unknown_name_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let result = cache.transition_source_status("missing", SourceStatus::Stale);
    assert!(result.is_err());
}
