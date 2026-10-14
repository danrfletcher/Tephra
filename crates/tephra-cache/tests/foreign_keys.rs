//! Inserting a `unit`/`link`/`tag`/`block` referencing a nonexistent
//! parent id is rejected by foreign-key constraints
//! (`PRAGMA foreign_keys = ON` verified).
//!
//! Per the schema (G5): `units.source_id` references `sources.id`
//! directly; `links.from_unit_id`/`to_unit_id`, `tags.unit_id`, and
//! `blocks.unit_id` reference `units.id` (and so are only indirectly,
//! transitively tied back to `sources` — `links`/`tags`/`blocks` have
//! no `source_id` column of their own in this schema). This file
//! exercises every one of those foreign keys.

mod common;

use tephra_cache::{NewBlock, NewLink, NewTag, NewUnit, SourceStatus};

#[test]
fn foreign_keys_pragma_is_verified_on_first() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    assert!(cache.foreign_keys_enabled().unwrap());
}

#[test]
fn inserting_a_unit_with_nonexistent_source_id_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let result = cache.insert_unit(&NewUnit {
        source_id: 999_999,
        rel_path: "ghost.md",
        mtime: None,
        frontmatter_json: None,
    });
    assert!(
        result.is_err(),
        "inserting a unit against a nonexistent source_id must fail"
    );
    assert_eq!(cache.table_row_count("units").unwrap(), 0);
}

#[test]
fn inserting_a_link_with_nonexistent_from_unit_id_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let result = cache.insert_link(&NewLink {
        from_unit_id: 999_999,
        to_raw: "[[Ghost]]",
        to_unit_id: None,
        kind: "wikilink",
    });
    assert!(
        result.is_err(),
        "inserting a link against a nonexistent from_unit_id must fail"
    );
    assert_eq!(cache.table_row_count("links").unwrap(), 0);
}

#[test]
fn inserting_a_link_with_nonexistent_to_unit_id_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    let seeded =
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let real_unit_id = seeded.unit_ids[0];

    let result = cache.insert_link(&NewLink {
        from_unit_id: real_unit_id,
        to_raw: "[[Ghost]]",
        to_unit_id: Some(999_999),
        kind: "wikilink",
    });
    assert!(
        result.is_err(),
        "a non-null to_unit_id pointing at a nonexistent unit must fail"
    );
}

#[test]
fn inserting_a_tag_with_nonexistent_unit_id_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let result = cache.insert_tag(&NewTag {
        unit_id: 999_999,
        tag: "ghost",
    });
    assert!(
        result.is_err(),
        "inserting a tag against a nonexistent unit_id must fail"
    );
    assert_eq!(cache.table_row_count("tags").unwrap(), 0);
}

#[test]
fn inserting_a_block_with_nonexistent_unit_id_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);

    let result = cache.insert_block(&NewBlock {
        unit_id: 999_999,
        block_id: "ghost",
        line: 1,
    });
    assert!(
        result.is_err(),
        "inserting a block against a nonexistent unit_id must fail"
    );
    assert_eq!(cache.table_row_count("blocks").unwrap(), 0);
}

#[test]
fn valid_rows_against_real_parents_insert_cleanly() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = common::open_temp_cache(&dir);
    let seeded =
        common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);

    // seed_source_with_units already inserted 2 units, 2 links, 2
    // tags, 2 blocks successfully — if the FK chain were broken this
    // helper itself would have panicked already. Add one more of each
    // explicitly here to double check the happy path directly in this
    // file too.
    let unit_id = seeded.unit_ids[0];
    assert!(cache
        .insert_link(&NewLink {
            from_unit_id: unit_id,
            to_raw: "[[Real]]",
            to_unit_id: Some(unit_id),
            kind: "wikilink",
        })
        .is_ok());
    assert!(cache
        .insert_tag(&NewTag {
            unit_id,
            tag: "real",
        })
        .is_ok());
    assert!(cache
        .insert_block(&NewBlock {
            unit_id,
            block_id: "real-block",
            line: 5,
        })
        .is_ok());
}
