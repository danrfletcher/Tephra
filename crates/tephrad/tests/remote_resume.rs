//! PR-6 (E8): a GitHub source across a restart. A completed index survives
//! it: the next start reads no tree and no blob, sends the stored ETag when
//! the poller next runs, and writes nothing if nothing changed. An index cut
//! short by a kill, or one whose cursor row is gone, is finished or redone at
//! the next start. A schema rebuild keeps the cursor
//! (`tephra-cache/tests/remote_sync.rs`). The 10,000-file first index is
//! `#[ignore]`d for the default run and run once by
//! `scripts/verify/remote-pr-6.sh`.
#![allow(dead_code)]

mod remote_support;

use std::time::{Duration, Instant};

use remote_support::{index_snapshot, Rig, REPO};
use serde_json::{json, Value};
use tephra_cache::Cache;

fn s(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn file_paths(r: &Rig) -> Vec<String> {
    let mut v: Vec<String> =
        r.d.entries("g")
            .iter()
            .filter(|e| e["kind"] != "folder")
            .map(|e| e["rel_path"].as_str().unwrap().to_string())
            .collect();
    v.sort();
    v
}

#[test]
fn a_completed_index_survives_a_restart_with_no_tree_read_no_blob_read_and_no_write() {
    let r = Rig::start();
    let listed = file_paths(&r);
    let before = index_snapshot(&r.d.cache_path);
    let first_etag = r
        .fake
        .polls()
        .last()
        .and_then(|p| p.header("etag").map(str::to_string));
    let _ = first_etag;

    let r = r.restart();
    r.fake.gh.request_log().clear();
    // The listing answers at once, from the stored index.
    assert_eq!(file_paths(&r), listed);
    let row = r.wait_status("active");
    assert_eq!(row["index_done"], row["index_total"]);
    r.quiet();
    assert!(r.fake.tree_reads().is_empty(), "no tree read at start");
    assert!(r.fake.reads().is_empty(), "no blob read at start");
    assert_eq!(r.fake.requests(), 0, "the start asked GitHub nothing");
    assert_eq!(index_snapshot(&r.d.cache_path), before, "nothing written");

    // The first poll after the restart is conditional, with the stored ETag,
    // and a 304 writes nothing either.
    r.advance(s(61));
    r.wait_polls(1);
    r.quiet();
    let polls = r.fake.polls();
    assert_eq!(polls.len(), 1);
    let sent = polls[0].header("if-none-match");
    assert!(sent.is_some(), "the stored ETag is sent: {:?}", polls[0]);
    assert!(r.fake.tree_reads().is_empty() && r.fake.reads().is_empty());
    assert_eq!(index_snapshot(&r.d.cache_path), before);
    r.d.close();
}

#[test]
fn the_stored_etag_is_the_one_the_server_issued() {
    let r = Rig::start();
    r.fake.gh.request_log().clear();
    r.advance(s(61));
    r.wait_polls(1);
    r.quiet();
    let polls_before = r.fake.polls();
    let sent_before = polls_before[0].header("if-none-match").unwrap().to_string();
    let r = r.restart();
    r.fake.gh.request_log().clear();
    r.advance(s(61));
    r.wait_polls(1);
    r.quiet();
    let sent_after = r.fake.polls()[0]
        .header("if-none-match")
        .unwrap()
        .to_string();
    assert_eq!(
        sent_before, sent_after,
        "the same ETag before and after a restart"
    );
    r.d.close();
}

#[test]
fn a_change_made_while_the_daemon_was_stopped_is_found_by_the_first_poll() {
    let r = Rig::start();
    let dir_rig = r;
    // Stop, change the remote, start.
    let r = dir_rig.restart();
    r.fake
        .gh
        .put_file(REPO, "main", "later.md", b"# Later\n\nSee [[README]].\n");
    r.fake.gh.request_log().clear();
    r.advance(s(61));
    r.d.wait_for("the new file is indexed", || {
        file_paths(&r)
            .contains(&"later.md".to_string())
            .then_some(())
    });
    // One tree, one blob: the change, nothing else re-read.
    assert_eq!(r.fake.tree_reads().len(), 1);
    assert_eq!(r.fake.reads().len(), 1);
    let back = r.d.ok("graph.get_backlinks", json!({"id": "@g:README.md"}));
    assert!(back
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["from"] == json!("@g:later.md")));
    r.d.close();
}

#[test]
fn an_index_cut_short_by_a_kill_is_read_again_from_scratch_at_the_next_start() {
    // E8: an interrupted first index re-indexes from scratch on restart:
    // every text file is read again, whether or not it was read before the
    // kill. A slow fake keeps the first index running; the daemon stops in
    // the middle of it.
    let clock = std::sync::Arc::new(tephra_remote::FakeClock::at_epoch_2026());
    let fake = remote_support::Fake::start_gated(clock.clone());
    fake.atlas();
    fake.gh.set_latency(Duration::from_millis(150));
    let d = remote_support::Daemon::start_full(
        &fake,
        remote_support::MemStore::signed_in(),
        None,
        clock.clone(),
        Default::default(),
    );
    d.add_github("g", REPO);
    // At least one file is read before the kill, and not all of them.
    d.wait_for("the index is part-way", || {
        d.source_row("g").filter(|r| {
            r["index_done"].as_u64().unwrap_or(0) > 0
                && r["index_done"].as_u64().unwrap_or(0) < r["index_total"].as_u64().unwrap_or(0)
        })
    });
    let dir = d.close_keeping_data();
    {
        let cache = Cache::open_read_only(dir.path().join("cache.db")).unwrap();
        let cursor = cache.remote_cursor("g").unwrap().unwrap();
        assert!(!cursor.index_complete, "the index was cut short");
    }

    fake.gh.set_latency(Duration::ZERO);
    fake.gh.request_log().clear();
    let d = remote_support::Daemon::start_full(
        &fake,
        remote_support::MemStore::signed_in(),
        Some(dir.path().to_path_buf()),
        clock.clone(),
        Default::default(),
    );
    let row = d.wait_active("g");
    assert_eq!(row["index_done"], row["index_total"]);
    assert_eq!(row["index_total"], json!(3));
    // From scratch: all three text files were read again, including the
    // one(s) already read before the kill.
    assert_eq!(fake.reads().len(), 3, "{:?}", fake.reads());
    // Every text file is indexed: it has a hash, and README's links exist.
    let forward = d.ok("graph.get_forward_links", json!({"id": "@g:README.md"}));
    assert!(!forward.as_array().unwrap().is_empty(), "{forward}");
    let cache = Cache::open_read_only(dir.path().join("cache.db")).unwrap();
    let source = cache.get_source("g").unwrap().unwrap();
    for p in ["README.md", "docs/Guide.md", "src/lib.rs"] {
        let unit = cache.get_unit_by_rel_path(source.id, p).unwrap().unwrap();
        assert!(unit.hash.is_some(), "{p} indexed");
    }
    d.close();
}

