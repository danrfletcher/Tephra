//! PR-7 (E10): every remote name lands at a safe path under the cache root,
//! through the whole fetch path (the table of rules is in the unit tests of
//! `remote::safe_path`).
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use remote_support::{FetchRig, REPO};
use serde_json::json;

fn inside(root: &std::path::Path, p: &std::path::Path) -> bool {
    p.canonicalize()
        .unwrap()
        .starts_with(root.canonicalize().unwrap())
}

#[test]
fn awkward_names_are_fetched_to_safe_paths_inside_the_cache_and_read_back() {
    let long = format!("{}.md", "n".repeat(300));
    let long_utf8 = format!("{}.md", "é".repeat(200)); // 400 bytes
    let names: Vec<String> = vec![
        "100%.md".to_string(),
        "a%2Fb.md".to_string(),
        "back\\slash.md".to_string(),
        "tab\tname.md".to_string(),
        "weird \u{1b}[31m.md".to_string(),
        ".gitignore".to_string(),
        "sub/.git/config".to_string(),
        "Dir/.GIT/x".to_string(),
        long.clone(),
        long_utf8.clone(),
        "Readme.md".to_string(),
        "README.MD".to_string(),
        "dir/Same.md".to_string(),
        "dir/same.md".to_string(),
        "dots/.../x.md".to_string(),
        " lead and trail .md ".to_string(),
        "ünï/çödé.md".to_string(),
    ];
    let rig = FetchRig::start(|fake| {
        for (i, name) in names.iter().enumerate() {
            fake.gh
                .put_file(REPO, "main", name, format!("body {i}\n").as_bytes());
        }
    });
    let root = rig.d.data_dir.join("remote").join("g");
    let mut written = Vec::new();
    for (i, name) in names.iter().enumerate() {
        let reply = rig.fetch(name);
        assert!(reply.get("error").is_none(), "{name:?}: {reply}");
        let path = std::path::PathBuf::from(reply["result"]["path"].as_str().unwrap());
        assert!(path.starts_with(root.join("clone")), "{name:?} -> {path:?}");
        assert!(inside(&root, &path), "{name:?} -> {path:?}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            format!("body {i}\n").as_bytes(),
            "{name:?}"
        );
        for part in path.strip_prefix(&root).unwrap().components() {
            assert!(
                part.as_os_str().len() <= 255,
                "{name:?}: a name over 255 bytes"
            );
        }
        assert!(
            !path
                .components()
                .any(|c| c.as_os_str() == ".git" || c.as_os_str() == ".."),
            "{name:?} -> {path:?}"
        );
        written.push(path);
    }
    // Injective: no two remote names share a file, so every body is its own.
    let mut unique = written.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "{written:?}");
    // Again: the same name goes to the same place, with no new download.
    let reads = rig.reads();
    for (name, path) in names.iter().zip(&written) {
        assert_eq!(
            rig.fetch(name)["result"]["path"],
            json!(path.display().to_string()),
            "{name:?}"
        );
    }
    assert_eq!(rig.reads(), reads);
    // And nothing was written outside of `clone/`, nor as a `.git`.
    let files = rig.cache_files();
    assert_eq!(files.len(), names.len());
    assert!(files.iter().all(|f| f.starts_with("clone")));
}

#[test]
fn a_poll_that_deletes_a_file_finds_the_copy_a_fetch_wrote_under_a_mapped_name() {
    let rig = FetchRig::start(|fake| {
        fake.gh.put_file(REPO, "main", "50% off/.git/x.md", b"x\n");
    });
    let reply = rig.fetch("50% off/.git/x.md");
    assert!(reply.get("error").is_none(), "{reply}");
    let path = std::path::PathBuf::from(reply["result"]["path"].as_str().unwrap());
    assert!(path.exists());
    rig.fake.gh.remove_file(REPO, "main", "50% off/.git/x.md");
    rig.clock.advance(std::time::Duration::from_secs(61));
    rig.d.wait_for("the deleted file's copy is removed", || {
        (!path.exists()).then_some(())
    });
}
