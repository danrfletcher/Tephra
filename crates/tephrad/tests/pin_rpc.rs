//! PR-12 (G11, E10, E20, E27): `unit.pin` and `unit.unpin` over the RPC
//! server: params, results, `needs_confirm`, idempotency, and every error.
//! Each refusal writes no pin row.
#![allow(dead_code)]

mod remote_support;

use remote_support::{item_rows, DriveDaemon, FetchRig, MemStore, Tuning};
use serde_json::{json, Value};
use tephra_cache::Cache;
use tephrad::rpc::dispatch::METHOD_NAMES;

fn drive(d: &tephra_remote::testing::FakeDrive) {
    d.add_folder("fld", "Notes", None);
    d.add_folder("trip", "Trip Plans", Some("fld"));
    d.add_file("a", "a.md", "text/markdown", Some("trip"), b"# a\n");
    d.add_file("b", "b.md", "text/markdown", Some("trip"), b"# b\n");
    d.add_file("c", "c.md", "text/markdown", Some("fld"), b"# c\n");
    d.add_doc("d1", "Notes Doc", Some("fld"), "# doc\n");
    d.add_native(
        "s1",
        "Budget",
        "application/vnd.google-apps.spreadsheet",
        Some("fld"),
    );
    d.add_folder("old", "Trip Plans-old", Some("fld"));
    d.add_file("z", "z.md", "text/markdown", Some("old"), b"# z\n");
    // Two files with one name.
    d.add_file("p-old", "Plan.md", "text/markdown", Some("fld"), b"# old\n");
    d.add_file("p-new", "Plan.md", "text/markdown", Some("fld"), b"# new\n");
    d.set_created_time("p-old", "2026-01-01T00:00:00.000Z");
    d.set_created_time("p-new", "2026-02-01T00:00:00.000Z");
}

fn start() -> DriveDaemon {
    let dd = DriveDaemon::start(drive);
    dd.add_active("gdrive", "fld");
    dd
}

fn pin_rows(dd: &DriveDaemon) -> Vec<(String, bool)> {
    Cache::open_read_only(&dd.d.cache_path)
        .unwrap()
        .remote_pins("gdrive")
        .unwrap()
        .into_iter()
        .map(|p| (p.path, p.is_folder))
        .collect()
}

fn call(dd: &DriveDaemon, method: &str, params: Value) -> Value {
    dd.d.rpc(method, params)
}

fn reason(reply: &Value) -> &str {
    assert!(reply.get("error").is_some(), "expected a refusal: {reply}");
    reply["error"]["data"]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("no reason: {reply}"))
}

fn wait_pinned(dd: &DriveDaemon, path: &str) {
    dd.d.wait_for(&format!("{path} pinned"), || {
        (dd.row(path).state == "pinned").then_some(())
    });
}

#[test]
fn both_names_are_in_the_frozen_method_list() {
    assert!(METHOD_NAMES.contains(&"unit.pin"));
    assert!(METHOD_NAMES.contains(&"unit.unpin"));
    assert_eq!(METHOD_NAMES.len(), 27);
}

#[test]
fn pin_of_a_folder_answers_with_the_plan_and_returns_before_downloading() {
    let dd = start();
    let reply = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Trip Plans/" }));
    assert!(reply.get("error").is_none(), "{reply}");
    let r = &reply["result"];
    assert_eq!(r["unit"], json!("@gdrive:Trip Plans/"));
    assert_eq!(r["status"], json!("pinned"));
    assert_eq!(r["kind"], json!("folder"));
    assert_eq!(r["files"], json!(2));
    assert_eq!(r["total_bytes"], json!(8));
    assert!(r["cap_bytes"].as_u64().unwrap() > 0);
    assert!(r.get("warning").is_none(), "{r}");
    assert_eq!(pin_rows(&dd), [("Trip Plans".to_string(), true)]);
    // The sibling that shares the prefix is untouched.
    wait_pinned(&dd, "Trip Plans/a.md");
    wait_pinned(&dd, "Trip Plans/b.md");
    assert_eq!(dd.row("Trip Plans-old/z.md").state, "remote");
    assert_eq!(dd.row("c.md").state, "remote");
    dd.d.close();
}

#[test]
fn a_folder_pin_without_the_trailing_slash_is_the_same_pin_and_idempotent() {
    let dd = start();
    let first = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Trip Plans" }));
    assert_eq!(first["result"]["status"], json!("pinned"), "{first}");
    assert_eq!(first["result"]["kind"], json!("folder"));
    let second = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Trip Plans/" }));
    assert_eq!(second["result"]["status"], json!("already_pinned"));
    assert_eq!(second["result"]["files"], json!(2));
    assert_eq!(pin_rows(&dd).len(), 1);
    dd.d.close();
}

