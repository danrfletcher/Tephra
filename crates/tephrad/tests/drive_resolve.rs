//! PR-11 (G21, GP12): `resolve` clears a Drive conflict. `--mine` sends
//! Dan's version as a new revision; `--theirs` keeps his edits as
//! `<name>.mine` and refreshes the file from Drive. Any other id, and a file
//! with no conflict, is refused with a reason and nothing changes. Real
//! registry, RPC server and watcher against the PR-3 fake Drive.
#![allow(dead_code)]

mod remote_support;

use std::path::PathBuf;
use std::time::Duration;

use remote_support::{item_rows, save, DriveDaemon};
use serde_json::{json, Value};

const MINE: &[u8] = b"# Plan\nmine\n";
const THEIRS: &[u8] = b"# Plan\nsomeone else\n";

fn reason(reply: &Value) -> &str {
    reply["error"]["data"]["reason"]
        .as_str()
        .unwrap_or_else(|| {
            panic!("no reason in {reply}");
        })
}

/// `Plan.md` fetched, changed on Drive, then saved: held as `changed on
/// Drive`. Returns the file on disk.
fn held(dd: &DriveDaemon) -> PathBuf {
    let file = dd.fetch_file("Plan.md");
    dd.drive.update_file("p1", THEIRS);
    save(&file, MINE);
    let row = dd.wait_state("Plan.md", "needs_attention");
    assert_eq!(row.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0);
    file
}

/// Everything a refused `resolve` must leave alone.
fn snapshot(dd: &DriveDaemon, files: &[&PathBuf]) -> String {
    format!(
        "{:?}|{:?}|{:?}|{}|{:?}",
        item_rows(&dd.d.cache_path, "gdrive"),
        dd.queue(),
        files
            .iter()
            .map(|f| std::fs::read(f).ok())
            .collect::<Vec<_>>(),
        dd.drive.head_revision("p1"),
        dd.drive.upload_count("p1"),
    )
}

#[test]
fn gp12_mine_sends_dans_version_as_a_new_revision_on_top_of_theirs() {
    let dd = DriveDaemon::start_edits();
    let file = held(&dd);
    let theirs = dd.drive.head_revision("p1");

    let reply = dd.resolve("Plan.md", "mine");
    let result = &reply["result"];
    assert_eq!(result["unit"], json!("@gdrive:Plan.md"), "{reply}");
    assert_eq!(result["keep"], json!("mine"));
    assert_eq!(result["state"], json!("cached"), "{reply}");

    let head = dd.drive.head_revision("p1");
    assert_ne!(head, theirs, "a new revision on top of theirs");
    assert_eq!(dd.drive.content("p1"), MINE);
    assert_eq!(dd.drive.upload_count("p1"), 1);
    let row = dd.row("Plan.md");
    assert_eq!(row.state, "cached");
    assert_eq!(row.reason, None);
    assert_eq!(row.revision.as_deref(), Some(head.as_str()));
    assert!(dd.queue().is_empty());
    assert_eq!(std::fs::read(&file).unwrap(), MINE, "the file is as saved");
    assert!(!file.with_file_name("Plan.md.mine").exists());
    assert_eq!(
        dd.writes(),
        vec!["PATCH /upload/drive/v3/files/p1".to_string()],
        "the one write there is"
    );
    dd.d.close();
}

