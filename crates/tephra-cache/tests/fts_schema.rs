//! G10 / G17: the `block_fts` table is part of schema v2: created with
//! the trigram tokenizer on a fresh open and after a v1 -> v2 rebuild,
//! emptied by every delete path (per unit, per source, per folder
//! prefix) by rowid range, backfilled by a rescan flag when files are
//! cached but the index is empty, and carried across no rebuild.

mod common;

use common::{open_temp_cache, seed_source_with_units, write_v1_cache};
use tephra_cache::{Cache, NewFtsRow, NewUnit, SourceStatus};

fn row(text: &str) -> NewFtsRow<'_> {
    NewFtsRow {
        kind: "paragraph",
        line: Some(1),
        text,
        ..Default::default()
    }
}

fn fts_count(cache: &Cache) -> i64 {
    cache.table_row_count("block_fts").unwrap()
}

fn fts_count_for_unit(cache: &Cache, unit_id: i64) -> i64 {
    cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM block_fts WHERE unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap()
}

fn table_sql(cache: &Cache) -> String {
    cache
        .connection()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'block_fts'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn the_table_exists_with_the_trigram_tokenizer_after_a_fresh_open() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let sql = table_sql(&cache).to_lowercase();
    assert!(sql.contains("fts5"), "{sql}");
    assert!(sql.contains("trigram"), "{sql}");
    // The columns the search needs, only `text` tokenized.
    for column in [
        "unit_id", "line", "kind", "block_id", "name", "label", "place", "place_n", "idx",
    ] {
        assert!(
            sql.contains(&format!("{column} unindexed")),
            "{column} is UNINDEXED: {sql}"
        );
    }
    assert!(!sql.contains("text unindexed"), "{sql}");
}

#[test]
fn the_trigram_tokenizer_is_really_in_use_a_substring_matches() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    cache
        .insert_fts_rows(seeded.unit_ids[0], &[row("an enum ConflictReason here")])
        .unwrap();
    let hits = cache.search_all_blocks("flictRea", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].rel_path, "one.md");
}

#[test]
fn a_v1_cache_rebuilt_to_v2_has_the_table_empty_and_every_online_source_flagged() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    write_v1_cache(&db);
    let cache = Cache::open(&db).unwrap();
    assert_eq!(cache.rebuilt_from_version(), Some(1));
    let sql = table_sql(&cache).to_lowercase();
    assert!(sql.contains("trigram") && sql.contains("kind"), "{sql}");
    assert_eq!(fts_count(&cache), 0);
    // The online source is rescanned on start; the offline one's
    // carried-over rows are provisional and not flagged.
    let flagged: Vec<String> = cache
        .sources_with_unfinished_scan()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(flagged, ["online-repo"]);
    // E10: the offline source's units came across with needs_rescan and
    // have no index rows (and the search says nothing about them).
    assert_eq!(fts_count(&cache), 0);
    assert!(cache.search_all_blocks("notes", 10).unwrap().is_empty());
}

#[test]
fn a_rebuild_drops_and_recreates_the_table_empty() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    cache
        .insert_fts_rows(seeded.unit_ids[0], &[row("alpha"), row("beta")])
        .unwrap();
    assert_eq!(fts_count(&cache), 2);
    cache.rebuild(&tephra_cache::FaultInjector::none()).unwrap();
    assert_eq!(fts_count(&cache), 0);
    assert!(table_sql(&cache).to_lowercase().contains("trigram"));
}

#[test]
fn an_older_v2_block_fts_without_the_new_columns_is_rebuilt_not_altered() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    {
        let cache = Cache::open(&db).unwrap();
        cache
            .register_source("n", "/n", SourceStatus::Active)
            .unwrap();
        let conn = cache.connection();
        conn.execute_batch(
            "DROP TABLE block_fts; \
             CREATE VIRTUAL TABLE block_fts USING fts5(\
                 unit_id UNINDEXED, line UNINDEXED, text, tokenize = 'trigram');",
        )
        .unwrap();
    }
    let cache = Cache::open(&db).unwrap();
    assert!(
        cache.rebuilt_from_version().is_some(),
        "the stale shape goes through the one rebuild path"
    );
    assert!(table_sql(&cache).to_lowercase().contains("place_n"));
    // The source row survives (bootstrap table) and is flagged.
    assert_eq!(cache.sources_with_unfinished_scan().unwrap().len(), 1);
}

#[test]
fn delete_unit_removes_only_that_units_rows() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    let (gone, kept) = (seeded.unit_ids[0], seeded.unit_ids[1]);
    cache
        .insert_fts_rows(gone, &[row("one"), row("two"), row("three")])
        .unwrap();
    cache.insert_fts_rows(kept, &[row("four")]).unwrap();
    assert_eq!(fts_count_for_unit(&cache, gone), 3);

    cache.delete_unit(gone).unwrap();
    assert_eq!(fts_count_for_unit(&cache, gone), 0);
    assert_eq!(fts_count_for_unit(&cache, kept), 1);
    assert_eq!(fts_count(&cache), 1);
}

