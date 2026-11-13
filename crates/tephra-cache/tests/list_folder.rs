//! `Cache::list_folder` (`unit.list`, G1) against a temp database: the
//! cache-only listing an offline source is served from, and the file
//! entries an online one is served.

mod common;

use tephra_cache::{
    Cache, CacheError, FaultInjector, FolderEntry, FolderState, NewUnit, SourceStatus, FOLDER_KIND,
};

struct Seed<'a> {
    rel_path: &'a str,
    kind: &'a str,
    size: Option<i64>,
    mtime: Option<i64>,
    frontmatter: Option<&'a str>,
}

fn file<'a>(rel_path: &'a str, kind: &'a str, size: i64) -> Seed<'a> {
    Seed {
        rel_path,
        kind,
        size: Some(size),
        mtime: Some(1_700_000_000 + size),
        frontmatter: None,
    }
}

fn seed(cache: &Cache, name: &str, status: SourceStatus, units: &[Seed]) -> Vec<i64> {
    let source_id = cache
        .register_source(name, &format!("/vault/{name}"), status)
        .expect("register_source");
    units
        .iter()
        .map(|u| {
            cache
                .insert_unit(&NewUnit {
                    source_id,
                    rel_path: u.rel_path,
                    mtime: u.mtime,
                    frontmatter_json: u.frontmatter,
                    kind: u.kind,
                    size: u.size,
                    too_large: false,
                })
                .expect("insert_unit")
        })
        .collect()
}

fn names(entries: &[FolderEntry]) -> Vec<&str> {
    entries.iter().map(|e| e.rel_path.as_str()).collect()
}

fn tree_units() -> Vec<Seed<'static>> {
    vec![
        file("README.md", "prose", 10),
        file("Cargo.toml", "code", 20),
        file("src/main.rs", "code", 30),
        file("src/lib.rs", "code", 40),
        file("src/rpc/server.rs", "code", 50),
        file("src/rpc/dispatch.rs", "code", 60),
        file("docs/guide.md", "prose", 70),
        file("Attachments/a.pdf", "document", 80),
        file("Attachments/logo.png", "binary", 90),
    ]
}

#[test]
fn direct_children_of_the_root_are_folders_first_then_files_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "tephra", SourceStatus::Active, &tree_units());

    let listing = cache.list_folder("tephra", "", false).unwrap();

    assert_eq!(listing.state, FolderState::Folder);
    assert_eq!(listing.source_status, SourceStatus::Active);
    assert!(!listing.needs_rescan);
    assert_eq!(
        names(&listing.entries),
        ["Attachments", "docs", "src", "Cargo.toml", "README.md"]
    );
    let kinds: Vec<&str> = listing.entries.iter().map(|e| e.kind.as_str()).collect();
    assert_eq!(kinds, ["folder", "folder", "folder", "code", "prose"]);
    // A file's name is its last component and its mtime/size are the row's.
    let cargo = &listing.entries[3];
    assert_eq!(cargo.name, "Cargo.toml");
    assert_eq!(cargo.size, Some(20));
    assert_eq!(cargo.mtime, Some(1_700_000_020));
}

#[test]
fn a_folder_is_derived_from_prefixes_and_sized_by_its_distinct_direct_children() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "tephra", SourceStatus::Active, &tree_units());

    let listing = cache.list_folder("tephra", "", false).unwrap();
    let size_of = |name: &str| {
        listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap()
            .size
    };
    // src holds main.rs, lib.rs and the rpc/ folder: 3 distinct children
    // (rpc counts once though two files sit under it).
    assert_eq!(size_of("src"), Some(3));
    assert_eq!(size_of("docs"), Some(1));
    assert_eq!(size_of("Attachments"), Some(2));
    let src = listing.entries.iter().find(|e| e.name == "src").unwrap();
    assert_eq!(src.kind, FOLDER_KIND);
    assert_eq!(src.mtime, None, "the cache has no folder mtimes");
}

#[test]
fn listing_a_subfolder_lists_only_its_direct_children() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "tephra", SourceStatus::Active, &tree_units());

    let listing = cache.list_folder("tephra", "src", false).unwrap();

    assert_eq!(
        names(&listing.entries),
        ["src/rpc", "src/lib.rs", "src/main.rs"]
    );
    assert_eq!(listing.entries[0].size, Some(2));
    assert_eq!(listing.entries[0].name, "rpc");
}

