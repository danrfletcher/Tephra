//! E15: a fix running while `link.apply_refactor` or `block.ensure_id`
//! holds the same gate or file is serialised, never interleaved, and no
//! change is lost. Real threads, a real `RefactorEngine`, a real
//! `BlockIdEngine` and the real fix writer on the same file; real files
//! in temp dirs. No mocks.
//!
//! The big test runs 50 repeats (in batches of ten at a time), each with
//! a different start order of the three writers, and checks the exact
//! final bytes: every change a writer reported as done is there, a
//! change it reported refused is not, and nothing else differs, so no
//! half-written or doubled edit can pass.

mod fixtures;

use std::fs;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Mutex};
use std::time::Duration;

use fixtures::{empty_source, tmp_files};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::block_id::BlockIdEngine;
use tephrad::refactor::fix_write::{FileFixRequest, FixWriter, RewriteMode, WriteStatus};
use tephrad::refactor::{ApplyOutcome, NoopWatcherPause, RefactorEngine};
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{GraphBackend, SharedGraph, SourceBackend};

const HOST: &str = "# Host\n\nSee [[Old A]] here.\n\nPara needs an id.\n\nSee [[Old B]] too.\n";

/// The paragraph `block.ensure_id` is asked about (1-based line).
const PARA_LINE: u64 = 5;

struct Rig {
    src: fixtures::TempSource,
    _registry: Arc<SourceRegistry>,
    refactor: Arc<RefactorEngine>,
    blocks: Arc<BlockIdEngine>,
    fix_request: FileFixRequest,
    _data: tempfile::TempDir,
}

fn rig() -> Rig {
    let src = empty_source();
    src.write("Host.md", HOST);
    src.write("Old A.md", "# A\n");
    src.write("Old B.md", "# B\n");
    let data = tempfile::tempdir().unwrap();
    let cache_path = data.path().join("cache.db");
    let registry = Arc::new(SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap());
    registry.register("notes", src.root()).unwrap();

    let graph = UnitGraph::build_from_cache(&Cache::open(&cache_path).unwrap()).unwrap();
    let edges: Vec<_> = graph
        .get_backlinks_all("@notes:Old A.md")
        .into_iter()
        .filter(|e| e.from == "@notes:Host.md")
        .collect();
    assert_eq!(edges.len(), 1, "{edges:?}");
    let fix_request = FileFixRequest::new(
        "@notes:Host.md",
        src.root().join("Host.md"),
        edges,
        "Old A.md",
        "New A.md",
        RewriteMode::Fix {
            bare_resolves: true,
        },
    );

    let shared = Arc::new(SharedGraph::new(graph));
    let refactor = Arc::new(RefactorEngine::new(
        shared as Arc<dyn GraphBackend>,
        registry.clone() as Arc<dyn SourceBackend>,
        Arc::new(NoopWatcherPause),
    ));
    let blocks = Arc::new(
        BlockIdEngine::with_parts(
            registry.clone(),
            Arc::new(tephrad::refactor::RealFs),
            Arc::new(NoopWatcherPause),
        )
        .with_settle(Duration::ZERO),
    );
    Rig {
        src,
        _registry: registry,
        refactor,
        blocks,
        fix_request,
        _data: data,
    }
}

fn host_text(rig: &Rig) -> String {
    fs::read_to_string(rig.src.root().join("Host.md")).unwrap()
}

// ---------------------------------------------------------------
// Deterministic: the second writer starts from the first one's bytes
// ---------------------------------------------------------------

/// Runs `f` on a thread; the receiver gets its result.
fn spawn<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> mpsc::Receiver<T> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx
}

#[test]
fn e15_a_fix_waits_for_a_holder_of_the_file_lock_and_starts_from_the_holders_bytes() {
    let rig = rig();
    let path = rig.src.root().join("Host.md");
    let writer = FixWriter::new(&rig.refactor);
    let req = rig.fix_request.clone();

    let guard = tephrad::file_lock::lock(&path);
    let rx = spawn(move || writer.write_file(&req));
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "the fix must wait while another writer holds the file's lock"
    );
    assert_eq!(
        host_text(&rig),
        HOST,
        "nothing written while the lock is held"
    );

    // The holder finishes its own write (as `block.ensure_id` would).
    let held_write = HOST.replace("Para needs an id.", "Para needs an id. ^k3f9a2");
    fs::write(&path, &held_write).unwrap();
    drop(guard);

    let r = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("fix finishes");
    assert_eq!(r.status, WriteStatus::Written, "{r:?}");
    assert_eq!(r.attempts, 1, "it read the holder's bytes in one go");
    assert_eq!(
        host_text(&rig),
        held_write.replace("[[Old A]]", "[[New A]]"),
        "the holder's write and the rewrite are both there"
    );
    assert!(tmp_files(rig.src.root()).is_empty());
}

