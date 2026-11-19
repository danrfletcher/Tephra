//! FR22's order for an unpromote, and what a kill leaves behind: the one
//! transaction (the units back in the parent, the source's row and the
//! exclusion gone, the log entry and the pending list), then the
//! per-file rewrites. A test hook on the work-list loop
//! (`FixEngine::with_stop_after_writes`) stops the run as a kill would;
//! the next start works the persisted list off.

mod fix_world;
mod fixtures;

use fix_world::{real_writer, Daemon};
use tephra_cache::Cache;
use tephrad::fixes::{FixEngine, FixError};
use tephrad::registry::RegistryError;

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("proj/A.md", "# A\n\n[[Outside]] [[Two]]\n"),
        ("proj/Two.md", "# Two\n\n[[Outside]]\n"),
        ("One.md", "# One\n\n[[proj/A]]\n"),
        ("Three.md", "# Three\n\n[[proj/Two]]\n"),
        ("Four.md", "# Four\n\n[[proj/A]] and [[Outside]]\n"),
        ("Outside.md", "# O\n"),
    ]
}

const LINKING: [&str; 5] = ["proj/A.md", "proj/Two.md", "One.md", "Three.md", "Four.md"];

fn promoted() -> Daemon {
    let d = Daemon::start(&files(), &[], real_writer(), false);
    d.registry
        .promote("proj", d.notes_root().join("proj"), &d.engine)
        .expect("promote");
    d
}

fn stopped(result: &Result<tephrad::registry::UnpromoteOutcome, RegistryError>) -> bool {
    matches!(result, Err(RegistryError::Fix(m)) if m.contains(&FixError::Stopped.to_string()))
}

fn texts(d: &Daemon) -> Vec<String> {
    LINKING.iter().map(|f| d.text(f)).collect()
}

/// A kill right after the transaction, before any file: the cache is
/// settled (source gone, units back), the pending list is stored, and no
/// file has changed yet.
#[test]
fn killed_after_the_transaction_the_cache_is_settled_and_the_list_is_stored() {
    let d = promoted();
    let before = texts(&d);
    let stopper = FixEngine::new(&d.cache_path, real_writer()).with_stop_after_writes(0);
    let result = d.registry.unpromote("proj", &stopper);
    assert!(stopped(&result), "{result:?}");

    let cache = Cache::open(&d.cache_path).unwrap();
    let parent = cache.get_source("notes").unwrap().unwrap();
    assert!(cache.get_source("proj").unwrap().is_none());
    assert!(cache.promoted_roots().unwrap().is_empty());
    assert!(cache
        .get_unit_by_rel_path(parent.id, "proj/A.md")
        .unwrap()
        .is_some());
    assert!(!cache.pending_fixes(None, None).unwrap().is_empty());
    assert_eq!(texts(&d), before, "no file was written");
    assert_eq!(
        d.entries().iter().filter(|e| e.kind == "unpromote").count(),
        1
    );

    d.engine.resume().expect("resume");
    assert!(d.text("One.md").contains("[[A]]"), "{}", d.text("One.md"));
    assert!(!d.text("proj/A.md").contains("@notes"));
    assert!(cache.pending_fixes(None, None).unwrap().is_empty());
    d.close();
}

/// A kill after file N: the files already written are not touched again,
/// the rest converge on the next start, there is ONE log entry, and a run
/// after that writes nothing.
#[test]
fn killed_after_file_n_the_rest_converge_with_one_entry_and_no_half_written_file() {
    // The reference: the same unpromote with no kill.
    let whole = promoted();
    whole
        .registry
        .unpromote("proj", &whole.engine)
        .expect("unpromote");
    let want = texts(&whole);
    whole.close();

    let d = promoted();
    let original = texts(&d);
    let stopper = FixEngine::new(&d.cache_path, real_writer()).with_stop_after_writes(1);
    let result = d.registry.unpromote("proj", &stopper);
    assert!(stopped(&result), "{result:?}");

    let after_kill = texts(&d);
    let changed: Vec<usize> = (0..LINKING.len())
        .filter(|i| after_kill[*i] != original[*i])
        .collect();
    assert_eq!(changed.len(), 1, "exactly one file was written");

    d.engine.resume().expect("resume");
    let settled = texts(&d);
    assert_eq!(settled, want, "converges on what an unbroken run writes");
    assert_eq!(
        settled[changed[0]], after_kill[changed[0]],
        "the file done before the kill is untouched"
    );
    let entries = d.entries();
    let unpromote: Vec<_> = entries.iter().filter(|e| e.kind == "unpromote").collect();
    assert_eq!(unpromote.len(), 1, "one entry for the whole unpromote");
    assert_eq!(unpromote[0].status, "applied");
    assert_eq!(entries.len(), 2, "the promote entry stays");

    d.engine.resume().expect("resume again");
    assert_eq!(texts(&d), settled, "a second pass writes nothing");
    assert_eq!(d.entries().len(), 2);
    d.close();
}
