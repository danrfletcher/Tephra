//! G9 / G10 retry rules, table-driven: the failure classes and the
//! intent re-checks (old name reused, chain, superseded, deleted again).
//!
//! The write loop is mocked: each test hands the engine the
//! `FileWriteResult` the real loop would return, so no filesystem flag
//! is needed. The sources, the cache, the graph and the log are real.

mod fixtures;

use std::fs;
use std::sync::{Arc, Mutex};

use fixtures::{FixWorld, GP1_FILES};
use tephra_cache::{Cache, SourceStatus, STATE_NEEDS_MANUAL_FIX, STATE_PENDING};
use tephrad::fixes::log::{
    FixDetail, FILE_CANCELLED, FILE_NEEDS_MANUAL_FIX, FILE_NOTHING_TO_FIX, FILE_WRITTEN,
};
use tephrad::fixes::{Clock, FileWriter, FixEngine, FixSpec, ManualClock};
use tephrad::refactor::fix_write::{
    item_backoff, FailureClass, FileFixRequest, FileWriteResult, RetryPlan, WriteStatus,
    MAX_ITEM_ROUNDS,
};

const T0: i64 = 1_000_000;
const GOALS: &str = "Goals.md";

// ---------------------------------------------------------------
// The mocked write loop
// ---------------------------------------------------------------

type Script = Box<dyn FnMut(&FileFixRequest) -> FileWriteResult + Send>;

struct Scripted {
    calls: Mutex<Vec<FileFixRequest>>,
    script: Mutex<Script>,
}

impl Scripted {
    fn new(script: impl FnMut(&FileFixRequest) -> FileWriteResult + Send + 'static) -> Arc<Self> {
        Arc::new(Scripted {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(Box::new(script)),
        })
    }
    fn set(&self, script: impl FnMut(&FileFixRequest) -> FileWriteResult + Send + 'static) {
        *self.script.lock().unwrap() = Box::new(script);
    }
    fn calls(&self) -> Vec<FileFixRequest> {
        self.calls.lock().unwrap().clone()
    }
    fn count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
    fn calls_for(&self, rel: &str) -> Vec<FileFixRequest> {
        let id = format!("@notes:{rel}");
        self.calls().into_iter().filter(|c| c.file == id).collect()
    }
}

impl FileWriter for Scripted {
    fn write_file(&self, req: &FileFixRequest) -> FileWriteResult {
        self.calls.lock().unwrap().push(req.clone());
        (self.script.lock().unwrap())(req)
    }
}

fn base(req: &FileFixRequest, status: WriteStatus) -> FileWriteResult {
    FileWriteResult {
        file: req.file.clone(),
        status,
        links_changed: 0,
        recovered_edits: 0,
        attempts: 1,
        rounds: req.prior_rounds,
        round_cap: MAX_ITEM_ROUNDS,
        failure_class: None,
        reason: None,
        retry: RetryPlan::NoRetry,
        intent: None,
    }
}

fn written(req: &FileFixRequest) -> FileWriteResult {
    FileWriteResult {
        links_changed: 1,
        ..base(req, WriteStatus::Written)
    }
}

/// A permanent failure: first attempt ends it.
fn permanent(req: &FileFixRequest) -> FileWriteResult {
    FileWriteResult {
        failure_class: Some(FailureClass::Permanent),
        reason: Some("permission denied".to_string()),
        retry: RetryPlan::OnlyAtStartupRescanOrFileChange,
        ..base(req, WriteStatus::NeedsManualFix)
    }
}

/// A transient failure as the real loop reports it: a deferred round,
/// and `needs_manual_fix` once the round cap is reached.
fn transient(req: &FileFixRequest) -> FileWriteResult {
    let rounds = req.prior_rounds + 1;
    if rounds >= MAX_ITEM_ROUNDS {
        return FileWriteResult {
            rounds,
            failure_class: Some(FailureClass::Transient),
            reason: Some("changed".to_string()),
            retry: RetryPlan::OnlyAtStartupRescanOrFileChange,
            ..base(req, WriteStatus::NeedsManualFix)
        };
    }
    FileWriteResult {
        rounds,
        failure_class: Some(FailureClass::Transient),
        reason: Some("changed".to_string()),
        retry: RetryPlan::AfterBackoff {
            after_ms: item_backoff(rounds).as_millis() as u64,
        },
        ..base(req, WriteStatus::Deferred)
    }
}

fn is(req: &FileFixRequest, rel: &str) -> bool {
    req.file == format!("@notes:{rel}")
}

