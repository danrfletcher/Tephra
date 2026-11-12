//! `delete_unit`/`delete_units_for_source`/`delete_source`: the
//! cascading-delete helpers PR-4's registry and watcher pipeline rely
//! on (an unregister, a rescan's scoped clear, and a watcher's
//! delete-then-reinsert on a file edit).

mod common;

use common::{open_temp_cache, seed_source_with_units};
use tephra_cache::SourceStatus;

#[test]
fn delete_unit_removes_the_row_and_its_links_tags_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let unit_id = seeded.unit_ids[0];

    cache.delete_unit(unit_id).expect("delete_unit");

    assert!(cache.get_unit(unit_id).expect("get_unit").is_none());

    // The other unit (and its own links/tags/blocks) survive untouched.
    let other_unit_id = seeded.unit_ids[1];
    assert!(cache.get_unit(other_unit_id).expect("get_unit").is_some());

    let conn = cache.connection();
    let links: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap();
    let tags: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM tags WHERE unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap();
    let blocks: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM blocks WHERE unit_id = ?1",
            [unit_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(links, 0);
    assert_eq!(tags, 0);
    assert_eq!(blocks, 0);
}

#[test]
fn delete_unit_is_a_no_op_for_a_nonexistent_id() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    // No seed data at all: id 999 never existed.
    cache
        .delete_unit(999)
        .expect("delete_unit on missing id must not error");
}

#[test]
fn get_unit_by_rel_path_finds_the_right_row() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);

    let found = cache
        .get_unit_by_rel_path(seeded.source_id, "one.md")
        .expect("get_unit_by_rel_path")
        .expect("row should exist");
    assert_eq!(found.id, seeded.unit_ids[0]);
    assert_eq!(found.rel_path, "one.md");

    let missing = cache
        .get_unit_by_rel_path(seeded.source_id, "nonexistent.md")
        .expect("get_unit_by_rel_path");
    assert!(missing.is_none());
}

#[test]
fn delete_units_for_source_clears_only_that_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let notes = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let repo = seed_source_with_units(&cache, "repo", "/code/repo", SourceStatus::Active);

    cache
        .delete_units_for_source(notes.source_id)
        .expect("delete_units_for_source");

    for unit_id in &notes.unit_ids {
        assert!(cache.get_unit(*unit_id).expect("get_unit").is_none());
    }
    // The other source's rows are completely untouched (GP4
    // independence).
    for unit_id in &repo.unit_ids {
        assert!(cache.get_unit(*unit_id).expect("get_unit").is_some());
    }
    // Both `sources` rows themselves survive — only the core-table
    // rows for `notes` were cleared.
    assert!(cache.get_source("notes").unwrap().is_some());
    assert!(cache.get_source("repo").unwrap().is_some());
}

#[test]
fn delete_source_removes_the_source_row_and_cascades() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let notes = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    let repo = seed_source_with_units(&cache, "repo", "/code/repo", SourceStatus::Active);

    cache.delete_source("notes").expect("delete_source");

    assert!(cache.get_source("notes").unwrap().is_none());
    for unit_id in &notes.unit_ids {
        assert!(cache.get_unit(*unit_id).expect("get_unit").is_none());
    }
    // `repo` is entirely unaffected.
    assert!(cache.get_source("repo").unwrap().is_some());
    for unit_id in &repo.unit_ids {
        assert!(cache.get_unit(*unit_id).expect("get_unit").is_some());
    }
}

