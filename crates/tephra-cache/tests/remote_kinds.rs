//! PR-10: the Drive item kinds in the durable listing. `remote_items` keeps
//! the kind a listing knew (`gdoc`, `gsheet`, ...), the file id and the
//! revision; `units` stores what the graph needs (`prose` for a Doc,
//! `binary` for the Google kinds nobody reads). A rename moves a row and
//! the unit under it to a new path without losing either. Real SQLite in a
//! tempdir.

mod common;

use tephra_cache::{
    is_drive_item_kind, is_text_item_kind, Cache, FaultInjector, NewRemoteItem, NewRemoteSource,
    ITEM_KIND_GDOC, ITEM_KIND_GDRAWING, ITEM_KIND_GFORM, ITEM_KIND_GSHEET, ITEM_KIND_GSHORTCUT,
    ITEM_KIND_GSLIDES,
};

const CAP: u64 = 20 * 1024 * 1024;

const NEW: NewRemoteSource<'static> = NewRemoteSource {
    name: "gdrive",
    kind: "drive",
    target: "Notes",
    branch: None,
    cursor: "103",
    now: 1_700_000_000,
};

fn item<'a>(path: &'a str, kind: &'a str, file_id: &'a str, rev: &'a str) -> NewRemoteItem<'a> {
    NewRemoteItem {
        path,
        kind,
        sha: "",
        size: Some(10),
        file_id: Some(file_id),
        revision: Some(rev),
    }
}

fn listing() -> Vec<NewRemoteItem<'static>> {
    vec![
        item("Meeting.md", ITEM_KIND_GDOC, "d1", "r1"),
        item("Budget", ITEM_KIND_GSHEET, "d2", "r2"),
        item("Deck", ITEM_KIND_GSLIDES, "d3", "r3"),
        item("Survey", ITEM_KIND_GFORM, "d4", "r4"),
        item("Sketch", ITEM_KIND_GDRAWING, "d5", "r5"),
        item("Link", ITEM_KIND_GSHORTCUT, "d6", "r6"),
        item("Plan.md", "prose", "d7", "r7"),
        item("logo.png", "binary", "d8", "r8"),
    ]
}

fn add(cache: &Cache) -> i64 {
    cache.add_remote_source(&NEW, &listing(), CAP).expect("add")
}

#[test]
fn drive_kinds_are_kept_in_the_listing_and_stored_as_graph_kinds_in_units() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = add(&cache);

    let rows = cache.remote_items("gdrive").unwrap();
    let kind = |p: &str| {
        rows.iter()
            .find(|r| r.path == p)
            .unwrap()
            .kind
            .clone()
            .unwrap()
    };
    assert_eq!(kind("Meeting.md"), "gdoc");
    assert_eq!(kind("Budget"), "gsheet");
    assert_eq!(kind("Deck"), "gslides");
    assert_eq!(kind("Survey"), "gform");
    assert_eq!(kind("Sketch"), "gdrawing");
    assert_eq!(kind("Link"), "gshortcut");

    let unit = |p: &str| cache.get_unit_by_rel_path(id, p).unwrap().unwrap();
    assert_eq!(
        unit("Meeting.md").kind,
        "prose",
        "a Doc is Markdown once read"
    );
    for p in ["Budget", "Deck", "Survey", "Sketch", "Link", "logo.png"] {
        assert_eq!(unit(p).kind, "binary", "{p}");
    }
    assert_eq!(unit("Plan.md").kind, "prose");
}

#[test]
fn the_file_id_and_the_revision_are_stored_and_a_drive_file_has_no_sha() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    add(&cache);
    let rows = cache.remote_items("gdrive").unwrap();
    let plan = rows.iter().find(|r| r.path == "Plan.md").unwrap();
    assert_eq!(plan.file_id.as_deref(), Some("d7"));
    assert_eq!(plan.revision.as_deref(), Some("r7"));
    assert_eq!(plan.sha, None, "no git sha for a Drive file");
    assert!(rows.iter().all(|r| r.state == "remote"));

    // A new revision of the same file brings the row to it.
    cache
        .upsert_remote_item("gdrive", &item("Plan.md", "prose", "d7", "r8"))
        .unwrap();
    let rows = cache.remote_items("gdrive").unwrap();
    let plan = rows.iter().find(|r| r.path == "Plan.md").unwrap();
    assert_eq!(plan.revision.as_deref(), Some("r8"));
    assert_eq!(plan.file_id.as_deref(), Some("d7"));
}

