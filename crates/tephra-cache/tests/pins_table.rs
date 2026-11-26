//! The `remote_pins` rows (atlas-27 PR-12): adding, removing, what a pin
//! covers, how a pin goes with its file, and that pins survive a schema
//! rebuild byte for byte. Real SQLite in a tempdir.

mod common;

use tephra_cache::{pin_covers, Cache, FaultInjector, NewRemoteItem, PinWrite, SourceStatus};

fn cache_with_source(dir: &tempfile::TempDir, name: &str) -> Cache {
    let cache = common::open_temp_cache(dir);
    cache
        .connection()
        .execute(
            "INSERT INTO remote_sources (name, kind, target, created_at) \
             VALUES (?1, 'drive', 'Notes', 1)",
            [name],
        )
        .unwrap();
    cache
}

fn add_item(cache: &Cache, source: &str, path: &str, state: &str) {
    cache
        .upsert_remote_item(
            source,
            &NewRemoteItem {
                path,
                kind: "prose",
                sha: "",
                size: Some(10),
                file_id: Some(path),
                revision: Some("r1"),
            },
        )
        .unwrap();
    cache
        .connection()
        .execute(
            "UPDATE remote_items SET state = ?3 WHERE source = ?1 AND path = ?2",
            rusqlite::params![source, path, state],
        )
        .unwrap();
}

/// (pin path, is folder, file path, covered)
const COVERS: &[(&str, bool, &str, bool)] = &[
    // A file pin covers that file only.
    ("a.md", false, "a.md", true),
    ("a.md", false, "a.md.bak", false),
    ("a.md", false, "x/a.md", false),
    // A folder pin covers everything below it, at any depth.
    ("docs", true, "docs/a.md", true),
    ("docs", true, "docs/deep/er/a.md", true),
    // A file that has the folder's own name is covered too (the pin names
    // it exactly); a sibling that only shares the prefix is not (the trap).
    ("docs", true, "docs", true),
    ("docs", true, "docs-old/a.md", false),
    ("docs", true, "docsx", false),
    ("docs", true, "other/docs/a.md", false),
    // A trailing slash is not stored; a pin with one covers nothing under
    // a double slash path, and the planner never writes one.
    ("docs/", true, "docs/a.md", false),
    // The empty folder covers the whole source.
    ("", true, "a.md", true),
    ("", true, "docs/a.md", true),
    ("", false, "a.md", false),
    // Unicode and parentheses (a Drive duplicate name).
    ("Café", true, "Café/menu.md", true),
    ("Café", true, "Cafe/menu.md", false),
    ("Trip Plans", true, "Trip Plans/Ideas (2).md", true),
    (
        "Trip Plans/Ideas (2).md",
        false,
        "Trip Plans/Ideas (2).md",
        true,
    ),
    (
        "Trip Plans/Ideas (2).md",
        false,
        "Trip Plans/Ideas.md",
        false,
    ),
    ("日本", true, "日本/メモ.md", true),
    ("日本", true, "日本語/メモ.md", false),
    // Case matters.
    ("Docs", true, "docs/a.md", false),
];

#[test]
fn covers_in_rust_and_in_sql_agree_on_every_case() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_source(&dir, "d");
    for (pin, is_folder, path, covered) in COVERS {
        assert_eq!(
            pin_covers(pin, *is_folder, path),
            *covered,
            "pin_covers({pin:?}, folder={is_folder}, {path:?})"
        );
        cache
            .connection()
            .execute("DELETE FROM remote_pins", [])
            .unwrap();
        cache
            .connection()
            .execute(
                "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('d', ?1, ?2, 1)",
                rusqlite::params![pin, is_folder],
            )
            .unwrap();
        assert_eq!(
            cache.remote_pin_covers("d", path).unwrap(),
            *covered,
            "SQL cover of {path:?} by {pin:?} (folder={is_folder})"
        );
        // Another source's pin never covers it.
        assert!(!cache.remote_pin_covers("other", path).unwrap());
    }
}

