//! PR-6 (G18, E10, E11, E13): a file removed on GitHub. It leaves the index
//! and `unit.list`; links to it become unresolved as for a deleted local
//! file; its unmodified cached copy under `remote/<source>/clone/` is
//! deleted, a modified one is kept and marked needs_attention with the
//! reason "deleted on GitHub"; a deleted folder takes every file under it;
//! an unsafe name removes nothing outside `remote/<source>/`; a repo that is
//! gone, moved or no longer shared is never run through this path.
#![allow(dead_code)]

mod remote_support;

use std::path::PathBuf;
use std::time::Duration;

use remote_support::{index_snapshot, item_rows, seed_cached_copy, seed_queued_upload, Rig, REPO};
use serde_json::{json, Value};
use tephra_cache::Cache;

const GUIDE: &[u8] = b"# Guide\n\nBack to [[README]].\n";

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn files(r: &Rig) -> Vec<String> {
    let mut v: Vec<String> =
        r.d.entries("g")
            .iter()
            .filter(|e| e["kind"] != "folder")
            .map(|e| e["rel_path"].as_str().unwrap().to_string())
            .collect();
    v.sort();
    v
}

/// Waits until `path` is no longer listed (the poll was applied).
fn wait_gone(r: &Rig, path: &str) {
    r.advance(s(61));
    r.d.wait_for(&format!("{path} removed"), || {
        (!files(r).contains(&path.to_string())).then_some(())
    });
}

/// README's `[[Guide]]` edge: `Some(true)` when it resolves.
fn readme_guide_resolves(r: &Rig) -> bool {
    let forward =
        r.d.ok("graph.get_forward_links", json!({"id": "@g:README.md"}));
    let edge = forward
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["to_raw"] == json!("Guide"))
        .unwrap_or_else(|| panic!("README links [[Guide]]: {forward}"))
        .clone();
    edge["to"].get("resolved").is_some()
}

fn guide_row(r: &Rig) -> Option<tephra_cache::RemoteItemRow> {
    item_rows(&r.d.cache_path, "g")
        .into_iter()
        .find(|i| i.path == "docs/Guide.md")
}

/// The unit rows and links (the checks of `delete_cascades.rs`): a deleted
/// unit leaves no row, no edge from it, no heading of it.
fn assert_unit_gone(r: &Rig, rel: &str) {
    let cache = Cache::open_read_only(&r.d.cache_path).unwrap();
    let source = cache.get_source("g").unwrap().unwrap();
    assert!(cache
        .get_unit_by_rel_path(source.id, rel)
        .unwrap()
        .is_none());
    let conn = cache.connection();
    let n = |sql: &str| -> i64 { conn.query_row(sql, [], |row| row.get(0)).unwrap() };
    assert_eq!(
        n("SELECT COUNT(*) FROM links WHERE from_unit_id NOT IN (SELECT id FROM units)"),
        0,
        "no edge from a deleted unit"
    );
    assert_eq!(
        n("SELECT COUNT(*) FROM headings WHERE unit_id NOT IN (SELECT id FROM units)"),
        0
    );
    assert_eq!(
        n("SELECT COUNT(*) FROM blocks WHERE unit_id NOT IN (SELECT id FROM units)"),
        0
    );
    assert_eq!(
        n("SELECT COUNT(*) FROM links WHERE to_unit_id IS NOT NULL AND to_unit_id NOT IN (SELECT id FROM units)"),
        0,
        "no edge points at a deleted unit"
    );
}

#[test]
fn a_removed_file_leaves_the_index_and_its_links_become_unresolved() {
    let r = Rig::start();
    assert!(readme_guide_resolves(&r), "resolved before the delete");
    assert!(guide_row(&r).is_some());
    r.fake.gh.remove_file(REPO, "main", "docs/Guide.md");

    wait_gone(&r, "docs/Guide.md");
    assert_eq!(files(&r), ["README.md", "assets/logo.png", "src/lib.rs"]);
    assert!(
        !readme_guide_resolves(&r),
        "unresolved, as for a deleted local file"
    );
    assert!(guide_row(&r).is_none(), "the item row is gone");
    assert_unit_gone(&r, "docs/Guide.md");
    // The source is healthy and its 0 bytes are still 0 (GP2 / F12).
    let row = r.d.source_row("g").unwrap();
    assert_eq!(row["status"], json!("active"));
    assert_eq!(row["cache_bytes"], json!(0));
    for e in r.d.entries("g").iter().filter(|e| e["kind"] != "folder") {
        assert_eq!(e["state"], json!("remote"), "{e}");
    }
    // The deleted file's backlink edge to README went with it.
    let back = r.d.ok("graph.get_backlinks", json!({"id": "@g:README.md"}));
    assert!(back
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["from"] != json!("@g:docs/Guide.md")));
    assert!(r.fake.non_get_requests().is_empty());
    r.d.close();
}

