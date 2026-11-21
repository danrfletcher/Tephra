//! `held_renames`: the files the rename detector holds, kept durably so a
//! hash pair whose hold was running when the process stopped is still
//! there at the next start (PR-9 R8). Real SQLite in a tempdir.

mod common;

use tephra_cache::{Cache, FaultInjector, HeldFile, HeldKind};

fn file(kind: HeldKind, source: &str, path: &str, hash: Option<&str>) -> HeldFile {
    HeldFile {
        kind,
        source: source.into(),
        path: path.into(),
        hash: hash.map(str::to_string),
    }
}

fn pair() -> Vec<HeldFile> {
    vec![
        file(HeldKind::Vanished, "notes", "Old.md", Some("h1")),
        file(HeldKind::Appeared, "notes", "New.md", Some("h1")),
        file(HeldKind::Vanished, "notes", "Big.bin.md", None),
    ]
}

#[test]
fn held_files_are_read_back_and_a_second_hold_replaces_not_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    assert!(cache.held_renames().unwrap().is_empty());
    cache.hold_renames(&pair(), 10).unwrap();
    cache
        .hold_renames(
            &[file(HeldKind::Vanished, "notes", "Old.md", Some("h2"))],
            11,
        )
        .unwrap();
    let mut got = cache.held_renames().unwrap();
    got.sort();
    let mut want = pair();
    want[0].hash = Some("h2".into());
    want.sort();
    assert_eq!(got, want);
}

#[test]
fn a_release_removes_only_what_it_names() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache.hold_renames(&pair(), 10).unwrap();
    cache.release_held_renames(&pair()[..2]).unwrap();
    assert_eq!(cache.held_renames().unwrap(), vec![pair()[2].clone()]);
    cache.release_held_renames(&pair()).unwrap();
    assert!(cache.held_renames().unwrap().is_empty());
}

#[test]
fn held_files_survive_a_reopen_and_a_rebuild() {
    let dir = tempfile::tempdir().unwrap();
    let cache = common::open_temp_cache(&dir);
    cache.hold_renames(&pair(), 10).unwrap();
    let path = dir.path().join("cache.db");
    drop(cache);
    let mut cache = Cache::open(&path).unwrap();
    assert_eq!(cache.held_renames().unwrap().len(), 3, "reopen");
    cache.rebuild(&FaultInjector::none()).expect("rebuild");
    assert_eq!(cache.held_renames().unwrap().len(), 3, "rebuild");
}
