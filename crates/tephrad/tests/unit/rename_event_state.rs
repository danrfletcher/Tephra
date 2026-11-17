//! Idempotency by state (FR7, G4): what `link.rename_event` and the
//! watcher's pairing do, given what the fix log already says. The
//! decision is a pure function over an in-memory log and a set of paths
//! that are on disk; nothing here touches a disk.

use tephrad::fixes::state::{decide, follow, Decision, LogRec, NothingReason};

/// The paths "on disk" for one case.
fn on_disk<'a>(paths: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
    move |p| paths.contains(&p)
}

fn apply(target: &str) -> Decision {
    Decision::Apply {
        target: target.to_string(),
        via: Vec::new(),
    }
}

fn apply_via(target: &str, via: &[&str]) -> Decision {
    Decision::Apply {
        target: target.to_string(),
        via: via.iter().map(|s| s.to_string()).collect(),
    }
}

struct Case {
    name: &'static str,
    log: Vec<LogRec>,
    old: &'static str,
    new: &'static str,
    exact: bool,
    disk: &'static [&'static str],
    want: Decision,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            name: "a first rename is a new fix",
            log: vec![],
            old: "Goals.md",
            new: "Aims.md",
            exact: true,
            disk: &["Aims.md"],
            want: apply("Aims.md"),
        },
        Case {
            name: "the same event again is already applied",
            log: vec![LogRec::new(1, "Goals.md", "Aims.md")],
            old: "Goals.md",
            new: "Aims.md",
            exact: true,
            disk: &["Aims.md"],
            want: Decision::AlreadyApplied,
        },
        Case {
            name: "the watcher's pairing after the event's fix does nothing",
            log: vec![LogRec::new(1, "Goals.md", "Aims.md")],
            old: "Goals.md",
            new: "Aims.md",
            exact: false,
            disk: &["Aims.md"],
            want: Decision::AlreadyApplied,
        },
        Case {
            name: "already applied even when the target has gone since",
            log: vec![LogRec::new(1, "A.md", "B.md")],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &[],
            want: Decision::AlreadyApplied,
        },
        Case {
            name: "rename-back: the reverse move is a new fix (E20)",
            log: vec![LogRec::new(1, "Plan.md", "Training Plan.md")],
            old: "Training Plan.md",
            new: "Plan.md",
            exact: true,
            disk: &["Plan.md"],
            want: apply("Plan.md"),
        },
        Case {
            name:
                "rename-back, then forth again: new fix, the older entries do not answer it (E20)",
            log: vec![
                LogRec::new(1, "Plan.md", "Training Plan.md"),
                LogRec::new(2, "Training Plan.md", "Plan.md"),
            ],
            old: "Plan.md",
            new: "Training Plan.md",
            exact: true,
            disk: &["Training Plan.md"],
            want: apply("Training Plan.md"),
        },
        Case {
            name: "rename-back by the watcher is a new fix too",
            log: vec![LogRec::new(1, "Plan.md", "Training Plan.md")],
            old: "Training Plan.md",
            new: "Plan.md",
            exact: false,
            disk: &["Plan.md"],
            want: apply("Plan.md"),
        },
        Case {
            name: "the late event for the first move is not a new fix (old name is in use again)",
            log: vec![
                LogRec::new(1, "Plan.md", "Training Plan.md"),
                LogRec::new(2, "Training Plan.md", "Plan.md"),
            ],
            old: "Plan.md",
            new: "Training Plan.md",
            exact: true,
            disk: &["Plan.md"],
            want: Decision::NothingToFix(NothingReason::OldNameInUse),
        },
        Case {
            name: "late A to B after B to C is logged: resolves through the chain to C (E1)",
            log: vec![LogRec::new(1, "B.md", "C.md")],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &["C.md"],
            want: apply_via("C.md", &["B.md", "C.md"]),
        },
        Case {
            name: "late A to B through a chain of three",
            log: vec![
                LogRec::new(1, "B.md", "C.md"),
                LogRec::new(2, "C.md", "D.md"),
            ],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &["D.md"],
            want: apply_via("D.md", &["B.md", "C.md", "D.md"]),
        },
        Case {
            name: "A to B logged, B to C logged, the event for A to B is answered from the log",
            log: vec![
                LogRec::new(1, "A.md", "B.md"),
                LogRec::new(2, "B.md", "C.md"),
            ],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &["C.md"],
            want: Decision::AlreadyApplied,
        },
        Case {
            name: "the event for A to C after A to B, B to C is answered from the log",
            log: vec![
                LogRec::new(1, "A.md", "B.md"),
                LogRec::new(2, "B.md", "C.md"),
            ],
            old: "A.md",
            new: "C.md",
            exact: true,
            disk: &["C.md"],
            want: Decision::AlreadyApplied,
        },
        Case {
            name: "the target is gone and the log does not say where: nothing to fix",
            log: vec![],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &[],
            want: Decision::NothingToFix(NothingReason::TargetGone),
        },
        Case {
            name: "the log chain ends at a path that is not on disk either: nothing to fix",
            log: vec![LogRec::new(1, "B.md", "C.md")],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &[],
            want: Decision::NothingToFix(NothingReason::TargetGone),
        },
        Case {
            name: "old equals new is nothing to fix",
            log: vec![],
            old: "A.md",
            new: "A.md",
            exact: true,
            disk: &["A.md"],
            want: Decision::NothingToFix(NothingReason::SamePath),
        },
        Case {
            name: "the old name reused by a new file is never rewritten (E22)",
            log: vec![],
            old: "Goals.md",
            new: "Aims.md",
            exact: true,
            disk: &["Goals.md", "Aims.md"],
            want: Decision::NothingToFix(NothingReason::OldNameInUse),
        },
        Case {
            name: "a hash guess A to X, then the exact event A to B: the exact event wins (E6)",
            log: vec![LogRec::new(1, "A.md", "X.md")],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &["X.md", "B.md"],
            want: Decision::Override {
                target: "B.md".to_string(),
                via: Vec::new(),
                guess_id: 1,
                guess_target: "X.md".to_string(),
            },
        },
        Case {
            name: "the guess moved on to Y: its links are re-pointed from Y (E6, E23)",
            log: vec![
                LogRec::new(1, "A.md", "X.md"),
                LogRec::new(2, "X.md", "Y.md"),
            ],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &["Y.md", "B.md"],
            want: Decision::Override {
                target: "B.md".to_string(),
                via: Vec::new(),
                guess_id: 1,
                guess_target: "Y.md".to_string(),
            },
        },
        Case {
            name: "the exact event's target moved on too: fixed to where it is (E6, E23)",
            log: vec![
                LogRec::new(1, "A.md", "X.md"),
                LogRec::new(2, "B.md", "B2.md"),
            ],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &["X.md", "B2.md"],
            want: Decision::Override {
                target: "B2.md".to_string(),
                via: vec!["B.md".to_string(), "B2.md".to_string()],
                guess_id: 1,
                guess_target: "X.md".to_string(),
            },
        },
        Case {
            name: "the guess's target is gone: nothing to re-point, a plain fix",
            log: vec![LogRec::new(1, "A.md", "X.md")],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &["B.md"],
            want: apply("B.md"),
        },
        Case {
            name: "the watcher never beats what the log holds",
            log: vec![LogRec::new(1, "A.md", "X.md")],
            old: "A.md",
            new: "B.md",
            exact: false,
            disk: &["X.md", "B.md"],
            want: Decision::NothingToFix(NothingReason::LogHoldsAnotherTarget),
        },
        Case {
            name: "a damaged log with a loop does not run forever",
            log: vec![
                LogRec::new(1, "B.md", "C.md"),
                LogRec::new(2, "C.md", "B.md"),
            ],
            old: "A.md",
            new: "B.md",
            exact: true,
            disk: &[],
            want: Decision::NothingToFix(NothingReason::TargetGone),
        },
    ]
}

