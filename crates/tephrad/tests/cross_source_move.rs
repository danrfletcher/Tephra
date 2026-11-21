//! G6 / FR21: a file that goes from one source to another is ONE fix, of
//! kind `move`, whichever signal reaches Tephra (the exact
//! `link.rename_event`, or the watcher's own pairing through
//! `RenameFixer`). Three rewrites: the links to the file in the source it
//! left (now `@<new source>:<path>`), the links in the source it went to
//! (the `@old:` prefix goes), and the moved file's own links.
//!
//! One whole daemon per case (real registry, cache, graph and write loop
//! over `notes` and `other`); the move is made on the real disk and the
//! text of every file is read back.

mod fix_world;
mod fixtures;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use fix_world::{wait_until, writer_over, Daemon, WAIT};
use serde_json::Value;
use tephra_cache::Cache;
use tephra_graph::{ResolveError, UnitGraph};
use tephrad::fixes::event::RenameEventWire;
use tephrad::fixes::{FixEngine, FixEntryWire, FixError};
use tephrad::refactor::{AtomicFs, RealFs};
use tephrad::rename_detect::{dispatch, Appeared, Detector, RenameFixer, Vanished};

const TARGET: &str = "---\nrelated: \"[[Sibling]]\"\n---\n# Target\n\nBack to [[Sibling|the sibling]] and [[Sibling#Part]], \
me again [[Target]] and [[Target#^k3f9a2]]. Gone: [[Nowhere]].\n";

fn notes() -> Vec<(&'static str, &'static str)> {
    vec![
        ("Ideas/Target.md", TARGET),
        ("Sibling.md", "# Sibling\n\n## Part\n"),
        (
            "Index.md",
            "plain [[Target]], alias [[Target|the target]], embed ![[Target]], block [[Target#^k3f9a2]],\n\
             heading [[Target#Part|part]], path [[Ideas/Target]], ext [[Target.md]], md [t](Ideas/Target.md)\n",
        ),
    ]
}

fn others() -> Vec<(&'static str, &'static str)> {
    vec![
        (
            "Reader.md",
            "reads [[@notes:Ideas/Target]] and [[@notes:Ideas/Target|alias]] and [[Elsewhere]]\n",
        ),
        ("Elsewhere.md", "# Elsewhere\n"),
    ]
}

fn daemon() -> Daemon {
    Daemon::start(&notes(), &others(), fix_world::real_writer(), false)
}

/// `Ideas/Target.md` of notes goes to `docs/target.md` of other, on disk.
fn move_on_disk(d: &Daemon) {
    let to = d.other.root().join("docs/target.md");
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::rename(d.notes.root().join("Ideas/Target.md"), to).expect("move on disk");
}

fn event(d: &Daemon) -> Result<RenameEventWire, tephrad::fixes::event::RenameEventError> {
    use tephrad::rpc::backend::FixBackend;
    let from = d.notes_root().join("Ideas/Target.md");
    let to = d.other_root().join("docs/target.md");
    d.service
        .rename_event(&from.display().to_string(), &to.display().to_string())
}

fn moves(d: &Daemon) -> Vec<FixEntryWire> {
    d.entries()
        .into_iter()
        .filter(|e| e.kind == "move")
        .collect()
}

const INDEX_AFTER: &str = "plain [[@other:docs/target]], alias [[@other:docs/target|the target]], embed ![[@other:docs/target]], block [[@other:docs/target#^k3f9a2]],\n\
     heading [[@other:docs/target#Part|part]], path [[@other:docs/target]], ext [[@other:docs/target]], md [t](Ideas/Target.md)\n";

const READER_AFTER: &str = "reads [[target]] and [[target|alias]] and [[Elsewhere]]\n";

const TARGET_AFTER: &str = "---\nrelated: \"[[@notes:Sibling]]\"\n---\n# Target\n\nBack to [[@notes:Sibling|the sibling]] and [[@notes:Sibling#Part]], \
me again [[target]] and [[target#^k3f9a2]]. Gone: [[Nowhere]].\n";

fn check_fixed(d: &Daemon) {
    assert_eq!(
        d.text("Index.md"),
        INDEX_AFTER,
        "rewrite 1: links in the old source"
    );
    assert_eq!(
        d.other_text("Reader.md"),
        READER_AFTER,
        "rewrite 2: links in the new source"
    );
    assert_eq!(
        d.other_text("docs/target.md"),
        TARGET_AFTER,
        "rewrite 3: the moved file's own links"
    );
}

#[test]
fn an_exact_event_for_a_move_fixes_all_three_kinds_of_link_in_one_entry() {
    let d = daemon();
    move_on_disk(&d);
    let reply = event(&d).expect("event");
    assert!(
        matches!(reply, RenameEventWire::Applied { .. }),
        "{reply:?}"
    );
    check_fixed(&d);
    let m = moves(&d);
    assert_eq!(
        m.len(),
        1,
        "one entry for the whole move: {:?}",
        d.entries()
    );
    assert_eq!(m[0].sources, vec!["notes".to_string(), "other".to_string()]);
    assert_eq!(m[0].old.as_deref(), Some("Ideas/Target.md"));
    assert_eq!(m[0].new.as_deref(), Some("docs/target.md"));
    assert!(d.entries().iter().all(|e| e.kind != "rename"));
    d.close();
}