#[test]
fn delete_units_under_prefix_clears_only_the_nested_rel_paths() {
    use tephra_cache::{NewLink, NewUnit};

    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source_id = cache
        .register_source("notes", "/vault/notes", SourceStatus::Active)
        .expect("register_source");

    // `dir/a.md` and `dir/sub/b.md` are both nested under `dir`;
    // `dir2/c.md` only shares a string prefix (not a real path
    // component boundary) and must survive; `top.md` is unrelated.
    let dir_a = cache
        .insert_unit(&NewUnit {
            kind: "prose",
            size: None,
            too_large: false,
            source_id,
            rel_path: "dir/a.md",
            mtime: None,
            frontmatter_json: None,
        })
        .expect("insert dir/a.md");
    cache
        .insert_link(&NewLink {
            from_unit_id: dir_a,
            to_raw: "[[Other]]",
            to_unit_id: None,
            kind: "wikilink",
        })
        .expect("insert link for dir/a.md");
    let dir_sub_b = cache
        .insert_unit(&NewUnit {
            kind: "prose",
            size: None,
            too_large: false,
            source_id,
            rel_path: "dir/sub/b.md",
            mtime: None,
            frontmatter_json: None,
        })
        .expect("insert dir/sub/b.md");
    let dir2_c = cache
        .insert_unit(&NewUnit {
            kind: "prose",
            size: None,
            too_large: false,
            source_id,
            rel_path: "dir2/c.md",
            mtime: None,
            frontmatter_json: None,
        })
        .expect("insert dir2/c.md");
    let top = cache
        .insert_unit(&NewUnit {
            kind: "prose",
            size: None,
            too_large: false,
            source_id,
            rel_path: "top.md",
            mtime: None,
            frontmatter_json: None,
        })
        .expect("insert top.md");

    let deleted = cache
        .delete_units_under_prefix(source_id, "dir")
        .expect("delete_units_under_prefix");
    assert_eq!(
        deleted, 2,
        "only dir/a.md and dir/sub/b.md are nested under dir"
    );

    assert!(cache.get_unit(dir_a).expect("get_unit").is_none());
    assert!(cache.get_unit(dir_sub_b).expect("get_unit").is_none());
    assert!(
        cache.get_unit(dir2_c).expect("get_unit").is_some(),
        "dir2/c.md only shares a string prefix with \"dir\", not a path component boundary"
    );
    assert!(cache.get_unit(top).expect("get_unit").is_some());

    // dir/a.md's link row was cascaded away too.
    let conn = cache.connection();
    let links: i64 = conn
        .query_row("SELECT COUNT(*) FROM links", [], |r| r.get(0))
        .unwrap();
    assert_eq!(links, 0);
}

#[test]
fn delete_units_under_prefix_does_not_match_on_underscore_percent_or_case() {
    // R10: `_`/`%` must never be treated as SQL `LIKE` wildcards here,
    // and the comparison must be case-sensitive — a prefix match is a
    // real path-component boundary, not a pattern.
    use tephra_cache::NewUnit;

    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let source_id = cache
        .register_source("notes", "/vault/notes", SourceStatus::Active)
        .expect("register_source");

    let insert = |rel_path: &str| {
        cache
            .insert_unit(&NewUnit {
                kind: "prose",
                size: None,
                too_large: false,
                source_id,
                rel_path,
                mtime: None,
                frontmatter_json: None,
            })
            .unwrap_or_else(|_| panic!("insert {rel_path}"))
    };

    // Nested under "a_b": must be deleted.
    let a_b_x = insert("a_b/x.md");
    // Nested under "a%": must be deleted.
    let a_pct_z = insert("a%/z.md");
    // A naive `LIKE 'a_b/%'` would also match "acb/y.md" (`_` as a
    // one-char wildcard); a naive `LIKE 'a%/%'` would match almost
    // anything. Neither must be touched by deleting "a_b" or "a%".
    let acb_y = insert("acb/y.md");
    // A naive `LIKE 'a_b/%'` or `LIKE 'a%/%'` would also match this
    // via the single-character wildcard semantics of `_`.
    let a_qq_q = insert("aQQ/q.md");
    // A case-differing sibling: default SQLite `LIKE` is
    // case-insensitive for ASCII, so a naive prefix match on "docs"
    // would also delete "Docs/x.md".
    let docs_x = insert("docs/x.md");
    let big_docs_x = insert("Docs/x.md");

    let deleted_ab = cache
        .delete_units_under_prefix(source_id, "a_b")
        .expect("delete_units_under_prefix a_b");
    assert_eq!(deleted_ab, 1, "only a_b/x.md is nested under \"a_b\"");
    assert!(cache.get_unit(a_b_x).expect("get_unit").is_none());
    assert!(
        cache.get_unit(acb_y).expect("get_unit").is_some(),
        "acb/y.md must survive deleting \"a_b\" -- \"_\" is not a wildcard here"
    );

    let deleted_apct = cache
        .delete_units_under_prefix(source_id, "a%")
        .expect("delete_units_under_prefix a%");
    assert_eq!(deleted_apct, 1, "only a%/z.md is nested under \"a%\"");
    assert!(cache.get_unit(a_pct_z).expect("get_unit").is_none());
    assert!(
        cache.get_unit(a_qq_q).expect("get_unit").is_some(),
        "aQQ/q.md must survive deleting \"a%\" -- \"%\" is not a wildcard here"
    );

    let deleted_docs = cache
        .delete_units_under_prefix(source_id, "docs")
        .expect("delete_units_under_prefix docs");
    assert_eq!(
        deleted_docs, 1,
        "only docs/x.md matches the lowercase prefix"
    );
    assert!(cache.get_unit(docs_x).expect("get_unit").is_none());
    assert!(
        cache.get_unit(big_docs_x).expect("get_unit").is_some(),
        "Docs/x.md (different case) must survive deleting \"docs\""
    );
}

