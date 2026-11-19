//! `source.unpromote` over HTTP and WebSocket against a whole daemon: the
//! reply, the refusal error data, the one `graph:changed` with the
//! `sources_changed` and `fixes` entries, and `source.unregister` of a
//! promoted source (which is an unpromote). The sibling of
//! `rpc_promote.rs`; it lives at the top level because it drives the
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
            ("Outside.md", "# O\n\n[[proj/A]]\n"),
        ],
        &[],
        real_writer(),
        false,
    )
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

#[test]
fn unpromote_is_a_known_method_with_its_own_params_check() {
    let d = daemon();
    let mut client = d.connect();
    for params in [json!({}), json!({"name": ""}), json!({"name": 3})] {
        let frame = client.call("source.unpromote", params.clone());
        assert_eq!(frame["error"]["code"], -32602, "{params}: {frame}");
    }
    d.close();
}

#[test]
fn unpromote_over_websocket_replies_and_pushes_once_with_the_fix() {
    let d = daemon();
    let root = d.notes_root().join("proj");
    d.registry
        .promote("proj", root, &d.engine)
        .expect("promote first");
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let result = client.request("source.unpromote", json!({"name": "proj"}));
    assert_eq!(result["name"], "proj");
    assert_eq!(result["parent"], "notes");
    assert_eq!(result["folder"], "proj");
    assert_eq!(result["units_moved"], 1);
    assert_eq!(result["files_changed"], 2);
    assert_eq!(result["status"], "applied");

    let (push, _) = client.next_fix_push(Duration::from_secs(8));
    let push = push.expect("the push");
    assert_eq!(
        push["params"]["sources_changed"],
        json!([{"kind": "unpromoted", "name": "proj"}])
    );
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 1, "{push}");
    assert_eq!(fixes[0]["kind"], "unpromote");
    assert!(
        client
            .drain(Duration::from_millis(600))
            .iter()
            .all(|p| p["params"].get("sources_changed").is_none()),
        "one source-list push"
    );
    // Back in the parent, spelled in its shortest unique form.
    assert_eq!(d.text("Outside.md"), "# O\n\n[[A]]\n");
    d.close();
}

#[test]
fn unpromote_refusals_carry_a_reason_over_http() {
    let d = daemon();
    let ask = |id: u64, name: &str| {
        http(
            &d,
            json!({"jsonrpc": "2.0", "id": id, "method": "source.unpromote",
                   "params": {"name": name}}),
        )
        .1
    };
    let body = ask(1, "notes");
    assert_eq!(body["error"]["code"], SOURCE_ERROR, "{body}");
    assert_eq!(body["error"]["data"]["reason"], "not_promoted");
    let body = ask(2, "ghost");
    assert_eq!(body["error"]["code"], SOURCE_ERROR, "{body}");
    assert_eq!(body["error"]["data"]["reason"], "unknown_source");
    d.close();
}

#[test]
fn source_unregister_of_a_promoted_source_is_an_unpromote_and_of_a_plain_one_is_not() {
    let d = daemon();
    d.registry
        .promote("proj", d.notes_root().join("proj"), &d.engine)
        .expect("promote first");
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    let result = client.request("source.unregister", json!({"name": "proj"}));
    assert!(result.is_null(), "{result}");
    // The links went back (an unpromote), not left broken (a delete).
    // Back in the parent, spelled in its shortest unique form.
    assert_eq!(d.text("Outside.md"), "# O\n\n[[A]]\n");
    let (push, _) = client.next_fix_push(Duration::from_secs(8));
    let push = push.expect("the push");
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 1);
    assert_eq!(fixes[0]["kind"], "unpromote");
    let listed = client.request("source.list", json!({}));
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(!names.contains(&"proj"), "{names:?}");

    // A plain source is unregistered the old way: no unpromote entry.
    let _ = client.request("source.unregister", json!({"name": "other"}));
    let listed = client.request("source.list", json!({}));
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    d.close();
}