#[test]
fn pin_and_unpin_of_a_file_are_idempotent() {
    let dd = start();
    let one = call(&dd, "unit.pin", json!({ "unit": "@gdrive:c.md" }));
    assert_eq!(one["result"]["status"], json!("pinned"), "{one}");
    assert_eq!(one["result"]["kind"], json!("file"));
    assert_eq!(
        (
            one["result"]["files"].clone(),
            one["result"]["total_bytes"].clone()
        ),
        (json!(1), json!(4))
    );
    wait_pinned(&dd, "c.md");
    let again = call(&dd, "unit.pin", json!({ "unit": "@gdrive:c.md" }));
    assert_eq!(again["result"]["status"], json!("already_pinned"));
    assert_eq!(pin_rows(&dd).len(), 1);

    let off = call(&dd, "unit.unpin", json!({ "unit": "@gdrive:c.md" }));
    assert_eq!(off["result"]["status"], json!("unpinned"), "{off}");
    assert_eq!(off["result"]["kind"], json!("file"));
    assert!(pin_rows(&dd).is_empty());
    // The copy stays and may be evicted again.
    assert_eq!(dd.row("c.md").state, "cached");
    assert!(dd.root().join("c.md").exists());
    let off2 = call(&dd, "unit.unpin", json!({ "unit": "@gdrive:c.md" }));
    assert_eq!(off2["result"]["status"], json!("not_pinned"), "{off2}");
    assert!(off2["result"].get("kind").is_none());
    dd.d.close();
}

#[test]
fn unpin_of_a_file_under_a_pinned_folder_says_the_folder_keeps_it() {
    let dd = start();
    call(&dd, "unit.pin", json!({ "unit": "@gdrive:Trip Plans/" }));
    wait_pinned(&dd, "Trip Plans/a.md");
    let off = call(
        &dd,
        "unit.unpin",
        json!({ "unit": "@gdrive:Trip Plans/a.md" }),
    );
    assert_eq!(off["result"]["status"], json!("not_pinned"), "{off}");
    assert_eq!(off["result"]["covered_by"], json!("Trip Plans"));
    assert_eq!(pin_rows(&dd), [("Trip Plans".to_string(), true)]);
    assert_eq!(dd.row("Trip Plans/a.md").state, "pinned");
    dd.d.close();
}

#[test]
fn overlapping_pins_a_file_in_a_pinned_folder_survives_unpinning_the_folder() {
    let dd = start();
    call(&dd, "unit.pin", json!({ "unit": "@gdrive:Trip Plans/" }));
    wait_pinned(&dd, "Trip Plans/a.md");
    wait_pinned(&dd, "Trip Plans/b.md");
    let file = call(
        &dd,
        "unit.pin",
        json!({ "unit": "@gdrive:Trip Plans/a.md" }),
    );
    assert_eq!(file["result"]["status"], json!("pinned"), "{file}");

    let off = call(&dd, "unit.unpin", json!({ "unit": "@gdrive:Trip Plans/" }));
    assert_eq!(off["result"]["status"], json!("unpinned"), "{off}");
    assert_eq!(off["result"]["kind"], json!("folder"));
    assert_eq!(dd.row("Trip Plans/a.md").state, "pinned", "still pinned");
    assert_eq!(dd.row("Trip Plans/b.md").state, "cached", "evictable");

    // Unpinning both makes it evictable.
    call(
        &dd,
        "unit.unpin",
        json!({ "unit": "@gdrive:Trip Plans/a.md" }),
    );
    assert_eq!(dd.row("Trip Plans/a.md").state, "cached");
    assert!(pin_rows(&dd).is_empty());
    dd.d.close();
}

#[test]
fn a_duplicate_drive_name_is_pinned_by_its_own_path() {
    let dd = start();
    let before = dd.media_reads();
    let reply = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Plan (2).md" }));
    assert_eq!(reply["result"]["status"], json!("pinned"), "{reply}");
    wait_pinned(&dd, "Plan (2).md");
    assert_eq!(dd.row("Plan.md").state, "remote");
    assert_eq!(dd.media_reads() - before, 1);
    assert!(!dd.drive.request_log().matching("/files/p-new").is_empty());
    assert_eq!(pin_rows(&dd), [("Plan (2).md".to_string(), false)]);
    dd.d.close();
}

