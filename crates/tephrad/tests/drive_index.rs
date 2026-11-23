//! PR-10 (G8, G9, GP3, E8, E10): the first index of a Drive source. The
//! tree is listed first (names, kinds, no content), then every text file is
//! read exactly once, parsed and dropped. Real registry, poller and RPC
//! server against the PR-3 fake Drive.
#![allow(dead_code)]

mod remote_support;

use remote_support::{files_under, item_rows, DriveDaemon};
use serde_json::{json, Value};
use tephrad::file_kind::MAX_INDEXED_BYTES;

const SHEET: &str = "application/vnd.google-apps.spreadsheet";
const SLIDES: &str = "application/vnd.google-apps.presentation";
const FORM: &str = "application/vnd.google-apps.form";
const DRAWING: &str = "application/vnd.google-apps.drawing";
const SHORTCUT: &str = "application/vnd.google-apps.shortcut";

fn mixed(d: &tephra_remote::testing::FakeDrive) {
    d.add_folder("f", "Notes", None);
    d.add_file(
        "a",
        "Plan.md",
        "text/markdown",
        Some("f"),
        b"# Plan\n\nSee [[Other]] and [[@vault:x]].\n",
    );
    d.add_file("b", "Other.md", "text/markdown", Some("f"), b"# Other\n");
    d.add_file("t", "todo.txt", "text/plain", Some("f"), b"todo\n");
    d.add_file(
        "c",
        "main.rs",
        "text/x-rust",
        Some("f"),
        b"pub fn main() {}\n",
    );
    d.add_doc("g", "Meeting", Some("f"), "# Meeting\n\nSee [[Plan]].\n");
    d.add_native("s", "Budget", SHEET, Some("f"));
    d.add_native("sl", "Deck", SLIDES, Some("f"));
    d.add_native("fo", "Survey", FORM, Some("f"));
    d.add_native("dr", "Sketch", DRAWING, Some("f"));
    d.add_native("sh", "Link", SHORTCUT, Some("f"));
    d.add_file(
        "p",
        "logo.png",
        "image/png",
        Some("f"),
        &[0x89, b'P', b'N', b'G', 0, 1, 2],
    );
    d.add_folder("sub", "Sub", Some("f"));
    d.add_file(
        "d",
        "deep.md",
        "text/markdown",
        Some("sub"),
        b"# Deep\n\n[[Plan]]\n",
    );
}

fn kind_of(rig: &DriveDaemon, path: &str) -> String {
    rig.listing("g")
        .into_iter()
        .find(|(p, _)| p == path)
        .unwrap_or_else(|| panic!("{path} listed"))
        .1
}