/// One row of the table: what is on disk, and in the queue, when the
/// remote deletes `docs/Guide.md`.
struct Case {
    name: &'static str,
    /// The cached copy's bytes (None: no copy).
    copy: Option<&'static [u8]>,
    queued: bool,
    /// The copy survives, and the item is kept `needs_attention`.
    kept: bool,
}

#[test]
fn the_cached_copy_goes_or_stays_by_whether_anyone_edited_it() {
    let table = [
        Case {
            name: "no copy",
            copy: None,
            queued: false,
            kept: false,
        },
        Case {
            name: "unmodified copy",
            copy: Some(GUIDE),
            queued: false,
            kept: false,
        },
        Case {
            name: "modified copy",
            copy: Some(b"# Guide\n\nI edited this.\n"),
            queued: false,
            kept: true,
        },
        Case {
            name: "empty copy (truncated edit)",
            copy: Some(b""),
            queued: false,
            kept: true,
        },
        Case {
            name: "unmodified bytes but an edit is queued",
            copy: Some(GUIDE),
            queued: true,
            kept: true,
        },
        Case {
            name: "queued edit and no copy",
            copy: None,
            queued: true,
            kept: true,
        },
    ];
    for case in table {
        let r = Rig::start();
        let copy: Option<PathBuf> = case
            .copy
            .map(|b| seed_cached_copy(&r.d.data_dir, "g", "docs/Guide.md", b));
        if case.queued {
            seed_queued_upload(&r.d.cache_path, "g", "docs/Guide.md");
        }
        r.fake.gh.remove_file(REPO, "main", "docs/Guide.md");
        wait_gone(&r, "docs/Guide.md");
        assert!(!readme_guide_resolves(&r), "{}", case.name);
        assert_unit_gone(&r, "docs/Guide.md");

        match (&copy, case.kept) {
            (Some(path), true) => {
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    case.copy.unwrap(),
                    "{}: kept byte for byte",
                    case.name
                );
            }
            (Some(path), false) => {
                assert!(
                    !path.exists(),
                    "{}: the unmodified copy is deleted",
                    case.name
                );
                assert!(
                    !path.parent().unwrap().exists(),
                    "{}: and the folder it emptied",
                    case.name
                );
                assert!(
                    r.d.data_dir.join("remote/g/clone").exists(),
                    "{}: never the clone root",
                    case.name
                );
            }
            (None, _) => {}
        }
        let row = guide_row(&r);
        if case.kept {
            let row = row.unwrap_or_else(|| panic!("{}: the item is kept", case.name));
            assert_eq!(row.state, "needs_attention", "{}", case.name);
            assert_eq!(
                row.reason.as_deref(),
                Some("deleted on GitHub"),
                "{}",
                case.name
            );
        } else {
            assert!(row.is_none(), "{}: the item is gone", case.name);
        }
        if case.queued {
            let cache = Cache::open_read_only(&r.d.cache_path).unwrap();
            assert!(
                cache.has_queued_upload("g", "docs/Guide.md").unwrap(),
                "{}: the queued edit is kept",
                case.name
            );
        }
        r.d.close();
    }
}

#[test]
fn a_deleted_folder_removes_every_file_under_it_and_its_unmodified_copies() {
    let r = Rig::start();
    r.fake.gh.put_files(
        REPO,
        "main",
        [
            ("docs/a.md".to_string(), &b"# A\n"[..]),
            ("docs/deep/b.md".to_string(), &b"# B\n"[..]),
            ("docs/deep/er/c.md".to_string(), &b"# C\n"[..]),
            ("docsbook.md".to_string(), &b"# Not under docs/\n"[..]),
        ],
    );
    r.advance(s(61));
    r.d.wait_for("the new files are listed", || {
        files(&r)
            .contains(&"docs/deep/er/c.md".to_string())
            .then_some(())
    });
    let copy_a = seed_cached_copy(&r.d.data_dir, "g", "docs/a.md", b"# A\n");
    let copy_c = seed_cached_copy(&r.d.data_dir, "g", "docs/deep/er/c.md", b"# C (mine)\n");

    for p in [
        "docs/Guide.md",
        "docs/a.md",
        "docs/deep/b.md",
        "docs/deep/er/c.md",
    ] {
        r.fake.gh.remove_file(REPO, "main", p);
    }
    r.advance(s(61));
    r.d.wait_for("the folder's files are gone", || {
        files(&r)
            .iter()
            .all(|f| !f.starts_with("docs/"))
            .then_some(())
    });
    assert_eq!(
        files(&r),
        ["README.md", "assets/logo.png", "docsbook.md", "src/lib.rs"],
        "a sibling that only shares the prefix stays"
    );
    assert!(!copy_a.exists(), "the unmodified copy went");
    assert_eq!(
        std::fs::read(&copy_c).unwrap(),
        b"# C (mine)\n",
        "the edited one stayed"
    );
    let rows = item_rows(&r.d.cache_path, "g");
    let kept: Vec<&str> = rows
        .iter()
        .filter(|i| i.state == "needs_attention")
        .map(|i| i.path.as_str())
        .collect();
    assert_eq!(kept, ["docs/deep/er/c.md"]);
    r.d.close();
}