#[test]
fn the_same_event_sent_again_is_already_applied_and_changes_nothing() {
    let d = daemon();
    move_on_disk(&d);
    assert!(matches!(
        event(&d).unwrap(),
        RenameEventWire::Applied { .. }
    ));
    for _ in 0..2 {
        assert_eq!(event(&d).unwrap(), RenameEventWire::AlreadyApplied);
    }
    check_fixed(&d);
    assert_eq!(moves(&d).len(), 1);
    d.close();
}

#[test]
fn the_watchers_pairing_fixes_the_same_move_and_a_later_event_is_already_applied() {
    let d = daemon();
    move_on_disk(&d);
    d.registry.rescan("notes").expect("rescan notes");
    d.registry.rescan("other").expect("rescan other");
    for _ in 0..2 {
        d.engine
            .fix_move("notes", "Ideas/Target.md", "other", "docs/target.md")
            .expect("pairing");
    }
    check_fixed(&d);
    assert_eq!(moves(&d).len(), 1);
    assert_eq!(event(&d).unwrap(), RenameEventWire::AlreadyApplied);
    assert_eq!(moves(&d).len(), 1);
    d.close();
}

#[test]
fn a_move_nothing_links_to_is_logged_and_there_is_nothing_to_fix() {
    let d = Daemon::start(
        &[("Ideas/Lonely.md", "# Lonely\n")],
        &[("Other.md", "# Other\n")],
        fix_world::real_writer(),
        false,
    );
    let to = d.other.root().join("Lonely.md");
    std::fs::rename(d.notes.root().join("Ideas/Lonely.md"), &to).unwrap();
    use tephrad::rpc::backend::FixBackend;
    let reply = d
        .service
        .rename_event(
            &d.notes_root().join("Ideas/Lonely.md").display().to_string(),
            &d.other_root().join("Lonely.md").display().to_string(),
        )
        .expect("event");
    assert_eq!(reply, RenameEventWire::NothingToFix);
    // It was still logged: the watcher's pairing of it does nothing more.
    d.registry.rescan("notes").unwrap();
    d.registry.rescan("other").unwrap();
    d.engine
        .fix_move("notes", "Ideas/Lonely.md", "other", "Lonely.md")
        .unwrap();
    assert_eq!(moves(&d).len(), 1);
    assert!(d.entries().iter().all(|e| e.kind != "uncertain"));
    d.close();
}

#[test]
fn a_copy_to_another_source_is_not_a_move() {
    let d = daemon();
    let to = d.other.root().join("docs/target.md");
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(d.notes.root().join("Ideas/Target.md"), &to).unwrap();
    assert_eq!(event(&d).unwrap(), RenameEventWire::NothingToFix);
    assert!(d.entries().is_empty(), "{:?}", d.entries());
    assert_eq!(d.text("Index.md"), notes()[2].1);
    d.close();
}

#[test]
fn a_move_whose_new_path_is_ignored_in_its_source_changes_nothing() {
    let d = daemon();
    d.other.write(".tephraignore", "docs/\n");
    let to = d.other.root().join("docs/target.md");
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::rename(d.notes.root().join("Ideas/Target.md"), &to).unwrap();
    assert_eq!(event(&d).unwrap(), RenameEventWire::NothingToFix);
    assert_eq!(d.text("Index.md"), notes()[2].1);
    d.close();
}

#[test]
fn the_real_watchers_pair_a_move_across_sources_after_the_hold() {
    let d = Daemon::start(&notes(), &others(), fix_world::real_writer(), true);
    d.registry
        .set_rename_hold(std::time::Duration::from_millis(300));
    move_on_disk(&d);
    let done = wait_until(WAIT, || !moves(&d).is_empty());
    if !done && !d.watcher_live {
        eprintln!("the OS watcher was deaf; nothing to check");
        d.close();
        return;
    }
    assert!(done, "no move was fixed: {:?}", d.entries());
    // The three files are written one after the other: wait for all of
    // them, not for the first.
    assert!(
        wait_until(WAIT, || fixed(&d) == [true; 3]),
        "not all three rewrites were written: {:?}",
        fixed(&d)
    );
    check_fixed(&d);
    assert_eq!(moves(&d).len(), 1);
    d.close();
}

fn fresh_engine(d: &Daemon) -> FixEngine {
    FixEngine::new(&d.cache_path, fix_world::real_writer())
}

fn fixed(d: &Daemon) -> [bool; 3] {
    [
        d.text("Index.md") == INDEX_AFTER,
        d.other_text("Reader.md") == READER_AFTER,
        d.other_text("docs/target.md") == TARGET_AFTER,
    ]
}