#[test]
fn gp12_theirs_keeps_the_edits_as_name_mine_and_refreshes_the_file_from_drive() {
    let dd = DriveDaemon::start_edits();
    let file = held(&dd);
    let theirs = dd.drive.head_revision("p1");

    let reply = dd.resolve("Plan.md", "theirs");
    let result = &reply["result"];
    assert_eq!(result["keep"], json!("theirs"), "{reply}");
    assert_eq!(result["state"], json!("cached"), "{reply}");
    let mine = file.with_file_name("Plan.md.mine");
    assert_eq!(result["mine_path"], json!(mine.display().to_string()));
    assert_eq!(result["path"], json!(file.display().to_string()));

    assert_eq!(std::fs::read(&mine).unwrap(), MINE, "Dan's edits are kept");
    assert_eq!(std::fs::read(&file).unwrap(), THEIRS, "Drive's are back");
    let row = dd.row("Plan.md");
    assert_eq!(row.state, "cached");
    assert_eq!(row.reason, None);
    assert_eq!(row.revision.as_deref(), Some(theirs.as_str()));
    assert!(dd.queue().is_empty());
    assert_eq!(dd.drive.upload_count("p1"), 0, "nothing was sent");
    assert!(dd.writes().is_empty());
    assert_eq!(dd.drive.content("p1"), THEIRS);

    // `.mine` is a file of the cache like any other, but it is not an item
    // of the source: it is never queued as an edit.
    std::thread::sleep(Duration::from_millis(900));
    assert!(dd.queue().is_empty());
    assert!(dd.writes().is_empty());
    assert_eq!(dd.row("Plan.md").state, "cached");
    dd.d.close();
}

#[test]
fn theirs_does_not_overwrite_a_mine_file_that_is_already_there() {
    let dd = DriveDaemon::start_edits();
    let file = held(&dd);
    let taken = file.with_file_name("Plan.md.mine");
    std::fs::write(&taken, b"an older one\n").unwrap();

    let reply = dd.resolve("Plan.md", "theirs");
    let kept = file.with_file_name("Plan.md.mine.2");
    assert_eq!(
        reply["result"]["mine_path"],
        json!(kept.display().to_string()),
        "{reply}"
    );
    assert_eq!(std::fs::read(&taken).unwrap(), b"an older one\n");
    assert_eq!(std::fs::read(&kept).unwrap(), MINE);
    assert_eq!(std::fs::read(&file).unwrap(), THEIRS);
    dd.d.close();
}

#[test]
fn theirs_while_drive_cannot_be_reached_keeps_the_edits_and_clears_the_hold() {
    let dd = DriveDaemon::start_edits();
    let file = held(&dd);
    // Drive answers the metadata read, but not the content.
    dd.drive.fail_content_reads(1000, 503);
    let reply = dd.resolve("Plan.md", "theirs");
    let result = &reply["result"];
    assert_eq!(result["keep"], json!("theirs"), "{reply}");
    assert!(result["fetch_failed"].is_string(), "{reply}");
    assert!(result.get("path").is_none(), "{reply}");
    let mine = file.with_file_name("Plan.md.mine");
    assert_eq!(std::fs::read(&mine).unwrap(), MINE, "the edits are kept");
    let row = dd.row("Plan.md");
    assert_eq!(row.state, "remote", "the hold is cleared");
    // What the item says now is the fetch's own reason, not the conflict.
    assert!(
        row.reason
            .as_deref()
            .unwrap_or("")
            .starts_with("fetch failed"),
        "{row:?}"
    );
    assert!(dd.queue().is_empty());
    assert!(dd.writes().is_empty());

    // The refused reads put the source offline; the next good poll brings it
    // back, and the file then comes with a plain fetch.
    dd.drive.fail_content_reads(0, 503);
    dd.poll();
    let path = dd.fetch_file("Plan.md");
    assert_eq!(std::fs::read(&path).unwrap(), THEIRS);
    dd.d.close();
}

#[test]
fn mine_is_held_again_when_drive_moved_once_more_after_the_conflict() {
    let dd = DriveDaemon::start_edits();
    let file = held(&dd);
    // A second change of somebody else, after the hold was raised.
    dd.drive.update_file("p1", b"# Plan\nsomeone else, again\n");
    let reply = dd.resolve("Plan.md", "mine");
    let result = &reply["result"];
    assert_eq!(result["state"], json!("needs_attention"), "{reply}");
    assert_eq!(result["reason"], json!("changed on Drive"), "{reply}");
    assert_eq!(dd.drive.upload_count("p1"), 0, "nothing was sent");
    assert_eq!(std::fs::read(&file).unwrap(), MINE);
    assert_eq!(dd.queue().len(), 1);
    // The next resolve is on the head Drive has now.
    let again = dd.resolve("Plan.md", "mine");
    assert_eq!(again["result"]["state"], json!("cached"), "{again}");
    assert_eq!(dd.drive.content("p1"), MINE);
    dd.d.close();
}

