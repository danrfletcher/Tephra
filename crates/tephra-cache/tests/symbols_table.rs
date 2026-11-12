//! G4 / G13 / E7 / E8: the `symbols` table and the `blocks` rows that
//! point at it, through the real `Cache` API on an on-disk cache in a
//! temp dir: insert, read back, cascade delete, one `blocks` row per
//! occurrence, and `blocks_with_id` scoped to one source.

mod common;

use common::open_temp_cache;
use tephra_cache::{Cache, NewBlock, NewSymbol, NewUnit, SourceStatus};

fn code_unit(cache: &Cache, source_id: i64, rel_path: &str) -> i64 {
    cache
        .insert_unit(&NewUnit {
            kind: "code",
            size: Some(10),
            too_large: false,
            source_id,
            rel_path,
            mtime: None,
            frontmatter_json: None,
        })
        .expect("insert_unit")
}

#[allow(clippy::too_many_arguments)]
fn symbol(
    cache: &Cache,
    unit_id: i64,
    kind: &str,
    name: &str,
    parent: Option<&str>,
    line: i64,
    start_line: i64,
    end_line: i64,
) -> i64 {
    cache
        .insert_symbol(&NewSymbol {
            unit_id,
            kind,
            name,
            parent,
            line,
            start_line,
            end_line,
        })
        .expect("insert_symbol")
}

fn block(cache: &Cache, unit_id: i64, id: &str, line: i64, symbol_id: Option<i64>) -> i64 {
    cache
        .insert_block(&NewBlock {
            unit_id,
            block_id: Some(id),
            line,
            end_line: Some(line),
            symbol_id,
        })
        .expect("insert_block")
}

fn count(cache: &Cache, table: &str, unit_id: i64) -> i64 {
    cache
        .connection()
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE unit_id = ?1"),
            [unit_id],
            |r| r.get(0),
        )
        .unwrap()
}

#[test]
fn symbols_round_trip_in_file_order_with_every_field() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source = cache
        .register_source("repo", "/r", SourceStatus::Active)
        .unwrap();
    let u = code_unit(&cache, source, "registry.rs");
    let other = code_unit(&cache, source, "other.rs");

    // Inserted out of order on purpose: reads come back by line.
    symbol(&cache, u, "impl", "ConflictReason", None, 83, 83, 91);
    symbol(&cache, u, "enum", "ConflictReason", None, 73, 72, 81);
    symbol(
        &cache,
        u,
        "method",
        "fmt",
        Some("ConflictReason"),
        84,
        84,
        90,
    );
    symbol(&cache, u, "function", "café", None, 100, 100, 100);
    symbol(&cache, other, "function", "elsewhere", None, 1, 1, 2);

    let rows = cache.symbols_for_unit(u).unwrap();
    let shape: Vec<_> = rows
        .iter()
        .map(|s| {
            (
                s.kind.as_str(),
                s.name.as_str(),
                s.parent.as_deref(),
                s.line,
                s.start_line,
                s.end_line,
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("enum", "ConflictReason", None, 73, 72, 81),
            ("impl", "ConflictReason", None, 83, 83, 91),
            ("method", "fmt", Some("ConflictReason"), 84, 84, 90),
            ("function", "café", None, 100, 100, 100),
        ],
        "a duplicate name (enum + impl) keeps both rows; UTF-8 round-trips"
    );
    assert!(rows.iter().all(|s| s.unit_id == u));
    assert_eq!(cache.symbols_for_unit(other).unwrap().len(), 1);
}

#[test]
fn a_block_names_its_owning_symbol_and_a_loose_id_has_none() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source = cache
        .register_source("repo", "/r", SourceStatus::Active)
        .unwrap();
    let u = code_unit(&cache, source, "a.rs");
    let s = symbol(&cache, u, "enum", "E", None, 73, 72, 81);
    block(&cache, u, "k3f9a2", 73, Some(s));
    block(&cache, u, "orphan", 90, None);

    let rows = cache.blocks_for_unit(u).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].block_id.as_deref(), rows[0].symbol_id),
        (Some("k3f9a2"), Some(s))
    );
    assert_eq!(
        (rows[1].block_id.as_deref(), rows[1].symbol_id),
        (Some("orphan"), None)
    );
}