#[test]
fn a_kill_between_the_rewrites_resumes_the_rest_and_leaves_one_entry_and_no_half_file() {
    // n = 0 is a kill after the transaction and before the first write.
    for n in 0..=2usize {
        let d = daemon();
        move_on_disk(&d);
        d.registry.rescan("notes").unwrap();
        d.registry.rescan("other").unwrap();
        let killed = fresh_engine(&d)
            .with_stop_after_writes(n)
            .begin_move_announced("notes", "Ideas/Target.md", "other", "docs/target.md");
        assert_eq!(killed.unwrap_err(), FixError::Stopped, "n={n}");

        // Every file is whole: either as it was or fully rewritten.
        let was = [
            notes()[2].1.to_string(),
            others()[0].1.to_string(),
            TARGET.to_string(),
        ];
        let now = [
            d.text("Index.md"),
            d.other_text("Reader.md"),
            d.other_text("docs/target.md"),
        ];
        let after = [INDEX_AFTER, READER_AFTER, TARGET_AFTER];
        let mut written = 0;
        for i in 0..3 {
            assert!(
                now[i] == was[i] || now[i] == after[i],
                "n={n} file {i} is half-written: {}",
                now[i]
            );
            written += usize::from(now[i] == after[i]);
        }
        // At least the writes before the kill: the daemon's own forwarder
        // retries pending rows on its own thread, so it may have finished
        // more by now. The end state is checked after the resume below.
        assert!(
            written >= n,
            "n={n}: the writes before the kill are on disk"
        );
        assert_eq!(
            moves(&d).len(),
            1,
            "n={n}: the entry exists before the first write"
        );

        // Restart: a new engine over the same cache finishes the work.
        // How many rows this resume wrote is not fixed (the forwarder may
        // have finished some first), so check the end state only.
        fresh_engine(&d).resume().expect("resume");
        assert!(
            wait_until(WAIT, || fixed(&d) == [true; 3]
                && Cache::open(&d.cache_path)
                    .unwrap()
                    .pending_fixes(None, None)
                    .unwrap()
                    .is_empty()),
            "n={n}: not finished: {:?}",
            fixed(&d)
        );
        let m = moves(&d);
        assert_eq!(m.len(), 1, "n={n}: still one entry");
        assert_eq!((m[0].status.as_str(), m[0].files_changed), ("applied", 3));
        d.close();
    }
}

#[test]
fn the_push_and_the_log_entry_carry_both_sources_and_the_total_of_all_three_rewrites() {
    let d = daemon();
    let mut client = d.connect();
    move_on_disk(&d);
    assert!(matches!(
        event(&d).unwrap(),
        RenameEventWire::Applied { files_changed: 3 }
    ));
    let pushes = client.drain(std::time::Duration::from_millis(800));
    let notices: Vec<_> = fix_world::with_fixes(&pushes)
        .iter()
        .flat_map(|p| fix_world::fixes_of(p).clone())
        .collect();
    assert_eq!(
        notices.len(),
        1,
        "one push entry for the whole move: {notices:?}"
    );
    assert_eq!(notices[0]["kind"], "move");
    assert_eq!(notices[0]["source"], "notes");
    assert_eq!(notices[0]["new_source"], "other");
    assert_eq!(notices[0]["files_changed"], 3);
    assert_eq!(moves(&d)[0].files_changed, 3);
    d.close();
}

/// PR-7 R1 / G17: the move writes files in BOTH sources, and the clients
/// still get ONE push for it: not one per source with the second one
/// empty of fixes (which the forwarder would take for a watcher batch).
/// The push names the files of both sources.
#[test]
fn a_move_that_writes_in_two_sources_is_one_push_naming_the_files_of_both() {
    let d = daemon();
    let mut client = d.connect();
    move_on_disk(&d);
    assert!(matches!(
        event(&d).unwrap(),
        RenameEventWire::Applied { files_changed: 3 }
    ));
    let pushes = client.drain(std::time::Duration::from_millis(1200));
    let notes_root = d.notes_root();
    let other_root = d.other_root();
    let named = |p: &Value| -> Vec<PathBuf> {
        p["params"]["paths"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(PathBuf::from)
                    .collect()
            })
            .unwrap_or_default()
    };
    // The user's own `mv` makes the watcher announce the moved file (a
    // plain push, no fixes). The fix makes ONE push of its own, and no
    // other push names a file only the fix rewrote (Index.md, Reader.md),
    // which is what a second per-source push for the fix would do.
    let with = fix_world::with_fixes(&pushes);
    assert_eq!(
        with.len(),
        1,
        "one push for the whole move, none for the second source: {pushes:?}"
    );
    for p in &pushes {
        if p["params"].get("fixes").is_some() {
            continue;
        }
        for f in named(p) {
            assert!(
                f != notes_root.join("Index.md") && f != other_root.join("Reader.md"),
                "a push without fixes names a file only the fix wrote: {p}"
            );
        }
    }
    let push = with[0];
    assert_eq!(fix_world::fixes_of(push).len(), 1, "{push}");
    assert_eq!(push["params"]["source"], "notes", "{push}");
    let paths = named(push);
    for want in [
        notes_root.join("Index.md"),
        other_root.join("Reader.md"),
        other_root.join("docs/target.md"),
    ] {
        assert!(
            paths.contains(&want),
            "{} is named in {paths:?}",
            want.display()
        );
    }
    d.close();
}

#[test]
fn a_read_only_linking_file_is_skipped_and_the_others_are_written() {
    use std::os::unix::fs::PermissionsExt;
    let d = daemon();
    move_on_disk(&d);
    let ro = d.other.root().join("Reader.md");
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o444)).unwrap();
    let reply = event(&d).unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!matches!(reply, RenameEventWire::NothingToFix), "{reply:?}");
    assert_eq!(d.text("Index.md"), INDEX_AFTER);
    assert_eq!(d.other_text("docs/target.md"), TARGET_AFTER);
    assert_eq!(
        d.other_text("Reader.md"),
        others()[0].1,
        "the read-only file is untouched"
    );
    let m = moves(&d);
    assert_eq!(m.len(), 1);
    assert_ne!(m[0].status, "applied", "{m:?}");
    d.close();
}

