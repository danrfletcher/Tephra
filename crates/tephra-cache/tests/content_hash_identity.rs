//! `units.hash`, `units.inode` and `units.device`: the content hash and
//! file identity a rename detector reads. Real SQLite in a tempdir, no
//! mocks: column round trip, the hash lookup across sources, the identity
//! lookup, NULLs for a too-large file, a u64 inode above `i64::MAX`, the
//! hash of zero bytes and the shape of `NewUnit` and `UnitRow`.

mod common;

use tephra_cache::{Cache, NewUnit, SourceStatus, UnitRow};

/// SHA-256 of zero bytes.
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

fn unit<'a>(source_id: i64, rel_path: &'a str) -> NewUnit<'a> {
    NewUnit {
        source_id,
        rel_path,
        mtime: Some(1_700_000_000),
        frontmatter_json: None,
        kind: "prose",
        size: Some(10),
        too_large: false,
        hash: None,
        inode: None,
        device: None,
    }
}

fn two_sources(cache: &Cache) -> (i64, i64) {
    (
        cache
            .register_source("notes", "/vault/notes", SourceStatus::Active)
            .unwrap(),
        cache
            .register_source("repo", "/dev/repo", SourceStatus::Active)
            .unwrap(),
    )
}

fn paths(rows: &[UnitRow]) -> Vec<(i64, &str)> {
    rows.iter()
        .map(|r| (r.source_id, r.rel_path.as_str()))
        .collect()
}

#[test]
fn hash_inode_and_device_round_trip_through_a_row() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, _) = two_sources(&cache);
    let id = cache
        .insert_unit(&NewUnit {
            hash: Some("a".repeat(64).as_str()),
            inode: Some(8_589_934_592),
            device: Some(16_777_231),
            ..unit(notes, "a.md")
        })
        .unwrap();

    let by_id = cache.get_unit(id).unwrap().unwrap();
    let by_path = cache.get_unit_by_rel_path(notes, "a.md").unwrap().unwrap();
    assert_eq!(by_id, by_path);
    assert_eq!(by_id.hash.as_deref(), Some("a".repeat(64).as_str()));
    assert_eq!(by_id.inode, Some(8_589_934_592));
    assert_eq!(by_id.device, Some(16_777_231));
    // The row that comes with the source's status carries them too.
    let with_status = cache.units_with_status_for_source("notes").unwrap();
    assert_eq!(with_status[0].unit, by_id);
    assert_eq!(with_status[0].source_status, SourceStatus::Active);
}

#[test]
fn a_row_without_them_reads_back_none() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, _) = two_sources(&cache);
    cache.insert_unit(&unit(notes, "a.md")).unwrap();
    let row = cache.get_unit_by_rel_path(notes, "a.md").unwrap().unwrap();
    assert_eq!((row.hash, row.inode, row.device), (None, None, None));
}

#[test]
fn hash_lookup_returns_every_row_with_that_hash_across_all_sources() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, repo) = two_sources(&cache);
    let h = "1".repeat(64);
    let other = "2".repeat(64);
    for (source, rel, hash) in [
        (notes, "b.md", &h),
        (notes, "a.md", &h),
        (repo, "docs/copy.md", &h),
        (repo, "x.md", &other),
    ] {
        cache
            .insert_unit(&NewUnit {
                hash: Some(hash.as_str()),
                ..unit(source, rel)
            })
            .unwrap();
    }
    cache.insert_unit(&unit(notes, "no-hash.md")).unwrap();

    let found = cache.units_by_hash(&h).unwrap();
    assert_eq!(
        paths(&found),
        vec![(notes, "a.md"), (notes, "b.md"), (repo, "docs/copy.md")],
        "both sources, ordered by source then path"
    );
    assert_eq!(
        paths(&cache.units_by_hash(&other).unwrap()),
        vec![(repo, "x.md")]
    );
    assert!(cache.units_by_hash(&"3".repeat(64)).unwrap().is_empty());
    assert!(
        cache.units_by_hash("").unwrap().is_empty(),
        "a NULL hash is never matched by any value"
    );
}

#[test]
fn identity_lookup_finds_the_row_by_device_and_inode_and_ignores_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, repo) = two_sources(&cache);
    cache
        .insert_unit(&NewUnit {
            inode: Some(42),
            device: Some(7),
            ..unit(notes, "old/name.md")
        })
        .unwrap();
    // Same inode on another volume, and another inode on the same one.
    cache
        .insert_unit(&NewUnit {
            inode: Some(42),
            device: Some(8),
            ..unit(repo, "other-volume.md")
        })
        .unwrap();
    cache
        .insert_unit(&NewUnit {
            inode: Some(43),
            device: Some(7),
            ..unit(notes, "other-inode.md")
        })
        .unwrap();

    let found = cache.units_by_identity(7, 42).unwrap();
    assert_eq!(paths(&found), vec![(notes, "old/name.md")]);
    assert!(cache.units_by_identity(9, 42).unwrap().is_empty());
    assert!(cache.units_by_identity(7, 44).unwrap().is_empty());
}