#[test]
fn a_pin_is_added_once_and_removed_once() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_source(&dir, "d");
    assert_eq!(
        cache.add_remote_pin("d", "Trip Plans", true, 5).unwrap(),
        PinWrite::New
    );
    assert_eq!(
        cache.add_remote_pin("d", "Trip Plans", true, 6).unwrap(),
        PinWrite::Existing
    );
    // The same name pinned as the other kind is reported, not written.
    assert_eq!(
        cache.add_remote_pin("d", "Trip Plans", false, 7).unwrap(),
        PinWrite::OtherKind
    );
    let pins = cache.remote_pins("d").unwrap();
    assert_eq!(pins.len(), 1);
    assert!(pins[0].is_folder);
    assert_eq!(pins[0].created_at, 5, "the first pin's row is untouched");

    // Removing as the wrong kind changes nothing.
    assert!(!cache.remove_remote_pin("d", "Trip Plans", false).unwrap());
    assert!(cache.remove_remote_pin("d", "Trip Plans", true).unwrap());
    assert!(!cache.remove_remote_pin("d", "Trip Plans", true).unwrap());
    assert!(cache.remote_pins("d").unwrap().is_empty());
}

#[test]
fn a_folder_pin_covers_a_file_added_later_with_no_new_row() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_source(&dir, "d");
    cache.add_remote_pin("d", "Trip", true, 1).unwrap();
    add_item(&cache, "d", "Trip/a.md", "remote");
    assert_eq!(cache.remote_pin_downloads("d").unwrap().len(), 1);
    // A file added under the folder later:
    add_item(&cache, "d", "Trip/sub/b.md", "remote");
    add_item(&cache, "d", "Trip-old/c.md", "remote");
    let paths: Vec<String> = cache
        .remote_pin_downloads("d")
        .unwrap()
        .into_iter()
        .map(|r| r.path)
        .collect();
    assert_eq!(paths, ["Trip/a.md", "Trip/sub/b.md"]);
    assert_eq!(cache.remote_pins("d").unwrap().len(), 1);
}

#[test]
fn a_file_pin_goes_with_its_file_and_a_folder_pin_stays() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_source(&dir, "d");
    add_item(&cache, "d", "Trip/a.md", "cached");
    add_item(&cache, "d", "Trip/b.md", "cached");
    cache.add_remote_pin("d", "Trip", true, 1).unwrap();
    cache.add_remote_pin("d", "Trip/a.md", false, 1).unwrap();

    cache.delete_remote_item("d", "Trip/a.md").unwrap();
    let pins = cache.remote_pins("d").unwrap();
    assert_eq!(pins.len(), 1, "the file's pin went, the folder's stayed");
    assert_eq!(pins[0].path, "Trip");
    // The other file is still covered.
    assert!(cache.remote_pin_covers("d", "Trip/b.md").unwrap());
}

#[test]
fn pinning_and_unpinning_move_a_copy_between_cached_and_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_source(&dir, "d");
    add_item(&cache, "d", "Trip/a.md", "cached");
    add_item(&cache, "d", "Trip/b.md", "pending_upload");
    add_item(&cache, "d", "Trip/c.md", "remote");
    add_item(&cache, "d", "Other/d.md", "cached");
    let state = |path: &str| {
        cache
            .remote_items("d")
            .unwrap()
            .into_iter()
            .find(|r| r.path == path)
            .unwrap()
            .state
    };

    cache.add_remote_pin("d", "Trip", true, 1).unwrap();
    assert_eq!(cache.reconcile_remote_pins("d").unwrap(), 1);
    assert_eq!(state("Trip/a.md"), "pinned");
    assert_eq!(
        state("Trip/b.md"),
        "pending_upload",
        "edits keep their state"
    );
    assert_eq!(state("Trip/c.md"), "remote", "no copy, nothing to mark");
    assert_eq!(state("Other/d.md"), "cached");

    // A file pin inside the folder, then the folder is unpinned: the file
    // stays pinned, the rest go back to cached.
    add_item(&cache, "d", "Trip/e.md", "cached");
    cache.add_remote_pin("d", "Trip/a.md", false, 1).unwrap();
    cache.reconcile_remote_pins("d").unwrap();
    assert_eq!(state("Trip/e.md"), "pinned");
    assert!(cache.remove_remote_pin("d", "Trip", true).unwrap());
    cache.reconcile_remote_pins("d").unwrap();
    assert_eq!(state("Trip/a.md"), "pinned");
    assert_eq!(state("Trip/e.md"), "cached");
    assert!(cache.remove_remote_pin("d", "Trip/a.md", false).unwrap());
    cache.reconcile_remote_pins("d").unwrap();
    assert_eq!(state("Trip/a.md"), "cached");
    // Nothing left to change.
    assert_eq!(cache.reconcile_remote_pins("d").unwrap(), 0);
}