#[test]
fn a_move_out_to_a_path_no_source_owns_fixes_nothing_and_a_move_in_is_a_plain_create() {
    let d = daemon();
    let outside = tempfile::tempdir().unwrap();
    use tephrad::rpc::backend::FixBackend;
    // Out: the file leaves for a folder that is no source.
    let to = outside.path().join("target.md");
    std::fs::rename(d.notes.root().join("Ideas/Target.md"), &to).unwrap();
    let reply = d.service.rename_event(
        &d.notes_root().join("Ideas/Target.md").display().to_string(),
        &to.canonicalize().unwrap().display().to_string(),
    );
    assert!(
        matches!(reply, Ok(RenameEventWire::NothingToFix) | Err(_)),
        "{reply:?}"
    );
    assert!(moves(&d).is_empty());
    assert_eq!(d.text("Index.md"), notes()[2].1, "nothing is rewritten");
    // In: a file from outside appears in a source. Nothing vanished with
    // it, so no pair and no fix.
    d.other.write("docs/new.md", "# New\n");
    d.registry.rescan("other").unwrap();
    assert!(moves(&d).is_empty());
    d.close();
}

#[test]
fn a_move_into_a_source_that_is_offline_is_refused_and_nothing_is_written() {
    let d = daemon();
    move_on_disk(&d);
    let root = d.other_root();
    d.registry.unregister("other").expect("unregister");
    use tephrad::rpc::backend::FixBackend;
    let reply = d.service.rename_event(
        &d.notes_root().join("Ideas/Target.md").display().to_string(),
        &root.join("docs/target.md").display().to_string(),
    );
    assert!(
        matches!(reply, Ok(RenameEventWire::NothingToFix) | Err(_)),
        "{reply:?}"
    );
    assert!(moves(&d).is_empty(), "{:?}", d.entries());
    assert_eq!(d.text("Index.md"), notes()[2].1);
    assert_eq!(d.other_text("Reader.md"), others()[0].1);
    d.close();
}

// ---------------------------------------------------------------
// R1: the same move again, after it was undone (E7 is not "ever")
// ---------------------------------------------------------------

/// `docs/target.md` of other goes back to `Ideas/Target.md` of notes.
fn move_back_on_disk(d: &Daemon) {
    let to = d.notes.root().join("Ideas/Target.md");
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::rename(d.other.root().join("docs/target.md"), to).expect("move back on disk");
}

fn event_back(d: &Daemon) -> Result<RenameEventWire, tephrad::fixes::event::RenameEventError> {
    use tephrad::rpc::backend::FixBackend;
    d.service.rename_event(
        &d.other_root().join("docs/target.md").display().to_string(),
        &d.notes_root().join("Ideas/Target.md").display().to_string(),
    )
}

fn rescan_both(d: &Daemon) {
    d.registry.rescan("notes").expect("rescan notes");
    d.registry.rescan("other").expect("rescan other");
}

/// The links as they must read when the file is in `other` again: the
/// golden of the first move, which a there-and-back must restore.
fn check_in_other_again(d: &Daemon, round: usize) {
    let index = d.text("Index.md");
    assert!(
        index.contains("[[@other:docs/target]]") && !index.contains("[[Target"),
        "round {round}: every link in notes names the file in other again: {index}"
    );
    assert_eq!(
        d.other_text("Reader.md"),
        READER_AFTER,
        "round {round}: Reader"
    );
    assert_eq!(
        d.other_text("docs/target.md"),
        TARGET_AFTER,
        "round {round}: the moved file's own links"
    );
}

#[test]
fn a_move_there_back_and_there_again_is_fixed_each_time_by_the_event() {
    let d = daemon();
    for round in 0..3 {
        move_on_disk(&d);
        let reply = event(&d).expect("there");
        assert!(
            matches!(reply, RenameEventWire::Applied { files_changed } if files_changed > 0),
            "round {round}: there: {reply:?}"
        );
        check_in_other_again(&d, round);
        assert_eq!(moves(&d).len(), 2 * round + 1, "round {round}");
        // The same event again is the same move, seen twice.
        assert_eq!(event(&d).unwrap(), RenameEventWire::AlreadyApplied);

        if round < 2 {
            move_back_on_disk(&d);
            let reply = event_back(&d).expect("back");
            assert!(
                matches!(reply, RenameEventWire::Applied { .. }),
                "round {round}: back: {reply:?}"
            );
            assert!(d.other_text("Reader.md").contains("@notes:Ideas/Target"));
            assert_eq!(moves(&d).len(), 2 * round + 2, "round {round}");
        }
    }
    d.close();
}

#[test]
fn a_move_there_back_and_there_again_is_fixed_each_time_by_the_watchers_pairing() {
    let d = daemon();
    for round in 0..3 {
        move_on_disk(&d);
        rescan_both(&d);
        d.engine
            .fix_move("notes", "Ideas/Target.md", "other", "docs/target.md")
            .expect("pairing there");
        check_in_other_again(&d, round);
        assert_eq!(moves(&d).len(), 2 * round + 1, "round {round}");
        // A second signal of the same move does nothing.
        d.engine
            .fix_move("notes", "Ideas/Target.md", "other", "docs/target.md")
            .unwrap();
        assert_eq!(moves(&d).len(), 2 * round + 1, "round {round}: repeat");

        if round < 2 {
            move_back_on_disk(&d);
            rescan_both(&d);
            d.engine
                .fix_move("other", "docs/target.md", "notes", "Ideas/Target.md")
                .expect("pairing back");
            assert_eq!(moves(&d).len(), 2 * round + 2, "round {round}: back");
        }
    }
    d.close();
}