#[test]
fn the_conflict_and_resolve_survive_a_restart() {
    let dd = DriveDaemon::start_edits();
    held(&dd);
    let dd = dd.restart();
    std::thread::sleep(Duration::from_millis(600));
    let row = dd.row("Plan.md");
    assert_eq!(row.state, "needs_attention");
    assert_eq!(row.reason.as_deref(), Some("changed on Drive"));
    assert_eq!(dd.drive.upload_count("p1"), 0);
    let reply = dd.resolve("Plan.md", "mine");
    assert_eq!(reply["result"]["state"], json!("cached"), "{reply}");
    assert_eq!(dd.drive.content("p1"), MINE);
    dd.d.close();
}

#[test]
fn a_second_resolve_of_the_same_conflict_finds_nothing_and_changes_nothing() {
    let dd = DriveDaemon::start_edits();
    let file = held(&dd);
    let first = dd.resolve("Plan.md", "mine");
    assert!(first.get("result").is_some(), "{first}");
    let before = snapshot(&dd, &[&file]);
    for keep in ["mine", "theirs"] {
        let again = dd.resolve("Plan.md", keep);
        assert_eq!(reason(&again), "nothing_to_resolve", "{again}");
    }
    assert_eq!(snapshot(&dd, &[&file]), before);
    dd.d.close();
}

#[test]
fn any_other_id_is_refused_with_a_reason_and_nothing_changes() {
    let dd = DriveDaemon::start_edits();
    let plan = held(&dd);
    let other = dd.fetch_file("Other.md");
    let vault = tempfile::tempdir().unwrap();
    std::fs::write(vault.path().join("note.md"), "# Note\n").unwrap();
    dd.d.ok(
        "source.register",
        json!({"name": "vault", "root": vault.path().display().to_string()}),
    );
    let before = snapshot(&dd, &[&plan, &other]);

    let refused =
        |unit: &str, keep: &str| dd.d.rpc("resolve", json!({ "unit": unit, "keep": keep }));
    // A file with no conflict (cached).
    let r = refused("@gdrive:Other.md", "mine");
    assert_eq!(reason(&r), "nothing_to_resolve", "{r}");
    assert!(
        r["error"]["message"]
            .as_str()
            .unwrap()
            .contains("nothing to resolve"),
        "{r}"
    );
    let r = refused("@gdrive:Other.md", "theirs");
    assert_eq!(reason(&r), "nothing_to_resolve", "{r}");
    // A file Drive has but that was never fetched.
    let doc = dd
        .paths("gdrive")
        .into_iter()
        .find(|p| p != "Plan.md" && p != "Other.md")
        .expect("the Google Doc is listed");
    let r = refused(&format!("@gdrive:{doc}"), "mine");
    assert_eq!(reason(&r), "nothing_to_resolve", "{r}");
    // A file that is not there, a source that is not there, a folder.
    let r = refused("@gdrive:Nope.md", "mine");
    assert_eq!(reason(&r), "unknown_unit", "{r}");
    let r = refused("@nowhere:Plan.md", "mine");
    assert_eq!(reason(&r), "unknown_unit", "{r}");
    // A local file.
    let r = refused("@vault:note.md", "theirs");
    assert_eq!(reason(&r), "not_drive", "{r}");
    // Not a unit id, no keep.
    for params in [
        json!({"unit": "gdrive:Plan.md", "keep": "mine"}),
        json!({"unit": "@gdrive", "keep": "mine"}),
        json!({"unit": "@gdrive:Plan.md", "keep": "both"}),
        json!({"unit": "@gdrive:Plan.md"}),
        json!({"keep": "mine"}),
    ] {
        let r = dd.d.rpc("resolve", params.clone());
        assert!(
            r["error"]["code"] == json!(-32602),
            "{params}: expected invalid params: {r}"
        );
    }
    assert_eq!(snapshot(&dd, &[&plan, &other]), before, "nothing changed");
    assert_eq!(dd.row("Plan.md").state, "needs_attention", "still held");
    dd.d.close();
}

