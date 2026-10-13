//! Integration test: seed a source with units/links/tags/blocks, mark
//! it offline, run a schema rebuild, then assert rows are intact and
//! status unchanged (E2 + F2 together).
//!
//! F2's fence says the rebuild unconditionally drops and recreates the
//! core tables. E2 (and the goal/scope text: "an unmounted or offline
//! source's cached index is preserved rather than wiped") requires an
//! *offline* source's cached units/links/tags/blocks to survive that
//! same rebuild. The implementation reconciles both: the tables are
//! still genuinely dropped and recreated (no incremental migration
//! path, satisfying F2's actual intent), but any row belonging to a
//! currently-offline source is snapshotted first and reinserted
//! afterward with its original id, so it comes back byte-for-byte.

mod common;

use tephra_cache::{FaultInjector, NewLink, SourceStatus};

#[test]
fn offline_source_survives_a_schema_rebuild_with_status_and_rows_intact() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cache = common::open_temp_cache(&dir);

    let seeded =
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);

    // The source goes offline (its path vanished) before the rebuild.
    cache
        .transition_source_status("notes", SourceStatus::Offline)
        .expect("active -> offline");

    let units_before = cache.units_with_status_for_source("notes").unwrap();
    assert_eq!(units_before.len(), 2);

    // A second, still-active source, to prove the rebuild still wipes
    // non-offline sources' core-table rows (F2's "unconditionally"
    // still holds for everything that *isn't* offline).
    common::seed_source_with_units(&cache, "repo", "/code/repo", SourceStatus::Active);
    assert_eq!(cache.table_row_count("units").unwrap(), 4);

    // A schema-version rebuild happens (e.g. the daemon was upgraded)
    // while "notes" is still offline.
    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    // E2: the source row itself (name, path, and crucially its
    // `offline` status) must be preserved by the rebuild — not reset
    // to `active`.
    let row = cache
        .get_source("notes")
        .unwrap()
        .expect("source row survives rebuild");
    assert_eq!(row.status, SourceStatus::Offline);
    assert_eq!(row.id, seeded.source_id);
    assert_eq!(row.path, "/vault/notes");

    // E2: "notes" is offline, so its cached units/links/tags/blocks
    // rows must come back exactly as they were, ids included.
    let units_after = cache.units_with_status_for_source("notes").unwrap();
    assert_eq!(
        units_after, units_before,
        "an offline source's cached rows must survive a schema rebuild unchanged"
    );

    // F2: "repo" is active (not offline), so its core-table rows were
    // genuinely wiped by the unconditional drop/recreate.
    let repo_units = cache.units_with_status_for_source("repo").unwrap();
    assert!(
        repo_units.is_empty(),
        "an active source's cached rows are not exempt from the rebuild"
    );

    // Overall table counts: only the two units/links/tags/blocks rows
    // that belonged to the offline "notes" source remain.
    assert_eq!(cache.table_row_count("units").unwrap(), 2);
    assert_eq!(cache.table_row_count("links").unwrap(), 2);
    assert_eq!(cache.table_row_count("tags").unwrap(), 2);
    assert_eq!(cache.table_row_count("blocks").unwrap(), 2);

    // sources itself is always fully preserved, regardless of status.
    assert_eq!(cache.source_row_count().unwrap(), 2);
}

