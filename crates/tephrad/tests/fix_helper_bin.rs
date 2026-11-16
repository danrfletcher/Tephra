//! The `fix_helper` harness binary (used by the container checkpoints):
//! a fix stopped by its hook leaves the cache and disk as a kill would,
//! and a second run resumes it. Real processes, real files, a real cache.

mod fixtures;

use std::process::{Command, Output};

use fixtures::{FixWorld, GP1_FILES};
use tephra_cache::Cache;

fn helper(world: &FixWorld, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_fix_helper"))
        .arg(&world.cache_path)
        .args(args)
        .env_clear()
        .output()
        .expect("run fix_helper")
}

fn fixed(world: &FixWorld) -> Vec<bool> {
    ["Goals.md", "Ideas/Rename Notes.md", "Weekly Review.md"]
        .iter()
        .map(|p| world.text(p).contains("[[Plan"))
        .collect()
}

#[test]
fn a_killed_fix_is_resumed_by_the_next_run_and_logs_one_entry() {
    let world = FixWorld::new(&GP1_FILES);
    world.rename("Training Plan.md", "Plan.md");

    let killed = helper(
        &world,
        &[
            "rename",
            "notes",
            "Training Plan.md",
            "Plan.md",
            "--stop-after",
            "1",
        ],
    );
    assert_eq!(
        killed.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&killed.stderr)
    );
    assert_eq!(fixed(&world), [true, false, false]);
    let cache = Cache::open(&world.cache_path).unwrap();
    assert_eq!(cache.pending_fixes(None, None).unwrap().len(), 2);
    let log = cache.list_fix_log(None, 50).unwrap();
    assert_eq!(
        (log.len(), log[0].status.as_str(), log[0].files_changed),
        (1, "partial", 1)
    );

    let resumed = helper(&world, &["resume"]);
    assert_eq!(
        resumed.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&resumed.stderr)
    );
    assert_eq!(fixed(&world), [true, true, true]);
    let cache = Cache::open(&world.cache_path).unwrap();
    assert!(cache.pending_fixes(None, None).unwrap().is_empty());
    let log = cache.list_fix_log(None, 50).unwrap();
    assert_eq!(
        (log.len(), log[0].status.as_str(), log[0].files_changed),
        (1, "applied", 3)
    );
}

#[test]
fn a_complete_run_prints_the_entry_and_uncertain_logs_one_without_work() {
    let world = FixWorld::new(&GP1_FILES);
    world.rename("Training Plan.md", "Plan.md");
    let done = helper(&world, &["rename", "notes", "Training Plan.md", "Plan.md"]);
    assert_eq!(
        done.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&done.stderr)
    );
    let entry: serde_json::Value = serde_json::from_slice(&done.stdout).unwrap();
    assert_eq!(entry["kind"], "rename");
    assert_eq!(entry["old"], "Training Plan.md");
    assert_eq!(entry["new"], "Plan.md");
    assert_eq!(entry["files_changed"], 3);
    assert_eq!(entry["status"], "applied");

    let lost = helper(&world, &["uncertain", "notes", "Gone.md"]);
    assert_eq!(lost.status.code(), Some(0));
    let entry: serde_json::Value = serde_json::from_slice(&lost.stdout).unwrap();
    assert_eq!(
        (entry["kind"].as_str(), entry["new"].is_null()),
        (Some("uncertain"), true)
    );

    let bad = helper(&world, &["rename", "notes", "Nope.md"]);
    assert_eq!(bad.status.code(), Some(1));
}