#[test]
fn a_block_cannot_point_at_a_symbol_that_does_not_exist() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source = cache
        .register_source("repo", "/r", SourceStatus::Active)
        .unwrap();
    let u = code_unit(&cache, source, "a.rs");
    let result = cache.insert_block(&NewBlock {
        unit_id: u,
        block_id: Some("x"),
        line: 1,
        end_line: Some(1),
        symbol_id: Some(9999),
    });
    assert!(
        result.is_err(),
        "blocks.symbol_id is a foreign key into symbols"
    );
}

#[test]
fn delete_unit_cascades_symbols_and_the_blocks_that_name_them() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source = cache
        .register_source("repo", "/r", SourceStatus::Active)
        .unwrap();
    let u = code_unit(&cache, source, "a.rs");
    let keep = code_unit(&cache, source, "keep.rs");
    for unit in [u, keep] {
        let s = symbol(&cache, unit, "function", "f", None, 1, 1, 2);
        block(&cache, unit, "k3f9a2", 1, Some(s));
    }

    cache.delete_unit(u).unwrap();

    assert_eq!(count(&cache, "symbols", u), 0);
    assert_eq!(count(&cache, "blocks", u), 0);
    assert_eq!(count(&cache, "symbols", keep), 1, "other units are intact");
    assert_eq!(count(&cache, "blocks", keep), 1);
}

#[test]
fn delete_units_for_source_and_under_prefix_cascade_symbols() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let a = cache
        .register_source("a", "/a", SourceStatus::Active)
        .unwrap();
    let b = cache
        .register_source("b", "/b", SourceStatus::Active)
        .unwrap();
    let in_dir = code_unit(&cache, a, "src/x.rs");
    let at_root = code_unit(&cache, a, "y.rs");
    let in_b = code_unit(&cache, b, "src/x.rs");
    for unit in [in_dir, at_root, in_b] {
        let s = symbol(&cache, unit, "function", "f", None, 1, 1, 1);
        block(&cache, unit, "k3f9a2", 1, Some(s));
    }

    cache.delete_units_under_prefix(a, "src").unwrap();
    assert_eq!(count(&cache, "symbols", in_dir), 0);
    assert_eq!(count(&cache, "blocks", in_dir), 0);
    assert_eq!(count(&cache, "symbols", at_root), 1);

    cache.delete_units_for_source(a).unwrap();
    assert_eq!(count(&cache, "symbols", at_root), 0);
    assert_eq!(count(&cache, "blocks", at_root), 0);
    assert_eq!(count(&cache, "symbols", in_b), 1, "source b is untouched");
    assert_eq!(count(&cache, "blocks", in_b), 1);
}

#[test]
fn blocks_store_every_occurrence_including_two_in_one_file() {
    // E7 data: the same ID twice in one file is two rows, never one.
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source = cache
        .register_source("repo", "/r", SourceStatus::Active)
        .unwrap();
    let u = code_unit(&cache, source, "a.rs");
    let s1 = symbol(&cache, u, "function", "one", None, 3, 3, 4);
    let s2 = symbol(&cache, u, "function", "two", None, 9, 9, 10);
    block(&cache, u, "k3f9a2", 3, Some(s1));
    block(&cache, u, "k3f9a2", 9, Some(s2));

    let rows = cache.blocks_for_unit(u).unwrap();
    assert_eq!(rows.len(), 2);
    let hits = cache.blocks_with_id(source, "k3f9a2").unwrap();
    assert_eq!(
        hits.iter()
            .map(|h| (h.rel_path.as_str(), h.line))
            .collect::<Vec<_>>(),
        vec![("a.rs", 3), ("a.rs", 9)]
    );
}

