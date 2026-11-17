//! E17: the fix engine's own writes are recognised as Tephra's own, by the
//! same path + size + mtime record the duplicate ID rename uses
//! (`dup_ids::note_self_write` / `drop_self_writes`), so a fix never
//! starts another watcher batch, another fix, or another push.
//!
//! Two layers, both through the real code:
//!
//! - the table tests write with the real `FixWriter` and ask the real
//!   `drop_self_writes` (the very call the watcher's batch makes) what it
//!   does with the written file and its temp file: one row per kind of fix
//!   write;
//! - the live tests run the whole daemon (`fix_world::Daemon`: real
//!   registry and watchers, real engine, real forwarder) and count the
//!   events, log entries and pushes after each write. The only fixed waits
//!   are the quiet windows that let a watcher debounce (250 ms) pass.

mod fix_world;
mod fixtures;

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use fix_world::{fixes_of, real_writer, with_fixes, writer_over, Daemon, NoSleep, QUIET, WAIT};
use fixtures::FixWorld;
use tephrad::dup_ids::{drop_self_writes, note_self_write};
use tephrad::refactor::fix_write::{FixWriter, WriteStatus};
use tephrad::refactor::{AtomicFs, RealFs};
use tephrad::registry::DaemonEvent;
use tephrad::rename_detect::RenameFixer;

/// One kind of fix write: the files before the rename `Training Plan.md`
/// to `Plan.md`, the file that links, and what must be in it afterwards.
struct Kind {
    name: &'static str,
    target: &'static str,
    linking: &'static str,
    before: &'static str,
    after_has: &'static str,
}

const PLAIN_TARGET: &str = "# Training Plan\n\n## Goals\n";

const KINDS: [Kind; 6] = [
    Kind {
        name: "wikilink",
        target: PLAIN_TARGET,
        linking: "Link.md",
        before: "See [[Training Plan]].\n",
        after_has: "See [[Plan]].\n",
    },
    Kind {
        name: "embed",
        target: PLAIN_TARGET,
        linking: "Embed.md",
        before: "![[Training Plan]]\n",
        after_has: "![[Plan]]\n",
    },
    Kind {
        name: "frontmatter link",
        target: PLAIN_TARGET,
        linking: "Front.md",
        before: "---\nup: \"[[Training Plan]]\"\n---\n# Note\n",
        after_has: "up: \"[[Plan]]\"",
    },
    Kind {
        name: "@source form",
        target: PLAIN_TARGET,
        linking: "Prefixed.md",
        before: "see [[@notes:Training Plan]]\n",
        after_has: "see [[@notes:Plan]]\n",
    },
    Kind {
        name: "block link",
        target: "# Training Plan\n\nx ^k3f9a2\n",
        linking: "Block.md",
        before: "Block [[Training Plan#^k3f9a2]]\n",
        after_has: "Block [[Plan#^k3f9a2]]\n",
    },
    Kind {
        name: "comment link in a code file",
        target: PLAIN_TARGET,
        linking: "lib.rs",
        before: "// see [[@notes:Training Plan]]\nfn f() {}\n",
        after_has: "// see [[@notes:Plan]]\n",
    },
];

fn files_of(k: &Kind) -> Vec<(&'static str, &'static str)> {
    vec![("Training Plan.md", k.target), (k.linking, k.before)]
}

fn temp_beside(file: &Path) -> PathBuf {
    let name = file.file_name().unwrap().to_str().unwrap();
    file.with_file_name(format!(".{name}.tephra-tmp-3"))
}

fn recording_writer() -> FixWriter {
    let writer = FixWriter::with_parts(
        Arc::new(Mutex::new(())),
        Arc::new(RealFs),
        Arc::new(NoSleep),
    );
    writer.record_own_writes(true);
    writer
}

// ---------------------------------------------------------------
// The table: the record, the written file and its temp file
// ---------------------------------------------------------------

