//! PR-8: eviction against the things that happen at the same time (a fetch,
//! a save, a failing disk, a crash). The pass is stopped between "chosen"
//! and "claimed" by the registry's test hook, so each race is made to
//! happen in a fixed order and no test sleeps to get it.
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use remote_support::{cache_status, DriveRig, FetchRig, Tuning};
use serde_json::{json, Value};

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

fn logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(Sink(buf.clone()))
                .finish(),
        );
        buf
    })
    .clone()
}

fn log_text() -> String {
    String::from_utf8_lossy(&logs().lock().unwrap()).to_string()
}

const README: &[u8] = b"# Atlas\n\nSee [[@vault:note]] and [[Guide]].\n\n## Setup\n";

fn ok(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "{reply}");
    &reply["result"]
}

/// A rig with README fetched, the clock past the idle window.
fn idle_readme() -> FetchRig {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    rig.clock.advance(Duration::from_secs(700));
    rig
}

/// Runs `f` the first time the pass has chosen `README.md` and not yet
/// claimed it.
fn on_chosen(rig: &FetchRig, f: impl Fn() + Send + Sync + 'static) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    rig.d
        .registry
        .set_evict_hook(Some(Arc::new(move |_source, path| {
            if path == "README.md" && seen.fetch_add(1, Ordering::SeqCst) == 0 {
                f();
            }
        })));
    calls
}

/// Race: a fetch touches `last_used` after the pass picked the file; the
/// claim sees it and the file is kept, and nobody gets an error.
#[test]
fn a_fetch_after_the_pick_wins_and_the_file_stays() {
    let rig = Arc::new(idle_readme());
    let fetcher = rig.clone();
    let reply = Arc::new(Mutex::new(Value::Null));
    let got = reply.clone();
    let calls = on_chosen(&rig, move || {
        *got.lock().unwrap() = fetcher.fetch("README.md");
    });
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the pass reached the file"
    );
    assert_eq!(set["dropped"], json!(0), "{set}");
    let fetched = reply.lock().unwrap().clone();
    assert_eq!(ok(&fetched)["state"], json!("cached"), "{fetched}");
    assert_eq!(rig.row("README.md").state, "cached");
    assert_eq!(std::fs::read(rig.cached_path("README.md")).unwrap(), README);
    rig.d.registry.set_evict_hook(None);
}

/// Race: Dan saves the cached file before the watcher has made it
/// `pending_upload` or `not_pushed`. The bytes differ from the remote's,
/// so the pass keeps the file, whatever the row says.
#[test]
fn a_save_after_the_pick_is_not_deleted() {
    let rig = Arc::new(idle_readme());
    let path = rig.cached_path("README.md");
    let edit = path.clone();
    let calls = on_chosen(&rig, move || {
        std::fs::write(&edit, b"# Atlas\n\nMy own edit, saved just now.\n").unwrap();
    });
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert!(calls.load(Ordering::SeqCst) >= 1);
    assert_eq!(set["dropped"], json!(0), "{set}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"# Atlas\n\nMy own edit, saved just now.\n"
    );
    assert_eq!(rig.row("README.md").state, "cached", "put back, not lost");
    rig.d.registry.set_evict_hook(None);
    // The same size with other bytes is caught too (the hash differs).
    let same_size: Vec<u8> = README.iter().map(|b| b.to_ascii_uppercase()).collect();
    std::fs::write(&path, &same_size).unwrap();
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(0), "{set}");
    assert_eq!(std::fs::read(&path).unwrap(), same_size);
}