#[test]
fn the_event_first_then_the_pairing_does_nothing() {
    let d = daemon();
    move_on_disk(&d);
    assert!(matches!(
        event(&d).unwrap(),
        RenameEventWire::Applied { .. }
    ));
    rescan_both(&d);
    d.engine
        .fix_move("notes", "Ideas/Target.md", "other", "docs/target.md")
        .unwrap();
    check_fixed(&d);
    assert_eq!(moves(&d).len(), 1);
    assert!(d.entries().iter().all(|e| e.kind == "move"));
    d.close();
}

#[test]
fn the_event_and_the_pairing_started_together_fix_and_log_the_move_once() {
    let d = daemon();
    for round in 0..3 {
        move_on_disk(&d);
        rescan_both(&d);
        let gate = Arc::new(Barrier::new(2));
        let watcher = {
            let (engine, gate) = (d.engine.clone(), gate.clone());
            std::thread::spawn(move || {
                gate.wait();
                engine
                    .fix_move("notes", "Ideas/Target.md", "other", "docs/target.md")
                    .expect("pairing");
            })
        };
        gate.wait();
        let reply = event(&d).expect("event");
        watcher.join().unwrap();
        assert!(
            matches!(
                reply,
                RenameEventWire::Applied { .. } | RenameEventWire::AlreadyApplied
            ),
            "round {round}: {reply:?}"
        );
        check_in_other_again(&d, round);
        assert_eq!(moves(&d).len(), 2 * round + 1, "round {round}");
        if round < 2 {
            move_back_on_disk(&d);
            assert!(matches!(
                event_back(&d).unwrap(),
                RenameEventWire::Applied { .. }
            ));
        }
    }
    d.close();
}

// ---------------------------------------------------------------
// E8: a linking file that changed since Tephra read it
// ---------------------------------------------------------------

/// Edits the file called `victim` each time the writer is about to
/// replace it, so every attempt finds it changed since it was read.
struct ChangingFs {
    victim: &'static str,
    edits: AtomicUsize,
}

fn target_of(tmp: &Path) -> PathBuf {
    let name = tmp.file_name().unwrap().to_str().unwrap();
    let original = name
        .strip_prefix('.')
        .unwrap()
        .rsplit_once(".tephra-tmp-")
        .unwrap()
        .0;
    tmp.with_file_name(original)
}

impl AtomicFs for ChangingFs {
    fn write_new(&self, path: &Path, content: &[u8]) -> io::Result<()> {
        RealFs.write_new(path, content)
    }
    fn set_permissions(&self, path: &Path, perms: fs::Permissions) -> io::Result<()> {
        RealFs.set_permissions(path, perms)
    }
    fn sync(&self, path: &Path) -> io::Result<()> {
        RealFs.sync(path)?;
        let target = target_of(path);
        if target.file_name().and_then(|n| n.to_str()) == Some(self.victim) {
            use std::io::Write;
            let mut f = fs::OpenOptions::new().append(true).open(&target)?;
            f.write_all(b"edited meanwhile\n")?;
            drop(f);
            let now = fs::metadata(&target)?.modified()?;
            filetime::set_file_mtime(
                &target,
                filetime::FileTime::from_system_time(now + Duration::from_secs(7)),
            )?;
            self.edits.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.rename(from, to)
    }
}

#[test]
fn a_linking_file_changed_since_it_was_read_is_kept_and_the_move_is_partial_never_applied() {
    // One victim in the old source, one in the new one.
    for (victim, in_other, deferred_name) in [
        ("Index.md", false, "Index.md"),
        ("Reader.md", true, "@other:Reader.md"),
    ] {
        let fs_ = Arc::new(ChangingFs {
            victim,
            edits: AtomicUsize::new(0),
        });
        let d = Daemon::start(&notes(), &others(), writer_over(fs_.clone()), false);
        move_on_disk(&d);
        let reply = event(&d).expect("event");
        match &reply {
            RenameEventWire::Partial {
                files_changed,
                deferred,
            } => {
                assert_eq!(*files_changed, 2, "{victim}: the other two files");
                assert_eq!(deferred.len(), 1, "{victim}: {deferred:?}");
                assert_eq!(deferred[0].file, deferred_name, "{victim}");
                assert_eq!(deferred[0].reason, "changed", "{victim}");
            }
            other => panic!("{victim}: not partial: {other:?}"),
        }
        assert!(fs_.edits.load(Ordering::SeqCst) >= 1, "{victim}");

        let now = if in_other {
            d.other_text(victim)
        } else {
            d.text(victim)
        };
        assert!(now.ends_with("edited meanwhile\n"), "{victim}: kept: {now}");
        let before = if in_other {
            others()[0].1
        } else {
            notes()[2].1
        };
        assert!(now.starts_with(before), "{victim}: not overwritten: {now}");

        let m = moves(&d);
        assert_eq!(m.len(), 1, "{victim}");
        assert_ne!(m[0].status, "applied", "{victim}: {m:?}");
        assert_eq!(m[0].status, "partial", "{victim}: {m:?}");
        let pending = Cache::open(&d.cache_path)
            .unwrap()
            .pending_fixes(None, None)
            .unwrap();
        assert_eq!(pending.len(), 1, "{victim}: {pending:?}");
        assert_eq!(pending[0].file_path, victim);
        assert_eq!(pending[0].source, if in_other { "other" } else { "notes" });
        // The same event again is answered from the log.
        assert_eq!(event(&d).unwrap(), RenameEventWire::AlreadyApplied);
        d.close();
    }
}

// ---------------------------------------------------------------
// Reverse prefix: a link that was @other: and now resolves at home
// ---------------------------------------------------------------

#[test]
fn a_prefixed_link_of_the_moved_file_into_its_new_source_loses_the_prefix() {
    const OWN: &str = "# Target\n\nthere [[@other:Elsewhere]] and [[@other:Elsewhere|the alias]] and ![[@other:Elsewhere]]; \
here [[Sibling]]; gone [[@other:Nothing]].\n";
    const OWN_AFTER: &str =
        "# Target\n\nthere [[Elsewhere]] and [[Elsewhere|the alias]] and ![[Elsewhere]]; \
here [[@notes:Sibling]]; gone [[@other:Nothing]].\n";
    let d = Daemon::start(
        &[
            ("Ideas/Target.md", OWN),
            ("Sibling.md", "# Sibling\n"),
            ("Index.md", "see [[Target]]\n"),
        ],
        &others(),
        fix_world::real_writer(),
        false,
    );
    move_on_disk(&d);
    let reply = event(&d).expect("event");
    assert!(
        matches!(reply, RenameEventWire::Applied { files_changed: 3 }),
        "{reply:?}"
    );
    assert_eq!(d.other_text("docs/target.md"), OWN_AFTER);
    assert_eq!(d.text("Index.md"), "see [[@other:docs/target]]\n");
    d.close();
}

// ---------------------------------------------------------------
// E4, E5, pairing in both orders and the hold: the pool's decisions,
// fixed by the real engine
// ---------------------------------------------------------------

fn at(secs_ms: u64, t0: tokio::time::Instant) -> tokio::time::Instant {
    t0 + Duration::from_millis(secs_ms)
}

fn vanished(source: &str, path: &str, hash: &str) -> Vanished {
    Vanished {
        source: source.into(),
        path: path.into(),
        identity: None,
        hash: Some(hash.into()),
        os: None,
    }
}

fn appeared(source: &str, path: &str, hash: &str) -> Appeared {
    Appeared {
        source: source.into(),
        path: path.into(),
        identity: None,
        hash: Some(hash.into()),
        os: None,
    }
}

fn root_of(d: &Daemon) -> impl Fn(&str) -> Option<PathBuf> + '_ {
    move |source| match source {
        "notes" => Some(d.notes_root()),
        "other" => Some(d.other_root()),
        _ => None,
    }
}