// ---------------------------------------------------------------
// The world
// ---------------------------------------------------------------

struct Rig {
    w: FixWorld,
    writer: Arc<Scripted>,
    clock: Arc<ManualClock>,
    engine: FixEngine,
}

fn rig(script: impl FnMut(&FileFixRequest) -> FileWriteResult + Send + 'static) -> Rig {
    let w = FixWorld::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    let writer = Scripted::new(script);
    let clock = Arc::new(ManualClock::new(T0));
    let engine = FixEngine::new(&w.cache_path, writer.clone()).with_clock(clock.clone());
    Rig {
        w,
        writer,
        clock,
        engine,
    }
}

impl Rig {
    fn cache(&self) -> Cache {
        Cache::open(&self.w.cache_path).unwrap()
    }
    fn begin(&self) -> tephrad::fixes::FixEntryWire {
        self.engine
            .begin_rename("notes", "Training Plan.md", "Plan.md")
            .unwrap()
    }
    fn batch(&self, rel: &str) -> tephrad::fixes::RunSummary {
        self.engine
            .after_batch("notes", &[self.w.notes.root().join(rel)])
            .unwrap()
    }
    fn detail(&self, id: i64) -> FixDetail {
        let row = self.cache().get_fix_log(id).unwrap().unwrap();
        FixDetail::parse(row.detail_json.as_deref())
    }
    fn pending_row(&self, rel: &str) -> Option<tephra_cache::PendingFixRow> {
        self.cache()
            .pending_fixes(None, None)
            .unwrap()
            .into_iter()
            .find(|r| r.file_path == rel)
    }
}

// ---------------------------------------------------------------
// Failure classes
// ---------------------------------------------------------------

/// A permanent failure goes to needs_manual_fix at once and is retried
/// only at startup, on `source.rescan` or when that file changes.
#[test]
fn a_permanent_failure_is_retried_only_by_its_three_triggers() {
    struct Case {
        name: &'static str,
        run: fn(&Rig),
        retries: usize,
    }
    let cases = [
        Case {
            name: "startup",
            run: |r| {
                r.engine.resume().unwrap();
            },
            retries: 1,
        },
        Case {
            name: "source.rescan of its source",
            run: |r| {
                r.engine.after_rescan("notes").unwrap();
            },
            retries: 1,
        },
        Case {
            name: "that file changed",
            run: |r| {
                r.batch(GOALS);
            },
            retries: 1,
        },
        Case {
            name: "another file changed",
            run: |r| {
                r.batch("Unrelated.md");
            },
            retries: 0,
        },
        Case {
            name: "rescan of another source",
            run: |r| {
                r.engine.after_rescan("elsewhere").unwrap();
            },
            retries: 0,
        },
        Case {
            name: "a batch in another source",
            run: |r| {
                r.engine
                    .after_batch("elsewhere", &[std::path::PathBuf::from("/x/Goals.md")])
                    .unwrap();
            },
            retries: 0,
        },
        Case {
            name: "time passing",
            run: |r| {
                r.clock.advance(86_400);
            },
            retries: 0,
        },
    ];
    for case in cases {
        let r = rig(|req| {
            if is(req, GOALS) {
                permanent(req)
            } else {
                written(req)
            }
        });
        let entry = r.begin();
        assert_eq!(r.writer.calls_for(GOALS).len(), 1, "{}", case.name);
        let row = r.pending_row(GOALS).expect("row kept");
        assert_eq!(row.state, STATE_NEEDS_MANUAL_FIX, "{}", case.name);
        assert_eq!(row.reason.as_deref(), Some("permission denied"));
        // The other two were handed to the write loop, once each.
        assert_eq!(r.writer.count(), 3);
        assert_eq!(
            (entry.status.as_str(), entry.files_changed),
            ("partial", 2),
            "{}",
            case.name
        );

        (case.run)(&r);

        assert_eq!(
            r.writer.calls_for(GOALS).len(),
            1 + case.retries,
            "{}: retries of the permanent file",
            case.name
        );
        assert_eq!(r.writer.count(), 3 + case.retries, "{}", case.name);
        assert_eq!(
            r.cache().list_fix_log(None, 50).unwrap().len(),
            1,
            "{}",
            case.name
        );
    }
}