#[test]
fn e15_a_fix_waits_for_the_refactor_gate_and_starts_from_what_the_apply_left() {
    let rig = rig();
    let path = rig.src.root().join("Host.md");
    let writer = FixWriter::new(&rig.refactor);
    let req = rig.fix_request.clone();

    // `link.apply_refactor` holds this lock for its whole body.
    let gate = rig.refactor.writer_gate();
    let held = gate.lock().unwrap();
    let rx = spawn(move || writer.write_file(&req));
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "the fix must wait while the apply gate is held"
    );
    assert_eq!(host_text(&rig), HOST);

    let after_apply = HOST.replace("[[Old B]]", "[[New B]]");
    fs::write(&path, &after_apply).unwrap();
    drop(held);

    let r = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("fix finishes");
    assert_eq!(r.status, WriteStatus::Written, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(
        host_text(&rig),
        after_apply.replace("[[Old A]]", "[[New A]]")
    );
    assert!(tmp_files(rig.src.root()).is_empty());
}

#[test]
fn e15_a_real_apply_in_progress_holds_the_fix_back_until_it_is_done() {
    let rig = rig();
    rig.refactor
        .preview("notes", "Old B.md", "New B.md")
        .unwrap();
    let writer = FixWriter::new(&rig.refactor);
    let req = rig.fix_request.clone();

    let refactor = rig.refactor.clone();
    let apply = spawn(move || refactor.apply("notes", "Old B.md", "New B.md"));
    // Let the apply take its gate (it then spends a while settling).
    std::thread::sleep(Duration::from_millis(60));
    let fix = spawn(move || writer.write_file(&req));

    let applied = apply
        .recv_timeout(Duration::from_secs(10))
        .unwrap()
        .unwrap();
    assert_eq!(applied, ApplyOutcome::Applied { files_changed: 1 });
    let r = fix.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(r.status, WriteStatus::Written, "{r:?}");
    assert_eq!(r.attempts, 1);
    assert_eq!(
        host_text(&rig),
        "# Host\n\nSee [[New A]] here.\n\nPara needs an id.\n\nSee [[New B]] too.\n"
    );
}

// ---------------------------------------------------------------
// 50 repeats, three writers, one file
// ---------------------------------------------------------------

const REPEATS: usize = 50;
const AT_ONCE: usize = 10;

#[derive(Default)]
struct Tally {
    apply_done: AtomicUsize,
    apply_refused: AtomicUsize,
    id_created: AtomicUsize,
    id_refused: AtomicUsize,
    all_three: AtomicUsize,
}