#[test]
fn needs_confirm_writes_nothing_and_confirm_stores_the_pin() {
    // A 1 MB cap: a fifth is 200 000 bytes.
    let dd = DriveDaemon::start_custom(
        MemStore::signed_in(),
        Tuning {
            limit: Some("1MB"),
            ..Tuning::default()
        },
        None,
        |d| {
            d.add_folder("fld", "Notes", None);
            d.add_folder("big", "Big", Some("fld"));
            d.add_file(
                "x1",
                "x1.bin",
                "application/octet-stream",
                Some("big"),
                &vec![1; 150_000],
            );
            d.add_file(
                "x2",
                "x2.bin",
                "application/octet-stream",
                Some("big"),
                &vec![2; 150_000],
            );
        },
    );
    dd.add_active("gdrive", "fld");
    let reads = dd.media_reads();
    let reply = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Big/" }));
    let r = &reply["result"];
    assert_eq!(r["status"], json!("needs_confirm"), "{reply}");
    assert_eq!(r["total_bytes"], json!(300_000));
    assert_eq!(r["cap_bytes"], json!(1_000_000));
    assert_eq!(r["files"], json!(2));
    // Nothing was written, started or read.
    assert!(pin_rows(&dd).is_empty());
    assert!(item_rows(&dd.d.cache_path, "gdrive")
        .iter()
        .all(|i| i.state == "remote"));
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert_eq!(dd.media_reads(), reads);
    // `confirm: false` is the same as none.
    let no = call(
        &dd,
        "unit.pin",
        json!({ "unit": "@gdrive:Big/", "confirm": false }),
    );
    assert_eq!(no["result"]["status"], json!("needs_confirm"));
    assert!(pin_rows(&dd).is_empty());

    let yes = call(
        &dd,
        "unit.pin",
        json!({ "unit": "@gdrive:Big/", "confirm": true }),
    );
    assert_eq!(yes["result"]["status"], json!("pinned"), "{yes}");
    assert_eq!(pin_rows(&dd), [("Big".to_string(), true)]);
    wait_pinned(&dd, "Big/x1.bin");
    wait_pinned(&dd, "Big/x2.bin");
    dd.d.close();
}

#[test]
fn exactly_a_fifth_of_the_cap_does_not_ask_and_one_byte_over_does() {
    let dd = DriveDaemon::start_custom(
        MemStore::signed_in(),
        Tuning {
            limit: Some("1MB"),
            ..Tuning::default()
        },
        None,
        |d| {
            d.add_folder("fld", "Notes", None);
            d.add_folder("e", "Exact", Some("fld"));
            d.add_folder("o", "Over", Some("fld"));
            d.add_file(
                "e1",
                "e.bin",
                "application/octet-stream",
                Some("e"),
                &vec![1; 200_000],
            );
            d.add_file(
                "o1",
                "o.bin",
                "application/octet-stream",
                Some("o"),
                &vec![1; 200_001],
            );
        },
    );
    dd.add_active("gdrive", "fld");
    let exact = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Exact/" }));
    assert_eq!(exact["result"]["status"], json!("pinned"), "{exact}");
    let over = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Over/" }));
    assert_eq!(over["result"]["status"], json!("needs_confirm"), "{over}");
    assert_eq!(pin_rows(&dd), [("Exact".to_string(), true)]);
    dd.d.close();
}

#[test]
fn a_pin_over_the_cap_only_warns_and_refuses_nothing() {
    let dd = DriveDaemon::start_custom(
        MemStore::signed_in(),
        Tuning {
            limit: Some("1MB"),
            ..Tuning::default()
        },
        None,
        |d| {
            d.add_folder("fld", "Notes", None);
            d.add_file(
                "x1",
                "x1.bin",
                "application/octet-stream",
                Some("fld"),
                &vec![1; 1_500_000],
            );
        },
    );
    dd.add_active("gdrive", "fld");
    let reply = call(
        &dd,
        "unit.pin",
        json!({ "unit": "@gdrive:x1.bin", "confirm": true }),
    );
    let r = &reply["result"];
    assert_eq!(r["status"], json!("pinned"), "{reply}");
    let warning = r["warning"].as_str().expect("a warning");
    assert!(warning.contains("more than the cache limit"), "{warning}");
    wait_pinned(&dd, "x1.bin");
    assert!(dd.root().join("x1.bin").exists());
    dd.d.close();
}

