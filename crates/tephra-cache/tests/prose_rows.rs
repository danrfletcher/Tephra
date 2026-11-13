//! G3: the `headings` and line-ranged `blocks` tables as the prose
//! scan fills them: headings read back in file order with level, text
//! and line; blocks with a range, a NULL `block_id` for a paragraph
//! with no `^id`, and every occurrence of a repeated ID kept. Also
//! the guard for a cache file from the first v2 build, whose
//! `blocks.block_id` was NOT NULL.

mod common;

use common::open_temp_cache;
use tephra_cache::{Cache, NewBlock, NewHeading, NewUnit, SourceStatus};

fn unit(cache: &Cache, source_id: i64, rel_path: &str) -> i64 {
    cache
        .insert_unit(&NewUnit {
            kind: "prose",
            size: Some(10),
            too_large: false,
            source_id,
            rel_path,
            mtime: None,
            frontmatter_json: None,
        })
        .expect("insert_unit")
}

#[test]
fn headings_round_trip_in_file_order_with_unicode_and_deep_levels() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source = cache
        .register_source("notes", "/n", SourceStatus::Active)
        .unwrap();
    let u = unit(&cache, source, "café notes.org");
    let other = unit(&cache, source, "other.org");

    // Inserted out of order on purpose: reads come back by line.
    for (level, text, line) in [
        (3, "Deep ☕", 9),
        (1, "Café 日本語", 1),
        (12, "Twelve stars", 20),
    ] {
        cache
            .insert_heading(&NewHeading {
                unit_id: u,
                level,
                text,
                line,
            })
            .unwrap();
    }
    cache
        .insert_heading(&NewHeading {
            unit_id: other,
            level: 2,
            text: "Elsewhere",
            line: 4,
        })
        .unwrap();

    let rows = cache.headings_for_unit(u).unwrap();
    let got: Vec<_> = rows
        .iter()
        .map(|h| (h.level, h.text.as_str(), h.line))
        .collect();
    assert_eq!(
        got,
        [
            (1, "Café 日本語", 1),
            (3, "Deep ☕", 9),
            (12, "Twelve stars", 20)
        ]
    );
    assert!(rows.iter().all(|h| h.unit_id == u));
    assert_eq!(cache.headings_for_unit(other).unwrap().len(), 1);
    assert!(cache.headings_for_unit(9999).unwrap().is_empty());
}

#[test]
fn blocks_keep_ranges_null_ids_and_every_occurrence_of_a_repeated_id() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source = cache
        .register_source("notes", "/n", SourceStatus::Active)
        .unwrap();
    let u = unit(&cache, source, "a.txt");

    for (block_id, line, end_line) in [
        (Some("dup"), 1, Some(2)),
        (None, 4, Some(6)),
        (Some("dup"), 8, Some(8)),
        (Some("dup"), 8, None),
    ] {
        cache
            .insert_block(&NewBlock {
                unit_id: u,
                block_id,
                line,
                end_line,
                symbol_id: None,
            })
            .expect("a NULL block_id and a repeated (unit, block_id) are both storable");
    }

    let rows = cache.blocks_for_unit(u).unwrap();
    let got: Vec<_> = rows
        .iter()
        .map(|b| (b.block_id.as_deref(), b.line, b.end_line))
        .collect();
    assert_eq!(
        got,
        [
            (Some("dup"), 1, Some(2)),
            (None, 4, Some(6)),
            (Some("dup"), 8, Some(8)),
            (Some("dup"), 8, None),
        ]
    );
}

#[test]
fn deleting_a_unit_cascades_to_its_headings_and_leaves_other_units_alone() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source = cache
        .register_source("notes", "/n", SourceStatus::Active)
        .unwrap();
    let gone = unit(&cache, source, "gone.rst");
    let kept = unit(&cache, source, "kept.rst");
    for u in [gone, kept] {
        cache
            .insert_heading(&NewHeading {
                unit_id: u,
                level: 1,
                text: "H",
                line: 1,
            })
            .unwrap();
    }

    cache.delete_unit(gone).unwrap();

    assert!(cache.headings_for_unit(gone).unwrap().is_empty());
    assert_eq!(cache.headings_for_unit(kept).unwrap().len(), 1);
    let orphans: i64 = cache
        .connection()
        .query_row(
            "SELECT COUNT(*) FROM headings WHERE unit_id = ?1",
            [gone],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphans, 0);
}

