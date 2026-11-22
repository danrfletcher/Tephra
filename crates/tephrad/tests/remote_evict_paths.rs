//! PR-8 (E10): eviction deletes only what resolves inside the source's own
//! cache folder, whatever the row's path is, and leaves its folders tidy:
//! empty parents go; the cache root and `.git` never do.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::os::unix::fs::symlink;
use std::time::Duration;

use remote_support::{exec_sql, files_under, FetchRig, REPO};
use serde_json::{json, Value};

fn ok(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    &reply["result"]
}

fn evict_everything(rig: &FetchRig) -> Value {
    rig.clock.advance(Duration::from_secs(700));
    rig.d.ok("cache.set_limit", json!({"limit": "0"}))
}

/// Awkward names (the PR-7 set): each is fetched, then dropped, and the
/// disk is left with no file, no placeholder and no empty folder; the cache
/// root and the `clone` folder stay.
#[test]
fn awkward_names_are_dropped_cleanly_and_the_folders_they_leave_go() {
    let long = format!("deep/{}.md", "n".repeat(250));
    let long_utf8 = format!("é/{}.md", "é".repeat(120));
    let names = vec![
        "a/b/c/d.md".to_string(),
        "100%.md".to_string(),
        "back\\slash.md".to_string(),
        "sub/.git/config".to_string(),
        "Dir/.GIT/x".to_string(),
        long,
        long_utf8,
        "dir/Same.md".to_string(),
        "dir/same.md".to_string(),
        "dots/.../x.md".to_string(),
    ];
    let rig = FetchRig::start(|fake| {
        for (i, n) in names.iter().enumerate() {
            fake.gh
                .put_file(REPO, "main", n, format!("body {i}\n").as_bytes());
        }
    });
    for n in &names {
        ok(&rig.fetch(n));
    }
    assert_eq!(rig.cache_files().len(), names.len());
    let set = evict_everything(&rig);
    assert_eq!(set["dropped"], json!(names.len()), "{set}");
    assert!(rig.cache_files().is_empty(), "{:?}", rig.cache_files());
    for n in &names {
        assert_eq!(rig.row(n).state, "remote", "{n}");
    }
    let root = rig.d.data_dir.join("remote").join("g");
    assert!(root.is_dir(), "the cache root stays");
    let left: Vec<_> = std::fs::read_dir(root.join("clone"))
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "no folder is left behind: {left:?}");
}

/// A row whose path climbs out (`..`) never makes the pass delete a file
/// outside the cache folder, even when that file exists.
#[test]
fn a_row_path_with_dot_dot_deletes_nothing_outside_the_cache() {
    let rig = FetchRig::start(|_| {});
    let root = rig.d.data_dir.join("remote").join("g");
    std::fs::create_dir_all(root.join("clone")).unwrap();
    let outside_root = rig.d.data_dir.join("remote").join("precious.txt");
    let beside_clone = root.join("precious2.txt");
    let top = rig.d.data_dir.join("precious3.txt");
    for f in [&outside_root, &beside_clone, &top] {
        std::fs::write(f, b"precious").unwrap();
    }
    exec_sql(
        &rig.d.cache_path,
        "INSERT INTO remote_items (source, path, kind, size, state, last_used) VALUES
            ('g', '../../precious.txt', 'file', 8, 'cached', 1),
            ('g', '../precious2.txt', 'file', 8, 'cached', 1),
            ('g', '../../../precious3.txt', 'file', 8, 'cached', 1),
            ('g', '/etc/hosts', 'file', 8, 'cached', 1);",
    );
    // The names are mapped to safe ones inside `clone/` (PR-7), where no
    // copy exists: the rows go back to `remote` and no file is deleted.
    evict_everything(&rig);
    for f in [&outside_root, &beside_clone, &top] {
        assert_eq!(std::fs::read(f).unwrap(), b"precious", "{f:?}");
    }
    assert!(std::path::Path::new("/etc/hosts").exists());
}

/// A symlink where a cached file or folder should be is never followed:
/// what it points at stays, and so does the link.
#[test]
fn a_symlink_in_the_cache_is_not_followed_or_deleted_through() {
    let outside = tempfile::tempdir().unwrap();
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    ok(&rig.fetch("docs/Guide.md"));
    let clone = rig.d.data_dir.join("remote").join("g").join("clone");

    // README.md becomes a link to a precious file.
    let precious = outside.path().join("precious.txt");
    std::fs::write(&precious, b"precious").unwrap();
    std::fs::remove_file(clone.join("README.md")).unwrap();
    symlink(&precious, clone.join("README.md")).unwrap();
    // docs becomes a link to a folder holding Guide.md.
    std::fs::remove_dir_all(clone.join("docs")).unwrap();
    std::fs::write(
        outside.path().join("Guide.md"),
        b"# Guide\n\nBack to [[README]].\n",
    )
    .unwrap();
    symlink(outside.path(), clone.join("docs")).unwrap();

    let set = evict_everything(&rig);
    assert_eq!(set["dropped"], json!(0), "{set}");
    assert_eq!(std::fs::read(&precious).unwrap(), b"precious");
    assert!(outside.path().join("Guide.md").exists());
    assert!(std::fs::symlink_metadata(clone.join("README.md"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(std::fs::symlink_metadata(clone.join("docs"))
        .unwrap()
        .file_type()
        .is_symlink());
}

/// A clone with git data of its own is left alone: nothing is dropped from
/// it, its `.git` is never touched, and `cache status` counts the `.git`
/// bytes on their own line.
#[test]
fn a_clone_with_git_data_is_not_evicted_from_and_its_git_folder_is_never_touched() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    let git = rig
        .d
        .data_dir
        .join("remote")
        .join("g")
        .join("clone")
        .join(".git");
    std::fs::create_dir_all(git.join("objects")).unwrap();
    std::fs::write(git.join("HEAD"), b"ref: refs/heads/main\n").unwrap();
    let set = evict_everything(&rig);
    assert_eq!(set["dropped"], json!(0), "{set}");
    assert_eq!(set["git_bytes"], json!(21), "{set}");
    assert!(rig.cached_path("README.md").exists());
    assert_eq!(
        std::fs::read(git.join("HEAD")).unwrap(),
        b"ref: refs/heads/main\n"
    );
    assert_eq!(files_under(&git).len(), 1);
}
