//! PR-10 (E9): two Drive files with one name in one folder (or names that
//! differ only by case). The later one (created time, then file id) is
//! `name (2)` for good: the path is tied to the Drive file id, so it
//! survives a restart, a full re-index and the first file's deletion.
#![allow(dead_code)]

mod remote_support;

use remote_support::DriveDaemon;
use serde_json::{json, Value};

fn two_plans(d: &tephra_remote::testing::FakeDrive) {
    d.add_folder("f", "Notes", None);
    d.add_file("p-old", "Plan.md", "text/markdown", Some("f"), b"# Old\n");
    d.add_file("p-new", "Plan.md", "text/markdown", Some("f"), b"# New\n");
    d.set_created_time("p-old", "2026-01-01T00:00:00.000Z");
    d.set_created_time("p-new", "2026-02-01T00:00:00.000Z");
    d.add_file(
        "ref",
        "ref.md",
        "text/markdown",
        Some("f"),
        b"[[Plan]] and [[Plan (2)]]\n",
    );
}

fn resolve(rig: &DriveDaemon, link: &str) -> Value {
    rig.d.rpc("unit.resolve_link", json!({ "link": link }))
}

#[test]
fn the_later_file_is_name_2_and_its_unit_id_carries_it() {
    let rig = DriveDaemon::start(two_plans);
    rig.add_active("gdrive", "Notes");
    assert_eq!(
        rig.paths("gdrive"),
        vec!["Plan (2).md", "Plan.md", "ref.md"]
    );
    let ids: Vec<String> = rig
        .d
        .entries("gdrive")
        .iter()
        .filter_map(|e| e["id"].as_str().map(str::to_string))
        .collect();
    assert!(ids.contains(&"@gdrive:Plan (2).md".to_string()), "{ids:?}");
    // The earlier file, by created time, keeps the plain name.
    let rows = remote_support::item_rows(&rig.d.cache_path, "gdrive");
    let id_of = |p: &str| {
        rows.iter()
            .find(|r| r.path == p)
            .and_then(|r| r.file_id.clone())
    };
    assert_eq!(id_of("Plan.md").as_deref(), Some("p-old"));
    assert_eq!(id_of("Plan (2).md").as_deref(), Some("p-new"));
}

#[test]
fn names_that_differ_only_by_case_are_duplicates() {
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        d.add_file("a", "Plan.md", "text/markdown", Some("f"), b"# a\n");
        d.add_file("b", "plan.md", "text/markdown", Some("f"), b"# b\n");
        d.set_created_time("a", "2026-01-01T00:00:00.000Z");
        d.set_created_time("b", "2026-01-02T00:00:00.000Z");
    });
    rig.add_active("gdrive", "Notes");
    assert_eq!(rig.paths("gdrive"), vec!["Plan.md", "plan (2).md"]);
}

#[test]
fn the_same_created_time_falls_back_to_the_file_id() {
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        for id in ["id-b", "id-a"] {
            d.add_file(id, "Plan.md", "text/markdown", Some("f"), b"# p\n");
            d.set_created_time(id, "2026-01-01T00:00:00.000Z");
        }
    });
    rig.add_active("gdrive", "Notes");
    let rows = remote_support::item_rows(&rig.d.cache_path, "gdrive");
    let id_of = |p: &str| {
        rows.iter()
            .find(|r| r.path == p)
            .and_then(|r| r.file_id.clone())
    };
    assert_eq!(id_of("Plan.md").as_deref(), Some("id-a"));
    assert_eq!(id_of("Plan (2).md").as_deref(), Some("id-b"));
}