#[test]
fn hard_links_share_an_identity_and_the_lookup_returns_each_of_them() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, repo) = two_sources(&cache);
    for (source, rel) in [(notes, "a.md"), (repo, "link-of-a.md")] {
        cache
            .insert_unit(&NewUnit {
                inode: Some(5),
                device: Some(1),
                ..unit(source, rel)
            })
            .unwrap();
    }
    assert_eq!(
        paths(&cache.units_by_identity(1, 5).unwrap()),
        vec![(notes, "a.md"), (repo, "link-of-a.md")]
    );
}

#[test]
fn a_too_large_row_stores_identity_and_a_null_hash() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, _) = two_sources(&cache);
    cache
        .insert_unit(&NewUnit {
            too_large: true,
            size: Some(30_000_000),
            inode: Some(11),
            device: Some(2),
            ..unit(notes, "big.bin")
        })
        .unwrap();
    let row = cache
        .get_unit_by_rel_path(notes, "big.bin")
        .unwrap()
        .unwrap();
    assert!(row.too_large);
    assert_eq!(row.hash, None);
    assert_eq!((row.device, row.inode), (Some(2), Some(11)));
    assert_eq!(
        paths(&cache.units_by_identity(2, 11).unwrap()),
        vec![(notes, "big.bin")]
    );
}

#[test]
fn an_inode_above_i64_max_round_trips_unchanged_and_is_found_by_identity() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, _) = two_sources(&cache);
    for (rel, inode, device) in [
        ("max.md", u64::MAX, u64::MAX - 1),
        ("just-over.md", i64::MAX as u64 + 1, 3),
        ("at-i64-max.md", i64::MAX as u64, 3),
        ("zero.md", 0, 0),
    ] {
        cache
            .insert_unit(&NewUnit {
                inode: Some(inode),
                device: Some(device),
                ..unit(notes, rel)
            })
            .unwrap();
        let row = cache.get_unit_by_rel_path(notes, rel).unwrap().unwrap();
        assert_eq!(
            (row.inode, row.device),
            (Some(inode), Some(device)),
            "{rel}"
        );
        assert_eq!(
            paths(&cache.units_by_identity(device, inode).unwrap()),
            vec![(notes, rel)],
            "{rel}"
        );
    }
}

#[test]
fn the_hash_of_zero_bytes_is_stored_not_null_and_two_empty_files_share_it() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, repo) = two_sources(&cache);
    for (source, rel) in [(notes, "empty-a.md"), (repo, "empty-b.txt")] {
        cache
            .insert_unit(&NewUnit {
                size: Some(0),
                hash: Some(EMPTY_SHA256),
                ..unit(source, rel)
            })
            .unwrap();
    }
    let row = cache
        .get_unit_by_rel_path(notes, "empty-a.md")
        .unwrap()
        .unwrap();
    assert_eq!(row.hash.as_deref(), Some(EMPTY_SHA256));
    assert_eq!(
        paths(&cache.units_by_hash(EMPTY_SHA256).unwrap()),
        vec![(notes, "empty-a.md"), (repo, "empty-b.txt")]
    );
}

#[test]
fn deleting_a_unit_removes_it_from_both_lookups() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let (notes, _) = two_sources(&cache);
    let id = cache
        .insert_unit(&NewUnit {
            hash: Some("c".repeat(64).as_str()),
            inode: Some(1),
            device: Some(1),
            ..unit(notes, "a.md")
        })
        .unwrap();
    cache.delete_unit(id).unwrap();
    assert!(cache.units_by_hash(&"c".repeat(64)).unwrap().is_empty());
    assert!(cache.units_by_identity(1, 1).unwrap().is_empty());
}

#[test]
fn both_lookups_use_an_index() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let plan = |sql: &str| -> String {
        cache
            .connection()
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map([], |r| r.get::<_, String>(3))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join(" | ")
    };
    let by_hash = plan("SELECT id FROM units WHERE hash = 'x'");
    assert!(by_hash.contains("idx_units_hash"), "{by_hash}");
    let by_identity = plan("SELECT id FROM units WHERE device = 1 AND inode = 2");
    assert!(by_identity.contains("idx_units_identity"), "{by_identity}");
}