#[test]
fn every_kind_of_fix_write_is_dropped_with_its_temp_file_and_only_once() {
    for k in &KINDS {
        let w = FixWorld::new(&files_of(k));
        w.rename("Training Plan.md", "Plan.md");
        let reqs = w.requests("Training Plan.md", "Plan.md");
        assert_eq!(reqs.len(), 1, "{}: one linking file", k.name);
        // The registry's own watcher must not take the record first.
        w.registry.unregister("notes").unwrap();

        let result = recording_writer().write_file(&reqs[0]);
        assert_eq!(
            result.status,
            WriteStatus::Written,
            "{}: {result:?}",
            k.name
        );
        let file = reqs[0].abs_path.clone();
        assert!(
            fs::read_to_string(&file).unwrap().contains(k.after_has),
            "{}: {}",
            k.name,
            fs::read_to_string(&file).unwrap()
        );

        // The watcher's batch for that write: the file, the temp file the
        // write went through, and an unrelated change.
        let unrelated = file.with_file_name("Unrelated change.md");
        let mut batch: HashSet<PathBuf> =
            [file.clone(), temp_beside(&file), unrelated.clone()].into();
        assert_eq!(drop_self_writes(&mut batch), 1, "{}", k.name);
        assert_eq!(
            batch,
            HashSet::from([unrelated.clone()]),
            "{}: the file and its temp file are dropped, the rest stays",
            k.name
        );

        // Consumed once: the next change to the same path is a real one.
        fs::write(&file, "changed by someone else, and longer than before\n").unwrap();
        let mut again: HashSet<PathBuf> = [file.clone()].into();
        assert_eq!(drop_self_writes(&mut again), 0, "{}", k.name);
        assert_eq!(
            again.len(),
            1,
            "{}: a second change is not swallowed",
            k.name
        );
    }
}

#[test]
fn a_user_edit_after_the_write_differs_in_size_and_is_not_dropped() {
    let k = &KINDS[0];
    let w = FixWorld::new(&files_of(k));
    w.rename("Training Plan.md", "Plan.md");
    let reqs = w.requests("Training Plan.md", "Plan.md");
    w.registry.unregister("notes").unwrap();
    recording_writer().write_file(&reqs[0]);
    let file = reqs[0].abs_path.clone();

    fs::write(&file, "See [[Plan]].\n\nand Dan wrote a line more.\n").unwrap();
    let mut batch: HashSet<PathBuf> = [file.clone()].into();
    assert_eq!(drop_self_writes(&mut batch), 0);
    assert_eq!(batch, HashSet::from([file]));
}

#[test]
fn a_user_edit_after_the_write_differs_in_mtime_and_is_not_dropped() {
    let k = &KINDS[0];
    let w = FixWorld::new(&files_of(k));
    w.rename("Training Plan.md", "Plan.md");
    let reqs = w.requests("Training Plan.md", "Plan.md");
    w.registry.unregister("notes").unwrap();
    recording_writer().write_file(&reqs[0]);
    let file = reqs[0].abs_path.clone();

    // Same length, a later mtime.
    let text = fs::read_to_string(&file).unwrap();
    let edited = text.replace("Plan", "Plam");
    assert_eq!(edited.len(), text.len());
    fs::write(&file, edited).unwrap();
    let mtime = fs::metadata(&file).unwrap().modified().unwrap();
    filetime::set_file_mtime(
        &file,
        filetime::FileTime::from_system_time(mtime + Duration::from_secs(5)),
    )
    .unwrap();
    let mut batch: HashSet<PathBuf> = [file.clone()].into();
    assert_eq!(drop_self_writes(&mut batch), 0);
    assert_eq!(batch.len(), 1);
}

#[test]
fn an_edit_with_the_same_size_and_the_same_mtime_cannot_be_told_apart_the_known_limit() {
    // The record is path + size + mtime, as it is for the duplicate ID
    // rename. An edit that keeps the size and lands in the same mtime tick
    // looks like the write itself, and is dropped. This test documents
    // that limit; it is not a promise to keep it.
    let k = &KINDS[0];
    let w = FixWorld::new(&files_of(k));
    w.rename("Training Plan.md", "Plan.md");
    let reqs = w.requests("Training Plan.md", "Plan.md");
    w.registry.unregister("notes").unwrap();
    recording_writer().write_file(&reqs[0]);
    let file = reqs[0].abs_path.clone();

    let written_at = fs::metadata(&file).unwrap().modified().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let edited = text.replace("Plan", "Plam");
    assert_eq!(edited.len(), text.len());
    fs::write(&file, edited).unwrap();
    filetime::set_file_mtime(&file, filetime::FileTime::from_system_time(written_at)).unwrap();

    let mut batch: HashSet<PathBuf> = [file.clone()].into();
    assert_eq!(
        drop_self_writes(&mut batch),
        1,
        "the known limit of the path + size + mtime rule"
    );
    assert!(batch.is_empty());
}

