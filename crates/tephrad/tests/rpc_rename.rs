//! `source.rename` over HTTP and WebSocket against a whole daemon (G13,
//! G14): the reply, the one `graph:changed` with `sources_changed` and
//! `fixes`, and the refusal error (`SOURCE_ERROR` with `data.reason`),
//! which must be the same word for word on both transports. The sibling of
//! `rpc_unpromote.rs`; it lives at the top level because it drives the
//! `fix_world` daemon, which `tests/integration.rs` does not link.

mod fix_world;
mod fixtures;

use std::time::Duration;

use fix_world::{fixes_of, real_writer, Daemon};
use serde_json::{json, Value};

const SOURCE_ERROR: i64 = -32011;

fn daemon() -> Daemon {
    Daemon::start(
        &[
            ("proj/A.md", "# A\n\n[[Outside]]\n"),
            ("Outside.md", "# O\n\n[[@proj:A#^k3f9a2|the A]]\n"),
        ],
        &[("Vault.md", "# V\n\n[[@notes:Outside]] ![[@proj:A]]\n")],
        real_writer(),
        false,
    )
}

fn promoted() -> Daemon {
    let d = daemon();
    d.registry
        .promote("proj", d.notes_root().join("proj"), &d.engine)
        .expect("promote first");
    d
}

fn http(d: &Daemon, body: Value) -> (u16, Value) {
    d.rt.block_on(async {
        let response = reqwest::Client::new()
            .post(format!("http://{}/rpc", d.addr))
            .bearer_auth(&d.token)
            .json(&body)
            .send()
            .await
            .expect("http");
        (
            response.status().as_u16(),
            response.json().await.expect("json"),
        )
    })
}

fn over_http(d: &Daemon, method: &str, params: Value) -> Value {
    http(
        d,
        json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}),
    )
    .1
}

#[test]
fn rename_is_a_known_method_with_its_own_params_check() {
    let d = daemon();
    let mut client = d.connect();
    for params in [
        json!({}),
        json!({"name": "proj"}),
        json!({"new_name": "x"}),
        json!({"name": 3, "new_name": "x"}),
        json!({"name": "proj", "new_name": 3}),
        json!({"name": "", "new_name": "x"}),
    ] {
        let frame = client.call("source.rename", params.clone());
        assert_eq!(frame["error"]["code"], -32602, "{params}: {frame}");
        let body = over_http(&d, "source.rename", params.clone());
        assert_eq!(body["error"]["code"], -32602, "{params}: {body}");
    }
    // An invented near-miss is still "method not found" (F12).
    for method in ["source.rename_", "source.rename_preview", "source.set_name"] {
        let frame = client.call(method, json!({"name": "proj", "new_name": "x"}));
        assert_eq!(frame["error"]["code"], -32601, "{method}: {frame}");
    }
    d.close();
}

#[test]
fn rename_over_websocket_replies_and_pushes_once_with_the_fix() {
    let d = promoted();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let result = client.request("source.rename", json!({"name": "proj", "new_name": "work"}));
    assert_eq!(
        result,
        json!({"name": "proj", "new_name": "work", "files_changed": 2, "deferred": 0, "status": "applied"})
    );

    let (push, _) = client.next_fix_push(Duration::from_secs(8));
    let push = push.expect("the push");
    assert_eq!(
        push["params"]["sources_changed"],
        json!([{"kind": "renamed", "name": "work", "old_name": "proj"}])
    );
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 1, "{push}");
    assert_eq!(fixes[0]["kind"], "source_rename");
    assert!(
        client
            .drain(Duration::from_millis(600))
            .iter()
            .all(|p| p["params"].get("sources_changed").is_none()),
        "one source-list push"
    );
    assert_eq!(d.text("Outside.md"), "# O\n\n[[@work:A#^k3f9a2|the A]]\n");
    assert_eq!(
        d.other_text("Vault.md"),
        "# V\n\n[[@notes:Outside]] ![[@work:A]]\n"
    );

    // `fix.list` shows the one entry; `source.list` the new name only.
    let fixes = client.request("fix.list", json!({}));
    let renames: Vec<&Value> = fixes
        .as_array()
        .expect("a list")
        .iter()
        .filter(|f| f["kind"] == "source_rename")
        .collect();
    assert_eq!(renames.len(), 1, "{fixes}");
    assert_eq!(renames[0]["old"], "@proj:");
    assert_eq!(renames[0]["new"], "@work:");
    let list = client.request("source.list", json!({}));
    let names: Vec<&str> = list["sources"]
        .as_array()
        .or_else(|| list.as_array())
        .expect("a list")
        .iter()
        .filter_map(|s| s["name"].as_str())
        .collect();
    assert!(
        names.contains(&"work") && !names.contains(&"proj"),
        "{list}"
    );
    d.close();
}