#[test]
fn every_text_file_is_read_once_and_everything_else_is_listed_by_name() {
    let rig = DriveDaemon::start(mixed);
    let row = rig.add_active("g", "Notes");
    assert_eq!(row["status"], json!("active"));
    // Plan, Other, todo, main.rs, the Doc, deep.md.
    assert_eq!(row["index_total"], json!(6), "{row}");
    assert_eq!(row["index_done"], json!(6));
    assert_eq!(
        rig.media_reads(),
        5,
        "the four text files and deep.md, by media download"
    );
    assert_eq!(rig.exports(), 1, "the Doc, through one Markdown export");
    let log = rig.drive.request_log();
    let exports = log.matching("/export");
    assert!(
        exports[0].target().contains("text%2Fmarkdown")
            || exports[0].target().contains("text/markdown")
    );
    for id in ["s", "sl", "fo", "dr", "sh", "p"] {
        assert!(
            log.matching(&format!("/files/{id}?"))
                .iter()
                .all(|r| !r.target().contains("alt=media")),
            "{id} is never read"
        );
    }
    // Kinds visible in `ls`.
    assert_eq!(kind_of(&rig, "Meeting.md"), "gdoc");
    assert_eq!(kind_of(&rig, "Budget"), "gsheet");
    assert_eq!(kind_of(&rig, "Deck"), "gslides");
    assert_eq!(kind_of(&rig, "Survey"), "gform");
    assert_eq!(kind_of(&rig, "Sketch"), "gdrawing");
    assert_eq!(kind_of(&rig, "Link"), "gshortcut");
    assert_eq!(kind_of(&rig, "Plan.md"), "prose");
    assert_eq!(kind_of(&rig, "main.rs"), "code");
    assert_eq!(kind_of(&rig, "logo.png"), "binary");
    assert_eq!(kind_of(&rig, "Sub/deep.md"), "prose");
    let entries = rig.d.entries("g");
    let sheet = entries
        .iter()
        .find(|e| e["rel_path"] == json!("Budget"))
        .unwrap();
    // `unit.list` (and so `tephractl ls`) shows what Drive calls it (GP3),
    // while the graph keeps it as a binary unit.
    assert_eq!(sheet["kind"], json!("gsheet"));
    let kinds: std::collections::HashMap<String, String> = entries
        .iter()
        .map(|e| {
            (
                e["rel_path"].as_str().unwrap().to_string(),
                e["kind"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(kinds["Meeting.md"], "gdoc");
    assert_eq!(kinds["Deck"], "gslides");
    assert_eq!(kinds["Survey"], "gform");
    assert_eq!(kinds["Sketch"], "gdrawing");
    assert_eq!(kinds["Link"], "gshortcut");
    assert_eq!(kinds["logo.png"], "binary");
    assert_eq!(kinds["Plan.md"], "prose");
    assert_eq!(sheet["state"], json!("remote"));
    // Nothing on disk, and no content in the cache.
    assert!(files_under(&rig.d.data_dir.join("remote")).is_empty());
    assert!(entries.iter().all(|e| e["cache_bytes"] == json!(0)));
    rig.d.close();
}

#[test]
fn links_and_backlinks_work_with_nothing_downloaded() {
    let rig = DriveDaemon::start(mixed);
    rig.add_active("g", "Notes");
    let reads = rig.media_reads() + rig.exports();
    let requests = rig.drive.request_log().all().len();
    let resolved = rig
        .d
        .ok("unit.resolve_link", json!({"link": "[[@g:Plan]]"}));
    assert_eq!(resolved["unit"]["id"], json!("@g:Plan.md"), "{resolved}");
    // Forward and back, across the native Doc, a file and a subfolder.
    let back = rig
        .d
        .ok_when("graph.get_backlinks", json!({"id": "@g:Plan.md"}), |r| {
            r.as_array().is_some_and(|a| a.len() == 2)
        });
    let mut froms: Vec<&str> = back
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["from"].as_str().unwrap())
        .collect();
    froms.sort_unstable();
    assert_eq!(froms, ["@g:Meeting.md", "@g:Sub/deep.md"]);
    let forward = rig
        .d
        .ok("graph.get_forward_links", json!({"id": "@g:Plan.md"}));
    assert!(forward
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["to"] == json!({"resolved": "@g:Other.md"})));
    assert_eq!(
        rig.media_reads() + rig.exports(),
        reads,
        "no read after the index"
    );
    assert_eq!(
        rig.drive.request_log().all().len(),
        requests,
        "Drive was not asked anything"
    );
    rig.d.close();
}

#[test]
fn the_listing_is_visible_before_a_single_file_is_read() {
    let rig = DriveDaemon::start(mixed);
    rig.drive.hold_content_after(2);
    rig.add("g", "Notes");
    // Mid-index: the names are all there, the status counts the reading.
    let row = rig.d.wait_for("two files read", || {
        let r = rig.d.source_row("g")?;
        (r["index_done"] == json!(2)).then_some(r)
    });
    assert_eq!(row["status"], json!("indexing"));
    assert_eq!(row["index_total"], json!(6));
    let listed = rig.d.entries("g");
    let files = listed
        .iter()
        .filter(|e| e["kind"] != json!("folder"))
        .count();
    assert_eq!(files, 12, "every file is listed already: {listed:?}");
    rig.drive.release_content();
    let row = rig.d.wait_active("g");
    assert_eq!(row["index_done"], json!(6));
    rig.d.close();
}

#[test]
fn a_big_file_a_wrong_size_and_a_long_name_are_handled() {
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        d.add_file("big", "big.md", "text/markdown", Some("f"), b"# big\n");
        d.claim_size("big", MAX_INDEXED_BYTES + 1);
        d.add_file(
            "small",
            "small.md",
            "text/markdown",
            Some("f"),
            b"# small\n",
        );
    });
    rig.add_active("g", "Notes");
    let rows = item_rows(&rig.d.cache_path, "g");
    assert_eq!(rows.len(), 2);
    // Over 20 MB: listed, never downloaded.
    assert!(rig
        .drive
        .request_log()
        .matching("/files/big?")
        .iter()
        .all(|r| !r.target().contains("alt=media")));
    assert_eq!(rig.media_reads(), 1);
    let row = rig.d.source_row("g").unwrap();
    assert_eq!(
        row["index_total"],
        json!(1),
        "the big file is not a candidate: {row}"
    );
    rig.d.close();
}