#[test]
fn rebuild_nulls_a_preserved_links_to_unit_id_if_its_target_was_wiped() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cache = common::open_temp_cache(&dir);

    let offline_seeded =
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let active_seeded =
        common::seed_source_with_units(&cache, "repo", "/code/repo", SourceStatus::Active);

    // A link from the (soon-to-be-offline) "notes" source resolving to
    // a real unit in the (still-active) "repo" source.
    let cross_source_unit_id = offline_seeded.unit_ids[0];
    let target_unit_id = active_seeded.unit_ids[0];
    cache
        .insert_link(&NewLink {
            from_unit_id: cross_source_unit_id,
            to_raw: "[[Target]]",
            to_unit_id: Some(target_unit_id),
            kind: "wikilink",
        })
        .expect("insert cross-source link");

    cache
        .transition_source_status("notes", SourceStatus::Offline)
        .expect("active -> offline");

    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    // "repo" (active) was wiped, so target_unit_id no longer exists.
    // The preserved link from the offline "notes" source must still
    // come back (it belonged to an offline source), but with
    // to_unit_id nulled out rather than a dangling foreign key.
    let notes_units = cache.units_with_status_for_source("notes").unwrap();
    assert_eq!(notes_units.len(), 2);

    let links_from_cross_source_unit: i64 = cache
        .connection()
        .query_row(
            "SELECT to_unit_id FROM links WHERE from_unit_id = ?1 AND to_raw = '[[Target]]'",
            [cross_source_unit_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .expect("link survives")
        .unwrap_or(-1);
    assert_eq!(
        links_from_cross_source_unit, -1,
        "to_unit_id must be NULL once its target was wiped by the rebuild"
    );
}

#[test]
fn marking_a_source_offline_never_touches_its_cached_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let before = cache.units_with_status_for_source("notes").unwrap();

    cache
        .transition_source_status("notes", SourceStatus::Offline)
        .expect("active -> offline");

    let after = cache.units_with_status_for_source("notes").unwrap();
    let before_units: Vec<_> = before.into_iter().map(|u| u.unit).collect();
    let after_units: Vec<_> = after.into_iter().map(|u| u.unit).collect();
    assert_eq!(
        before_units, after_units,
        "E2: marking a source offline must update only sources.status"
    );
}

#[test]
fn remount_sequence_offline_to_stale_to_active_never_wipes_cached_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let seeded_units = cache.units_with_status_for_source("notes").unwrap();
    let seeded_rel_paths: Vec<String> = seeded_units
        .iter()
        .map(|u| u.unit.rel_path.clone())
        .collect();
    let row_counts_before = (
        cache.table_row_count("units").unwrap(),
        cache.table_row_count("links").unwrap(),
        cache.table_row_count("tags").unwrap(),
        cache.table_row_count("blocks").unwrap(),
    );

    // The source's path vanishes (goes offline). Cached rows must
    // still be queryable.
    cache
        .transition_source_status("notes", SourceStatus::Offline)
        .expect("active -> offline");
    let units_while_offline = cache.units_with_status_for_source("notes").unwrap();
    assert_eq!(units_while_offline.len(), seeded_units.len());
    assert!(units_while_offline
        .iter()
        .all(|u| u.source_status == SourceStatus::Offline));

    // The path reappears: the daemon observes the remount as
    // offline -> stale (E2), not a direct jump to active, and not a
    // cache wipe.
    cache
        .transition_source_status("notes", SourceStatus::Stale)
        .expect("offline -> stale");
    let units_while_stale = cache.units_with_status_for_source("notes").unwrap();
    let stale_rel_paths: Vec<String> = units_while_stale
        .iter()
        .map(|u| u.unit.rel_path.clone())
        .collect();
    assert_eq!(
        stale_rel_paths, seeded_rel_paths,
        "cached rows must remain queryable, and unchanged, through the stale step"
    );
    assert!(units_while_stale
        .iter()
        .all(|u| u.source_status == SourceStatus::Stale));

    // A rescan completes: stale -> active.
    cache
        .transition_source_status("notes", SourceStatus::Active)
        .expect("stale -> active");
    let units_while_active = cache.units_with_status_for_source("notes").unwrap();
    let active_rel_paths: Vec<String> = units_while_active
        .iter()
        .map(|u| u.unit.rel_path.clone())
        .collect();
    assert_eq!(active_rel_paths, seeded_rel_paths);
    assert!(units_while_active
        .iter()
        .all(|u| u.source_status == SourceStatus::Active));

    // No full cache wipe happened anywhere in the sequence: row
    // counts across every core table are exactly what they were
    // before the outage started.
    let row_counts_after = (
        cache.table_row_count("units").unwrap(),
        cache.table_row_count("links").unwrap(),
        cache.table_row_count("tags").unwrap(),
        cache.table_row_count("blocks").unwrap(),
    );
    assert_eq!(
        row_counts_after, row_counts_before,
        "offline -> stale -> active must never trigger a full cache wipe"
    );
}
