//! T2: the daemon log shows each automatic fix. The entry itself is in
//! `fix.list`; this checks the one `info` line per entry that
//! `tephrad.log` carries, through the real `FixEngine` and the same
//! `RenameFixer` calls the rename detector's dispatch makes.

mod fixtures;

use std::sync::{Arc, Mutex};

use fixtures::{FixWorld, GP1_FILES};
use tephrad::fixes::FixEngine;
use tephrad::refactor::fix_write::{FixWriter, ThreadSleeper};
use tephrad::refactor::RealFs;
use tephrad::rename_detect::RenameFixer;

/// Log capture. One subscriber is installed for the whole test binary,
/// once, and stays. A subscriber set per test with `with_default` races
/// with the tests running on parallel threads (`tracing` caches, per
/// callsite, whether anyone is listening, and a callsite first hit by a
/// thread with no subscriber can stay silent for a test that sets one a
/// moment later), which left some captures empty. The writer appends to
/// a buffer of the emitting thread, and every test runs on its own
/// thread, so a capture sees its own lines and no other test's.
mod capture {
    use std::cell::RefCell;
    use std::io;
    use std::sync::Once;

    thread_local! {
        static LOG: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    struct ThreadLog;

    impl io::Write for ThreadLog {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            LOG.with(|l| l.borrow_mut().extend_from_slice(buf));
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    struct MakeThreadLog;

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for MakeThreadLog {
        type Writer = ThreadLog;
        fn make_writer(&'a self) -> ThreadLog {
            ThreadLog
        }
    }

    /// Runs `f` and returns its result with the log lines it wrote on
    /// this thread.
    pub fn logged<T>(f: impl FnOnce() -> T) -> (T, String) {
        static INSTALL: Once = Once::new();
        INSTALL.call_once(|| {
            let subscriber = tracing_subscriber::fmt()
                .with_writer(MakeThreadLog)
                .with_ansi(false)
                .with_max_level(tracing::Level::INFO)
                .finish();
            tracing::subscriber::set_global_default(subscriber)
                .expect("no other global subscriber in this test binary");
        });
        LOG.with(|l| l.borrow_mut().clear());
        let out = f();
        let text = LOG.with(|l| String::from_utf8(std::mem::take(&mut *l.borrow_mut())).unwrap());
        (out, text)
    }
}

/// Runs `f` and returns what it logged.
fn logged(f: impl FnOnce()) -> String {
    capture::logged(f).1
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