#[test]
fn a_link_by_the_plain_name_is_ambiguous_and_name_2_reaches_the_later_file_only() {
    let rig = DriveDaemon::start(two_plans);
    rig.add_active("gdrive", "Notes");

    let plain = resolve(&rig, "[[@gdrive:Plan]]");
    let text = plain.to_string();
    assert!(plain.get("error").is_some(), "{plain}");
    assert!(text.contains("@gdrive:Plan.md"), "{text}");
    assert!(text.contains("@gdrive:Plan (2).md"), "{text}");
    assert!(text.to_lowercase().contains("ambig"), "{text}");

    let later = resolve(&rig, "[[@gdrive:Plan (2)]]");
    assert_eq!(
        later["result"]["unit"]["id"],
        json!("@gdrive:Plan (2).md"),
        "{later}"
    );
}

#[test]
fn the_later_file_keeps_name_2_when_the_first_is_deleted_and_links_do_not_move() {
    let rig = DriveDaemon::start(two_plans);
    rig.add_active("gdrive", "Notes");
    rig.drive.trash_file("p-old");
    rig.poll_applied();
    assert_eq!(rig.paths("gdrive"), vec!["Plan (2).md", "ref.md"]);
    // [[Plan]] is not quietly pointed at the survivor.
    let plain = resolve(&rig, "[[@gdrive:Plan]]");
    assert!(plain.get("error").is_some(), "{plain}");
    let later = resolve(&rig, "[[@gdrive:Plan (2)]]");
    assert_eq!(
        later["result"]["unit"]["id"],
        json!("@gdrive:Plan (2).md"),
        "{later}"
    );
}

#[test]
fn name_2_survives_a_restart_and_a_full_reindex() {
    let rig = DriveDaemon::start(two_plans);
    rig.add_active("gdrive", "Notes");
    let rig = rig.restart();
    rig.d.wait_active("gdrive");
    assert_eq!(
        rig.paths("gdrive"),
        vec!["Plan (2).md", "Plan.md", "ref.md"]
    );

    // A new, earlier-created duplicate must not take the name.
    rig.drive.add_file(
        "p-first",
        "Plan.md",
        "text/markdown",
        Some("f"),
        b"# First\n",
    );
    rig.drive
        .set_created_time("p-first", "2025-01-01T00:00:00.000Z");
    // The feed position is lost: everything is listed and read again.
    let tokens_before = rig.drive.request_log().matching("startPageToken").len();
    rig.drive.forget_old_tokens();
    rig.poll();
    rig.d.wait_active("gdrive");
    assert!(
        rig.drive.request_log().matching("startPageToken").len() > tokens_before,
        "a fresh feed position was taken: the index was redone"
    );
    let rows = remote_support::item_rows(&rig.d.cache_path, "gdrive");
    let id_of = |p: &str| {
        rows.iter()
            .find(|r| r.path == p)
            .and_then(|r| r.file_id.clone())
    };
    assert_eq!(id_of("Plan.md").as_deref(), Some("p-old"));
    assert_eq!(id_of("Plan (2).md").as_deref(), Some("p-new"));
    assert_eq!(id_of("Plan (3).md").as_deref(), Some("p-first"));
}

#[test]
fn a_name_that_is_already_taken_by_a_real_file_is_skipped() {
    let rig = DriveDaemon::start(|d| {
        d.add_folder("f", "Notes", None);
        d.add_file("a", "Plan.md", "text/markdown", Some("f"), b"# a\n");
        d.add_file("b", "Plan.md", "text/markdown", Some("f"), b"# b\n");
        d.add_file("c", "Plan (2).md", "text/markdown", Some("f"), b"# c\n");
        d.set_created_time("a", "2026-01-01T00:00:00.000Z");
        d.set_created_time("c", "2026-01-02T00:00:00.000Z");
        d.set_created_time("b", "2026-01-03T00:00:00.000Z");
    });
    rig.add_active("gdrive", "Notes");
    let paths = rig.paths("gdrive");
    assert_eq!(paths.len(), 3, "{paths:?}");
    let unique: std::collections::HashSet<_> = paths.iter().collect();
    assert_eq!(unique.len(), 3, "no two items share a path: {paths:?}");
    assert!(paths.contains(&"Plan.md".to_string()));
    assert!(paths.contains(&"Plan (2).md".to_string()));
}