/// Edits the file being written on every sync, so every attempt finds it
/// changed since it was read.
struct ChangingFs(AtomicBool);

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
        if self.0.load(Ordering::SeqCst) {
            use std::io::Write;
            let target = target_of(path);
            let mut f = fs::OpenOptions::new().append(true).open(&target)?;
            f.write_all(b"more\n")?;
            drop(f);
            let now = fs::metadata(&target)?.modified()?;
            filetime::set_file_mtime(
                &target,
                filetime::FileTime::from_system_time(now + Duration::from_secs(7)),
            )?;
        }
        Ok(())
    }
    fn rename(&self, from: &Path, to: &Path) -> io::Result<()> {
        RealFs.rename(from, to)
    }
}

#[test]
fn a_write_that_fails_leaves_no_record_that_could_hide_a_later_edit() {
    let k = &KINDS[0];
    let w = FixWorld::new(&files_of(k));
    w.rename("Training Plan.md", "Plan.md");
    let reqs = w.requests("Training Plan.md", "Plan.md");
    w.registry.unregister("notes").unwrap();
    let file = reqs[0].abs_path.clone();

    let writer = FixWriter::with_parts(
        Arc::new(Mutex::new(())),
        Arc::new(ChangingFs(AtomicBool::new(true))),
        Arc::new(NoSleep),
    );
    writer.record_own_writes(true);
    let result = writer.write_file(&reqs[0]);
    assert_ne!(result.status, WriteStatus::Written, "{result:?}");

    // The file is as the stand-in left it. Dan then saves it again: a
    // real edit, and nothing of ours to match it against.
    let mut batch: HashSet<PathBuf> = [file.clone()].into();
    assert_eq!(
        drop_self_writes(&mut batch),
        0,
        "no record of a failed write"
    );
    assert_eq!(batch.len(), 1);
    fs::write(&file, "Dan saved it.\n").unwrap();
    let mut batch: HashSet<PathBuf> = [file.clone()].into();
    assert_eq!(drop_self_writes(&mut batch), 0);
    assert_eq!(batch.len(), 1);
}

#[test]
fn a_writer_that_is_not_asked_to_record_leaves_no_record() {
    // The record is only made for a caller that indexes the written files
    // itself (`FixEngine::set_sink`); a bare writer's events still reach
    // the watcher.
    let k = &KINDS[0];
    let w = FixWorld::new(&files_of(k));
    w.rename("Training Plan.md", "Plan.md");
    let reqs = w.requests("Training Plan.md", "Plan.md");
    w.registry.unregister("notes").unwrap();
    let writer = FixWriter::with_parts(
        Arc::new(Mutex::new(())),
        Arc::new(RealFs),
        Arc::new(NoSleep),
    );
    assert_eq!(writer.write_file(&reqs[0]).status, WriteStatus::Written);
    let mut batch: HashSet<PathBuf> = [reqs[0].abs_path.clone()].into();
    assert_eq!(drop_self_writes(&mut batch), 0);
}

// ---------------------------------------------------------------
// Live: events, log entries and pushes after each write
// ---------------------------------------------------------------