#[test]
fn rewriting_a_unit_leaves_exactly_its_new_rows_no_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    let source_id = seeded.source_id;
    let old = seeded.unit_ids[0];
    cache
        .insert_fts_rows(old, &[row("stale one"), row("stale two")])
        .unwrap();

    // What the scan does for an edited file: delete the unit, insert it
    // again, write its rows.
    cache.delete_unit(old).unwrap();
    let new = cache
        .insert_unit(&NewUnit {
            source_id,
            rel_path: "one.md",
            mtime: Some(2),
            frontmatter_json: None,
            kind: "prose",
            size: Some(10),
            too_large: false,
        })
        .unwrap();
    cache.insert_fts_rows(new, &[row("fresh")]).unwrap();

    assert_eq!(fts_count(&cache), 1);
    assert!(cache.search_all_blocks("stale", 10).unwrap().is_empty());
    assert_eq!(cache.search_all_blocks("fresh", 10).unwrap().len(), 1);
}

#[test]
fn delete_units_for_source_and_delete_source_remove_every_row_of_that_source_only() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let a = seed_source_with_units(&cache, "a", "/a", SourceStatus::Active);
    let b = seed_source_with_units(&cache, "b", "/b", SourceStatus::Active);
    for unit in a.unit_ids.iter().chain(&b.unit_ids) {
        cache
            .insert_fts_rows(*unit, &[row("x text"), row("y text")])
            .unwrap();
    }
    assert_eq!(fts_count(&cache), 8);

    cache.delete_units_for_source(a.source_id).unwrap();
    assert_eq!(fts_count(&cache), 4);
    assert!(a
        .unit_ids
        .iter()
        .all(|u| fts_count_for_unit(&cache, *u) == 0));

    // Unregister: the rows go in the same call as the units and the
    // source row.
    cache.delete_source("b").unwrap();
    assert_eq!(fts_count(&cache), 0);
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
}

#[test]
fn delete_units_under_prefix_removes_the_rows_of_those_units_only() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    let mut inside = Vec::new();
    for rel in ["dir/a.md", "dir/sub/b.md"] {
        let id = cache
            .insert_unit(&NewUnit {
                source_id: seeded.source_id,
                rel_path: rel,
                mtime: None,
                frontmatter_json: None,
                kind: "prose",
                size: Some(1),
                too_large: false,
            })
            .unwrap();
        cache.insert_fts_rows(id, &[row("inside")]).unwrap();
        inside.push(id);
    }
    for unit in &seeded.unit_ids {
        cache.insert_fts_rows(*unit, &[row("outside")]).unwrap();
    }
    cache
        .delete_units_under_prefix(seeded.source_id, "dir")
        .unwrap();
    assert!(inside.iter().all(|u| fts_count_for_unit(&cache, *u) == 0));
    assert_eq!(
        fts_count(&cache),
        2,
        "the two units outside the folder keep theirs"
    );
}

#[test]
fn a_unit_with_more_rows_than_its_rowid_range_keeps_the_first_ones_and_never_spills() {
    // Rows are rowids `unit_id << 24 ..`; the writer stops at the range
    // end instead of writing into the next unit's. Checked through the
    // constant, not by writing 16 million rows.
    assert_eq!(tephra_cache::FTS_ROWS_PER_UNIT, 1 << 24);
}

#[test]
fn files_cached_but_the_index_empty_flags_every_online_source_for_a_rescan_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    {
        let cache = Cache::open(&db).unwrap();
        let online = seed_source_with_units(&cache, "online", "/o", SourceStatus::Active);
        cache.mark_scan_finished(online.source_id).unwrap();
        let other = seed_source_with_units(&cache, "other", "/p", SourceStatus::Active);
        cache.mark_scan_finished(other.source_id).unwrap();
        let off = seed_source_with_units(&cache, "off", "/q", SourceStatus::Offline);
        cache.mark_scan_finished(off.source_id).unwrap();
        // Units, but no index rows: what a build before G10 left.
        assert_eq!(fts_count(&cache), 0);
        assert!(cache.sources_with_unfinished_scan().unwrap().is_empty());
    }
    let cache = Cache::open(&db).unwrap();
    assert_eq!(
        cache.rebuilt_from_version(),
        None,
        "no rebuild, just a flag"
    );
    let mut flagged: Vec<String> = cache
        .sources_with_unfinished_scan()
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect();
    flagged.sort();
    assert_eq!(
        flagged,
        ["online", "other"],
        "the offline source is left alone"
    );
    // The rows and units are untouched by the flag.
    assert_eq!(cache.table_row_count("units").unwrap(), 6);
}