#[test]
fn blocks_with_id_lists_every_file_sorted_by_path_and_scoped_to_one_source() {
    // E8: the same ID in two files of one source gives two rows, each
    // with its unit and line. The same ID in another source's file is
    // never returned for the first.
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let repo = cache
        .register_source("repo", "/r", SourceStatus::Active)
        .unwrap();
    let vault = cache
        .register_source("vault", "/v", SourceStatus::Active)
        .unwrap();

    // Inserted in reverse path order: the result is sorted by path.
    let registry_copy = code_unit(&cache, repo, "crates/tephrad/src/registry_copy.rs");
    let conflict = code_unit(&cache, repo, "crates/tephrad/src/conflict.rs");
    let elsewhere = code_unit(&cache, vault, "conflict.rs");
    let unrelated = code_unit(&cache, repo, "unrelated.rs");
    for (unit, line) in [(registry_copy, 73), (conflict, 41), (elsewhere, 5)] {
        let s = symbol(
            &cache,
            unit,
            "enum",
            "ConflictReason",
            None,
            line,
            line,
            line,
        );
        block(&cache, unit, "k3f9a2", line, Some(s));
    }
    block(&cache, unrelated, "other-id", 1, None);

    let hits = cache.blocks_with_id(repo, "k3f9a2").unwrap();
    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits.iter()
            .map(|h| (h.rel_path.as_str(), h.unit_id, h.line))
            .collect::<Vec<_>>(),
        vec![
            ("crates/tephrad/src/conflict.rs", conflict, 41),
            ("crates/tephrad/src/registry_copy.rs", registry_copy, 73),
        ]
    );
    assert!(hits.iter().all(|h| h.block_id == "k3f9a2"));
    assert!(hits.iter().all(|h| h.symbol_id.is_some()));

    let vault_hits = cache.blocks_with_id(vault, "k3f9a2").unwrap();
    assert_eq!(vault_hits.len(), 1);
    assert_eq!(vault_hits[0].unit_id, elsewhere);

    assert!(cache.blocks_with_id(repo, "K3F9A2").unwrap().is_empty());
    assert!(cache.blocks_with_id(repo, "nope").unwrap().is_empty());
}

#[test]
fn a_cache_from_the_build_before_symbol_lines_is_rebuilt_on_open() {
    // The earlier v2 shape had no `symbols.line` and no
    // `blocks.symbol_id`, at the same version number. Opening such a
    // file rebuilds the derived tables (sources survive) so the new
    // columns exist.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("cache.db");
    {
        let cache = Cache::open(&db_path).unwrap();
        cache
            .register_source("repo", "/r", SourceStatus::Active)
            .unwrap();
        cache
            .connection()
            .execute_batch(
                "DROP TABLE blocks; DROP TABLE symbols; \
                 CREATE TABLE symbols ( \
                     id INTEGER PRIMARY KEY AUTOINCREMENT, \
                     unit_id INTEGER NOT NULL REFERENCES units(id), \
                     kind TEXT NOT NULL, name TEXT NOT NULL, parent TEXT, \
                     start_line INTEGER NOT NULL, end_line INTEGER NOT NULL); \
                 CREATE TABLE blocks ( \
                     id INTEGER PRIMARY KEY AUTOINCREMENT, \
                     unit_id INTEGER NOT NULL REFERENCES units(id), \
                     block_id TEXT, line INTEGER NOT NULL, end_line INTEGER);",
            )
            .unwrap();
    }

    let cache = Cache::open(&db_path).expect("reopen");
    assert_eq!(
        cache.rebuilt_from_version(),
        Some(tephra_cache::CURRENT_SCHEMA_VERSION)
    );
    let source = cache.get_source("repo").unwrap().expect("source survives");
    let u = code_unit(&cache, source.id, "a.rs");
    let s = symbol(&cache, u, "function", "f", None, 2, 1, 3);
    block(&cache, u, "k3f9a2", 2, Some(s));
    assert_eq!(cache.symbols_for_unit(u).unwrap()[0].line, 2);

    drop(cache);
    let again = Cache::open(&db_path).unwrap();
    assert_eq!(again.rebuilt_from_version(), None, "not rebuilt twice");
}