#[test]
fn recursive_listing_is_depth_first_with_each_folder_before_its_contents() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "tephra", SourceStatus::Active, &tree_units());

    let listing = cache.list_folder("tephra", "", true).unwrap();

    assert_eq!(
        names(&listing.entries),
        [
            "Attachments",
            "Attachments/a.pdf",
            "Attachments/logo.png",
            "docs",
            "docs/guide.md",
            "src",
            "src/rpc",
            "src/rpc/dispatch.rs",
            "src/rpc/server.rs",
            "src/lib.rs",
            "src/main.rs",
            "Cargo.toml",
            "README.md",
        ]
    );
}

#[test]
fn recursive_listing_of_a_subfolder_stays_inside_it() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "tephra", SourceStatus::Active, &tree_units());
    // A sibling whose name merely starts with "src" must not leak in.
    seed(
        &cache,
        "other",
        SourceStatus::Active,
        &[file("src/x.rs", "code", 1)],
    );
    let id = cache.get_source("tephra").unwrap().unwrap().id;
    cache
        .insert_unit(&NewUnit {
            source_id: id,
            rel_path: "src2/y.rs",
            mtime: None,
            frontmatter_json: None,
            kind: "code",
            size: Some(1),
            too_large: false,
        })
        .unwrap();
    cache
        .insert_unit(&NewUnit {
            source_id: id,
            rel_path: "src.md",
            mtime: None,
            frontmatter_json: None,
            kind: "prose",
            size: Some(1),
            too_large: false,
        })
        .unwrap();

    let listing = cache.list_folder("tephra", "src", true).unwrap();

    assert_eq!(
        names(&listing.entries),
        [
            "src/rpc",
            "src/rpc/dispatch.rs",
            "src/rpc/server.rs",
            "src/lib.rs",
            "src/main.rs",
        ]
    );
}

#[test]
fn sorting_is_case_insensitive_then_bytewise_and_stable_across_calls() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(
        &cache,
        "notes",
        SourceStatus::Active,
        &[
            file("b.md", "prose", 1),
            file("B.md", "prose", 2),
            file("a.md", "prose", 3),
            file("Zed.md", "prose", 4),
            file("zed.txt", "prose", 5),
            file("é.md", "prose", 6),
            file("É.md", "prose", 11),
            file("E.md", "prose", 7),
            file("Dir/x.md", "prose", 8),
            file("dir2/x.md", "prose", 9),
            file("_pool/x.md", "prose", 10),
        ],
    );

    let first = cache.list_folder("notes", "", false).unwrap();
    let second = cache.list_folder("notes", "", false).unwrap();

    assert_eq!(first, second);
    assert_eq!(
        names(&first.entries),
        // Folders first (case-insensitive: "_pool" < "dir" < "dir2"),
        // then files: "B.md" before "b.md" on the byte tie-break, and
        // non-ASCII letters after ASCII ones (Unicode lowercase only, no
        // accent folding), "É" before "é" on the tie-break.
        [
            "_pool", "Dir", "dir2", "a.md", "B.md", "b.md", "E.md", "Zed.md", "zed.txt", "É.md",
            "é.md"
        ]
    );
}

#[test]
fn a_file_and_a_folder_with_a_lookalike_name_are_never_merged() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    // NFC "é" and NFD "e" + U+0301 are different byte strings.
    let nfc = "caf\u{e9} notes.md";
    let nfd = "cafe\u{301} notes.md";
    seed(
        &cache,
        "notes",
        SourceStatus::Active,
        &[
            file(&format!("unicode/{nfc}"), "prose", 1),
            file(&format!("unicode/{nfd}"), "prose", 2),
        ],
    );

    let listing = cache.list_folder("notes", "unicode", false).unwrap();

    assert_eq!(listing.entries.len(), 2);
    let listed: Vec<&str> = listing.entries.iter().map(|e| e.name.as_str()).collect();
    assert!(listed.contains(&nfc));
    assert!(listed.contains(&nfd));
}