#[test]
fn unsafe_names_become_safe_unit_paths_that_never_leave_the_root() {
    let long = format!("{}.md", "n".repeat(300));
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        d.add_file("1", "a/b.md", "text/markdown", Some("f"), b"# 1\n");
        d.add_file("2", "..", "text/markdown", Some("f"), b"# 2\n");
        d.add_file(
            "3",
            "../../etc/passwd",
            "text/markdown",
            Some("f"),
            b"# 3\n",
        );
        d.add_file("4", &long, "text/markdown", Some("f"), b"# 4\n");
        d.add_file("5", ".", "text/markdown", Some("f"), b"# 5\n");
        d.add_folder("dd", "..", Some("f"));
        d.add_file("6", "in-dotdot.md", "text/markdown", Some("dd"), b"# 6\n");
    });
    rig.add_active("g", "Notes");
    let paths = rig.paths("g");
    assert_eq!(paths.len(), 6, "{paths:?}");
    let unique: std::collections::HashSet<&String> = paths.iter().collect();
    assert_eq!(unique.len(), 6, "no two share a path: {paths:?}");
    for p in &paths {
        assert!(!p.starts_with('/'), "{p}");
        assert!(
            p.split('/').all(|s| s != ".." && s != "." && !s.is_empty()),
            "{p}"
        );
        assert!(p.split('/').all(|s| s.len() <= 255), "{p}");
    }
    // `a/b.md` is one file called that, not a folder.
    assert!(!paths.iter().any(|p| p.starts_with("a/")), "{paths:?}");
    // Nothing was written outside the data dir.
    assert!(files_under(&rig.d.data_dir.join("remote")).is_empty());
    rig.d.close();
}

#[test]
fn a_shortcut_loop_a_multi_parent_file_and_an_empty_folder_neither_hang_nor_double_list() {
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        d.add_folder("a", "A", Some("f"));
        d.add_folder("b", "B", Some("f"));
        d.add_folder("empty", "Empty", Some("f"));
        d.add_native("loop", "Loop", SHORTCUT, Some("a"));
        d.add_file("m", "both.md", "text/markdown", Some("a"), b"# both\n");
        d.add_parent("m", "b");
        // A folder that is its own ancestor's child twice over.
        d.add_parent("a", "b");
        d.add_parent("b", "a");
    });
    rig.add_active("g", "Notes");
    let paths = rig.paths("g");
    assert_eq!(
        paths.iter().filter(|p| p.ends_with("both.md")).count(),
        1,
        "{paths:?}"
    );
    assert!(
        paths.contains(&"A/both.md".to_string()),
        "first parent wins: {paths:?}"
    );
    assert!(paths.contains(&"A/Loop".to_string()), "{paths:?}");
    rig.d.close();
}

#[test]
fn an_empty_folder_is_an_active_source_with_nothing_in_it() {
    let rig = DriveDaemon::start(|d| d.add_folder("f", "Notes", None));
    let row = rig.add_active("g", "Notes");
    assert_eq!(row["index_total"], json!(0));
    assert!(rig.paths("g").is_empty());
}