#[test]
fn a_download_that_finishes_under_a_pin_ends_pinned() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_source(&dir, "d");
    add_item(&cache, "d", "Trip/a.md", "downloading");
    add_item(&cache, "d", "Other/b.md", "downloading");
    cache.add_remote_pin("d", "Trip", true, 1).unwrap();
    let size = Some(10);
    assert!(cache
        .finish_remote_download("d", "Trip/a.md", None, size, 9)
        .unwrap());
    assert!(cache
        .finish_remote_download("d", "Other/b.md", None, size, 9)
        .unwrap());
    let states: Vec<(String, String)> = cache
        .remote_items("d")
        .unwrap()
        .into_iter()
        .map(|r| (r.path, r.state))
        .collect();
    assert_eq!(
        states,
        [
            ("Other/b.md".to_string(), "cached".to_string()),
            ("Trip/a.md".to_string(), "pinned".to_string())
        ]
    );
}

#[test]
fn the_footprint_counts_covered_files_in_every_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_source(&dir, "d");
    cache
        .connection()
        .execute(
            "INSERT INTO remote_sources (name, kind, target, created_at) VALUES ('e', 'drive', 'E', 1)",
            [],
        )
        .unwrap();
    add_item(&cache, "d", "Trip/a.md", "remote");
    add_item(&cache, "d", "Trip/b.md", "pinned");
    add_item(&cache, "d", "Other/c.md", "cached");
    add_item(&cache, "e", "x.md", "remote");
    assert_eq!(cache.remote_pin_footprint().unwrap(), 0);
    cache.add_remote_pin("d", "Trip", true, 1).unwrap();
    cache.add_remote_pin("e", "x.md", false, 1).unwrap();
    assert_eq!(cache.remote_pin_footprint().unwrap(), 30);
    assert_eq!(
        cache.remote_pinned_sources().unwrap(),
        ["d".to_string(), "e".to_string()]
    );
}

#[test]
fn pins_survive_a_schema_rebuild_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = cache_with_source(&dir, "gdrive");
    common::seed_source_with_units(&cache, "notes", "/vault/notes", SourceStatus::Active);
    cache
        .add_remote_pin("gdrive", "Trip Plans", true, 11)
        .unwrap();
    cache
        .add_remote_pin("gdrive", "Café/menu (2).md", false, 12)
        .unwrap();
    let before = common::dump_table(cache.connection(), "remote_pins");
    assert_eq!(before.len(), 2);

    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    assert_eq!(cache.table_row_count("units").unwrap(), 0);
    assert_eq!(
        common::dump_table(cache.connection(), "remote_pins"),
        before
    );
    assert_eq!(cache.remote_pins("gdrive").unwrap().len(), 2);
}

#[test]
fn moved_pins_keep_id_kind_and_age_swap_cleanly_and_come_back_after_a_delete() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_source(&dir, "gdrive");
    cache.add_remote_pin("gdrive", "a.md", false, 5).unwrap();
    cache.add_remote_pin("gdrive", "b.md", false, 6).unwrap();
    cache.add_remote_pin("gdrive", "Trip", true, 7).unwrap();
    cache.add_remote_pin("gdrive", "keep.md", false, 8).unwrap();
    let before = cache.remote_pins("gdrive").unwrap();
    let by = |p: &str| before.iter().find(|r| r.path == p).unwrap().clone();
    // A sync that moved b.md deleted its pin first.
    cache.delete_remote_item("gdrive", "b.md").unwrap();
    assert_eq!(cache.remote_pins("gdrive").unwrap().len(), 3);

    cache
        .move_remote_pins(
            "gdrive",
            &[
                (by("a.md"), "b.md".to_string()),
                (by("b.md"), "a.md".to_string()),
                (by("Trip"), "Trips".to_string()),
            ],
        )
        .unwrap();

    let after = cache.remote_pins("gdrive").unwrap();
    let shape: Vec<_> = after
        .iter()
        .map(|p| (p.path.as_str(), p.is_folder, p.created_at, p.id))
        .collect();
    assert_eq!(
        shape,
        [
            ("Trips", true, 7, by("Trip").id),
            ("a.md", false, 6, by("b.md").id),
            ("b.md", false, 5, by("a.md").id),
            ("keep.md", false, 8, by("keep.md").id),
        ]
    );
}
