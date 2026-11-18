//! `source.promote` over HTTP and WebSocket against a whole daemon: the
//! reply, the error codes, and the one `graph:changed` with the
//! `sources_changed` and `fixes` entries.

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
fn promote_over_websocket_replies_and_pushes_once_with_the_fix() {
    let d = daemon();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));
    let root = d.notes_root().join("proj");

    let started = std::time::Instant::now();
    let result = client.request(
        "source.promote",
        json!({"name": "proj", "root": root.display().to_string()}),
    );
    assert_eq!(result["name"], "proj");
    assert_eq!(result["parent"], "notes");
    assert_eq!(result["folder"], "proj");
    assert_eq!(result["units_moved"], 1);
    assert_eq!(result["files_changed"], 2);
    assert_eq!(result["status"], "applied");

    let (push, _) = client.next_fix_push(Duration::from_secs(8));
    let push = push.expect("the push");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        push["params"]["sources_changed"],
        json!([{"kind": "promoted", "name": "proj"}])
    );
    let fixes = fixes_of(&push);
    assert_eq!(fixes.len(), 1, "{push}");
    assert_eq!(fixes[0]["kind"], "promote");
    assert_eq!(fixes[0]["files_changed"], 2);
    assert!(
        client
            .drain(Duration::from_millis(600))
            .iter()
            .all(|p| p["params"].get("sources_changed").is_none()),
        "one source-list push"
    );
    d.close();
}

#[test]
fn promote_over_http_works_and_a_refusal_is_a_source_error_with_its_reason() {
    let d = daemon();
    let root = d.notes_root().join("proj");

    let (status, body) = http(
        &d,
        json!({"jsonrpc": "2.0", "id": 1, "method": "source.promote",
               "params": {"name": "proj", "root": root.display().to_string()}}),
    );
    assert_eq!(status, 200);
    assert_eq!(body["result"]["status"], "applied", "{body}");

    // The same folder again: it is a source root now.
    let (status, body) = http(
        &d,
        json!({"jsonrpc": "2.0", "id": 2, "method": "source.promote",
               "params": {"name": "again", "root": root.display().to_string()}}),
    );
    assert_eq!(status, 200);
    assert_eq!(body["error"]["code"], SOURCE_ERROR, "{body}");
    assert_eq!(body["error"]["data"]["reason"], "is_a_source_root");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("already the root"));
    d.close();
}

#[test]
fn bad_params_are_invalid_params_and_unbuilt_verbs_stay_unknown() {
    let d = daemon();
    let mut client = d.connect();
    for params in [
        json!({}),
        json!({"name": "x"}),
        json!({"name": "", "root": "/x"}),
        json!({"name": 3, "root": "/x"}),
    ] {
        let frame = client.call("source.promote", params.clone());
        assert_eq!(frame["error"]["code"], -32602, "{params}: {frame}");
    }
    // PR-13 and PR-14 own these (F12).
    for method in ["source.unpromote", "source.rename"] {
        let frame = client.call(method, json!({"name": "x"}));
        assert_eq!(frame["error"]["code"], -32601, "{method}: {frame}");
    }
    d.close();
}

#[test]
fn a_refusal_over_websocket_sends_no_push() {
    let d = daemon();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));
    let frame = client.call(
        "source.promote",
        json!({"name": "x", "root": d.notes_root().display().to_string()}),
    );
    assert_eq!(frame["error"]["code"], SOURCE_ERROR);
    assert_eq!(frame["error"]["data"]["reason"], "is_a_source_root");
    assert!(client.next_push(Duration::from_millis(600)).is_none());
    d.close();
}