#[test]
fn rename_over_http_replies_the_same_way() {
    let d = promoted();
    let body = over_http(
        &d,
        "source.rename",
        json!({"name": "proj", "new_name": "work"}),
    );
    assert_eq!(
        body["result"],
        json!({"name": "proj", "new_name": "work", "files_changed": 2, "deferred": 0, "status": "applied"}),
        "{body}"
    );
    // The same name again, as a client that never saw the answer sends it
    // for the new name: a no-op success with nothing changed.
    let again = over_http(
        &d,
        "source.rename",
        json!({"name": "work", "new_name": "work"}),
    );
    assert_eq!(
        again["result"],
        json!({"name": "work", "new_name": "work", "files_changed": 0, "deferred": 0, "status": "applied"}),
        "{again}"
    );
    d.close();
}

/// G14: the reason and the sentence are the same over HTTP and WebSocket.
#[test]
fn every_refusal_has_the_same_reason_and_message_over_both_transports() {
    let d = promoted();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));
    let cases: [(Value, &str); 8] = [
        (json!({"name": "proj", "new_name": ""}), "invalid_name"),
        (
            json!({"name": "proj", "new_name": "bad/name"}),
            "invalid_name",
        ),
        (json!({"name": "proj", "new_name": "a:b"}), "invalid_name"),
        (json!({"name": "proj", "new_name": "a*b"}), "invalid_name"),
        (json!({"name": "proj", "new_name": "notes"}), "name_taken"),
        (json!({"name": "proj", "new_name": "Notes"}), "name_taken"),
        (json!({"name": "ghost", "new_name": "x"}), "unknown_source"),
        (json!({"name": "proj", "new_name": "a b"}), "invalid_name"),
    ];
    let (notes, other) = (
        fixtures::tree_snapshot(d.notes.root()),
        fixtures::tree_snapshot(d.other.root()),
    );
    let entries = d.entries().len();
    for (params, reason) in cases {
        let ws = client.call("source.rename", params.clone());
        let http = over_http(&d, "source.rename", params.clone());
        assert_eq!(ws["error"]["code"], SOURCE_ERROR, "{params}: {ws}");
        assert_eq!(
            ws["error"]["data"],
            json!({"reason": reason}),
            "{params}: {ws}"
        );
        let message = ws["error"]["message"].as_str().expect("a sentence");
        assert!(message.len() > 10, "{message}");
        assert_eq!(ws["error"], http["error"], "{params}");
    }
    // A refusal changes nothing and pushes nothing.
    assert_eq!(fixtures::tree_snapshot(d.notes.root()), notes);
    assert_eq!(fixtures::tree_snapshot(d.other.root()), other);
    assert_eq!(d.entries().len(), entries);
    assert!(client.drain(Duration::from_millis(500)).is_empty());
    d.close();
}

/// G14: only `source.register` returns `nested_source`; `source.promote`
/// and `source.unpromote` keep their reasons on the same wire shape.
#[test]
fn the_other_source_calls_carry_their_reasons_on_the_same_wire_shape() {
    let d = daemon();
    let mut client = d.connect();
    let vault = d.other_root();
    let inside = vault.join("inner");
    std::fs::create_dir_all(&inside).unwrap();
    let above = vault.parent().unwrap().to_path_buf();
    let cases: Vec<(&str, Value, &str)> = vec![
        (
            "source.register",
            json!({"name": "x", "root": inside.to_string_lossy()}),
            "nested_source",
        ),
        (
            "source.register",
            json!({"name": "x", "root": above.to_string_lossy()}),
            "nested_source",
        ),
        (
            "source.register",
            json!({"name": "x", "root": vault.to_string_lossy()}),
            "is_a_source_root",
        ),
        (
            "source.register",
            json!({"name": "other", "root": inside.to_string_lossy()}),
            "name_taken",
        ),
        (
            "source.promote",
            json!({"name": "x", "root": "/definitely/not/a/dir"}),
            "not_a_folder",
        ),
        (
            "source.promote",
            json!({"name": "x", "root": vault.to_string_lossy()}),
            "is_a_source_root",
        ),
        (
            "source.promote",
            json!({"name": "a b", "root": inside.to_string_lossy()}),
            "invalid_name",
        ),
        ("source.unpromote", json!({"name": "notes"}), "not_promoted"),
        (
            "source.unpromote",
            json!({"name": "ghost"}),
            "unknown_source",
        ),
    ];
    for (method, params, reason) in cases {
        let ws = client.call(method, params.clone());
        let http = over_http(&d, method, params.clone());
        assert_eq!(ws["error"]["code"], SOURCE_ERROR, "{method} {params}: {ws}");
        assert_eq!(
            ws["error"]["data"]["reason"], reason,
            "{method} {params}: {ws}"
        );
        assert_eq!(ws["error"], http["error"], "{method} {params}");
    }
    d.close();
}
