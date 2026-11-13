//! `unit.search` (G8) over a real running `tephrad::rpc` server, on both
//! transports, in the style of `rpc_transport_parity.rs` and
//! `link_refactor_auth.rs`: the params it validates (query, scope,
//! limit), the shape of a result, and the bearer-token gate. The backend
//! is `MockSources`, which records every search it is handed, so "an
//! invalid request never reached the cache" is checked directly. The
//! real matching and ranking run in `tests/search_end_to_end.rs`.

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tephrad::rpc::backend::SearchHitWire;
use tephrad::rpc::protocol::{INVALID_PARAMS, SOURCE_ERROR};
use tokio_tungstenite::tungstenite::Message;

use crate::rpc_fixtures::{connect_ws, spawn, MockGraph, MockSources, TestServer};

fn hit(id_path: &str, score: f64, offline: bool) -> SearchHitWire {
    SearchHitWire {
        id: format!("@notes:{id_path}"),
        source: "notes".to_string(),
        title: "A Title".to_string(),
        kind: "prose".to_string(),
        score,
        offline,
    }
}

fn request(id: u64, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "unit.search", "params": params})
}

async fn over_http(server: &TestServer, body: &Value) -> Value {
    reqwest::Client::new()
        .post(server.http_url())
        .bearer_auth(&server.token)
        .json(body)
        .send()
        .await
        .expect("HTTP request completes")
        .json()
        .await
        .expect("JSON body")
}

async fn over_ws(server: &TestServer, body: &Value) -> Value {
    let mut ws = connect_ws(&server.ws_url(), &server.token)
        .await
        .expect("WS handshake");
    ws.send(Message::Text(body.to_string().into()))
        .await
        .expect("send");
    let reply = ws.next().await.expect("reply").expect("frame");
    serde_json::from_str(reply.into_text().expect("text").as_ref()).expect("JSON")
}

#[tokio::test]
async fn a_result_is_an_array_of_exactly_the_six_documented_fields_on_both_transports() {
    let sources = MockSources::new().with_search(Ok(vec![
        hit("a/one.md", 1.0, false),
        hit("b/two.md", 0.5, true),
    ]));
    let server = spawn(MockGraph::new(), sources).await;

    let expected = json!([
        {"id": "@notes:a/one.md", "source": "notes", "title": "A Title",
         "kind": "prose", "score": 1.0, "offline": false},
        {"id": "@notes:b/two.md", "source": "notes", "title": "A Title",
         "kind": "prose", "score": 0.5, "offline": true},
    ]);
    for (transport, response) in [
        (
            "http",
            over_http(&server, &request(1, json!({"query": "one"}))).await,
        ),
        (
            "ws",
            over_ws(&server, &request(1, json!({"query": "one"}))).await,
        ),
    ] {
        assert_eq!(response["jsonrpc"], "2.0", "{transport}");
        assert_eq!(response["id"], 1, "{transport}");
        assert_eq!(response["result"], expected, "{transport}");
        assert!(response.get("error").is_none(), "{transport}");
        for hit in response["result"].as_array().unwrap() {
            let mut keys: Vec<&str> = hit
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, ["id", "kind", "offline", "score", "source", "title"]);
        }
    }
}

#[tokio::test]
async fn the_default_limit_is_fifty_and_an_explicit_limit_and_scope_names_are_passed_through() {
    for (params, expected) in [
        (json!({"query": "atlas outside"}), ("atlas outside", 50)),
        (json!({"query": "q", "scope": "names"}), ("q", 50)),
        (json!({"query": "q", "limit": 7}), ("q", 7)),
        (json!({"query": " q ", "limit": 1}), (" q ", 1)),
    ] {
        let sources = std::sync::Arc::new(MockSources::new());
        let server = crate::rpc_fixtures::spawn_with_token_and_any_sources(
            MockGraph::new(),
            SharedMock(sources.clone()),
            "t".repeat(64).as_str(),
        )
        .await;
        for response in [
            over_http(&server, &request(2, params.clone())).await,
            over_ws(&server, &request(2, params.clone())).await,
        ] {
            assert_eq!(response["result"], json!([]), "{params}");
        }
        assert_eq!(
            sources.search_calls(),
            vec![
                (expected.0.to_string(), expected.1),
                (expected.0.to_string(), expected.1)
            ],
            "{params}"
        );
    }
}

/// Lets a test keep a handle on the mock the server owns.
struct SharedMock(std::sync::Arc<MockSources>);

impl tephrad::rpc::backend::SourceBackend for SharedMock {
    fn register(&self, n: &str, r: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        self.0.register(n, r)
    }
    fn unregister(&self, n: &str) -> Result<(), String> {
        self.0.unregister(n)
    }
    fn list(&self) -> Result<Vec<tephrad::rpc::backend::SourceRowWire>, String> {
        self.0.list()
    }
    fn rescan(&self, n: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        self.0.rescan(n)
    }
    fn search_names(
        &self,
        q: &str,
        l: usize,
    ) -> Result<Vec<SearchHitWire>, tephrad::rpc::backend::SearchError> {
        self.0.search_names(q, l)
    }
}