#[test]
fn names_with_like_wildcards_do_not_match_other_folders() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(
        &cache,
        "notes",
        SourceStatus::Active,
        &[
            file("100%/a.md", "prose", 1),
            file("1000/b.md", "prose", 2),
            file("a_b/c.md", "prose", 3),
            file("axb/d.md", "prose", 4),
        ],
    );

    let percent = cache.list_folder("notes", "100%", true).unwrap();
    assert_eq!(names(&percent.entries), ["100%/a.md"]);
    let underscore = cache.list_folder("notes", "a_b", true).unwrap();
    assert_eq!(names(&underscore.entries), ["a_b/c.md"]);
}

#[test]
fn prose_title_is_frontmatter_title_then_first_heading_and_only_for_prose() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let ids = seed(
        &cache,
        "notes",
        SourceStatus::Active,
        &[
            Seed {
                frontmatter: Some(r#"{"title":"From Frontmatter"}"#),
                ..file("a.md", "prose", 1)
            },
            file("b.md", "prose", 2),
            file("c.md", "prose", 3),
            Seed {
                frontmatter: Some(r#"{"title":42}"#),
                ..file("d.md", "prose", 4)
            },
            Seed {
                frontmatter: Some("not json at all {"),
                ..file("e.md", "prose", 5)
            },
            Seed {
                frontmatter: Some(r#"{"title":"   "}"#),
                ..file("f.md", "prose", 6)
            },
            Seed {
                frontmatter: Some(r#"{"title":"Code title"}"#),
                ..file("g.rs", "code", 7)
            },
        ],
    );
    let heading = |unit: i64, text: &str, line: i64| {
        cache
            .connection()
            .execute(
                "INSERT INTO headings (unit_id, level, text, line) VALUES (?1, 1, ?2, ?3)",
                rusqlite::params![unit, text, line],
            )
            .unwrap();
    };
    // b: two headings inserted out of line order; the first by line wins.
    heading(ids[1], "Second", 9);
    heading(ids[1], "First Heading", 2);
    // a also has a heading, which the frontmatter title beats.
    heading(ids[0], "Ignored Heading", 1);
    // d, e, f have an unusable title, so they fall back to a heading.
    heading(ids[3], "Heading D", 1);
    heading(ids[4], "Heading E", 1);
    heading(ids[5], "Heading F", 1);
    // g is code: never gets a title even with a heading row.
    heading(ids[6], "Code heading", 1);

    let listing = cache.list_folder("notes", "", false).unwrap();
    let title = |name: &str| {
        listing
            .entries
            .iter()
            .find(|e| e.name == name)
            .unwrap()
            .title
            .clone()
    };

    assert_eq!(title("a.md").as_deref(), Some("From Frontmatter"));
    assert_eq!(title("b.md").as_deref(), Some("First Heading"));
    assert_eq!(title("c.md"), None, "no frontmatter title, no heading");
    assert_eq!(title("d.md").as_deref(), Some("Heading D"));
    assert_eq!(title("e.md").as_deref(), Some("Heading E"));
    assert_eq!(title("f.md").as_deref(), Some("Heading F"));
    assert_eq!(title("g.rs"), None);
}

#[test]
fn a_missing_folder_and_a_file_path_are_told_apart() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "tephra", SourceStatus::Active, &tree_units());

    let missing = cache.list_folder("tephra", "nope", false).unwrap();
    assert_eq!(missing.state, FolderState::Missing);
    assert!(missing.entries.is_empty());

    let deep_missing = cache
        .list_folder("tephra", "src/nope/deeper", true)
        .unwrap();
    assert_eq!(deep_missing.state, FolderState::Missing);

    let file = cache.list_folder("tephra", "src/main.rs", false).unwrap();
    assert_eq!(file.state, FolderState::File);
    assert!(file.entries.is_empty());

    // A prefix of a folder name is not that folder.
    let partial = cache.list_folder("tephra", "sr", false).unwrap();
    assert_eq!(partial.state, FolderState::Missing);
}

#[test]
fn an_empty_source_root_lists_as_empty_and_an_unknown_source_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "empty", SourceStatus::Active, &[]);

    let listing = cache.list_folder("empty", "", true).unwrap();
    assert_eq!(listing.state, FolderState::Folder);
    assert!(listing.entries.is_empty());

    match cache.list_folder("ghost", "", false) {
        Err(CacheError::SourceNotFound(name)) => assert_eq!(name, "ghost"),
        other => panic!("expected SourceNotFound, got {other:?}"),
    }
}