#[test]
fn a_file_waiting_for_a_retry_is_not_a_conflict_and_resolve_does_not_touch_it() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.fail_uploads(1000, 503);
    save(&file, MINE);
    dd.wait_attempts("Plan.md", 1);
    let before = snapshot(&dd, &[&file]);
    for keep in ["mine", "theirs"] {
        let r = dd.resolve("Plan.md", keep);
        assert_eq!(reason(&r), "nothing_to_resolve", "{r}");
    }
    assert_eq!(snapshot(&dd, &[&file]), before);
    assert_eq!(dd.row("Plan.md").state, "pending_upload");
    dd.d.close();
}

#[test]
fn a_refusal_by_drive_is_not_a_change_on_drive_and_resolve_does_not_clear_it() {
    let dd = DriveDaemon::start_edits();
    let file = dd.fetch_file("Plan.md");
    dd.drive.fail_uploads(1, 403);
    save(&file, MINE);
    let held = dd.wait_state("Plan.md", "needs_attention");
    assert!(held
        .reason
        .as_deref()
        .unwrap()
        .starts_with("Drive refused the upload"));
    let before = snapshot(&dd, &[&file]);
    for keep in ["mine", "theirs"] {
        let r = dd.resolve("Plan.md", keep);
        assert_eq!(reason(&r), "nothing_to_resolve", "{r}");
    }
    assert_eq!(snapshot(&dd, &[&file]), before);
    dd.d.close();
}

#[test]
fn two_resolves_at_once_send_one_revision() {
    let dd = DriveDaemon::start_edits();
    held(&dd);
    let a = std::thread::scope(|scope| {
        let one = scope.spawn(|| dd.resolve("Plan.md", "mine"));
        let two = scope.spawn(|| dd.resolve("Plan.md", "mine"));
        (one.join().unwrap(), two.join().unwrap())
    });
    let results = [&a.0, &a.1];
    let ok = results.iter().filter(|r| r.get("result").is_some()).count();
    assert_eq!(ok, 1, "one wins: {a:?}");
    let refused = results.iter().find(|r| r.get("error").is_some()).unwrap();
    assert_eq!(reason(refused), "nothing_to_resolve");
    assert_eq!(dd.drive.upload_count("p1"), 1);
    assert_eq!(dd.drive.content("p1"), MINE);
    dd.d.close();
}

#[test]
fn source_list_and_unit_list_show_the_hold_and_its_reason_without_downloading() {
    let dd = DriveDaemon::start_edits();
    held(&dd);
    let reads = dd.media_reads();
    let exports = dd.exports();
    let entries = dd.d.entries("gdrive");
    let plan = entries
        .iter()
        .find(|e| e["rel_path"] == json!("Plan.md"))
        .unwrap();
    assert_eq!(plan["state"], json!("needs_attention"), "{plan}");
    assert_eq!(plan["reason"], json!("changed on Drive"), "{plan}");
    // `source.list` carries the count and the reason of the held file too.
    let list = dd.d.ok("source.list", json!({}));
    let row = list
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["name"] == json!("gdrive")))
        .unwrap_or_else(|| panic!("no gdrive row in {list}"));
    assert_eq!(row["needs_attention"], json!(1), "{row}");
    assert_eq!(
        row["attention"],
        json!([{ "path": "Plan.md", "reason": "changed on Drive" }]),
        "{row}"
    );
    assert!(row.get("pending_upload").is_none(), "{row}");
    assert_eq!(dd.media_reads(), reads, "nothing was downloaded to say so");
    assert_eq!(dd.exports(), exports);
    dd.d.close();
}