#[test]
fn a_server_error_on_one_read_leaves_it_listed_and_the_rest_indexed_and_a_poll_retries() {
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        for (id, name) in [("a", "a.md"), ("b", "b.md"), ("c", "c.md")] {
            d.add_file(
                id,
                name,
                "text/markdown",
                Some("f"),
                format!("# {name}\n").as_bytes(),
            );
        }
    });
    rig.drive.hold_content_after(0);
    rig.add("g", "Notes");
    // The first read is in flight (past the fake's error script); the next
    // request to fail is a later read.
    rig.d.wait_for("a read in flight", || {
        (rig.media_reads() >= 1).then_some(())
    });
    // Three tries fail: that file stays unread for this pass.
    rig.drive.fail_next(3, 500);
    rig.drive.release_content();
    let row = rig.d.wait_for("the pass ended", || {
        let r = rig.d.source_row("g")?;
        (r["status"] == json!("active")).then_some(r)
    });
    assert_eq!(rig.paths("g"), vec!["a.md", "b.md", "c.md"], "all listed");
    assert_eq!(row["index_done"], json!(2), "{row}");
    // The failed file is read again on a later poll, and only that file.
    let before = rig.media_reads();
    rig.poll();
    assert_eq!(rig.media_reads(), before + 1, "only the file that failed");
    let row = rig.d.source_row("g").unwrap();
    assert_eq!(row["index_done"], json!(3), "{row}");
    rig.d.close();
}

#[test]
fn a_cut_short_index_keeps_its_feed_position_and_a_finished_one_resumes_from_it() {
    let rig = DriveDaemon::start(mixed);
    rig.drive.hold_content_after(1);
    rig.add("g", "Notes");
    rig.d.wait_for("one file read", || {
        (rig.d.source_row("g")?["index_done"] == json!(1)).then_some(())
    });
    // Something changes while the index is cut short.
    rig.drive
        .add_file("late", "late.md", "text/markdown", Some("f"), b"# late\n");
    let rig = rig.restart();
    rig.drive.release_content();
    rig.d.wait_active("g");
    // It missed `late` in its listing, and the saved feed position finds it.
    rig.poll();
    assert!(rig.paths("g").contains(&"late.md".to_string()));
    // The index did not start over: a finished source is not re-listed after
    // a restart, and polling carries on from its cursor.
    let rig = rig.restart();
    let tokens = rig.drive.request_log().matching("startPageToken").len();
    rig.d.wait_active("g");
    rig.poll();
    assert_eq!(
        rig.drive.request_log().matching("startPageToken").len(),
        tokens,
        "no fresh start position"
    );
    let _: Value = json!(null);
}

#[test]
fn a_big_folder_is_listed_through_every_page_before_any_read() {
    let rig = DriveDaemon::start(|d| {
        d.set_page_size(7);
        d.add_folder("f", "Notes", None);
        for n in 0..60 {
            d.add_file(
                &format!("x{n}"),
                &format!("n{n:03}.md"),
                "text/markdown",
                Some("f"),
                b"# n\n",
            );
        }
    });
    rig.drive.hold_content_after(0);
    rig.add("g", "Notes");
    assert_eq!(rig.d.entries("g").len(), 60);
    rig.drive.release_content();
    let row = rig.d.wait_active("g");
    assert_eq!(row["index_done"], json!(60));
    assert_eq!(rig.media_reads(), 60, "each read once");
}