/// A delete that fails (permission denied) leaves the item `cached` with a
/// logged reason, and the pass goes on to the next file; the next pass,
/// with the disk healthy, drops it.
#[test]
fn a_failed_delete_leaves_the_item_cached_logs_why_and_the_pass_carries_on() {
    let _ = logs();
    let rig = FetchRig::start(|fake| {
        fake.gh
            .put_file(remote_support::REPO, "main", "z.txt", b"zzz");
    });
    ok(&rig.fetch("README.md"));
    ok(&rig.fetch("z.txt"));
    rig.clock.advance(Duration::from_secs(700));
    rig.probe.fail_remove(true);
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(0), "{set}");
    // A retry by the timer puts a row at `remote` for the moment between
    // its claim and the failed delete, so the settled state is waited for.
    for path in ["README.md", "z.txt"] {
        rig.d.wait_for("put back to cached", || {
            (rig.row(path).state == "cached").then_some(())
        });
    }
    assert!(rig.cached_path("README.md").exists());
    assert!(
        rig.probe.removes.lock().unwrap().len() >= 2,
        "both deletes were tried"
    );
    let text = log_text();
    assert!(text.contains("could not be dropped"), "a reason is logged");
    assert!(text.contains("permission denied"), "and it says why");
    assert!(
        cache_status(&rig.d)["cache_bytes"].as_u64().unwrap() > 0,
        "the file is still counted"
    );

    rig.probe.fail_remove(false);
    rig.d.wait_for("dropped once the disk allows it", || {
        (rig.row("README.md").state == "remote" && !rig.cached_path("README.md").exists())
            .then_some(())
    });
}

/// Crash after the delete, before the row: the row says `cached`, the file
/// is gone. At the next start the item is `remote` and its bytes are not
/// counted.
#[test]
fn a_cached_row_whose_file_is_gone_is_remote_again_after_a_restart() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    assert_eq!(cache_status(&rig.d)["cache_bytes"], json!(README.len()));
    std::fs::remove_file(rig.cached_path("README.md")).unwrap();
    let rig = rig.restart(Tuning::default());
    assert_eq!(rig.row("README.md").state, "remote");
    let status = cache_status(&rig.d);
    assert_eq!(status["cache_bytes"], json!(0), "{status}");
    assert_eq!(rig.d.source_row("g").unwrap()["cache_bytes"], json!(0));
}

/// Crash after the claim, before the delete: the row says `remote` and the
/// file is still there. At the next start the file is the remote's bytes,
/// so the item is `cached` again: counted once (not twice, not negatively),
/// no download, and the next pass drops it, which leaves no file on disk.
#[test]
fn a_remote_row_whose_file_is_still_there_is_cached_again_after_a_restart() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    remote_support::exec_sql(
        &rig.d.cache_path,
        "UPDATE remote_items SET state = 'remote' WHERE source = 'g' AND path = 'README.md';",
    );
    let reads = rig.reads();
    let rig = rig.restart(Tuning::default());
    assert_eq!(rig.row("README.md").state, "cached");
    let status = cache_status(&rig.d);
    assert_eq!(status["cache_bytes"], json!(README.len()), "{status}");
    assert_eq!(
        rig.d.source_row("g").unwrap()["cache_bytes"],
        json!(README.len())
    );
    assert_eq!(rig.reads(), reads, "nothing was downloaded");
    assert!(rig.cached_path("README.md").exists());

    // A second restart changes nothing: counted once.
    let rig = rig.restart(Tuning::default());
    assert_eq!(rig.row("README.md").state, "cached");
    assert_eq!(cache_status(&rig.d)["cache_bytes"], json!(README.len()));

    // It is evictable again, and the drop leaves nothing on disk (F1).
    rig.clock.advance(Duration::from_secs(700));
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(1), "{set}");
    assert_eq!(rig.row("README.md").state, "remote");
    assert!(!rig.cached_path("README.md").exists());
    assert_eq!(cache_status(&rig.d)["cache_bytes"], json!(0));
}

/// The same crash, but the file on disk is not the remote's bytes (Dan saved
/// over it): it stays, untouched, and is not deleted by the start or by a
/// pass; the row is not made `cached`.
#[test]
fn a_remote_row_whose_file_differs_is_left_alone_after_a_restart() {
    let rig = FetchRig::start(|_| {});
    ok(&rig.fetch("README.md"));
    let mine = b"# Atlas\n\nMy own edit.\n";
    std::fs::write(rig.cached_path("README.md"), mine).unwrap();
    remote_support::exec_sql(
        &rig.d.cache_path,
        "UPDATE remote_items SET state = 'remote' WHERE source = 'g' AND path = 'README.md';",
    );
    let rig = rig.restart(Tuning::default());
    assert_eq!(rig.row("README.md").state, "remote");
    assert_eq!(cache_status(&rig.d)["cache_bytes"], json!(0));
    rig.clock.advance(Duration::from_secs(700));
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(0), "{set}");
    assert_eq!(std::fs::read(rig.cached_path("README.md")).unwrap(), mine);
    assert_eq!(rig.row("README.md").state, "remote");
}

