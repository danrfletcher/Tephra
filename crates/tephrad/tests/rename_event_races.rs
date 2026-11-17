//! G4 / FR7 / E6 / E7: an exact `link.rename_event` and the watcher's own
//! pairing of the same rename fix it once, whichever comes first, and
//! even when both arrive at the same instant. The log is the one place
//! that says so.
//!
//! One whole daemon per case (real registry, cache and write loop), and
//! `REPS` different renames inside it, so a flake shows. Each case
//! asserts exactly one fix-log row and one announced fix per rename.
//! The ordered cases call the watcher's entry point (`RenameFixer`) by
//! hand, so the order is fixed and the test never waits on the OS; the
//! last test lets the real OS watcher race the event.

mod fix_world;
mod fixtures;

use std::sync::{Arc, Barrier};
use std::time::Duration;

use fix_world::{fixes_of, real_writer, wait_until, with_fixes, Client, Daemon};
use fixtures::GP1_FILES;
use tephrad::fixes::event::RenameEventWire;
use tephrad::rename_detect::RenameFixer;

const REPS: usize = 50;

/// `T{i}.md` linked from `L{i}.md` by `[[T{i}]]`.
fn files() -> Vec<(String, String)> {
    (0..REPS)
        .flat_map(|i| {
            [
                (format!("T{i}.md"), format!("# T{i}\n")),
                (format!("L{i}.md"), format!("See [[T{i}]].\n")),
            ]
        })
        .collect()
}

fn daemon() -> Daemon {
    let owned = files();
    let borrowed: Vec<(&str, &str)> = owned
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    Daemon::start(&borrowed, &[], real_writer(), false)
}

fn renames(d: &Daemon) -> Vec<tephrad::fixes::FixEntryWire> {
    d.entries()
        .into_iter()
        .filter(|e| e.kind == "rename")
        .collect()
}

/// Every fix announced to `client` until it has been quiet a moment.
fn announced(client: &mut Client) -> usize {
    let pushes = client.drain(Duration::from_millis(500));
    with_fixes(&pushes).iter().map(|p| fixes_of(p).len()).sum()
}

fn applied_or_already(reply: &RenameEventWire) -> bool {
    matches!(
        reply,
        RenameEventWire::Applied { .. } | RenameEventWire::AlreadyApplied
    )
}

fn check_all_fixed(d: &Daemon, client: &mut Client) {
    for i in 0..REPS {
        assert_eq!(
            d.text(&format!("L{i}.md")),
            format!("See [[N{i}]].\n"),
            "link {i}"
        );
    }
    assert_eq!(renames(d).len(), REPS, "one fix-log row per rename");
    assert_eq!(announced(client), REPS, "one announced fix per rename");
}

#[test]
fn event_first_then_the_watchers_pairing_does_nothing() {
    let d = daemon();
    let mut client = d.connect();
    for i in 0..REPS {
        let (old, new) = (format!("T{i}.md"), format!("N{i}.md"));
        d.mv(&old, &new);
        assert_eq!(
            d.event(&old, &new).expect("event"),
            RenameEventWire::Applied { files_changed: 1 }
        );
        d.registry.rescan("notes").expect("rescan");
        // The watcher reports the rename once per path of its batch.
        for _ in 0..2 {
            d.engine.fix_rename("notes", &old, &new).expect("pairing");
        }
    }
    check_all_fixed(&d, &mut client);
    d.close();
}

#[test]
fn watcher_first_then_the_event_is_already_applied() {
    let d = daemon();
    let mut client = d.connect();
    for i in 0..REPS {
        let (old, new) = (format!("T{i}.md"), format!("N{i}.md"));
        d.mv_and_rescan(&old, &new);
        d.engine.fix_rename("notes", &old, &new).expect("pairing");
        assert_eq!(
            d.event(&old, &new).expect("event"),
            RenameEventWire::AlreadyApplied
        );
    }
    check_all_fixed(&d, &mut client);
    d.close();
}