/// E8 at its stated size: 10,000 files, the fake's own paging (the adapter
/// asks for 100 a page, so 100 pages). The feed's start token is STORED
/// before the first listing page is asked for (the listing is held at its
/// first page while the cache is read), and the whole listing is in the
/// index before the first content read. (A change made while the reads are
/// going is found from that stored token: see
/// `a_cut_short_index_keeps_its_feed_position_and_a_finished_one_resumes_from_it`.)
#[test]
fn ten_thousand_files_are_listed_before_any_read_and_the_start_token_is_stored_first() {
    const FILES: usize = 10_000;
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        for n in 0..FILES {
            d.add_file(
                &format!("x{n}"),
                &format!("n{n:05}.md"),
                "text/markdown",
                Some("f"),
                b"# n\n",
            );
        }
    });
    rig.drive.hold_content_after(0);
    // The folder is found by its name through one page of the Drive root;
    // the pages of the folder itself are held from the first one.
    rig.drive.hold_listing_after(1);
    let listing_requests = || rig.drive.request_log().matching("%27f%27%20in%20parents");
    std::thread::scope(|scope| {
        let add = scope.spawn(|| rig.add("g", "Notes"));
        rig.d.wait_for("the first listing page to arrive", || {
            (!listing_requests().is_empty()).then_some(())
        });

        // The daemon has asked for the first page and is waiting for it.
        // The start token is in the cache already, and nothing of the
        // listing is.
        let cache = tephra_cache::Cache::open_read_only(&rig.d.cache_path).unwrap();
        let state = cache.remote_poll_state("g").unwrap().expect("a cursor row");
        let asked = rig.drive.request_log().matching("startPageToken").len();
        assert_eq!(asked, 1, "one start token, asked for before the listing");
        assert!(
            state.commit.is_some(),
            "the feed position is stored before the first listing page: {state:?}"
        );
        assert!(!state.index_complete);
        assert!(
            cache.remote_items("g").unwrap().is_empty(),
            "no file is listed yet"
        );
        drop(cache);

        rig.drive.release_listing();
        add.join().expect("the add did not panic");
    });
    rig.d.wait_for("the first read to arrive", || {
        (rig.media_reads() >= 1).then_some(())
    });

    // The whole listing is in the index, and no file has been read.
    assert_eq!(rig.d.entries("g").len(), FILES, "every file is listed");
    let log = rig.drive.request_log().all();
    let at = |needle: &str| -> Vec<usize> {
        log.iter()
            .enumerate()
            .filter(|(_, r)| r.target().contains(needle))
            .map(|(i, _)| i)
            .collect()
    };
    let token = at("startPageToken");
    // The pages of the picked folder (not the lookup of the folder itself).
    let listing = at("%27f%27%20in%20parents");
    let reads = at("alt=media");
    assert_eq!(token.len(), 1, "one start token");
    assert!(
        listing.len() >= FILES / 100,
        "the listing is paged: {} requests",
        listing.len()
    );
    assert!(
        token[0] < listing[0],
        "the start token is taken before the first listing page"
    );
    assert!(
        listing.last().unwrap() < reads.first().unwrap(),
        "every listing page comes before the first content read"
    );
    let state = {
        let cache = tephra_cache::Cache::open_read_only(&rig.d.cache_path).unwrap();
        cache.remote_poll_state("g").unwrap().expect("a cursor row")
    };
    assert!(
        state.commit.is_some() && state.etag.is_none(),
        "the feed position stays, the listing mark is gone: {state:?}"
    );

    // The reads were held the whole time; let go so the daemon can stop.
    rig.drive.release_content();
    rig.d.close();
}

/// E8: a daemon stopped while the folder was being listed has the feed
/// position and no listing. The next start lists it again from scratch (a
/// fresh start token, every page) before any read, and the source ends up
/// as if nothing had happened: it is never left `active` with an empty index.
#[test]
fn a_source_cut_off_while_its_folder_was_listed_is_listed_again_at_the_next_start() {
    let rig = DriveDaemon::start(mixed);
    // What `register_drive` leaves when the daemon stops after it stored the
    // start token and before the listing was stored.
    let rig = rig.restart_after(|cache_path| {
        let cache = tephra_cache::Cache::open(cache_path).unwrap();
        cache
            .begin_remote_source(&tephra_cache::NewRemoteSource {
                name: "g",
                kind: "drive",
                target: "f",
                branch: None,
                cursor: "100",
                now: 1,
            })
            .unwrap();
    });
    let row = rig.d.wait_active("g");
    assert_eq!(row["status_reason"], json!(null), "{row}");
    let listed = rig.paths("g");
    assert!(listed.contains(&"Plan.md".to_string()), "{listed:?}");
    assert!(listed.contains(&"Sub/deep.md".to_string()), "{listed:?}");
    assert_eq!(
        rig.drive.request_log().matching("startPageToken").len(),
        1,
        "a fresh start token for the new listing"
    );
    let log = rig.drive.request_log().all();
    let first_read = log.iter().position(|r| r.target().contains("alt=media"));
    let last_page = log
        .iter()
        .rposition(|r| r.target().contains("%27f%27%20in%20parents"));
    assert!(
        last_page.unwrap() < first_read.unwrap(),
        "the listing comes before any read"
    );
    let state = {
        let cache = tephra_cache::Cache::open_read_only(&rig.d.cache_path).unwrap();
        cache.remote_poll_state("g").unwrap().expect("a cursor row")
    };
    assert!(state.index_complete && state.etag.is_none(), "{state:?}");
    rig.d.close();
}
