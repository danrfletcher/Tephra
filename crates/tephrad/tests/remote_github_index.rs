//! PR-5 (G9): the first index of a GitHub source. Each text file is read
//! exactly once through the API (by its blob sha, at the commit the
//! listing was taken at), parsed into links, blocks, headings and symbols,
//! and its bytes dropped. Binary files, files over the cap, symlinks,
//! submodules and Git LFS pointers are listed by name and never fetched.
//! Real registry and RPC server against the PR-3 fake GitHub.
#![allow(dead_code)]

mod remote_support;

use std::time::Duration;

use remote_support::{files_under, Daemon, Fake, REPO};
use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_remote::testing::FakeGithub;
use tephrad::file_kind::MAX_INDEXED_BYTES;

/// The blob shas the fake was asked for, in order.
fn blob_reads(fake: &Fake) -> Vec<String> {
    fake.reads()
        .into_iter()
        .filter_map(|t| t.split("/git/blobs/").nth(1).map(str::to_string))
        .collect()
}

fn entry<'a>(entries: &'a [Value], path: &str) -> &'a Value {
    entries
        .iter()
        .find(|e| e["rel_path"] == json!(path))
        .unwrap_or_else(|| panic!("{path} listed"))
}

#[test]
fn each_text_file_is_read_exactly_once_and_a_binary_never() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    let row = d.wait_active("g");
    assert_eq!(row["index_total"], json!(3), "prose, prose and code");
    assert_eq!(row["index_done"], json!(3));

    let mut reads = blob_reads(&fake);
    reads.sort();
    let mut want: Vec<String> = [
        &b"# Atlas\n\nSee [[@vault:note]] and [[Guide]].\n\n## Setup\n"[..],
        b"# Guide\n\nBack to [[README]].\n",
        b"pub fn atlas() -> u32 {\n    1\n}\n",
    ]
    .iter()
    .map(|b| FakeGithub::blob_sha(b))
    .collect();
    want.sort();
    assert_eq!(
        reads, want,
        "one blob read per text file, none for logo.png"
    );
    assert!(
        !fake.reads().iter().any(|t| t.contains("/contents/")),
        "the first index uses blobs, not contents calls"
    );
    d.close();
}

#[test]
fn a_repeat_index_of_unchanged_files_makes_no_second_read() {
    let fake = Fake::start();
    fake.atlas();
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");
    let first = fake.reads().len();
    assert_eq!(first, 3);

    // Start-up again, and an index run again: every file is already
    // indexed, so nothing is read.
    d.registry.resume_remote_sources();
    d.registry.spawn_remote_index("g");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(fake.reads().len(), first, "no second read");
    d.close();
}

#[test]
fn the_text_is_parsed_into_links_headings_and_symbols_and_the_bytes_are_dropped() {
    let fake = Fake::start();
    fake.atlas();
    let marker = "UNIQUE-PARAGRAPH-MARKER-9c1f4b";
    let whole = format!("# Notes\n\n{marker}\n\n## Part ^blk1\n");
    fake.gh.put_file(REPO, "main", "notes.md", whole.as_bytes());
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");

    let cache = Cache::open_read_only(&d.cache_path).unwrap();
    let source = cache.get_source("g").unwrap().unwrap();
    let unit = |p: &str| cache.get_unit_by_rel_path(source.id, p).unwrap().unwrap();

    let readme = unit("README.md");
    let headings: Vec<String> = cache
        .headings_for_unit(readme.id)
        .unwrap()
        .into_iter()
        .map(|h| h.text)
        .collect();
    assert_eq!(headings, ["Atlas", "Setup"]);
    assert!(readme.hash.is_some(), "indexed: it has a hash");
    assert_eq!(readme.mtime, None, "no mtime: it is not a file on disk");

    let symbols = cache.symbols_for_unit(unit("src/lib.rs").id).unwrap();
    assert!(
        symbols.iter().any(|s| s.name == "atlas"),
        "the code file's symbols: {symbols:?}"
    );
    assert!(unit("assets/logo.png").hash.is_none(), "never read");

    // The raw bytes are nowhere: not a file in the data dir, and not in the
    // cache either (the cache keeps the index rows, as for a local file,
    // never the file).
    let on_disk: Vec<_> = files_under(&d.data_dir)
        .into_iter()
        .filter(|p| {
            let n = p.to_string_lossy();
            !(n.starts_with("cache.db") || n == "token")
        })
        .collect();
    assert!(on_disk.is_empty(), "unexpected files: {on_disk:?}");
    for path in [d.cache_path.clone(), d.cache_path.with_extension("db-wal")] {
        if let Ok(bytes) = std::fs::read(&path) {
            let hay = String::from_utf8_lossy(&bytes);
            assert!(!hay.contains(&whole), "{path:?} holds the whole file");
        }
    }
    d.close();
}