/// A transient failure gets a bounded backoff, then needs_manual_fix; a
/// per-item attempt counter and cap stop retry storms; a later edit
/// re-queues it once.
#[test]
fn a_transient_failure_backs_off_then_stops_at_the_cap_and_a_user_edit_requeues_it_once() {
    let r = rig(|req| {
        if is(req, GOALS) {
            transient(req)
        } else {
            written(req)
        }
    });
    let entry = r.begin();
    assert_eq!(entry.status, "partial");
    let row = r.pending_row(GOALS).unwrap();
    assert_eq!((row.state.as_str(), row.attempts), (STATE_PENDING, 1));
    assert_eq!(r.writer.calls_for(GOALS).len(), 1);

    // Inside the backoff window a batch does not retry it.
    r.batch("Unrelated.md");
    r.clock.advance(item_backoff(1).as_secs() as i64 - 1);
    r.batch("Unrelated.md");
    assert_eq!(r.writer.calls_for(GOALS).len(), 1, "still backing off");

    // Past it, the next batch retries, and the round count is passed on.
    for round in 1..MAX_ITEM_ROUNDS {
        r.clock.advance(item_backoff(round).as_secs() as i64 + 1);
        r.batch("Unrelated.md");
        let calls = r.writer.calls_for(GOALS);
        assert_eq!(calls.len(), round as usize + 1, "round {round}");
        assert_eq!(calls.last().unwrap().prior_rounds, round, "round {round}");
    }
    let row = r.pending_row(GOALS).unwrap();
    assert_eq!(
        (row.state.as_str(), row.attempts),
        (STATE_NEEDS_MANUAL_FIX, MAX_ITEM_ROUNDS as i64),
        "the cap ends it"
    );
    let calls_at_cap = r.writer.calls_for(GOALS).len();
    assert_eq!(calls_at_cap, MAX_ITEM_ROUNDS as usize);
    assert_eq!(
        r.detail(entry.id)
            .files
            .iter()
            .find(|f| f.path == GOALS)
            .unwrap()
            .status,
        FILE_NEEDS_MANUAL_FIX
    );

    // No storm: any number of later batches elsewhere, however late.
    for _ in 0..10 {
        r.clock.advance(3_600);
        r.batch("Unrelated.md");
    }
    assert_eq!(r.writer.calls_for(GOALS).len(), calls_at_cap);

    // A later edit of that file by the user re-queues it, once.
    r.batch(GOALS);
    let calls = r.writer.calls_for(GOALS);
    assert_eq!(calls.len(), calls_at_cap + 1, "one try for the edit");
    assert_eq!(calls.last().unwrap().prior_rounds, 0, "from a fresh count");
    r.batch(GOALS);
    assert_eq!(
        r.writer.calls_for(GOALS).len(),
        calls_at_cap + 1,
        "a waiting row is not re-run by the same edit's echo while backing off"
    );

    // The file finally goes through: the same entry goes partial to applied.
    r.writer.set(written);
    r.clock.advance(item_backoff(1).as_secs() as i64);
    r.batch("Unrelated.md");
    let log = r.cache().list_fix_log(None, 50).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].id, entry.id);
    assert_eq!(
        (log[0].status.as_str(), log[0].files_changed),
        ("applied", 3)
    );
    assert!(r.cache().pending_fixes(None, None).unwrap().is_empty());
}

#[test]
fn the_backoff_uses_the_named_constants_and_is_bounded() {
    use tephrad::refactor::fix_write::{ITEM_BACKOFF_BASE, ITEM_BACKOFF_MAX};
    assert_eq!(item_backoff(1), ITEM_BACKOFF_BASE);
    assert_eq!(item_backoff(2), ITEM_BACKOFF_BASE * 2);
    assert_eq!(item_backoff(40), ITEM_BACKOFF_MAX);
}

// ---------------------------------------------------------------
// Intent re-checks
// ---------------------------------------------------------------

struct Expect {
    /// Calls to the write loop made by the retry.
    calls: usize,
    file_status: &'static str,
    reason_has: &'static str,
    entries: usize,
}