/// A pass that has nothing to do while a fetch of the same file runs does
/// not touch it: a file `downloading` is never dropped.
#[test]
fn a_file_that_is_downloading_is_never_dropped() {
    let rig = Arc::new(FetchRig::start(|_| {}));
    rig.fake.gh.set_latency(Duration::from_millis(1200));
    let fetcher = rig.clone();
    let t = std::thread::spawn(move || fetcher.fetch("README.md"));
    rig.d.wait_for("downloading", || {
        (rig.row("README.md").state == "downloading").then_some(())
    });
    rig.clock.advance(Duration::from_secs(700));
    let set = rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(0), "{set}");
    let reply = t.join().unwrap();
    assert_eq!(ok(&reply)["state"], json!("cached"), "{reply}");
}

/// Race (R1): Dan saves a fetched Drive file with an edit of the same
/// length, and no watcher has told Tephra. A Drive listing has no checksum
/// and a size alone cannot tell the two copies apart, so the hash noted at
/// download is what decides: the edit is kept.
#[test]
fn a_same_size_edit_of_a_drive_copy_is_not_deleted() {
    let t = DriveRig::start(&[("id-a", b"teh plan")], &[("a.txt", "file", "id-a", 8)]);
    ok(&t.fetch("a.txt"));
    let path = t.root().join("a.txt");
    let fixed = b"the plan";
    assert_eq!(fixed.len(), 8);
    std::fs::write(&path, fixed).unwrap();
    t.rig.clock.advance(Duration::from_secs(700));
    let set = t.rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(0), "{set}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        fixed,
        "the edit is still there"
    );
    assert_eq!(t.state("a.txt"), "cached");
    // Time passing and a pass by the timer do not change that.
    t.rig.clock.advance(Duration::from_secs(3600));
    let set = t.rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(0), "{set}");
    assert_eq!(std::fs::read(&path).unwrap(), fixed);

    // The same file, unedited, is dropped (so the check is not just "never").
    let t2 = DriveRig::start(&[("id-a", b"teh plan")], &[("a.txt", "file", "id-a", 8)]);
    ok(&t2.fetch("a.txt"));
    t2.rig.clock.advance(Duration::from_secs(700));
    let set = t2.rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert_eq!(set["dropped"], json!(1), "{set}");
    assert!(!t2.root().join("a.txt").exists());
}

/// The same save, made after the pass picked the file and before it claimed
/// it (the registry's hook): the length and, within the clock's grain, the
/// modification time can be the same, so the bytes are read again after the
/// claim and the file is put back, not deleted.
#[test]
fn a_same_size_save_of_a_drive_copy_after_the_pick_is_not_deleted() {
    let t = DriveRig::start(&[("id-a", b"teh plan")], &[("a.txt", "file", "id-a", 8)]);
    ok(&t.fetch("a.txt"));
    let path = t.root().join("a.txt");
    let edit = path.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    t.rig
        .d
        .registry
        .set_evict_hook(Some(Arc::new(move |_source, p| {
            if p == "a.txt" && seen.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::write(&edit, b"the plan").unwrap();
            }
        })));
    t.rig.clock.advance(Duration::from_secs(700));
    let set = t.rig.d.ok("cache.set_limit", json!({"limit": "0"}));
    assert!(
        calls.load(Ordering::SeqCst) >= 1,
        "the pass reached the file"
    );
    assert_eq!(set["dropped"], json!(0), "{set}");
    assert_eq!(std::fs::read(&path).unwrap(), b"the plan");
    t.rig.d.registry.set_evict_hook(None);
}
