//! E13: a source whose root exists but the OS won't list (macOS Full
//! Disk Access not granted; reproduced here with `chmod 000`, which
//! `read_dir` reports with the same `PermissionDenied` kind) is
//! `permission_denied`: not empty, not offline. Its cached rows are
//! kept, and once access is back a rescan returns it to `active`.
//!
//! These run the real `SourceRegistry`, the real scan and the same
//! `SourceBackend::list` that `source.list` dispatches to. They are
//! skipped when run as root, which ignores file modes.

mod fixtures;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tephra_cache::{Cache, SourceStatus};
use tephrad::ignore::IgnoreRules;
use tephrad::registry::{PathProbe, SourceRegistry};
use tephrad::rpc::backend::{SourceBackend, SourceRowWire};
use tephrad::scan::{self, ScanError};

fn running_as_root() -> bool {
    unsafe {
        extern "C" {
            fn geteuid() -> u32;
        }
        geteuid() == 0
    }
}

fn set_mode(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

/// Restores a directory's mode on drop, so a failed assertion never
/// leaves an unremovable tempdir behind.
struct RestoreMode<'a>(&'a Path);

impl Drop for RestoreMode<'_> {
    fn drop(&mut self) {
        let _ = fs::set_permissions(self.0, fs::Permissions::from_mode(0o755));
    }
}

fn listed_status(registry: &SourceRegistry, name: &str) -> String {
    let rows: Vec<SourceRowWire> = SourceBackend::list(registry).expect("source.list");
    rows.into_iter()
        .find(|r| r.name == name)
        .unwrap_or_else(|| panic!("source {name} not listed"))
        .status
}

fn unit_count(cache_path: &Path, source: &str) -> usize {
    let cache = Cache::open(cache_path).expect("reopen cache");
    let row = cache.get_source(source).unwrap().expect("source row");
    ["one.md", "two.md"]
        .iter()
        .filter(|rel| cache.get_unit_by_rel_path(row.id, rel).unwrap().is_some())
        .count()
}

#[test]
fn unreadable_root_rescans_as_permission_denied_with_rows_kept_then_recovers() {
    if running_as_root() {
        eprintln!("skipped: running as root, file modes are ignored");
        return;
    }
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register("notes", fixture.root()).unwrap();
    assert_eq!(listed_status(&registry, "notes"), "active");
    assert_eq!(unit_count(&cache_path, "notes"), 2);

    let root = fixture.root().to_path_buf();
    let _restore = RestoreMode(&root);
    set_mode(&root, 0o000);

    let stats = registry
        .rescan("notes")
        .expect("a denied rescan reports, it does not error");
    assert_eq!(stats.units_written, 0, "nothing is scanned");
    assert_eq!(listed_status(&registry, "notes"), "permission_denied");

    // Rows preserved: the denied rescan must not read as "empty".
    set_mode(&root, 0o755);
    assert_eq!(
        unit_count(&cache_path, "notes"),
        2,
        "cached units must survive a denied rescan"
    );
    let stored = Cache::open(&cache_path)
        .unwrap()
        .get_source("notes")
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.status,
        SourceStatus::Active,
        "permission_denied is runtime-only; the stored status is untouched"
    );

    // Still flagged until a rescan sees access again.
    assert_eq!(listed_status(&registry, "notes"), "permission_denied");

    let stats = registry.rescan("notes").expect("rescan after the grant");
    assert_eq!(stats.units_written, 2);
    assert_eq!(listed_status(&registry, "notes"), "active");
    assert_eq!(unit_count(&cache_path, "notes"), 2);
}

#[test]
fn a_root_that_goes_missing_still_reports_offline_not_permission_denied() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register("notes", fixture.root()).unwrap();

    fs::remove_dir_all(fixture.root()).unwrap();
    registry.rescan("notes").expect("rescan of a missing root");

    assert_eq!(listed_status(&registry, "notes"), "offline");
    assert!(!registry.is_permission_denied("notes"));
}

#[test]
fn startup_access_sweep_marks_an_unreadable_source_and_clears_it_when_readable() {
    if running_as_root() {
        eprintln!("skipped: running as root, file modes are ignored");
        return;
    }
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register("notes", fixture.root()).unwrap();

    let root = fixture.root().to_path_buf();
    let _restore = RestoreMode(&root);
    set_mode(&root, 0o000);
    let marked = registry.refresh_access().expect("sweep");
    assert_eq!(marked, vec!["notes".to_string()]);
    assert_eq!(listed_status(&registry, "notes"), "permission_denied");

    set_mode(&root, 0o755);
    let marked = registry.refresh_access().expect("sweep");
    assert!(marked.is_empty());
    assert_eq!(listed_status(&registry, "notes"), "active");
    assert_eq!(unit_count(&cache_path, "notes"), 2);
}

#[test]
fn startup_access_sweep_leaves_a_missing_root_alone() {
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register("notes", fixture.root()).unwrap();
    fs::remove_dir_all(fixture.root()).unwrap();

    let marked = registry.refresh_access().expect("sweep");
    assert!(
        marked.is_empty(),
        "missing is offline, not permission_denied"
    );
    assert!(!registry.is_permission_denied("notes"));
}

#[test]
fn the_scan_itself_refuses_an_unreadable_root_and_leaves_every_row_in_place() {
    if running_as_root() {
        eprintln!("skipped: running as root, file modes are ignored");
        return;
    }
    // The registry's own probe catches this first; this drives the scan
    // directly (the case where the root lists at probe time and is
    // refused a moment later) to prove the walk does not treat the
    // refusal as an empty folder and clear the source's rows.
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry = SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap();
    registry.register("notes", fixture.root()).unwrap();

    let cache = Cache::open(&cache_path).unwrap();
    let source_id = cache.get_source("notes").unwrap().unwrap().id;
    let root = fixture.root().canonicalize().unwrap();
    let rules = IgnoreRules::load(&root).unwrap();

    let _restore = RestoreMode(&root);
    set_mode(&root, 0o000);
    let result = scan::rescan_source(&cache, source_id, &root, &rules);
    set_mode(&root, 0o755);

    assert!(
        matches!(result, Err(ScanError::PermissionDenied(ref p)) if *p == root),
        "expected PermissionDenied for the root, got {result:?}"
    );
    assert_eq!(
        unit_count(&cache_path, "notes"),
        2,
        "the rescan's clear must roll back with the refused walk"
    );
}

#[test]
fn register_of_an_unreadable_path_keeps_todays_message_without_the_privacy_line() {
    if running_as_root() {
        eprintln!("skipped: running as root, file modes are ignored");
        return;
    }
    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().unwrap();
    let registry =
        SourceRegistry::new(Cache::open(cache_dir.path().join("cache.db")).unwrap()).unwrap();

    let root = fixture.root().to_path_buf();
    let _restore = RestoreMode(&root);
    set_mode(&root, 0o000);
    assert_eq!(
        tephrad::registry::probe_path(&root),
        PathProbe::NotReadable,
        "chmod 000 is EACCES, not the macOS privacy block"
    );
    let err = registry.register("notes", &root).unwrap_err().to_string();
    assert!(err.contains("NotReadable"), "{err}");
    assert!(!err.contains("Full Disk Access"), "{err}");
}