/// Each case: a fix whose three files all wait (the write loop defers
/// them), the world changes, a restart retries them.
#[test]
fn the_intent_is_rechecked_before_each_retry() {
    type Change = fn(&Rig);
    let cases: [(&str, Change, Expect); 6] = [
        (
            "control: nothing changed",
            |_| {},
            Expect {
                calls: 3,
                file_status: FILE_WRITTEN,
                reason_has: "",
                entries: 1,
            },
        ),
        (
            "the old name was reused by a new file",
            |r| {
                r.w.notes.write("Training Plan.md", "# A different note\n");
                r.w.registry.rescan("notes").unwrap();
            },
            Expect {
                calls: 0,
                file_status: FILE_NOTHING_TO_FIX,
                reason_has: "used by another file",
                entries: 1,
            },
        ),
        (
            "a chain A to B to C resolves to C",
            |r| {
                r.w.rename("Plan.md", "Final.md");
                // The second rename's own fix: logged, no file links to B.
                r.writer.set(written);
                let e = r
                    .engine
                    .begin_rename("notes", "Plan.md", "Final.md")
                    .unwrap();
                assert_eq!((e.status.as_str(), e.files_changed), ("applied", 0));
            },
            Expect {
                calls: 3,
                file_status: FILE_WRITTEN,
                reason_has: "",
                entries: 2,
            },
        ),
        (
            "an exact event corrected the target",
            |r| {
                let e = r
                    .engine
                    .begin_fix(&FixSpec {
                        old_source: "notes".into(),
                        old_path: "Training Plan.md".into(),
                        new_source: "notes".into(),
                        new_path: "Elsewhere.md".into(),
                        files: vec![],
                    })
                    .unwrap();
                assert_eq!(e.status, "applied");
            },
            Expect {
                calls: 0,
                file_status: FILE_CANCELLED,
                reason_has: "Elsewhere.md",
                entries: 2,
            },
        ),
        (
            "the target was renamed again while down (no log entry)",
            |r| {
                r.w.rename("Plan.md", "Other.md");
            },
            Expect {
                calls: 0,
                file_status: FILE_NOTHING_TO_FIX,
                reason_has: "renamed or deleted again",
                entries: 1,
            },
        ),
        (
            "the target was deleted while down",
            |r| {
                fs::remove_file(r.w.notes.root().join("Plan.md")).unwrap();
                r.w.registry.rescan("notes").unwrap();
            },
            Expect {
                calls: 0,
                file_status: FILE_NOTHING_TO_FIX,
                reason_has: "renamed or deleted again",
                entries: 1,
            },
        ),
    ];

    for (name, change, expect) in cases {
        let r = rig(transient);
        let entry = r.begin();
        assert_eq!(r.writer.count(), 3, "{name}: all three deferred");
        assert_eq!(entry.status, "partial", "{name}");
        let on_disk: Vec<String> = ["Weekly Review.md", GOALS, "Ideas/Rename Notes.md"]
            .iter()
            .map(|p| r.w.text(p))
            .collect();

        change(&r);
        r.writer.set(written);
        let before = r.writer.count();
        let summary = r.engine.resume().unwrap();

        let made = r.writer.calls()[before..].to_vec();
        assert_eq!(made.len(), expect.calls, "{name}: calls {summary:?}");
        assert!(
            r.cache().pending_fixes(None, None).unwrap().is_empty(),
            "{name}: nothing left pending"
        );
        let d = r.detail(entry.id);
        assert_eq!(d.files.len(), 3, "{name}");
        for f in &d.files {
            assert_eq!(f.status, expect.file_status, "{name}: {}", f.path);
            assert!(
                f.reason
                    .as_deref()
                    .unwrap_or("")
                    .contains(expect.reason_has),
                "{name}: {:?}",
                f.reason
            );
        }
        assert_eq!(
            r.cache().list_fix_log(None, 50).unwrap().len(),
            expect.entries,
            "{name}"
        );
        let after: Vec<String> = ["Weekly Review.md", GOALS, "Ideas/Rename Notes.md"]
            .iter()
            .map(|p| r.w.text(p))
            .collect();
        assert_eq!(
            after, on_disk,
            "{name}: the mock wrote nothing, and nothing else did"
        );

        if expect.calls == 3 {
            // What the write loop was asked to do.
            let final_target = if name.contains("chain") {
                "Final.md"
            } else {
                "Plan.md"
            };
            for req in &made {
                assert_eq!(req.old_path, "Training Plan.md", "{name}");
                assert_eq!(req.new_path, final_target, "{name}");
            }
        }
        if name.contains("chain") {
            for req in &made {
                assert_eq!(
                    req.chain,
                    ["Training Plan.md", "Plan.md", "Final.md"],
                    "{name}"
                );
            }
            assert!(d
                .files
                .iter()
                .all(|f| f.chain == ["Training Plan.md", "Plan.md", "Final.md"]));
        }
    }
}

#[test]
fn a_dropped_item_is_never_rewritten_to_the_wrong_file() {
    // The old name is reused by a new file that the very same links now
    // (correctly) point at: a retry must not turn them into links to Plan.
    let r = rig(transient);
    r.begin();
    r.w.notes.write("Training Plan.md", "# Reused\n");
    r.w.registry.rescan("notes").unwrap();
    r.writer.set(written);
    let before = r.writer.count();
    r.engine.resume().unwrap();
    r.engine.after_rescan("notes").unwrap();
    assert_eq!(r.writer.count(), before);
}