#[test]
fn an_event_resent_after_the_fix_is_already_applied() {
    let d = daemon();
    let mut client = d.connect();
    for i in 0..REPS {
        let (old, new) = (format!("T{i}.md"), format!("N{i}.md"));
        d.mv(&old, &new);
        assert_eq!(
            d.event(&old, &new).expect("event"),
            RenameEventWire::Applied { files_changed: 1 }
        );
        for _ in 0..2 {
            assert_eq!(
                d.event(&old, &new).expect("resent"),
                RenameEventWire::AlreadyApplied
            );
        }
    }
    check_all_fixed(&d, &mut client);
    d.close();
}

#[test]
fn an_event_and_a_pairing_started_together_write_and_log_once() {
    let d = daemon();
    let mut client = d.connect();
    for i in 0..REPS {
        let (old, new) = (format!("T{i}.md"), format!("N{i}.md"));
        d.mv_and_rescan(&old, &new);
        let gate = Arc::new(Barrier::new(2));
        let watcher = {
            let (engine, gate) = (d.engine.clone(), gate.clone());
            let (old, new) = (old.clone(), new.clone());
            std::thread::spawn(move || {
                gate.wait();
                engine.fix_rename("notes", &old, &new).expect("pairing");
            })
        };
        gate.wait();
        let reply = d.event(&old, &new).expect("event");
        watcher.join().unwrap();
        assert!(applied_or_already(&reply), "round {i}: {reply:?}");
    }
    check_all_fixed(&d, &mut client);
    d.close();
}

#[test]
fn two_events_for_the_same_rename_started_together_write_and_log_once() {
    let d = daemon();
    let mut client = d.connect();
    for i in 0..REPS {
        let (old, new) = (format!("T{i}.md"), format!("N{i}.md"));
        d.mv(&old, &new);
        let gate = Arc::new(Barrier::new(2));
        let other = {
            let (service, gate) = (d.service.clone(), gate.clone());
            let root = d.notes_root();
            let (old, new) = (
                root.join(&old).display().to_string(),
                root.join(&new).display().to_string(),
            );
            std::thread::spawn(move || {
                use tephrad::rpc::backend::FixBackend;
                gate.wait();
                service.rename_event(&old, &new).expect("second event")
            })
        };
        gate.wait();
        let first = d.event(&old, &new).expect("event");
        let second = other.join().unwrap();
        assert!(applied_or_already(&first) && applied_or_already(&second));
        let applied = [&first, &second]
            .iter()
            .filter(|r| matches!(r, RenameEventWire::Applied { .. }))
            .count();
        assert_eq!(applied, 1, "round {i}: {first:?} and {second:?}");
    }
    check_all_fixed(&d, &mut client);
    d.close();
}

#[test]
fn the_watcher_reporting_a_vanish_the_event_already_fixed_logs_nothing_uncertain() {
    let d = daemon();
    let mut client = d.connect();
    for i in 0..REPS {
        let (old, new) = (format!("T{i}.md"), format!("N{i}.md"));
        d.mv(&old, &new);
        assert_eq!(
            d.event(&old, &new).unwrap(),
            RenameEventWire::Applied { files_changed: 1 }
        );
        d.registry.rescan("notes").expect("rescan");
        // The watcher saw the old name vanish and found no pair.
        RenameFixer::record_uncertain(&*d.engine, "notes", &old).expect("watcher");
    }
    assert!(
        d.entries().iter().all(|e| e.kind != "uncertain"),
        "{:?}",
        d.entries()
    );
    check_all_fixed(&d, &mut client);
    d.close();
}