#[tokio::test]
async fn bad_params_are_invalid_params_on_both_transports_and_never_reach_the_backend() {
    let sources = std::sync::Arc::new(MockSources::new());
    let server = crate::rpc_fixtures::spawn_with_token_and_any_sources(
        MockGraph::new(),
        SharedMock(sources.clone()),
        "t".repeat(64).as_str(),
    )
    .await;

    let cases: Vec<(Value, &str)> = vec![
        (json!({}), "query"),
        (json!({"query": ""}), "query"),
        (json!({"query": "  \t "}), "query"),
        (json!({"query": 5}), "query"),
        (json!({"query": null}), "query"),
        (json!({"query": "a", "scope": "headings"}), "headings"),
        (json!({"query": "a", "scope": "blocks"}), "blocks"),
        (json!({"query": "a", "scope": "all_blocks"}), "all_blocks"),
        (json!({"query": "a", "scope": "bogus"}), "bogus"),
        (json!({"query": "a", "scope": 3}), "scope"),
        (json!({"query": "a", "limit": 0}), "limit"),
        (json!({"query": "a", "limit": -1}), "limit"),
        (json!({"query": "a", "limit": 1.5}), "limit"),
        (json!({"query": "a", "limit": "5"}), "limit"),
        (json!({"query": "a", "limit": true}), "limit"),
    ];
    for (params, names) in cases {
        for (transport, response) in [
            (
                "http",
                over_http(&server, &request(3, params.clone())).await,
            ),
            ("ws", over_ws(&server, &request(3, params.clone())).await),
        ] {
            assert_eq!(
                response["error"]["code"], INVALID_PARAMS,
                "{transport} {params}: {response}"
            );
            assert!(response.get("result").is_none(), "{transport} {params}");
            let message = response["error"]["message"].as_str().unwrap();
            assert!(message.contains(names), "{transport} {params}: {message}");
        }
    }
    assert_eq!(
        sources.search_calls(),
        Vec::<(String, usize)>::new(),
        "an invalid request must make no cache read"
    );
}

#[tokio::test]
async fn a_backend_failure_is_a_source_error_not_an_empty_result() {
    let sources = MockSources::new().with_search(Err(tephrad::rpc::backend::SearchError::Source(
        "cache unreadable".to_string(),
    )));
    let server = spawn(MockGraph::new(), sources).await;
    let response = over_http(&server, &request(4, json!({"query": "a"}))).await;
    assert_eq!(response["error"]["code"], SOURCE_ERROR);
    assert_eq!(response["error"]["message"], "cache unreadable");
    assert!(response.get("result").is_none());
}

#[tokio::test]
async fn a_search_with_no_or_a_wrong_token_is_401_and_reaches_nothing() {
    let sources = std::sync::Arc::new(MockSources::new());
    let server = crate::rpc_fixtures::spawn_with_token_and_any_sources(
        MockGraph::new(),
        SharedMock(sources.clone()),
        "t".repeat(64).as_str(),
    )
    .await;
    let body = request(5, json!({"query": "atlas"}));

    let client = reqwest::Client::new();
    let none = client
        .post(server.http_url())
        .json(&body)
        .send()
        .await
        .expect("request completes");
    assert_eq!(none.status(), reqwest::StatusCode::UNAUTHORIZED);
    let parsed: Value = none.json().await.expect("JSON-RPC error body");
    assert!(parsed.get("error").is_some() && parsed.get("result").is_none());

    let wrong = client
        .post(server.http_url())
        .bearer_auth("not-the-right-token")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

    // The WS handshake refuses the same way.
    assert!(connect_ws(&server.ws_url(), "not-the-right-token")
        .await
        .is_err());

    assert!(sources.search_calls().is_empty());
}

#[tokio::test]
async fn http_and_ws_answer_a_search_with_byte_identical_responses() {
    let sources = MockSources::new().with_search(Ok(vec![hit("a.md", 0.875, true)]));
    let server = spawn(MockGraph::new(), sources).await;
    for params in [
        json!({"query": "a"}),
        json!({"query": "a", "scope": "headings"}),
        json!({"query": ""}),
    ] {
        let body = request(6, params);
        let http = reqwest::Client::new()
            .post(server.http_url())
            .bearer_auth(&server.token)
            .json(&body)
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .to_vec();
        let mut ws = connect_ws(&server.ws_url(), &server.token).await.unwrap();
        ws.send(Message::Text(body.to_string().into()))
            .await
            .unwrap();
        let frame = ws.next().await.unwrap().unwrap().into_text().unwrap();
        assert_eq!(http, frame.as_bytes(), "{body}");
    }
}