#[test]
fn kind_rules_known_binary_never_unknown_extension_read_once() {
    let fake = Fake::start();
    fake.gh.add_repo(REPO, "main");
    // Known binary extensions: never read, whatever the bytes are.
    for name in ["a.png", "b.zip", "c.woff2", "d.jar", "e.mp4", "f.sqlite"] {
        fake.gh.put_file(REPO, "main", name, b"# looks like text\n");
    }
    // Unknown extension: read once to classify.
    let unknown = b"just some text\n";
    fake.gh.put_file(REPO, "main", "data.zzz", unknown);
    // Text kinds by name: prose, code, text-other.
    let text = [
        ("p.md", &b"# P\n"[..]),
        ("c.py", b"def f():\n    pass\n"),
        ("o.txt", b"plain\n"),
        ("noext", b"no extension\n"),
    ];
    for (n, b) in text {
        fake.gh.put_file(REPO, "main", n, b);
    }
    // Documents: read once through the API, parsed or not.
    fake.gh
        .put_file(REPO, "main", "d.docx", b"PK not really a docx");
    fake.gh
        .put_file(REPO, "main", "e.pdf", b"%PDF-1.4 not really");
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");

    let mut reads = blob_reads(&fake);
    let before = reads.len();
    reads.sort();
    reads.dedup();
    assert_eq!(reads.len(), before, "no blob twice");
    let read = |b: &[u8]| blob_reads(&fake).contains(&FakeGithub::blob_sha(b));
    assert!(read(unknown), "an unknown extension is read once");
    for (_, b) in text {
        assert!(read(b));
    }
    assert!(read(b"PK not really a docx") && read(b"%PDF-1.4 not really"));
    assert!(
        !read(b"# looks like text\n"),
        "a known binary extension is never read"
    );
    assert_eq!(before, 1 + text.len() + 2, "exactly the text files");

    // The row is `binary` for the binary names.
    let entries = d.entries("g");
    for name in ["a.png", "b.zip", "c.woff2", "d.jar", "e.mp4", "f.sqlite"] {
        assert_eq!(entry(&entries, name)["kind"], json!("binary"), "{name}");
        assert_eq!(entry(&entries, name)["state"], json!("remote"));
    }
    d.close();
}

#[test]
fn the_size_cap_a_file_exactly_at_it_is_read_and_one_byte_over_is_not() {
    let fake = Fake::start();
    fake.gh.add_repo(REPO, "main");
    // Short lines: a parse of 20 MB must not be a 20 MB line.
    let line = format!("{}\n", "word ".repeat(15));
    let fill = |len: u64| -> Vec<u8> {
        let mut v = Vec::with_capacity(len as usize);
        while (v.len() as u64) < len {
            v.extend_from_slice(line.as_bytes());
        }
        v.truncate(len as usize);
        v
    };
    let at = fill(MAX_INDEXED_BYTES);
    let over = fill(MAX_INDEXED_BYTES + 1);
    // Different bytes, so the fake's sha-addressed blobs differ.
    let mut over = over;
    over[0] = b'X';
    fake.gh.put_file(REPO, "main", "at-cap.txt", &at);
    fake.gh.put_file(REPO, "main", "over-cap.txt", &over);
    fake.gh.put_file(REPO, "main", "small.txt", b"small\n");
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");

    let reads = blob_reads(&fake);
    assert!(
        reads.contains(&FakeGithub::blob_sha(&at)),
        "exactly at the cap: read"
    );
    assert!(
        !reads.contains(&FakeGithub::blob_sha(&over)),
        "one byte over: never read"
    );
    let entries = d.entries("g");
    assert_eq!(
        entry(&entries, "over-cap.txt")["size"],
        json!(MAX_INDEXED_BYTES + 1)
    );
    assert_eq!(entry(&entries, "over-cap.txt")["state"], json!("remote"));
    d.close();
}

#[test]
fn an_lfs_pointer_a_submodule_and_a_symlink_are_listed_by_name_and_never_fetched() {
    let fake = Fake::start();
    fake.gh.add_repo(REPO, "main");
    let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:4d7a214614ab2935c943f9e0ff69d22eadbb8f32b1258daaa5e2ca24d17e2393\nsize 12345\n";
    fake.gh.put_file(REPO, "main", "model.weights", pointer);
    fake.gh.put_file(REPO, "main", "README.md", b"# Hi\n");
    fake.gh.put_submodule(
        REPO,
        "main",
        "vendor/lib",
        "https://github.com/other/lib.git",
    );
    fake.gh.put_symlink(REPO, "main", "latest", "README.md");
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    d.wait_active("g");

    let entries = d.entries("g");
    for (path, reason) in [
        ("vendor/lib", "submodule"),
        ("latest", "symlink"),
        ("model.weights", "lfs_pointer"),
    ] {
        let e = entry(&entries, path);
        assert_eq!(e["state"], json!("remote"), "{path}");
        assert_eq!(e["cache_bytes"], json!(0), "{path}");
        assert_eq!(e["reason"], json!(reason), "{path}");
    }
    // Requests: the pointer's own blob and the README, nothing else. No
    // LFS endpoint, no other repository, no symlink target, no submodule.
    for r in fake.gh.request_log().all() {
        let t = r.target();
        assert!(!t.contains("lfs") || t.contains("/git/blobs/"), "{t}");
        assert!(!t.contains("other/lib"), "{t}");
    }
    let reads = blob_reads(&fake);
    assert_eq!(reads.len(), 2, "{reads:?}");
    assert!(
        !reads.contains(&FakeGithub::blob_sha(b"README.md")),
        "a symlink is not followed"
    );
    // The pointer is kept by name: no parse rows, no hash.
    let cache = Cache::open_read_only(&d.cache_path).unwrap();
    let source = cache.get_source("g").unwrap().unwrap();
    let unit = cache
        .get_unit_by_rel_path(source.id, "model.weights")
        .unwrap()
        .unwrap();
    assert!(unit.hash.is_none());
    d.close();
}