fn one_repeat(i: usize, tally: &Tally) {
    let rig = rig();
    rig.refactor
        .preview("notes", "Old B.md", "New B.md")
        .unwrap();

    // Which writer starts first varies with the repeat: all six orders,
    // several times each, with a small head start for the earlier ones.
    const ORDERS: [[u64; 3]; 6] = [
        [0, 20, 40],
        [0, 40, 20],
        [20, 0, 40],
        [40, 0, 20],
        [20, 40, 0],
        [40, 20, 0],
    ];
    let delay = ORDERS[i % 6];
    let barrier = Arc::new(Barrier::new(3));

    let fix_writer = FixWriter::new(&rig.refactor);
    let fix_req = rig.fix_request.clone();
    let (refactor, blocks) = (rig.refactor.clone(), rig.blocks.clone());

    let (fix, apply, ensure) = std::thread::scope(|s| {
        let b = barrier.clone();
        let fix = s.spawn(move || {
            b.wait();
            std::thread::sleep(Duration::from_millis(delay[0]));
            fix_writer.write_file(&fix_req)
        });
        let b = barrier.clone();
        let apply = s.spawn(move || {
            b.wait();
            std::thread::sleep(Duration::from_millis(delay[1]));
            refactor.apply("notes", "Old B.md", "New B.md")
        });
        let b = barrier.clone();
        let ensure = s.spawn(move || {
            b.wait();
            std::thread::sleep(Duration::from_millis(delay[2]));
            blocks.ensure_id("@notes:Host.md", PARA_LINE)
        });
        (
            fix.join().unwrap(),
            apply.join().unwrap(),
            ensure.join().unwrap(),
        )
    });

    // The fix always re-reads under the lock, and no Tephra writer can
    // slip in between its read and its rename: one attempt, written.
    assert_eq!(fix.status, WriteStatus::Written, "repeat {i}: {fix:?}");
    assert_eq!(fix.attempts, 1, "repeat {i}: {fix:?}");

    // Exactly the changes the writers say they made, and nothing else.
    let mut expected = HOST.replace("[[Old A]]", "[[New A]]");
    match apply {
        Ok(ApplyOutcome::Applied { files_changed }) => {
            assert_eq!(files_changed, 1, "repeat {i}");
            expected = expected.replace("[[Old B]]", "[[New B]]");
            tally.apply_done.fetch_add(1, Ordering::SeqCst);
        }
        // Refused whole because the file moved since the preview: it
        // writes nothing and says so.
        Ok(ApplyOutcome::Aborted { .. }) => {
            tally.apply_refused.fetch_add(1, Ordering::SeqCst);
        }
        Ok(ApplyOutcome::Partial {
            succeeded, failed, ..
        }) => {
            assert!(succeeded.is_empty(), "repeat {i}");
            assert_eq!(failed.len(), 1, "repeat {i}");
            tally.apply_refused.fetch_add(1, Ordering::SeqCst);
        }
        Err(e) => panic!("repeat {i}: apply errored: {e}"),
    }
    match ensure {
        Ok(wire) => {
            assert!(wire.created, "repeat {i}: {wire:?}");
            let id = wire.id.expect("a created id");
            expected = expected.replace("Para needs an id.", &format!("Para needs an id. ^{id}"));
            tally.id_created.fetch_add(1, Ordering::SeqCst);
        }
        // Refused because the cache no longer matched the file (a
        // writer got in first): nothing written.
        Err(_) => {
            tally.id_refused.fetch_add(1, Ordering::SeqCst);
        }
    }

    assert_eq!(
        host_text(&rig),
        expected,
        "repeat {i}: lost or half-written change"
    );
    assert!(tmp_files(rig.src.root()).is_empty(), "repeat {i}");
    if expected.contains("[[New B]]") && expected.contains(" ^") {
        tally.all_three.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn e15_fifty_repeats_of_a_fix_an_apply_and_an_ensure_id_on_one_file_lose_nothing() {
    let tally = Arc::new(Tally::default());
    let next = Arc::new(Mutex::new(0usize));
    std::thread::scope(|s| {
        for _ in 0..AT_ONCE {
            let (tally, next) = (tally.clone(), next.clone());
            s.spawn(move || loop {
                let i = {
                    let mut n = next.lock().unwrap();
                    if *n >= REPEATS {
                        return;
                    }
                    *n += 1;
                    *n - 1
                };
                one_repeat(i, &tally);
            });
        }
    });

    let get = |a: &AtomicUsize| a.load(Ordering::SeqCst);
    println!(
        "repeats={REPEATS} apply_done={} apply_refused={} id_created={} id_refused={} all_three={}",
        get(&tally.apply_done),
        get(&tally.apply_refused),
        get(&tally.id_created),
        get(&tally.id_refused),
        get(&tally.all_three),
    );
    // The test is not vacuous: both outcomes of each race were seen.
    assert!(
        get(&tally.apply_done) > 0,
        "no repeat let the apply through"
    );
    assert!(get(&tally.apply_refused) > 0, "no repeat refused the apply");
    assert!(
        get(&tally.id_created) > 0,
        "no repeat let ensure_id through"
    );
    assert_eq!(get(&tally.apply_done) + get(&tally.apply_refused), REPEATS);
    assert_eq!(get(&tally.id_created) + get(&tally.id_refused), REPEATS);
}
