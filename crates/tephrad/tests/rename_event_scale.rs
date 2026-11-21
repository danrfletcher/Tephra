//! E24: a large vault. 5,000 generated notes, 500 of them linking to one
//! file; the exact event for that file's rename is answered inside 5 s
//! with every linking file rewritten once, and an event resent while the
//! first is still running never applies twice.
//!
//! A whole daemon (real registry, cache and write loop), no mocks. The
//! spec's 5 s bound is held by the `#[ignore]`d test at the end, on a
//! quiet machine; the ordinary run checks what a busy host cannot
//! change (one answer, every file rewritten once, one log row) and only
//! guards against a reply that never comes.

mod fix_world;
mod fixtures;

use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

use fix_world::{real_writer, Daemon};
use tephrad::fixes::event::RenameEventWire;
use tephrad::fixes::FileWriter;
use tephrad::rpc::backend::FixBackend;

const NOTES: usize = 5_000;
const LINKING: usize = 500;

/// The spec's bound for the reply (E24).
const SPEC_BOUND: Duration = Duration::from_secs(5);

/// What the ordinary run holds the reply to: a hang guard, not a speed
/// test. This Mac takes 11 to 16 s in an unoptimised build for 500 files
/// on a quiet machine and several times that when the host is busy (92 s
/// was seen at load 50 to 80), so no wall-clock bound that is also a
/// useful regression check can hold in the ordinary run. The speed is
/// held by the `#[ignore]`d test below, meant for a quiet machine or
/// the CI box: `cargo test --release -p tephrad --test rename_event_scale
/// -- --ignored`.
const HANG_GUARD: Duration = Duration::from_secs(600);

fn vault() -> Vec<(String, String)> {
    let mut files = vec![("Target.md".to_string(), "# Target\n".to_string())];
    for i in 0..NOTES - 1 {
        let body = if i < LINKING {
            format!("# Linker {i}\n\nSee [[Target]] and [[Target|the target]].\n")
        } else {
            format!("# Filler {i}\n\nNothing links to the target from here.\n")
        };
        files.push((format!("n/{:02}/Note {i}.md", i % 50), body));
    }
    files
}

fn run(bound: Duration, writer: Arc<dyn FileWriter>) {
    let owned = vault();
    let borrowed: Vec<(&str, &str)> = owned
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let d = Daemon::start(&borrowed, &[], writer, false);
    d.mv("Target.md", "Renamed.md");

    let root = d.notes_root();
    let (old, new) = (
        root.join("Target.md").display().to_string(),
        root.join("Renamed.md").display().to_string(),
    );
    // The same event resent from another connection at the same moment.
    let gate = Arc::new(Barrier::new(2));
    let resent = {
        let (service, gate) = (d.service.clone(), gate.clone());
        let (old, new) = (old.clone(), new.clone());
        std::thread::spawn(move || {
            gate.wait();
            service.rename_event(&old, &new).expect("resent event")
        })
    };
    gate.wait();
    let started = Instant::now();
    let first = d.service.rename_event(&old, &new).expect("event");
    let second = resent.join().unwrap();
    let took = started.elapsed();

    assert!(
        took < bound,
        "both replies took {took:?} for {LINKING} linking files (bound {bound:?})"
    );
    let applied: Vec<_> = [&first, &second]
        .into_iter()
        .filter(|r| matches!(r, RenameEventWire::Applied { .. }))
        .collect();
    assert_eq!(applied.len(), 1, "{first:?} and {second:?}");
    assert_eq!(
        *applied[0],
        RenameEventWire::Applied {
            files_changed: LINKING as i64
        }
    );
    assert!(
        [&first, &second]
            .iter()
            .any(|r| matches!(r, RenameEventWire::AlreadyApplied)),
        "the resent event was answered from the log: {first:?} and {second:?}"
    );

    // Every linking file is rewritten, each exactly once; the rest are
    // untouched; one log row.
    for i in 0..LINKING {
        let text = d.text(&format!("n/{:02}/Note {i}.md", i % 50));
        assert_eq!(
            text,
            format!("# Linker {i}\n\nSee [[Renamed]] and [[Renamed|the target]].\n")
        );
    }
    let filler = d.text(&format!("n/{:02}/Note {}.md", LINKING % 50, LINKING));
    assert!(filler.starts_with("# Filler"));
    let renames: Vec<_> = d
        .entries()
        .into_iter()
        .filter(|e| e.kind == "rename")
        .collect();
    assert_eq!(renames.len(), 1, "{renames:?}");
    assert_eq!(renames[0].files_changed, LINKING as i64);
    eprintln!("5,000 notes, {LINKING} linking files: both replies in {took:?}");
    d.close();
}

#[test]
fn a_5000_note_vault_with_500_linking_files_is_fixed_once_and_a_resent_event_never_double_applies()
{
    run(HANG_GUARD, real_writer());
}

/// The spec's 5 s, with the production write path (`RealFs`, whose flush
/// before each rename is `F_BARRIERFSYNC` on macOS and `fsync` on Linux).
/// It was 8 to 9 s while that flush was `F_FULLFSYNC`, which is more than
/// half of what a 500-file rename cost. Wall-clock, so it is for a quiet
/// machine and the release build, not the ordinary run:
/// `cargo test --release -p tephrad --test rename_event_scale -- --ignored`.
#[test]
#[ignore = "the spec's 5 s bound is for a quiet machine; run with --release -- --ignored"]
fn the_reply_meets_the_specs_five_seconds() {
    run(SPEC_BOUND, real_writer());
}