#[test]
fn a_deleted_cursor_row_makes_the_next_start_index_from_scratch() {
    let r = Rig::start();
    let listed = file_paths(&r);
    let cache_path = r.d.cache_path.clone();
    let r = {
        // Stop the daemon, delete the cursor row, start again.
        let Rig { fake, clock, d, .. } = r;
        let dir = d.close_keeping_data();
        {
            let cache = Cache::open(&cache_path).unwrap();
            let n = cache
                .connection()
                .execute("DELETE FROM remote_cursors WHERE source = 'g'", [])
                .unwrap();
            assert_eq!(n, 1);
        }
        fake.gh.request_log().clear();
        let store = remote_support::MemStore::signed_in();
        let d = remote_support::Daemon::start_full(
            &fake,
            store,
            Some(dir.path().to_path_buf()),
            clock.clone(),
            Default::default(),
        );
        (fake, d, dir, clock)
    };
    let (fake, d, _dir, clock) = r;
    let row = d.wait_for("indexed from scratch", || {
        d.source_row("g")
            .filter(|r| r["status"] == json!("active") && r["index_total"] == json!(3))
    });
    assert_eq!(row["index_done"], json!(3));
    assert_eq!(fake.reads().len(), 3, "every text file was read again");
    // With no commit to compare, the first poll reads the tree and checks
    // the index against it.
    clock.advance(s(61));
    d.wait_for("the tree is read again", || {
        (!fake.tree_reads().is_empty()).then_some(())
    });
    let entries: Vec<Value> = d.ok(
        "unit.list",
        json!({"source": "g", "folder": "", "recursive": true}),
    )["entries"]
        .as_array()
        .unwrap()
        .clone();
    let mut now: Vec<String> = entries
        .iter()
        .filter(|e| e["kind"] != "folder")
        .map(|e| e["rel_path"].as_str().unwrap().to_string())
        .collect();
    now.sort();
    assert_eq!(now, listed);
    let cache = Cache::open_read_only(&cache_path).unwrap();
    let cursor = cache
        .remote_cursor("g")
        .unwrap()
        .expect("the cursor row is made again");
    assert!(cursor.index_complete);
    d.close();
}

/// First index of 10,000 files: the listing answers first, indexing runs in
/// the background with `indexing done/total` progress, and the source ends
/// `active`. Run by the verify script: `cargo test -p tephrad --test
/// remote_resume -- --ignored`.
#[test]
#[ignore = "10,000 files: run once by scripts/verify/remote-pr-6.sh"]
fn a_first_index_of_ten_thousand_files_lists_first_and_indexes_in_the_background() {
    const N: usize = 10_000;
    let clock = std::sync::Arc::new(tephra_remote::FakeClock::at_epoch_2026());
    let fake = remote_support::Fake::start_with_clock(clock.clone());
    fake.gh.add_repo(REPO, "main");
    let names: Vec<(String, Vec<u8>)> = (0..N)
        .map(|i| {
            (
                format!("dir{}/file{i}.md", i % 50),
                format!("# File {i}\n\nLinks to [[file{}]].\n", (i + 1) % N).into_bytes(),
            )
        })
        .collect();
    fake.gh.put_files(
        REPO,
        "main",
        names.iter().map(|(p, b)| (p.clone(), b.as_slice())),
    );
    let d = remote_support::Daemon::start_full(
        &fake,
        remote_support::MemStore::signed_in(),
        None,
        clock,
        Default::default(),
    );
    let t = Instant::now();
    d.add_github("g", REPO);
    // The add answers once the listing is in: every file is listed, and the
    // source is still `indexing` with progress.
    let row = d.source_row("g").expect("listed");
    assert_eq!(row["index_total"], json!(N), "{row}");
    let listed_after = t.elapsed();
    let entries = d.entries("g");
    assert!(
        entries.iter().filter(|e| e["kind"] != "folder").count() == N,
        "the listing answered with every file"
    );
    println!("listing answered after {listed_after:?}");
    let mut saw_progress = false;
    let start = Instant::now();
    loop {
        let row = d.source_row("g").unwrap();
        let done = row["index_done"].as_u64().unwrap();
        if row["status"] == json!("indexing") && done > 0 && done < N as u64 {
            saw_progress = true;
        }
        if row["status"] == json!("active") {
            assert_eq!(row["index_done"], json!(N));
            break;
        }
        assert!(start.elapsed() < s(1800), "the index did not finish: {row}");
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(saw_progress, "`indexing done/total` progress was visible");
    println!("indexed {N} files in {:?}", t.elapsed());
    d.close();
}