// ---------------------------------------------------------------
// The log
// ---------------------------------------------------------------

#[test]
fn a_recovered_edit_is_noted_on_the_entry_and_the_retry_updates_it_in_place() {
    let r = rig(|req| {
        if is(req, GOALS) {
            transient(req)
        } else {
            written(req)
        }
    });
    let entry = r.begin();
    assert_eq!(entry.status, "partial");
    r.writer.set(|req| FileWriteResult {
        recovered_edits: 2,
        attempts: 3,
        ..written(req)
    });
    r.clock.advance(60);
    r.batch("Unrelated.md");

    let log = r.cache().list_fix_log(None, 50).unwrap();
    assert_eq!(log.len(), 1, "one rename, one entry");
    assert_eq!(log[0].id, entry.id);
    assert_eq!(
        (log[0].status.as_str(), log[0].files_changed),
        ("applied", 3)
    );
    let d = r.detail(entry.id);
    assert_eq!(d.recovered_edits, 2);
    let goals = d.files.iter().find(|f| f.path == GOALS).unwrap();
    assert_eq!((goals.recovered_edits, goals.attempts), (2, 3));
    assert_eq!(goals.status, FILE_WRITTEN);
    assert_eq!(goals.links_changed, 1);
}

#[test]
fn an_uncertain_case_is_logged_and_creates_no_pending_work() {
    let r = rig(written);
    let first = r.begin();
    r.clock.advance(10);
    let uncertain = r.engine.record_uncertain("notes", "Gone.md").unwrap();

    assert_eq!(uncertain.kind, "uncertain");
    assert_eq!(uncertain.status, "uncertain");
    assert_eq!(uncertain.old.as_deref(), Some("Gone.md"));
    assert_eq!(uncertain.new, None);
    assert_eq!(uncertain.files_changed, 0);
    assert_eq!(uncertain.sources, ["notes"]);
    assert_eq!(uncertain.time, T0 + 10);
    let d = r.detail(uncertain.id);
    assert_eq!(
        d.message.as_deref(),
        Some("vanished, no certain pair: links broken")
    );
    assert!(d.files.is_empty());
    assert!(r.cache().pending_fixes(None, None).unwrap().is_empty());

    let listed = r.engine.list(None, 50).unwrap();
    assert_eq!(
        listed.iter().map(|e| e.id).collect::<Vec<_>>(),
        [uncertain.id, first.id],
        "newest first"
    );
    // Retry triggers never touch it.
    let calls = r.writer.count();
    r.engine.resume().unwrap();
    r.engine.after_rescan("notes").unwrap();
    assert_eq!(r.writer.count(), calls);
    assert_eq!(r.engine.list(None, 50).unwrap().len(), 2);
}

#[test]
fn rows_of_an_offline_source_wait_without_using_up_attempts() {
    let r = rig(transient);
    r.begin();
    let before = r.cache().pending_fixes(None, None).unwrap();
    r.cache()
        .transition_source_status("notes", SourceStatus::Offline)
        .unwrap();
    let calls = r.writer.count();
    let s = r.engine.resume().unwrap();
    assert_eq!(s.skipped_offline, 3);
    assert_eq!(r.writer.count(), calls, "the write loop was not asked");
    assert_eq!(r.cache().pending_fixes(None, None).unwrap(), before);
}

#[test]
fn a_row_of_a_source_that_was_unregistered_is_dropped_as_nothing_to_fix() {
    let r = rig(transient);
    let entry = r.begin();
    r.cache().delete_source("notes").unwrap();
    let calls = r.writer.count();
    let s = r.engine.resume().unwrap();
    assert_eq!(s.dropped, 3);
    assert_eq!(r.writer.count(), calls);
    assert!(r
        .detail(entry.id)
        .files
        .iter()
        .all(|f| f.status == FILE_NOTHING_TO_FIX));
}

#[test]
fn a_clock_is_only_read_never_waited_on() {
    // The engine takes its time from the injected clock: a retry at the
    // manual time the backoff allows needs no sleeping.
    let r = rig(|req| {
        if is(req, GOALS) {
            transient(req)
        } else {
            written(req)
        }
    });
    r.begin();
    assert_eq!(r.clock.now(), T0);
    r.clock.advance(item_backoff(1).as_secs() as i64);
    let s = r.batch("Unrelated.md");
    assert_eq!(s.deferred, 1);
}