#[test]
fn listing_is_per_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(
        &cache,
        "one",
        SourceStatus::Active,
        &[file("a/x.md", "prose", 1)],
    );
    seed(
        &cache,
        "two",
        SourceStatus::Active,
        &[file("b/y.md", "prose", 1)],
    );

    assert_eq!(
        names(&cache.list_folder("one", "", true).unwrap().entries),
        ["a", "a/x.md"]
    );
    assert_eq!(
        cache.list_folder("one", "b", true).unwrap().state,
        FolderState::Missing
    );
}

#[test]
fn an_offline_source_from_a_v1_cache_lists_with_null_fields_and_needs_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = common::open_temp_cache(&dir);
    seed(
        &cache,
        "notes",
        SourceStatus::Active,
        &[
            file("one.md", "prose", 5),
            file("Attachments/a.pdf", "document", 6),
            file("Attachments/deep/b.png", "binary", 7),
        ],
    );
    cache
        .transition_source_status("notes", SourceStatus::Offline)
        .unwrap();
    // The schema rebuild a v1 -> v2 upgrade does: an offline source's
    // rows come back provisional (kind guessed, size unknown,
    // needs_rescan set).
    cache.rebuild(&FaultInjector::none()).unwrap();

    let root = cache.list_folder("notes", "", false).unwrap();
    assert_eq!(root.source_status, SourceStatus::Offline);
    assert!(root.needs_rescan);
    assert_eq!(names(&root.entries), ["Attachments", "one.md"]);
    let one = &root.entries[1];
    assert_eq!(one.size, None);
    assert_eq!(one.kind, "text-other");
    assert!(one.needs_rescan);
    assert_eq!(one.mtime, Some(1_700_000_005), "mtime survives the rebuild");
    // A folder's size is a child count, known even without file sizes.
    assert_eq!(root.entries[0].size, Some(2));

    let nested = cache.list_folder("notes", "Attachments", true).unwrap();
    assert_eq!(
        names(&nested.entries),
        [
            "Attachments/deep",
            "Attachments/deep/b.png",
            "Attachments/a.pdf"
        ]
    );
    assert!(nested.needs_rescan);

    let missing = cache.list_folder("notes", "Nope", false).unwrap();
    assert_eq!(missing.state, FolderState::Missing);
}

#[test]
fn a_current_cache_does_not_report_needs_rescan() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "tephra", SourceStatus::Active, &tree_units());
    assert!(!cache.list_folder("tephra", "", true).unwrap().needs_rescan);
}

#[test]
fn a_read_only_connection_lists_and_cannot_write() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    seed(&cache, "tephra", SourceStatus::Active, &tree_units());

    let reader = Cache::open_read_only(cache.path()).unwrap();
    let before = reader.table_row_count("units").unwrap();
    let listing = reader.list_folder("tephra", "src", true).unwrap();
    assert_eq!(listing.entries.len(), 5);
    assert_eq!(reader.table_row_count("units").unwrap(), before);

    assert!(reader
        .connection()
        .execute("DELETE FROM units", [])
        .is_err());

    // A reader sees what the writer commits, while the writer is free to
    // keep going (WAL).
    cache
        .insert_unit(&NewUnit {
            source_id: cache.get_source("tephra").unwrap().unwrap().id,
            rel_path: "src/new.rs",
            mtime: None,
            frontmatter_json: None,
            kind: "code",
            size: Some(1),
            too_large: false,
        })
        .unwrap();
    assert_eq!(
        reader
            .list_folder("tephra", "src", true)
            .unwrap()
            .entries
            .len(),
        6
    );
}

#[test]
fn opening_a_missing_database_read_only_is_an_error_not_a_new_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.db");
    assert!(Cache::open_read_only(&path).is_err());
    assert!(!path.exists());
}