#[test]
fn an_unsafe_name_removes_nothing_outside_the_source_directory() {
    let r = Rig::start();
    let long = "n".repeat(256) + ".md";
    r.fake.gh.put_files(
        REPO,
        "main",
        [
            ("../../../outside.md".to_string(), &b"# out\n"[..]),
            ("a/../../../outside2.md".to_string(), &b"# out2\n"[..]),
            (long.clone(), &b"# long\n"[..]),
        ],
    );
    // Files a naive join of the clone directory and the name would hit,
    // holding exactly the bytes the remote's blob has: a delete that
    // trusted the name AND found an "unmodified copy" would remove them.
    // (The clone directory exists, as it does once anything is cached.)
    seed_cached_copy(&r.d.data_dir, "g", "README.md", b"a cached README\n");
    let outside = r.d.data_dir.join("outside.md");
    let outside2 = r.d.data_dir.join("outside2.md");
    let beside = r.d.data_dir.join("remote").join("outside.md");
    let beside2 = r.d.data_dir.join("remote").join("outside2.md");
    for (f, bytes) in [
        (&outside, &b"# out\n"[..]),
        (&beside, &b"# out\n"[..]),
        (&outside2, &b"# out2\n"[..]),
        (&beside2, &b"# out2\n"[..]),
    ] {
        std::fs::write(f, bytes).unwrap();
    }
    // A file of the clone with the over-long name is not looked up either.
    r.advance(s(61));
    let listed = r.d.wait_for("the odd names are indexed or refused", || {
        let rows = item_rows(&r.d.cache_path, "g");
        rows.iter().any(|i| i.path == long).then_some(rows)
    });
    assert!(listed.iter().any(|i| i.path == "../../../outside.md"));

    for p in [
        "../../../outside.md",
        "a/../../../outside2.md",
        long.as_str(),
    ] {
        r.fake.gh.remove_file(REPO, "main", p);
    }
    r.advance(s(61));
    r.d.wait_for("the odd names are removed", || {
        let rows = item_rows(&r.d.cache_path, "g");
        (!rows.iter().any(|i| i.path == long)).then_some(())
    });
    for (f, bytes) in [
        (&outside, &b"# out\n"[..]),
        (&beside, &b"# out\n"[..]),
        (&outside2, &b"# out2\n"[..]),
        (&beside2, &b"# out2\n"[..]),
    ] {
        assert_eq!(std::fs::read(f).unwrap(), bytes, "{f:?} untouched");
    }
    assert_eq!(
        r.d.source_row("g").unwrap()["status"],
        json!("active"),
        "an unsafe name is not an error"
    );
    r.d.close();
}

#[test]
fn a_repo_that_is_gone_or_hidden_is_needs_attention_and_no_file_is_deleted() {
    // Access lost: GitHub answers 404 for a private repo without access.
    let r = Rig::start();
    let before = index_snapshot(&r.d.cache_path);
    let copy = seed_cached_copy(&r.d.data_dir, "g", "docs/Guide.md", GUIDE);
    r.fake.gh.set_hidden(REPO, true);
    r.advance(s(61));
    let row = r.wait_status("needs_attention");
    assert!(
        row["status_reason"].as_str().unwrap().contains(REPO),
        "{row}"
    );
    assert_eq!(index_snapshot(&r.d.cache_path), before, "every item stays");
    assert!(copy.exists(), "and so does an unmodified cached copy");
    // Access back: the source is active again, nothing was re-indexed.
    r.fake.gh.set_hidden(REPO, false);
    r.advance(s(61));
    r.wait_status("active");
    assert_eq!(index_snapshot(&r.d.cache_path), before);
    r.d.close();
}

#[test]
fn a_moved_repo_deletes_nothing_even_with_cached_copies() {
    let r = Rig::start();
    let before = index_snapshot(&r.d.cache_path);
    let copy = seed_cached_copy(&r.d.data_dir, "g", "docs/Guide.md", GUIDE);
    r.fake
        .gh
        .move_repo(REPO, "https://api.github.com/repositories/9");
    r.advance(s(61));
    let row: Value = r.wait_status("needs_attention");
    assert!(row["status_reason"]
        .as_str()
        .unwrap()
        .contains("repositories/9"));
    assert_eq!(index_snapshot(&r.d.cache_path), before);
    assert!(copy.exists());
    r.d.close();
}