#[test]
fn delete_units_for_source_and_delete_source_cascade_to_headings() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let a = cache
        .register_source("a", "/a", SourceStatus::Active)
        .unwrap();
    let b = cache
        .register_source("b", "/b", SourceStatus::Active)
        .unwrap();
    let ua = unit(&cache, a, "x.org");
    let ub = unit(&cache, b, "y.org");
    for u in [ua, ub] {
        cache
            .insert_heading(&NewHeading {
                unit_id: u,
                level: 1,
                text: "H",
                line: 1,
            })
            .unwrap();
    }

    cache.delete_units_for_source(a).unwrap();
    assert!(cache.headings_for_unit(ua).unwrap().is_empty());
    assert_eq!(cache.headings_for_unit(ub).unwrap().len(), 1);

    cache.delete_source("b").unwrap();
    assert!(cache.headings_for_unit(ub).unwrap().is_empty());
    assert_eq!(cache.table_row_count("headings").unwrap(), 0);
}

#[test]
fn a_cache_from_the_first_v2_build_with_a_not_null_block_id_is_rebuilt_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    {
        let cache = Cache::open(&db_path).unwrap();
        let source = cache
            .register_source("notes", "/n", SourceStatus::Active)
            .unwrap();
        unit(&cache, source, "a.md");
        // Put the table back the way the first v2 build made it.
        let conn = cache.connection();
        conn.execute_batch(
            "DROP TABLE blocks; \
             CREATE TABLE blocks ( \
                 id INTEGER PRIMARY KEY AUTOINCREMENT, \
                 unit_id INTEGER NOT NULL REFERENCES units(id), \
                 block_id TEXT NOT NULL, \
                 line INTEGER NOT NULL, \
                 end_line INTEGER \
             );",
        )
        .unwrap();
    }

    let cache = Cache::open(&db_path).expect("reopen");
    assert_eq!(
        cache.rebuilt_from_version(),
        Some(tephra_cache::CURRENT_SCHEMA_VERSION)
    );
    assert_eq!(
        cache.schema_version().unwrap(),
        Some(tephra_cache::CURRENT_SCHEMA_VERSION)
    );
    // The rebuilt table accepts a paragraph with no ID.
    let source = cache.get_source("notes").unwrap().expect("source survives");
    let u = unit(&cache, source.id, "b.md");
    cache
        .insert_block(&NewBlock {
            unit_id: u,
            block_id: None,
            line: 1,
            end_line: Some(1),
            symbol_id: None,
        })
        .expect("nullable block_id after the rebuild");

    // A cache that is already current is not rebuilt again.
    drop(cache);
    let again = Cache::open(&db_path).unwrap();
    assert_eq!(again.rebuilt_from_version(), None);
    assert_eq!(again.table_row_count("blocks").unwrap(), 1);
}

#[test]
fn an_offline_sources_id_less_paragraph_rows_survive_a_rebuild() {
    use tephra_cache::FaultInjector;

    let dir = tempfile::tempdir().unwrap();
    let mut cache = open_temp_cache(&dir);
    let source = cache
        .register_source("notes", "/n", SourceStatus::Active)
        .unwrap();
    let u = unit(&cache, source, "a.txt");
    for (block_id, line) in [(None, 1), (Some("k"), 3)] {
        cache
            .insert_block(&NewBlock {
                unit_id: u,
                block_id,
                line,
                end_line: Some(line),
                symbol_id: None,
            })
            .unwrap();
    }
    cache
        .transition_source_status("notes", SourceStatus::Offline)
        .expect("active -> offline");

    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    // A NULL `block_id` must not make the carry-over fail and drop the
    // offline source's rows.
    let ids: Vec<_> = cache
        .blocks_for_unit(u)
        .unwrap()
        .into_iter()
        .map(|b| b.block_id)
        .collect();
    assert_eq!(ids, [None, Some("k".to_string())]);
}