const HOLD: Duration = Duration::from_millis(2000);

#[test]
fn the_delete_seen_before_the_create_pairs_inside_the_hold_and_is_one_move() {
    let d = daemon();
    move_on_disk(&d);
    rescan_both(&d);
    let t0 = tokio::time::Instant::now();
    let mut pool = Detector::new(HOLD);
    let first = pool.ingest(
        t0,
        vec![vanished("notes", "Ideas/Target.md", "h-target")],
        vec![],
    );
    assert!(first.is_empty());
    let second = pool.ingest(
        at(300, t0),
        vec![],
        vec![appeared("other", "docs/target.md", "h-target")],
    );
    assert!(second.renames.is_empty(), "held until the hold has passed");
    let decided = pool.expire(t0 + HOLD + Duration::from_millis(400));
    assert_eq!(decided.renames.len(), 1, "{decided:?}");
    assert!(decided.renames[0].is_move());
    dispatch(&*d.engine, &root_of(&d), decided);
    check_fixed(&d);
    assert_eq!(moves(&d).len(), 1);
    assert!(d.entries().iter().all(|e| e.kind == "move"));
    d.close();
}

#[test]
fn the_create_seen_before_the_delete_pairs_inside_the_hold_and_is_one_move() {
    let d = daemon();
    move_on_disk(&d);
    rescan_both(&d);
    let t0 = tokio::time::Instant::now();
    let mut pool = Detector::new(HOLD);
    assert!(pool
        .ingest(
            t0,
            vec![],
            vec![appeared("other", "docs/target.md", "h-target")]
        )
        .is_empty());
    let _ = pool.ingest(
        at(300, t0),
        vec![vanished("notes", "Ideas/Target.md", "h-target")],
        vec![],
    );
    let decided = pool.expire(t0 + HOLD + Duration::from_millis(400));
    assert_eq!(decided.renames.len(), 1, "{decided:?}");
    dispatch(&*d.engine, &root_of(&d), decided);
    check_fixed(&d);
    assert_eq!(moves(&d).len(), 1);
    d.close();
}

#[test]
fn after_the_hold_a_lone_delete_and_a_late_create_stay_plain_and_links_stay_broken() {
    let d = daemon();
    move_on_disk(&d);
    rescan_both(&d);
    let t0 = tokio::time::Instant::now();
    let mut pool = Detector::new(HOLD);
    let _ = pool.ingest(
        t0,
        vec![vanished("notes", "Ideas/Target.md", "h-target")],
        vec![],
    );
    // The hold ends with no partner: a plain delete, logged uncertain.
    let gave_up = pool.expire(t0 + HOLD + Duration::from_millis(1));
    assert_eq!(gave_up.gave_up_vanished.len(), 1);
    assert!(gave_up.renames.is_empty());
    dispatch(&*d.engine, &root_of(&d), gave_up);
    // The partner turns up late: no pairing after the fact.
    let late = pool.ingest(
        at(10_000, t0),
        vec![],
        vec![appeared("other", "docs/target.md", "h-target")],
    );
    assert!(late.renames.is_empty(), "{late:?}");
    let later = pool.expire(at(20_000, t0));
    assert!(later.renames.is_empty(), "{later:?}");
    dispatch(&*d.engine, &root_of(&d), later);

    assert!(moves(&d).is_empty(), "{:?}", d.entries());
    let uncertain: Vec<_> = d
        .entries()
        .into_iter()
        .filter(|e| e.kind == "uncertain")
        .collect();
    assert_eq!(uncertain.len(), 1, "{:?}", d.entries());
    assert_eq!(d.text("Index.md"), notes()[2].1, "links stay as they were");
    assert_eq!(d.other_text("Reader.md"), others()[0].1);
    d.close();
}