#[test]
fn only_a_doc_and_the_text_kinds_are_read_by_the_index() {
    for k in ["prose", "code", "document", "text-other", ITEM_KIND_GDOC] {
        assert!(is_text_item_kind(k), "{k}");
    }
    for k in [
        "binary",
        ITEM_KIND_GSHEET,
        ITEM_KIND_GSLIDES,
        ITEM_KIND_GFORM,
        ITEM_KIND_GDRAWING,
        ITEM_KIND_GSHORTCUT,
    ] {
        assert!(!is_text_item_kind(k), "{k}");
    }
    assert!(is_drive_item_kind(ITEM_KIND_GSHEET));
    assert!(is_drive_item_kind(ITEM_KIND_GDOC));
    assert!(!is_drive_item_kind("prose"));
    assert!(!is_drive_item_kind("binary"));
}

#[test]
fn a_rename_moves_the_row_and_the_unit_and_a_swap_does_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    let id = add(&cache);
    let unit_id = |p: &str| cache.get_unit_by_rel_path(id, p).unwrap().map(|u| u.id);
    let plan_unit = unit_id("Plan.md").unwrap();
    let meeting_unit = unit_id("Meeting.md").unwrap();

    cache
        .rename_remote_items(
            "gdrive",
            id,
            &[
                ("Plan.md".to_string(), "Meeting.md".to_string()),
                ("Meeting.md".to_string(), "Plan.md".to_string()),
                ("Missing.md".to_string(), "Nowhere.md".to_string()),
            ],
        )
        .unwrap();
    assert_eq!(
        unit_id("Meeting.md"),
        Some(plan_unit),
        "the units swapped names"
    );
    assert_eq!(unit_id("Plan.md"), Some(meeting_unit));
    assert_eq!(
        unit_id("Nowhere.md"),
        None,
        "a path that is not there is skipped"
    );
    let rows = cache.remote_items("gdrive").unwrap();
    let id_of = |p: &str| rows.iter().find(|r| r.path == p).unwrap().file_id.clone();
    assert_eq!(id_of("Meeting.md").as_deref(), Some("d7"));
    assert_eq!(id_of("Plan.md").as_deref(), Some("d1"));
    assert_eq!(rows.len(), 8, "no row gained or lost");
}

#[test]
fn drive_rows_survive_a_schema_bump_and_the_units_are_made_again_with_the_graph_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let mut cache = common::open_temp_cache(&dir);
    let list = vec![
        item("Plan (2).md", ITEM_KIND_GDOC, "d1", "r1"),
        item("Plan.md", ITEM_KIND_GDOC, "d2", "r2"),
        item("Budget", ITEM_KIND_GSHEET, "d3", "r3"),
        item("Deck", ITEM_KIND_GSLIDES, "d4", "r4"),
        item("Survey", ITEM_KIND_GFORM, "d5", "r5"),
        item("Sketch", ITEM_KIND_GDRAWING, "d6", "r6"),
        item("Link", ITEM_KIND_GSHORTCUT, "d7", "r7"),
        item("notes.txt", "text-other", "d8", "r8"),
        item("logo.png", "binary", "d9", "r9"),
    ];
    let id = cache.add_remote_source(&NEW, &list, CAP).expect("add");
    let before = cache.remote_items("gdrive").unwrap();
    assert_eq!(before.len(), list.len());

    // A schema bump rebuilds the core tables: `units` is dropped, the
    // durable remote rows are kept.
    cache.rebuild(&FaultInjector::none()).expect("rebuild");

    let after = cache.remote_items("gdrive").unwrap();
    assert_eq!(after, before, "every durable row is exactly as it was");
    for want in &list {
        let row = after.iter().find(|r| r.path == want.path).unwrap();
        assert_eq!(row.kind.as_deref(), Some(want.kind), "{}", want.path);
        assert_eq!(row.file_id.as_deref(), want.file_id, "{}", want.path);
        assert_eq!(row.revision.as_deref(), want.revision, "{}", want.path);
    }
    assert!(cache.remote_source("gdrive").unwrap().is_some());
    assert!(
        cache.get_unit_by_rel_path(id, "Plan.md").unwrap().is_none(),
        "the rebuild dropped the units"
    );

    // The units come back from the rows alone: no remote call.
    assert!(cache.restore_remote_units("gdrive", CAP).unwrap());
    let unit = |p: &str| cache.get_unit_by_rel_path(id, p).unwrap().unwrap();
    for p in ["Plan (2).md", "Plan.md"] {
        assert_eq!(unit(p).kind, "prose", "a Doc is prose: {p}");
    }
    for p in ["Budget", "Deck", "Survey", "Sketch", "Link", "logo.png"] {
        assert_eq!(unit(p).kind, "binary", "{p}");
    }
    assert_eq!(unit("notes.txt").kind, "text-other");

    // Restoring did not touch what the listing knew about the files.
    let again = cache.remote_items("gdrive").unwrap();
    for (a, b) in again.iter().zip(&before) {
        assert_eq!(
            (&a.path, &a.kind, &a.file_id, &a.revision),
            (&b.path, &b.kind, &b.file_id, &b.revision)
        );
    }
    assert!(!cache.restore_remote_units("gdrive", CAP).unwrap());
}
