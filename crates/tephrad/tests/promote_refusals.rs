//! E13: `source.promote` refused with a stable reason and NOTHING changed:
//! no registry row, no file edit, no log entry, no push. One test per
//! reason string, each asserting the error text and that the files, the
//! registry and the cache are byte-identical before and after.

mod fix_world;
mod fixtures;

use std::path::{Path, PathBuf};
use std::time::Duration;

use fix_world::{real_writer, Daemon};
use tephra_cache::{Cache, SourceStatus};
use tephrad::registry::RegistryError;

fn daemon() -> Daemon {
    Daemon::start(
        &[
            ("proj/A.md", "# A\n\n[[Outside]]\n"),
            ("proj/sub/B.md", "# B\n"),
            ("Outside.md", "# O\n\n[[proj/A]]\n"),
        ],
        &[("Other.md", "# other\n")],
        real_writer(),
        false,
    )
}

/// Everything a refusal must leave alone, as plain values.
#[derive(Debug, PartialEq)]
struct World {
    files: std::collections::BTreeMap<String, (Vec<u8>, std::time::SystemTime)>,
    sources: Vec<(String, String, String)>,
    promoted: usize,
    log: usize,
    pending: usize,
    units: Vec<(String, String)>,
}

fn world(d: &Daemon) -> World {
    let cache = Cache::open(&d.cache_path).expect("cache");
    let mut files = fixtures::tree_snapshot(d.notes.root());
    files.extend(
        fixtures::tree_snapshot(d.other.root())
            .into_iter()
            .map(|(k, v)| (format!("other/{k}"), v)),
    );
    let sources: Vec<_> = d
        .registry
        .list()
        .unwrap()
        .into_iter()
        .map(|r| (r.name, r.path, format!("{:?}", r.status)))
        .collect();
    let units: Vec<(String, String)> = {
        let mut stmt = cache
            .connection()
            .prepare(
                "SELECT s.name, u.rel_path FROM units u JOIN sources s ON s.id = u.source_id \
                 ORDER BY 1, 2",
            )
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    };
    assert!(units.len() >= 4, "the snapshot sees the units: {units:?}");
    World {
        files,
        sources,
        promoted: cache.promoted_roots().unwrap().len(),
        log: d.engine.list(None, 100).unwrap().len(),
        pending: cache.pending_fixes(None, None).unwrap().len(),
        units,
    }
}

/// Promotes `name` at `root`, wants the refusal `reason` whose text
/// contains `text`, and checks nothing at all changed or was pushed.
fn refused(d: &Daemon, name: &str, root: &Path, reason: &str, text: &str) {
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));
    let before = world(d);
    match d.registry.promote(name, root, &d.engine) {
        Err(RegistryError::Promote(e)) => {
            assert_eq!(e.reason(), reason, "{e}");
            assert!(e.to_string().contains(text), "{e}");
        }
        other => panic!("wanted {reason}, got {other:?}"),
    }
    assert_eq!(world(d), before, "{reason}: nothing may change");
    assert!(
        client.next_push(Duration::from_millis(600)).is_none(),
        "{reason}: nothing may be pushed"
    );
}

fn root(d: &Daemon, rel: &str) -> PathBuf {
    d.notes_root().join(rel)
}

#[test]
fn is_a_source_root_for_the_whole_vault() {
    let d = daemon();
    refused(
        &d,
        "x",
        &d.notes_root(),
        "is_a_source_root",
        "already the root of the source \"notes\"",
    );
    d.close();
}

#[test]
fn is_a_source_root_for_any_registered_root() {
    let d = daemon();
    refused(
        &d,
        "x",
        &d.other_root(),
        "is_a_source_root",
        "source \"other\"",
    );
    d.close();
}

#[test]
fn not_inside_a_source() {
    let d = daemon();
    let elsewhere = fixtures::empty_source();
    elsewhere.mkdir("folder");
    let folder = elsewhere.root().join("folder").canonicalize().unwrap();
    refused(
        &d,
        "x",
        &folder,
        "not_inside_a_source",
        "is not inside any registered source",
    );
    d.close();
}

#[test]
fn not_a_folder_for_a_file() {
    let d = daemon();
    refused(
        &d,
        "x",
        &root(&d, "Outside.md"),
        "not_a_folder",
        "is not a folder",
    );
    d.close();
}

#[test]
fn not_a_folder_for_a_path_that_is_not_there() {
    let d = daemon();
    refused(
        &d,
        "x",
        &root(&d, "missing"),
        "not_a_folder",
        "is not a folder",
    );
    d.close();
}

#[test]
fn source_offline() {
    let d = daemon();
    Cache::open(&d.cache_path)
        .unwrap()
        .transition_source_status("notes", SourceStatus::Offline)
        .expect("offline");
    refused(
        &d,
        "x",
        &root(&d, "proj"),
        "source_offline",
        "the source \"notes\" is offline",
    );
    d.close();
}

#[test]
fn invalid_name() {
    let d = daemon();
    for bad in ["", "has space", "a/b", "a@b", "a:b", "..", &"x".repeat(65)] {
        refused(
            &d,
            bad,
            &root(&d, "proj"),
            "invalid_name",
            "invalid source name",
        );
    }
    d.close();
}

#[test]
fn name_taken_without_regard_to_case() {
    let d = daemon();
    refused(
        &d,
        "Notes",
        &root(&d, "proj"),
        "name_taken",
        "is taken by the source \"notes\"",
    );
    refused(
        &d,
        "OTHER",
        &root(&d, "proj"),
        "name_taken",
        "is taken by the source \"other\"",
    );
    d.close();
}

#[test]
fn a_name_taken_by_an_earlier_promote_is_refused_too() {
    let d = daemon();
    d.registry
        .promote("proj", root(&d, "proj"), &d.engine)
        .expect("first");
    refused(
        &d,
        "PROJ",
        &root(&d, "Outside.md").parent().unwrap().join("proj"),
        "is_a_source_root",
        "already the root",
    );
    d.close();
}