#[test]
fn two_identical_files_vanishing_in_one_source_and_one_appearing_in_another_are_no_pair_and_an_event_resolves_it(
) {
    const EMPTY: &str = "";
    let d = Daemon::start(
        &[
            ("A.md", EMPTY),
            ("B.md", EMPTY),
            ("Links.md", "one [[A]] two [[B]]\n"),
        ],
        &[("Home.md", "# Home\n")],
        fix_world::real_writer(),
        false,
    );
    std::fs::rename(d.notes.root().join("A.md"), d.other.root().join("A2.md")).unwrap();
    std::fs::remove_file(d.notes.root().join("B.md")).unwrap();
    rescan_both(&d);
    let t0 = tokio::time::Instant::now();
    let mut pool = Detector::new(HOLD);
    let _ = pool.ingest(
        t0,
        vec![
            vanished("notes", "A.md", "h-empty"),
            vanished("notes", "B.md", "h-empty"),
        ],
        vec![appeared("other", "A2.md", "h-empty")],
    );
    let decided = pool.expire(t0 + HOLD + Duration::from_millis(1));
    assert!(
        decided.renames.is_empty(),
        "uncertain, no pair: {decided:?}"
    );
    assert_eq!(decided.gave_up_vanished.len(), 2);
    dispatch(&*d.engine, &root_of(&d), decided);

    assert!(moves(&d).is_empty());
    let uncertain = d
        .entries()
        .into_iter()
        .filter(|e| e.kind == "uncertain")
        .count();
    assert_eq!(uncertain, 2, "{:?}", d.entries());
    // The entry says why, in `fix.list` too: the spec's wording (E4).
    for e in d.entries().iter().filter(|e| e.kind == "uncertain") {
        let message = e.message.as_deref().unwrap_or_default();
        assert!(message.starts_with("vanished, no certain pair"), "{e:?}");
    }
    assert_eq!(d.text("Links.md"), "one [[A]] two [[B]]\n", "no fix");

    // An exact client event still resolves the one it names.
    use tephrad::rpc::backend::FixBackend;
    let reply = d
        .service
        .rename_event(
            &d.notes_root().join("A.md").display().to_string(),
            &d.other_root().join("A2.md").display().to_string(),
        )
        .expect("event");
    assert!(
        matches!(reply, RenameEventWire::Applied { .. }),
        "{reply:?}"
    );
    assert_eq!(d.text("Links.md"), "one [[@other:A2]] two [[B]]\n");
    d.close();
}

// ---------------------------------------------------------------
// F3, F8, F11: the fences
// ---------------------------------------------------------------

/// `s` with the text inside every `[[...]]` cut out: what is left is
/// everything outside the link spans.
fn outside_links(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find("[[") {
        out.push_str(&rest[..i + 2]);
        rest = &rest[i + 2..];
        match rest.find("]]") {
            Some(j) => rest = &rest[j..],
            None => break,
        }
    }
    out.push_str(rest);
    out
}

/// Every `[[...]]` span of `s`, without the brackets.
fn link_spans(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find("[[") {
        rest = &rest[i + 2..];
        let Some(j) = rest.find("]]") else { break };
        out.push(rest[..j].to_string());
        rest = &rest[j + 2..];
    }
    out
}

fn bytes_of(d: &Daemon) -> std::collections::BTreeMap<String, Vec<u8>> {
    let mut all = std::collections::BTreeMap::new();
    for (name, root) in [("notes", d.notes.root()), ("other", d.other.root())] {
        for (rel, (bytes, _)) in fixtures::tree_snapshot(root) {
            all.insert(format!("{name}:{rel}"), bytes);
        }
    }
    all
}

#[test]
fn f3_only_link_text_changes_and_the_moved_file_is_otherwise_untouched() {
    let d = daemon();
    let before = bytes_of(&d);
    move_on_disk(&d);
    let moved_unfixed = bytes_of(&d);
    assert!(matches!(
        event(&d).unwrap(),
        RenameEventWire::Applied { .. }
    ));
    let after = bytes_of(&d);

    assert_eq!(
        moved_unfixed.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "the fix adds, removes and renames no file"
    );
    assert!(after.contains_key("other:docs/target.md"));
    assert!(!after.contains_key("notes:Ideas/Target.md"));
    let mut rewritten = 0;
    for (name, was) in &moved_unfixed {
        let now = &after[name];
        let (was, now) = (
            String::from_utf8(was.clone()).unwrap(),
            String::from_utf8(now.clone()).unwrap(),
        );
        assert_eq!(
            outside_links(&was),
            outside_links(&now),
            "{name}: something outside the link spans changed"
        );
        rewritten += usize::from(was != now);
    }
    assert_eq!(rewritten, 3, "exactly the three files were rewritten");
    // The moved file's body is the one it had.
    assert_eq!(
        outside_links(&String::from_utf8(before["notes:Ideas/Target.md"].clone()).unwrap()),
        outside_links(&d.other_text("docs/target.md"))
    );
    d.close();
}