#[test]
fn delete_source_is_idempotent_on_an_already_removed_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);

    cache.delete_source("notes").expect("first delete_source");
    // Calling it again on an already-removed source must not error.
    cache
        .delete_source("notes")
        .expect("second delete_source must be a no-op, not an error");
    // Never-registered name: also not an error.
    cache
        .delete_source("never-existed")
        .expect("delete_source on an unknown name must not error");
}

#[test]
fn delete_unit_removes_the_units_headings_too() {
    // G3: prose files of every format write `headings` rows; deleting
    // the unit (the watcher's delete path) must take them with it.
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    for unit_id in &seeded.unit_ids {
        cache
            .insert_heading(&tephra_cache::NewHeading {
                unit_id: *unit_id,
                level: 2,
                text: "Heading",
                line: 3,
            })
            .expect("insert_heading");
    }

    cache.delete_unit(seeded.unit_ids[0]).expect("delete_unit");

    assert!(cache
        .headings_for_unit(seeded.unit_ids[0])
        .unwrap()
        .is_empty());
    assert_eq!(
        cache.headings_for_unit(seeded.unit_ids[1]).unwrap().len(),
        1,
        "the other unit's heading survives"
    );
}

#[test]
fn delete_unit_removes_the_units_symbols_and_the_blocks_that_name_them() {
    let dir = tempfile::tempdir().unwrap();
    let cache = open_temp_cache(&dir);
    let seeded = seed_source_with_units(&cache, "repo", "/repo", SourceStatus::Active);
    let unit_id = seeded.unit_ids[0];
    let other = seeded.unit_ids[1];
    for unit in [unit_id, other] {
        let symbol_id = cache
            .insert_symbol(&tephra_cache::NewSymbol {
                unit_id: unit,
                kind: "function",
                name: "f",
                parent: None,
                line: 2,
                start_line: 1,
                end_line: 3,
            })
            .unwrap();
        cache
            .insert_block(&tephra_cache::NewBlock {
                unit_id: unit,
                block_id: Some("k3f9a2"),
                line: 2,
                end_line: Some(3),
                symbol_id: Some(symbol_id),
            })
            .unwrap();
    }

    cache.delete_unit(unit_id).expect("delete_unit");

    let count = |table: &str, unit: i64| -> i64 {
        cache
            .connection()
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE unit_id = ?1"),
                [unit],
                |r| r.get(0),
            )
            .unwrap()
    };
    assert_eq!(count("symbols", unit_id), 0, "no orphaned symbol rows");
    assert_eq!(count("blocks", unit_id), 0);
    assert_eq!(count("symbols", other), 1);
    assert_eq!(count("blocks", other), 2, "seeded block plus the code one");
}
