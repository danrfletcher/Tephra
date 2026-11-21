//! `Cache::read_snapshot`: several reads see one state of the cache even
//! while another connection commits between them.

mod common;

use common::{open_temp_cache, seed_source_with_units};
use tephra_cache::{Cache, CacheError, NewUnit, SourceStatus};

fn add_unit(cache: &Cache, source_id: i64, rel: &str) {
    cache
        .insert_unit(&NewUnit {
            source_id,
            rel_path: rel,
            mtime: Some(1),
            frontmatter_json: None,
            kind: "prose",
            size: Some(1),
            too_large: false,
            hash: None,
            inode: None,
            device: None,
        })
        .unwrap();
}

#[test]
fn reads_inside_a_snapshot_ignore_what_another_connection_commits() {
    let dir = tempfile::tempdir().unwrap();
    let reader = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&reader, "notes", "/notes", SourceStatus::Active);
    let writer = Cache::open(reader.path()).unwrap();

    let (first, second) = reader
        .read_snapshot(|| {
            let first = reader.units_of_source(seeded.source_id)?.len();
            // Another connection commits a unit between the two reads.
            add_unit(&writer, seeded.source_id, "three.md");
            let second = reader.units_of_source(seeded.source_id)?.len();
            Ok::<_, CacheError>((first, second))
        })
        .unwrap();
    assert_eq!(first, 2);
    assert_eq!(second, 2, "the commit in between is not seen");

    assert_eq!(
        reader.units_of_source(seeded.source_id).unwrap().len(),
        3,
        "outside the snapshot the new unit is there"
    );
}

#[test]
fn a_snapshot_inside_a_transaction_just_runs() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "notes", "/notes", SourceStatus::Active);
    cache.connection().execute_batch("BEGIN").unwrap();
    let n = cache
        .read_snapshot(|| Ok::<_, CacheError>(cache.units_of_source(seeded.source_id)?.len()))
        .unwrap();
    cache.connection().execute_batch("ROLLBACK").unwrap();
    assert_eq!(n, 2);
}