#[test]
fn f11_every_cross_source_link_written_carries_its_prefix() {
    let d = daemon();
    move_on_disk(&d);
    assert!(matches!(
        event(&d).unwrap(),
        RenameEventWire::Applied { .. }
    ));
    for span in link_spans(&d.text("Index.md")) {
        assert!(
            span.starts_with("@other:"),
            "bare cross-source link: {span}"
        );
    }
    for span in link_spans(&d.other_text("docs/target.md")) {
        if span.contains("Sibling") {
            assert!(
                span.starts_with("@notes:"),
                "bare cross-source link: {span}"
            );
        }
    }
    d.close();
}

fn graph_of(d: &Daemon) -> UnitGraph {
    UnitGraph::build_from_cache(&Cache::open(&d.cache_path).unwrap()).unwrap()
}

#[test]
fn f11_a_bare_link_written_after_the_move_does_not_resolve_to_the_other_source() {
    let d = daemon();
    move_on_disk(&d);
    assert!(matches!(
        event(&d).unwrap(),
        RenameEventWire::Applied { .. }
    ));
    d.notes.write("Bare.md", "[[docs/target]] and [[target]]\n");
    rescan_both(&d);
    let graph = graph_of(&d);
    for bare in ["[[docs/target]]", "[[target]]", "[[rename-notes]]"] {
        let r = graph.resolve_link(bare, Some("notes"));
        assert!(
            matches!(r, Err(ResolveError::Unresolved { .. })),
            "{bare} resolved across sources: {r:?}"
        );
    }
    // The prefixed form, which the fix writes, does.
    let ok = graph
        .resolve_link("[[@other:docs/target]]", Some("notes"))
        .expect("prefixed");
    assert_eq!(ok.unit.id, "@other:docs/target.md");
    // And the bare link written there was not rewritten by anything.
    assert_eq!(d.text("Bare.md"), "[[docs/target]] and [[target]]\n");
    d.close();
}

#[test]
fn f8_a_block_cut_in_one_source_and_pasted_in_another_is_not_followed() {
    let d = Daemon::start(
        &[
            ("Old.md", "# Old\n\nA claim. ^k3f9a2\n"),
            ("Linker.md", "see [[Old#^k3f9a2]]\n"),
        ],
        &[("Pasted.md", "# Pasted\n")],
        fix_world::real_writer(),
        false,
    );
    // Cut from notes, pasted into other.
    d.notes.write("Old.md", "# Old\n");
    d.other.write("Pasted.md", "# Pasted\n\nA claim. ^k3f9a2\n");
    rescan_both(&d);
    let r = graph_of(&d).resolve_link("[[Old#^k3f9a2]]", Some("notes"));
    assert!(
        matches!(r, Err(ResolveError::Unresolved { .. })),
        "the moved-ID search left its source: {r:?}"
    );
    assert_eq!(
        d.text("Linker.md"),
        "see [[Old#^k3f9a2]]\n",
        "not re-pointed"
    );
    assert!(d.entries().is_empty(), "{:?}", d.entries());

    // Control: pasted into a file of the SAME source it is followed.
    d.notes.write("New.md", "# New\n\nA claim. ^k3f9a2\n");
    rescan_both(&d);
    let found = graph_of(&d)
        .resolve_link("[[Old#^k3f9a2]]", Some("notes"))
        .expect("followed inside its source");
    assert_eq!(found.unit.id, "@notes:New.md");
    assert!(found.block.is_some_and(|b| b.moved));
    d.close();
}

/// A bare link in the moved file that never named the moved file (three
/// `Plan` files tied before the move, so `[[Plan]]` pointed at none of
/// them) is left as it is. After the move it is still a tie, which the
/// graph reports the same way as a link to a file that is gone; it must
/// not be taken for a link to the moved file and respelled.
#[test]
fn a_bare_link_in_the_moved_file_that_was_a_tie_before_the_move_is_left_alone() {
    let own = "# Plan\n\nSee [[Plan]].\n";
    let d = Daemon::start(
        &[
            ("X/Plan.md", own),
            ("Y/Plan.md", "# Y\n"),
            ("Z/Plan.md", "# Z\n"),
            ("Index.md", "the plan: [[X/Plan]]\n"),
        ],
        &[],
        fix_world::real_writer(),
        false,
    );
    let to = d.other.root().join("docs/plan.md");
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::rename(d.notes.root().join("X/Plan.md"), &to).unwrap();
    {
        use tephrad::rpc::backend::FixBackend;
        let reply = d
            .service
            .rename_event(
                &d.notes_root().join("X/Plan.md").display().to_string(),
                &to.display().to_string(),
            )
            .expect("event");
        assert!(
            matches!(reply, RenameEventWire::Applied { .. }),
            "{reply:?}"
        );
    }
    assert_eq!(
        d.text("Index.md"),
        "the plan: [[@other:docs/plan]]\n",
        "a link that did name the file is fixed"
    );
    assert_eq!(
        d.other_text("docs/plan.md"),
        own,
        "the tied link in the moved file is not respelled"
    );
    d.close();
}