#[test]
fn every_state_gets_its_one_answer() {
    for case in cases() {
        let exists = on_disk(case.disk);
        let got = decide(&case.log, case.old, case.new, case.exact, &exists);
        assert_eq!(got, case.want, "case: {}", case.name);
    }
}

#[test]
fn the_decision_does_not_depend_on_how_often_it_is_asked() {
    // Deciding reads the log and writes nothing, so a repeat asked of the
    // same state is the same answer.
    for case in cases() {
        let exists = on_disk(case.disk);
        let first = decide(&case.log, case.old, case.new, case.exact, &exists);
        let again = decide(&case.log, case.old, case.new, case.exact, &exists);
        assert_eq!(first, again, "case: {}", case.name);
    }
}

#[test]
fn once_the_fix_is_logged_the_same_event_is_already_applied() {
    // Walk each "Apply" state forward one step: log the fix it asked
    // for, ask again.
    for case in cases() {
        let exists = on_disk(case.disk);
        let Decision::Apply { target, .. } =
            decide(&case.log, case.old, case.new, case.exact, &exists)
        else {
            continue;
        };
        let mut log = case.log.clone();
        let next = log.iter().map(|r| r.id).max().unwrap_or(0) + 1;
        log.push(LogRec::new(next, case.old, &target));
        let second = decide(&log, case.old, case.new, case.exact, &exists);
        assert!(
            matches!(second, Decision::AlreadyApplied)
                || matches!(second, Decision::NothingToFix(NothingReason::OldNameInUse)),
            "case: {}: {second:?}",
            case.name
        );
    }
}

#[test]
fn follow_walks_departures_only_and_stops_at_a_loop() {
    let log = [
        LogRec::new(1, "A", "B"),
        LogRec::new(2, "B", "C"),
        LogRec::new(3, "C", "A"),
    ];
    // The latest entry about A is the arrival (3): the chain from A ends there.
    assert_eq!(follow(&log, "A"), vec!["A"]);
    assert_eq!(follow(&log, "B"), vec!["B", "C", "A"]);
    assert_eq!(follow(&[], "A"), vec!["A"]);
}
