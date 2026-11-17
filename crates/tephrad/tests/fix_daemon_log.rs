//! T2: the daemon log shows each automatic fix. The entry itself is in
//! `fix.list`; this checks the one `info` line per entry that
//! `tephrad.log` carries, through the real `FixEngine` and the same
//! `RenameFixer` calls the rename detector's dispatch makes.

mod fixtures;

use std::io;
use std::sync::{Arc, Mutex};

use fixtures::{FixWorld, GP1_FILES};
use tephrad::fixes::FixEngine;
use tephrad::refactor::fix_write::{FixWriter, ThreadSleeper};
use tephrad::refactor::RealFs;
use tephrad::rename_detect::RenameFixer;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// The two tests hit the same `tracing::info!` callsite. tracing caches a
/// callsite's interest across all live subscribers, so two scoped
/// subscribers on parallel threads can leave one capture empty under host
/// load. Every capture takes this lock, so they never overlap.
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

/// Runs `f` with an info-level subscriber and returns what it logged.
fn logged(f: impl FnOnce()) -> String {
    let _serial = CAPTURE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let capture = Capture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = capture.0.lock().unwrap().clone();
    String::from_utf8(bytes).expect("utf8 log")
}

fn engine(w: &FixWorld) -> FixEngine {
    FixEngine::new(
        &w.cache_path,
        Arc::new(FixWriter::with_parts(
            Arc::new(Mutex::new(())),
            Arc::new(RealFs),
            Arc::new(ThreadSleeper),
        )),
    )
}

#[test]
fn a_rename_fix_is_one_info_line_with_kind_old_new_files_and_status() {
    let w = FixWorld::new(&GP1_FILES);
    w.rename("Training Plan.md", "Plan.md");
    let engine = engine(&w);
    let log = logged(|| {
        engine
            .fix_rename("notes", "Training Plan.md", "Plan.md")
            .expect("fix");
    });
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("fix")).collect();
    assert_eq!(lines.len(), 1, "one line per fix: {log}");
    let line = lines[0];
    assert!(line.contains(" INFO "), "{line}");
    for want in [
        "kind=rename",
        "old=\"Training Plan.md\"",
        "new=\"Plan.md\"",
        "files_changed=3",
        "status=applied",
    ] {
        assert!(line.contains(want), "{want} missing from: {line}");
    }
}

#[test]
fn an_uncertain_vanish_is_one_info_line_too() {
    let w = FixWorld::new(&GP1_FILES);
    let engine = engine(&w);
    let log = logged(|| {
        // The trait call dispatch makes, not the engine's own method.
        RenameFixer::record_uncertain(&engine, "notes", "Gone.md").expect("uncertain");
    });
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("fix")).collect();
    assert_eq!(lines.len(), 1, "{log}");
    for want in [
        "kind=uncertain",
        "old=\"Gone.md\"",
        "files_changed=0",
        "status=uncertain",
    ] {
        assert!(lines[0].contains(want), "{want} missing from: {}", lines[0]);
    }
}