#[test]
fn every_refusal_names_a_reason_and_writes_no_row() {
    let dd = start();
    // The first index read the Doc once; a pin must not read it again.
    let exports = dd.exports();
    let reads = dd.media_reads();
    let cases: &[(&str, &str)] = &[
        ("@nope:a.md", "unknown_unit"),
        ("@gdrive:Nope/", "not_found"),
        ("@gdrive:nope.md", "not_found"),
        ("@gdrive:Trip Plans/zzz.md", "not_found"),
        ("@gdrive:../x", "invalid_path"),
        ("@gdrive:Trip Plans/../../x", "invalid_path"),
        ("@gdrive:/etc/passwd", "invalid_path"),
        ("@gdrive:a//b", "invalid_path"),
        ("@gdrive:./c.md", "invalid_path"),
        ("@gdrive:Notes Doc.md", "not_fetchable"),
        ("@gdrive:Budget", "not_fetchable"),
    ];
    for (unit, expected) in cases {
        for method in ["unit.pin", "unit.unpin"] {
            if method == "unit.unpin" && !matches!(*expected, "unknown_unit" | "invalid_path") {
                continue;
            }
            let reply = call(&dd, method, json!({ "unit": unit }));
            assert_eq!(reason(&reply), *expected, "{method} {unit}: {reply}");
            assert!(!reply["error"]["message"].as_str().unwrap().is_empty());
        }
        assert!(pin_rows(&dd).is_empty(), "{unit} wrote a row");
    }
    // A Doc says where to open it, and is the E27 guard: no row, no read.
    let reply = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Notes Doc.md" }));
    assert_eq!(reply["error"]["data"]["kind"], json!("gdoc"));
    let url = reply["error"]["data"]["url"].as_str().expect("a url");
    assert!(
        url.starts_with("https://docs.google.com/document/d/d1"),
        "{url}"
    );
    assert!(reply["error"]["message"].as_str().unwrap().contains(url));
    let sheet = call(&dd, "unit.pin", json!({ "unit": "@gdrive:Budget" }));
    assert_eq!(sheet["error"]["data"]["kind"], json!("gsheet"));
    assert!(sheet["error"]["data"]["url"].as_str().is_some());
    assert_eq!(dd.exports(), exports, "no export request for a Doc");
    assert_eq!(dd.media_reads(), reads);
    dd.d.close();
}

#[test]
fn bad_params_are_invalid_params_not_source_errors() {
    let dd = start();
    for (method, params) in [
        ("unit.pin", json!({})),
        ("unit.pin", json!({ "unit": 7 })),
        ("unit.pin", json!({ "unit": "gdrive:a.md" })),
        ("unit.pin", json!({ "unit": "@gdrive" })),
        (
            "unit.pin",
            json!({ "unit": "@gdrive:a.md", "confirm": "yes" }),
        ),
        ("unit.unpin", json!({})),
        ("unit.unpin", json!({ "unit": "a.md" })),
    ] {
        let reply = call(&dd, method, params.clone());
        assert_eq!(
            reply["error"]["code"],
            json!(-32602),
            "{method} {params}: {reply}"
        );
    }
    assert!(pin_rows(&dd).is_empty());
    dd.d.close();
}

#[test]
fn unit_list_shows_a_pin_before_the_file_is_down_and_the_state_after() {
    let dd = start();
    dd.drive.hold_content_after(0);
    call(&dd, "unit.pin", json!({ "unit": "@gdrive:Trip Plans/" }));
    let entries = dd.d.entries("gdrive");
    let find = |p: &str| {
        entries
            .iter()
            .find(|e| e["rel_path"] == json!(p))
            .unwrap_or_else(|| panic!("{p} listed"))
            .clone()
    };
    assert_eq!(find("Trip Plans/a.md")["pinned"], json!(true));
    assert_eq!(find("Trip Plans")["pinned"], json!(true));
    assert!(find("c.md").get("pinned").is_none());
    assert!(find("Trip Plans-old").get("pinned").is_none());
    dd.drive.release_content();
    wait_pinned(&dd, "Trip Plans/a.md");
    assert_eq!(
        dd.d.entries("gdrive")
            .iter()
            .find(|e| e["rel_path"] == json!("Trip Plans/a.md"))
            .unwrap()["state"],
        json!("pinned")
    );
    dd.d.close();
}

#[test]
fn a_github_file_and_folder_can_be_pinned_the_same_way() {
    let rig = FetchRig::start(|_| {});
    let file = rig.d.rpc("unit.pin", json!({ "unit": "@g:docs/Guide.md" }));
    assert_eq!(file["result"]["status"], json!("pinned"), "{file}");
    rig.d.wait_for("the file to be pinned", || {
        (rig.row("docs/Guide.md").state == "pinned").then_some(())
    });
    assert!(rig.cached_path("docs/Guide.md").exists());
    let folder = rig.d.rpc("unit.pin", json!({ "unit": "@g:assets/" }));
    assert_eq!(folder["result"]["kind"], json!("folder"), "{folder}");
    rig.d.wait_for("the png to be pinned", || {
        (rig.row("assets/logo.png").state == "pinned").then_some(())
    });
    assert_eq!(rig.row("README.md").state, "remote");
    let off = rig.d.rpc("unit.unpin", json!({ "unit": "@g:assets/" }));
    assert_eq!(off["result"]["status"], json!("unpinned"));
    assert_eq!(rig.row("assets/logo.png").state, "cached");
    rig.d.close();
}