#[test]
fn an_exact_event_beats_a_hash_guess_the_watcher_already_applied() {
    // E6: the watcher paired `T` with `G` (a copy with the same content)
    // and fixed the links to `G`. The client says the file went to `N`.
    let d = daemon();
    let mut client = d.connect();
    for i in 0..REPS {
        let (old, guess, new) = (format!("T{i}.md"), format!("G{i}.md"), format!("N{i}.md"));
        d.mv_and_rescan(&old, &guess);
        d.notes.write(&new, &format!("# T{i}\n"));
        d.engine.fix_rename("notes", &old, &guess).expect("guess");
        assert_eq!(d.text(&format!("L{i}.md")), format!("See [[G{i}]].\n"));

        let reply = d.event(&old, &new).expect("event");
        assert!(
            matches!(reply, RenameEventWire::Applied { .. }),
            "round {i}: {reply:?}"
        );
        assert_eq!(
            d.text(&format!("L{i}.md")),
            format!("See [[N{i}]].\n"),
            "the exact event wins"
        );
        // The same event again is answered from the log.
        assert_eq!(
            d.event(&old, &new).unwrap(),
            RenameEventWire::AlreadyApplied
        );
    }
    // Per rename: the guess's entry and the event's entry, one push each.
    assert_eq!(renames(&d).len(), 2 * REPS, "{:?}", d.entries());
    assert_eq!(announced(&mut client), 2 * REPS, "one push per entry");
    d.close();
}

#[test]
fn an_exact_event_leaves_a_file_changed_since_the_guess_and_logs_it_needs_manual_fix() {
    // E6, the changed-file branch: after the guess fixed `L` to `[[G]]`,
    // the file gained a second link to `G`, so it is no longer only what
    // the guess wrote. The event leaves it as it is and logs it.
    let d = daemon();
    let mut client = d.connect();
    for i in 0..REPS {
        let (old, guess, new) = (format!("T{i}.md"), format!("G{i}.md"), format!("N{i}.md"));
        let linker = format!("L{i}.md");
        d.mv_and_rescan(&old, &guess);
        d.notes.write(&new, &format!("# T{i}\\n"));
        d.engine.fix_rename("notes", &old, &guess).expect("guess");
        let changed = format!("See [[G{i}]] and again [[G{i}]].\\n");
        d.notes.write(&linker, &changed);
        d.registry.rescan("notes").expect("rescan");

        match d.event(&old, &new).expect("event") {
            RenameEventWire::Partial {
                files_changed,
                deferred,
            } => {
                assert_eq!(files_changed, 0, "round {i}");
                assert_eq!(deferred.len(), 1, "round {i}: {deferred:?}");
                assert_eq!(deferred[0].file, linker, "round {i}");
            }
            other => panic!("round {i}: {other:?}"),
        }
        assert_eq!(d.text(&linker), changed, "round {i}: left as it is");
        // The same event again is answered from the log, as it was.
        assert!(
            matches!(
                d.event(&old, &new).expect("resent"),
                RenameEventWire::Partial { .. } | RenameEventWire::AlreadyApplied
            ),
            "round {i}"
        );
        assert_eq!(d.text(&linker), changed, "round {i}: still as it is");
    }
    // One entry for the guess, one for the event; the event's entry
    // carries each changed file as needs_manual_fix.
    let rows = renames(&d);
    assert_eq!(rows.len(), 2 * REPS, "{:?}", d.entries());
    let manual = rows.iter().filter(|e| e.status != "applied").count();
    assert_eq!(manual, REPS, "{:?}", d.entries());
    assert_eq!(announced(&mut client), 2 * REPS, "one push per entry");
    d.close();
}

#[test]
fn the_real_watcher_and_an_immediate_event_fix_the_rename_once() {
    let d = Daemon::start(&GP1_FILES, &[], real_writer(), true);
    d.mv("Training Plan.md", "Plan.md");
    let reply = d.event("Training Plan.md", "Plan.md").expect("event");
    assert!(applied_or_already(&reply), "{reply:?}");
    // Let the watcher have its say, then look: still one entry. A
    // watcher that reports nothing leaves the same single entry.
    let _ = wait_until(Duration::from_secs(3), || renames(&d).len() > 1);
    assert_eq!(renames(&d).len(), 1, "{:?}", d.entries());
    assert!(d.text("Goals.md").contains("[[Plan|plan]]"));
    assert!(
        d.entries().iter().all(|e| e.kind != "uncertain"),
        "{:?}",
        d.entries()
    );
    d.close();
}
