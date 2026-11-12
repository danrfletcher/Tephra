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

use tephra_cache::{Cache, FaultInjector, NewLink, SourceStatus};

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
    // rows must come back, ids included. The v2 snapshot only reads the
    // v1 column set, so a restored unit is its old self except for the
    // three provisional fields (E10): `kind` guessed from the path
    // (`text-other`, this plain `Cache::open` knows no extensions),
    // `size` unknown, and `needs_rescan` set.
    let units_after = cache.units_with_status_for_source("notes").unwrap();
    assert_eq!(units_after.len(), units_before.len());
    for (after, before) in units_after.iter().zip(&units_before) {
        assert_eq!(after.source_status, before.source_status);
        assert_eq!(after.unit.id, before.unit.id);
        assert_eq!(after.unit.rel_path, before.unit.rel_path);
        assert_eq!(after.unit.mtime, before.unit.mtime);
        assert_eq!(after.unit.frontmatter_json, before.unit.frontmatter_json);
        assert_eq!(after.unit.kind, "text-other");
        assert_eq!(after.unit.size, None);
        assert!(!after.unit.too_large);
        assert!(after.unit.needs_rescan);
    }

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

/// A stand-in for `tephrad`'s extension table: what `tephrad` passes to
/// `Cache::open_with_kind_inferrer`.
fn infer_by_extension(rel_path: &str) -> &'static str {
    match rel_path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("md") => "prose",
        Some("rs") => "code",
        _ => "text-other",
    }
}

#[test]
fn offline_rows_of_a_v1_cache_survive_the_v2_rebuild_with_needs_rescan() {
    // E10: today the carry-over gate would refuse a v1 cache (it has no
    // `kind` column). The snapshot now reads the v1 column set.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    common::write_v1_cache(&db_path);

    let cache = Cache::open_with_kind_inferrer(&db_path, infer_by_extension).expect("open v1");
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(tephra_cache::CURRENT_SCHEMA_VERSION)
    );
    assert_eq!(tephra_cache::CURRENT_SCHEMA_VERSION, 2);

    let offline = cache.units_with_status_for_source("offline-notes").unwrap();
    assert_eq!(offline.len(), 2, "offline v1 units must survive");
    let by_path = |p: &str| {
        &offline
            .iter()
            .find(|u| u.unit.rel_path == p)
            .unwrap_or_else(|| panic!("missing {p}"))
            .unit
    };
    let md = by_path("Café notes.md");
    assert_eq!((md.id, md.mtime), (1, Some(1_700_000_001)));
    assert_eq!(md.frontmatter_json.as_deref(), Some("{\"title\":\"One\"}"));
    assert_eq!(md.kind, "prose", "kind is inferred from the extension");
    assert_eq!(md.size, None);
    assert!(md.needs_rescan);
    assert!(!md.too_large);
    // Extension match is case-insensitive in the inferrer under test.
    let rs = by_path("src/main.RS");
    assert_eq!(rs.kind, "code");
    assert!(rs.needs_rescan);

    // Links, tags and blocks are carried as before.
    assert_eq!(cache.table_row_count("links").unwrap(), 1);
    assert_eq!(cache.table_row_count("tags").unwrap(), 1);
    let blocks = cache.blocks_for_unit(1).unwrap();
    assert_eq!(blocks.len(), 1);
    assert_eq!(
        (blocks[0].block_id.as_str(), blocks[0].line),
        ("abcd1234", 7)
    );
    assert_eq!(blocks[0].end_line, None);

    // The online source was wiped (a rescan rebuilds it) and the
    // offline source keeps its status.
    assert!(cache
        .units_with_status_for_source("online-repo")
        .unwrap()
        .is_empty());
    assert_eq!(
        cache.get_source("offline-notes").unwrap().unwrap().status,
        SourceStatus::Offline
    );
}

#[test]
fn a_rebuild_flags_online_sources_for_rescan_but_not_offline_ones() {
    // E11 + E10: the rebuild emptied every online source's rows, so each
    // must show up as "scan unfinished"; the offline source's rows were
    // carried across and it can't be scanned, so it must not.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("cache.db");
    common::write_v1_cache(&db_path);

    let cache = Cache::open(&db_path).expect("open v1");
    let unfinished: Vec<String> = cache
        .sources_with_unfinished_scan()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(unfinished, vec!["online-repo".to_string()]);
}