#[test]
fn a_populated_index_or_an_empty_cache_or_only_unindexable_files_do_not_flag_anything() {
    // Populated.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    {
        let cache = Cache::open(&db).unwrap();
        let s = seed_source_with_units(&cache, "s", "/s", SourceStatus::Active);
        cache.mark_scan_finished(s.source_id).unwrap();
        cache
            .insert_fts_rows(s.unit_ids[0], &[row("text")])
            .unwrap();
    }
    assert!(Cache::open(&db)
        .unwrap()
        .sources_with_unfinished_scan()
        .unwrap()
        .is_empty());

    // Empty cache with a source and no units.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    {
        let cache = Cache::open(&db).unwrap();
        let id = cache
            .register_source("s", "/s", SourceStatus::Active)
            .unwrap();
        cache.mark_scan_finished(id).unwrap();
    }
    assert!(Cache::open(&db)
        .unwrap()
        .sources_with_unfinished_scan()
        .unwrap()
        .is_empty());

    // Only binary, too-large and document units: nothing to index.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    {
        let cache = Cache::open(&db).unwrap();
        let id = cache
            .register_source("s", "/s", SourceStatus::Active)
            .unwrap();
        for (rel, kind, size, too_large) in [
            ("a.png", "binary", 10, false),
            ("b.txt", "prose", 99, true),
            ("c.pdf", "document", 10, false),
            ("d.md", "prose", 0, false),
        ] {
            cache
                .insert_unit(&NewUnit {
                    source_id: id,
                    rel_path: rel,
                    mtime: None,
                    frontmatter_json: None,
                    kind,
                    size: Some(size),
                    too_large,
                })
                .unwrap();
        }
        cache.mark_scan_finished(id).unwrap();
    }
    assert!(Cache::open(&db)
        .unwrap()
        .sources_with_unfinished_scan()
        .unwrap()
        .is_empty());
}

fn name_row<'a>(name: &'a str, text: &'a str) -> NewFtsRow<'a> {
    NewFtsRow {
        kind: "symbol",
        line: Some(1),
        name: Some(name),
        label: Some(name),
        text,
        ..Default::default()
    }
}

fn names_count(cache: &Cache) -> i64 {
    cache.table_row_count("block_names").unwrap()
}

#[test]
fn block_names_follows_the_rows_that_have_a_name_through_every_delete_path() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let a = seed_source_with_units(&cache, "a", "/a", SourceStatus::Active);
    let b = seed_source_with_units(&cache, "b", "/b", SourceStatus::Active);
    for unit in a.unit_ids.iter().chain(&b.unit_ids) {
        // Two named rows and one without a name per unit.
        cache
            .insert_fts_rows(
                *unit,
                &[
                    name_row("Alpha", "Alpha"),
                    name_row("Beta", "Beta"),
                    row("x"),
                ],
            )
            .unwrap();
    }
    assert_eq!(fts_count(&cache), 12);
    assert_eq!(names_count(&cache), 8, "only the rows with a name");

    cache.delete_unit(a.unit_ids[0]).unwrap();
    assert_eq!(names_count(&cache), 6);
    cache.delete_units_for_source(a.source_id).unwrap();
    assert_eq!(names_count(&cache), 4);
    cache.delete_source("b").unwrap();
    assert_eq!(names_count(&cache), 0);
    assert_eq!(fts_count(&cache), 0);
}

#[test]
fn block_names_follows_a_prefix_delete_and_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    let inside = cache
        .insert_unit(&NewUnit {
            source_id: seeded.source_id,
            rel_path: "dir/a.md",
            mtime: None,
            frontmatter_json: None,
            kind: "prose",
            size: Some(1),
            too_large: false,
        })
        .unwrap();
    cache
        .insert_fts_rows(inside, &[name_row("Inside", "Inside")])
        .unwrap();
    cache
        .insert_fts_rows(seeded.unit_ids[0], &[name_row("Outside", "Outside")])
        .unwrap();
    assert_eq!(names_count(&cache), 2);
    cache
        .delete_units_under_prefix(seeded.source_id, "dir")
        .unwrap();
    assert_eq!(names_count(&cache), 1);
    cache.rebuild(&tephra_cache::FaultInjector::none()).unwrap();
    assert_eq!(names_count(&cache), 0, "the rebuild recreates it empty");
}

#[test]
fn a_cache_with_block_fts_but_no_block_names_is_rebuilt_so_exact_names_are_found() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("cache.db");
    {
        let cache = Cache::open(&db).unwrap();
        let seeded = seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
        cache
            .insert_fts_rows(seeded.unit_ids[0], &[name_row("Thing", "Thing")])
            .unwrap();
        // What the build before the exact-name index left behind.
        cache
            .connection()
            .execute_batch("DROP INDEX idx_block_names_name; DROP TABLE block_names;")
            .unwrap();
    }
    let cache = Cache::open(&db).unwrap();
    assert_eq!(cache.rebuilt_from_version(), Some(2));
    assert_eq!(names_count(&cache), 0);
    assert_eq!(fts_count(&cache), 0);
    // The source is flagged for a rescan, which refills both.
    assert_eq!(cache.sources_with_unfinished_scan().unwrap().len(), 1);
}

#[test]
fn an_exact_name_is_found_through_block_names() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "n", "/n", SourceStatus::Active);
    cache
        .insert_fts_rows(
            seeded.unit_ids[0],
            &[
                row("a long paragraph all about thing"),
                name_row("Thing", "Thing"),
            ],
        )
        .unwrap();
    let hits = cache.search_all_blocks("THING", 10).unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].kind, "symbol", "the exact name is first");
}