/// Every event the registry reports until none has come for `quiet`.
fn drain_events(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    quiet: Duration,
) -> Vec<DaemonEvent> {
    let mut out = Vec::new();
    let mut last = Instant::now();
    while last.elapsed() < quiet {
        match events.try_recv() {
            // The warm-up files of `fix_world` are not part of any test.
            Ok(DaemonEvent::SourceChanged { paths, .. })
                if paths.iter().all(|p| p.ends_with("Warmup.md")) =>
            {
                last = Instant::now();
            }
            Ok(e) => {
                out.push(e);
                last = Instant::now();
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    out
}

fn live_case(k: &Kind) {
    let d = Daemon::start(&files_of(k), &[], real_writer(), false);
    let mut client = d.connect();
    let mut events = d.registry.subscribe();

    // The rename itself is the user's, with its own ordinary events.
    d.mv_and_rescan("Training Plan.md", "Plan.md");
    drain_events(&mut events, QUIET);
    drain_events(&mut events, Duration::from_millis(1));
    let _ = client.drain(Duration::from_millis(300));
    assert_eq!(d.entries().len(), 0, "{}", k.name);

    d.engine
        .fix_rename("notes", "Training Plan.md", "Plan.md")
        .expect("fix");
    let file = d.notes_root().join(k.linking);
    assert!(
        fs::read_to_string(&file).unwrap().contains(k.after_has),
        "{}: written",
        k.name
    );

    // After the write, past the debounce window: exactly the fix's own
    // event for the linking file, and nothing the watcher made of the
    // write or of its temp file. (The user's rename has its own ordinary
    // events, which the OS may deliver late; they are about the renamed
    // file, not the linking one.)
    let after = drain_events(&mut events, QUIET);
    for e in &after {
        let DaemonEvent::SourceChanged { paths, .. } = e;
        assert!(
            paths
                .iter()
                .all(|p| !p.to_string_lossy().contains(".tephra-tmp-")),
            "{}: the temp file was reported: {e:?}",
            k.name
        );
    }
    let about_link: Vec<_> = after
        .iter()
        .filter(|DaemonEvent::SourceChanged { paths, .. }| paths.contains(&file))
        .collect();
    assert_eq!(
        about_link.len(),
        1,
        "{}: events after the fix: {after:?}",
        k.name
    );
    let DaemonEvent::SourceChanged {
        source,
        paths,
        fixes,
    } = about_link[0];
    assert_eq!(source, "notes");
    assert_eq!(paths, &vec![file.clone()], "{}", k.name);
    assert_eq!(fixes.len(), 1, "{}: one fixes entry", k.name);

    // One push, with the entry; no log entry added.
    let pushes = client.drain(Duration::from_millis(300));
    let fixed = with_fixes(&pushes);
    assert_eq!(fixed.len(), 1, "{}: {pushes:?}", k.name);
    assert_eq!(fixes_of(fixed[0])[0]["files_changed"], serde_json::json!(1));
    // No push of any other kind names the linking file: it was indexed
    // once, by the sink, and the watcher added nothing.
    let naming_link = pushes
        .iter()
        .filter(|p| {
            p["params"]["paths"]
                .as_array()
                .is_some_and(|a| a.contains(&serde_json::json!(file.display().to_string())))
        })
        .count();
    assert_eq!(naming_link, 1, "{}: {pushes:?}", k.name);
    assert_eq!(d.entries().len(), 1, "{}", k.name);
    assert_eq!(d.backlinks("@notes:Plan.md"), 1, "{}", k.name);
    d.close();
}

macro_rules! live_kind {
    ($fn_name:ident, $idx:expr) => {
        #[test]
        fn $fn_name() {
            live_case(&KINDS[$idx]);
        }
    };
}

live_kind!(live_a_wikilink_fix_is_one_event_one_push_one_entry, 0);
live_kind!(live_an_embed_fix_is_one_event_one_push_one_entry, 1);
live_kind!(
    live_a_frontmatter_link_fix_is_one_event_one_push_one_entry,
    2
);
live_kind!(live_an_at_source_fix_is_one_event_one_push_one_entry, 3);
live_kind!(live_a_block_link_fix_is_one_event_one_push_one_entry, 4);
live_kind!(live_a_code_comment_fix_is_one_event_one_push_one_entry, 5);

/// Saves `file` with `text(round)` until the registry reports an ordinary
/// change (no `fixes`) naming it, or `WAIT` has passed. The OS watcher is
/// known to miss an event now and then, so a save that went unseen is
/// repeated with different content (a different size, so it can never
/// be taken for the fix's own write).
fn save_until_reported(
    events: &mut tokio::sync::broadcast::Receiver<DaemonEvent>,
    file: &Path,
    text: impl Fn(usize) -> String,
) -> bool {
    let deadline = Instant::now() + WAIT;
    let mut round = 0;
    while Instant::now() < deadline {
        fs::write(file, text(round)).unwrap();
        round += 1;
        let seen = drain_events(events, Duration::from_millis(1500))
            .iter()
            .any(|DaemonEvent::SourceChanged { paths, fixes, .. }| {
                fixes.is_empty() && paths.contains(&file.to_path_buf())
            });
        if seen {
            return true;
        }
    }
    false
}

#[test]
fn live_a_user_edit_made_right_after_the_fix_is_indexed_and_reported_as_an_ordinary_change() {
    let k = &KINDS[0];
    let d = Daemon::start(&files_of(k), &[], real_writer(), false);
    let mut events = d.registry.subscribe();
    d.mv_and_rescan("Training Plan.md", "Plan.md");
    drain_events(&mut events, QUIET);

    // The fix writes, and Dan saves the same file again before the
    // watcher's debounce window has passed: both reach one batch, with
    // the file's size and mtime no longer those of the write.
    d.engine
        .fix_rename("notes", "Training Plan.md", "Plan.md")
        .expect("fix");
    let file = d.notes_root().join(k.linking);
    let ok = save_until_reported(&mut events, &file, |n| {
        format!(
            "See [[Plan]] and [[Goals]], Dan added a link{}.\n",
            "!".repeat(n)
        )
    });
    assert!(ok, "the edit is indexed and reported as an ordinary change");
    // Indexed: the graph sees the link Dan added.
    let cache = tephra_cache::Cache::open(&d.cache_path).unwrap();
    let graph = tephra_graph::UnitGraph::build_from_cache(&cache).unwrap();
    assert_eq!(graph.get_forward_links("@notes:Link.md").len(), 2);
    assert_eq!(d.entries().len(), 1, "no entry for the edit");
    d.close();
}

#[test]
fn live_a_second_external_change_after_the_fixs_own_event_is_not_swallowed() {
    let k = &KINDS[0];
    let d = Daemon::start(&files_of(k), &[], real_writer(), false);
    let mut events = d.registry.subscribe();
    d.mv_and_rescan("Training Plan.md", "Plan.md");
    drain_events(&mut events, QUIET);
    d.engine
        .fix_rename("notes", "Training Plan.md", "Plan.md")
        .expect("fix");
    let file = d.notes_root().join(k.linking);
    let own = drain_events(&mut events, QUIET);
    assert_eq!(
        own.iter()
            .filter(|DaemonEvent::SourceChanged { paths, .. }| paths.contains(&file))
            .count(),
        1,
        "{own:?}"
    );

    let ok = save_until_reported(&mut events, &file, |n| {
        format!(
            "See [[Plan]] and then some, from outside{}.\n",
            "!".repeat(n)
        )
    });
    assert!(ok, "the second change was reported");
    d.close();
}

#[test]
fn live_a_fix_that_rewrites_three_files_makes_no_further_fix() {
    // The watcher reads the rename itself and fixes it, as in `main.rs`;
    // the three rewritten files' events must not be read as a rename.
    let (d, mut client, push) =
        Daemon::start_and_see_rename_fixed(&fixtures::GP1_FILES, "Training Plan.md", "Plan.md");
    assert_eq!(fixes_of(&push)[0]["files_changed"], 3);

    let later = client.drain(QUIET);
    assert!(with_fixes(&later).is_empty(), "{later:?}");
    let entries = d.entries();
    assert_eq!(
        entries.len(),
        1,
        "one rename fix, no more, no uncertain: {entries:?}"
    );
    assert_eq!(entries[0].kind, "rename");
    d.close();
}

#[test]
fn note_self_write_of_a_missing_path_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let gone = dir.path().join("never-written.md");
    note_self_write(&gone);
    let mut batch: HashSet<PathBuf> = [gone].into();
    assert_eq!(drop_self_writes(&mut batch), 0);
    let _ = writer_over(Arc::new(RealFs));
}