#[test]
fn a_rate_limit_during_the_index_loses_no_listing_and_the_index_continues() {
    let fake = Fake::start();
    fake.atlas();
    // Slow answers keep the first index running while the limit is set.
    fake.gh.set_latency(Duration::from_millis(150));
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    fake.gh
        .rate_limit_requests(2, tephra_remote::testing::GithubLimit::RetryAfter429);
    let listed = d.entries("g");

    let row = d.wait_active("g");
    assert_eq!(row["index_done"], json!(3));
    assert_ne!(row["status"], json!("auth_required"));
    assert_eq!(d.entries("g").len(), listed.len(), "the listing is intact");
    // Every text file was indexed in the end.
    let cache = Cache::open_read_only(&d.cache_path).unwrap();
    let source = cache.get_source("g").unwrap().unwrap();
    for p in ["README.md", "docs/Guide.md", "src/lib.rs"] {
        let unit = cache.get_unit_by_rel_path(source.id, p).unwrap().unwrap();
        assert!(unit.hash.is_some(), "{p}");
    }
    d.close();
}

#[test]
fn a_branch_that_moves_mid_index_is_not_mixed_in_blobs_are_read_by_the_listing_sha() {
    let fake = Fake::start();
    fake.atlas();
    fake.gh.set_latency(Duration::from_millis(200));
    let d = Daemon::start(&fake);
    let added = d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    let listed_at = added["commit"].as_str().unwrap().to_string();
    // The branch moves: a new file, and a changed README.
    let new_readme = b"# Atlas v2\n\nTotally different.\n";
    fake.gh.put_file(REPO, "main", "later.md", b"# Later\n");
    fake.gh.put_file(REPO, "main", "README.md", new_readme);
    assert_ne!(fake.gh.head_sha(REPO, "main"), listed_at);
    d.wait_active("g");

    let entries = d.entries("g");
    assert!(
        !entries.iter().any(|e| e["rel_path"] == json!("later.md")),
        "the new commit's file is not mixed into this listing"
    );
    let reads = blob_reads(&fake);
    assert!(
        !reads.contains(&FakeGithub::blob_sha(new_readme))
            && !reads.contains(&FakeGithub::blob_sha(b"# Later\n")),
        "no blob of the new commit was read: {reads:?}"
    );
    let log = fake.gh.request_log().all();
    assert!(
        !log.iter().any(|r| r.target().contains("/contents/")),
        "no by-path read that could follow the branch"
    );
    d.close();
}

#[test]
fn progress_is_indexing_done_of_total_then_active() {
    let fake = Fake::start();
    fake.atlas();
    // Slow enough that a stalled test thread (a loaded machine) still sees
    // the index mid-way: three blob reads of 700 ms each.
    fake.gh.set_latency(Duration::from_millis(700));
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    let row = d.source_row("g").unwrap();
    assert_eq!(row["status"], json!("indexing"));
    assert_eq!(row["index_total"], json!(3));
    assert!(row["index_done"].as_u64().unwrap() < 3, "{row}");
    let done = d.wait_active("g");
    assert_eq!(
        (done["index_done"].as_u64(), done["index_total"].as_u64()),
        (Some(3), Some(3))
    );
    assert!(
        done["last_scanned_at"].is_number(),
        "a last sync time: {done}"
    );
    d.close();
}

#[test]
fn a_rejected_token_mid_index_leaves_the_listing_and_says_auth_required() {
    let fake = Fake::start();
    fake.atlas();
    // Slow enough that the token is revoked before the last blob is read,
    // even when this thread is stalled by a loaded machine.
    fake.gh.set_latency(Duration::from_millis(700));
    let d = Daemon::start(&fake);
    d.ok("source.add_github", json!({"name": "g", "repo": REPO}));
    fake.gh.revoke_token();
    let row = d.wait_for("auth_required", || {
        d.source_row("g")
            .filter(|r| r["status"] == json!("auth_required"))
    });
    assert!(row["status_reason"]
        .as_str()
        .unwrap()
        .contains("tephractl auth github"));
    let files = d
        .entries("g")
        .iter()
        .filter(|e| e["kind"] != "folder")
        .count();
    assert_eq!(files, 4, "the listing stays");
    d.close();
}
