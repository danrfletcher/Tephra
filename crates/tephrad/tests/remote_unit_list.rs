//! PR-5 (G8): what `unit.list` and `source.list` say about a GitHub
//! source, answered from the cache with zero adapter requests and no file
//! content on disk; the local wire shapes unchanged byte for byte.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::time::{Duration, Instant};

use remote_support::{files_under, Daemon, Fake, REPO};
use serde_json::{json, Value};

fn by_path<'a>(entries: &'a [Value], p: &str) -> &'a Value {
    entries
        .iter()
        .find(|e| e["rel_path"] == json!(p))
        .unwrap_or_else(|| panic!("{p} listed"))
}

fn keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[test]
fn every_g8_field_of_every_entry_answers_from_the_cache_with_no_adapter_request() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh
        .put_file(REPO, "main", "data.zzz", b"unknown extension\n");
    fake.gh.put_symlink(REPO, "main", "latest", "README.md");
    fake.gh.put_submodule(
        REPO,
        "main",
        "vendor/lib",
        "https://github.com/other/lib.git",
    );
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");
    let after_index = fake.requests();

    let entries = d.entries("g");
    // path, kind, size, reason
    let table: [(&str, &str, Option<u64>, Option<&str>); 8] = [
        ("README.md", "prose", Some(54), None),
        ("docs/Guide.md", "prose", Some(29), None),
        ("src/lib.rs", "code", Some(32), None),
        ("assets/logo.png", "binary", Some(7), None),
        ("data.zzz", "text-other", Some(18), None),
        ("latest", "binary", Some(9), Some("symlink")),
        ("vendor/lib", "binary", None, Some("submodule")),
        ("docs", "folder", None, None),
    ];
    for (path, kind, size, reason) in table {
        let e = by_path(&entries, path);
        assert_eq!(e["kind"], json!(kind), "{path} kind");
        assert_eq!(e["id"], json!(format!("@g:{path}")), "{path} id");
        assert_eq!(e["name"], json!(path.rsplit('/').next().unwrap()));
        if kind == "folder" {
            assert!(e.get("state").is_none(), "a folder has no file state");
            assert_eq!(e["cache_bytes"], json!(0));
            continue;
        }
        assert_eq!(e["state"], json!("remote"), "{path} state");
        assert_eq!(e["cache_bytes"], json!(0), "{path} cache_bytes");
        if let Some(s) = size {
            assert_eq!(e["size"], json!(s), "{path} size");
        }
        assert_eq!(
            e.get("reason").and_then(Value::as_str),
            reason,
            "{path} reason"
        );
        assert!(e["mtime"].is_null(), "{path}: a remote file has no mtime");
    }
    // The folder form, too.
    let docs = d.ok("unit.list", json!({"source": "g", "folder": "docs"}));
    assert_eq!(docs["offline"], json!(false));
    assert_eq!(docs["entries"].as_array().unwrap().len(), 1);

    assert_eq!(
        fake.requests(),
        after_index,
        "unit.list asks GitHub nothing"
    );
    assert!(files_under(&d.data_dir.join("remote")).is_empty());
    d.close();
}

#[test]
fn source_list_says_kind_branch_cache_bytes_status_and_last_sync() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    let row = d.wait_active("g");
    assert_eq!(row["name"], json!("g"));
    assert_eq!(row["kind"], json!("github"));
    assert_eq!(row["repo"], json!(REPO));
    assert_eq!(row["path"], json!(format!("github:{REPO}")));
    assert_eq!(row["branch"], json!("main"));
    assert_eq!(row["cache"], json!(true));
    assert_eq!(row["cache_bytes"], json!(0));
    assert_eq!(row["git_bytes"], json!(0));
    assert_eq!(row["status"], json!("active"));
    assert!(
        row["last_scanned_at"].as_i64().unwrap() > 1_700_000_000,
        "{row}"
    );
    let before = fake.requests();
    d.ok("source.list", json!({}));
    assert_eq!(fake.requests(), before, "source.list asks GitHub nothing");
    d.close();
}

#[test]
fn local_sources_keep_their_wire_shape_byte_for_byte() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    let local = fixtures::empty_source();
    local.write("a.md", "# A\n");
    d.ok(
        "source.register",
        json!({"name": "notes", "root": local.root().to_string_lossy()}),
    );
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");

    let row = d.source_row("notes").unwrap();
    assert_eq!(keys(&row), ["last_scanned_at", "name", "path", "status"]);
    let listed = d.ok(
        "unit.list",
        json!({"source": "notes", "folder": "", "recursive": true}),
    );
    for e in listed["entries"].as_array().unwrap() {
        let mut k = keys(e);
        k.retain(|k| k != "title");
        assert_eq!(
            k,
            ["id", "kind", "mtime", "name", "rel_path", "size"],
            "{e}"
        );
    }
    d.close();
}

#[test]
fn the_listing_returns_before_a_slowed_index_finishes() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh.set_latency(Duration::from_millis(400));
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    // The call returned with the listing while the index is still going: the
    // row says `indexing` and the three blob reads are not all done. (A
    // structural check, not a wall-clock bound, so a loaded machine cannot
    // fail it.)
    let row = d.source_row("g").unwrap();
    assert_eq!(row["status"], json!("indexing"), "still indexing: {row}");
    assert!(
        fake.reads().len() < 3,
        "the index had already read every blob: {:?}",
        fake.reads()
    );
    let entries = d.entries("g");
    assert_eq!(entries.iter().filter(|e| e["kind"] != "folder").count(), 4);
    assert!(entries
        .iter()
        .all(|e| e.get("state").is_none() || e["state"] == json!("remote")));
    d.wait_active("g");
    d.close();
}

#[test]
fn ten_thousand_files_list_within_the_time_budget_while_the_text_indexes_in_the_background() {
    let setup = Instant::now();
    let fake = Fake::start();
    fake.gh.add_repo(REPO, "main");
    let bins: Vec<(String, Vec<u8>)> = (0..9_900)
        .map(|i| {
            (
                format!("blobs/d{}/f{i}.png", i % 50),
                i.to_string().into_bytes(),
            )
        })
        .chain((0..100).map(|i| (format!("notes/n{i}.md"), format!("# N{i}\n").into_bytes())))
        .collect();
    fake.gh.put_files(
        REPO,
        "main",
        bins.iter().map(|(p, b)| (p.clone(), b.as_slice())),
    );
    let d = Daemon::start(&fake);
    assert!(setup.elapsed() < Duration::from_secs(20), "fixture build");
    let start = Instant::now();
    let added = d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    let took = start.elapsed();
    assert_eq!(added["files"], json!(10_000));
    assert!(took < Duration::from_secs(30), "the listing took {took:?}");

    let listed_start = Instant::now();
    let entries = d.entries("g");
    assert!(listed_start.elapsed() < Duration::from_secs(10));
    assert_eq!(
        entries.iter().filter(|e| e["kind"] != "folder").count(),
        10_000
    );
    // Only the 100 notes are ever read.
    d.wait_active("g");
    assert_eq!(fake.reads().len(), 100);
    d.close();
}
