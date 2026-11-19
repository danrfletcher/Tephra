//! `fix.list` (G18) end to end: the real RPC server on an ephemeral
//! loopback port with a throwaway token, backed by the real fix log in a
//! real cache. Every entry is made through the real engine. Each request
//! goes over HTTP and over WebSocket and the two replies must be the
//! same bytes.

mod fixtures;
mod rpc_fixtures;

use std::sync::Arc;

use fixtures::{FixWorld, GP1_FILES};
use futures_util::{SinkExt, StreamExt};
use rpc_fixtures::{
    connect_ws, spawn_with_token_sources_blocks_and_fixes, MockGraph, MockSources, TestServer,
};
use serde_json::{json, Value};
use tephrad::fixes::{FileWriter, FixEngine, ManualClock};
use tephrad::refactor::fix_write::FixWriter;
use tephrad::rpc::backend::UnsupportedBlockIds;
use tokio_tungstenite::tungstenite::Message;

const TOKEN: &str = "f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1";

struct Rig {
    _world: FixWorld,
    engine: Arc<FixEngine>,
    clock: Arc<ManualClock>,
}

/// Three entries at 1000, 2000 and 3000: a rename fixed in three files,
/// a rename nothing links to, and an uncertain one.
fn rig() -> Rig {
    let world = FixWorld::new(&GP1_FILES);
    world.rename("Training Plan.md", "Plan.md");
    world.notes.write("Solo.md", "# Solo\n");
    world.registry.rescan("notes").unwrap();
    let refactor = tephrad::refactor::RefactorEngine::new(
        Arc::new(tephrad::rpc::backend::SharedGraph::new(world.graph())),
        Arc::new(NoSources),
        Arc::new(NoPause),
    );
    let writer: Arc<dyn FileWriter> = Arc::new(FixWriter::new(&refactor));
    let clock = Arc::new(ManualClock::new(1_000));
    let engine = Arc::new(FixEngine::new(&world.cache_path, writer).with_clock(clock.clone()));
    engine
        .begin_rename("notes", "Training Plan.md", "Plan.md")
        .unwrap();
    clock.set(2_000);
    world.rename("Solo.md", "Single.md");
    engine
        .begin_rename("notes", "Solo.md", "Single.md")
        .unwrap();
    clock.set(3_000);
    engine.record_uncertain("notes", "Gone.md").unwrap();
    Rig {
        _world: world,
        engine,
        clock,
    }
}

// `RefactorEngine::new` wants a source backend and a pause control; the
// write loop used here needs neither, so these refuse if ever touched.
struct NoSources;
impl tephrad::rpc::backend::SourceBackend for NoSources {
    fn register(&self, _: &str, _: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        Err("unused".into())
    }
    fn unregister(&self, _: &str) -> Result<(), String> {
        Err("unused".into())
    }
    fn list(&self) -> Result<Vec<tephrad::rpc::backend::SourceRowWire>, String> {
        Err("unused".into())
    }
    fn rescan(&self, _: &str) -> Result<tephrad::rpc::backend::ScanStatsWire, String> {
        Err("unused".into())
    }
}
struct NoPause;
impl tephrad::refactor::WatcherPauseControl for NoPause {
    fn pause_all(&self) {}
    fn resume_all(&self, _: &std::collections::HashSet<std::path::PathBuf>) {}
}

/// The rig's registry owns its own runtime, which must not be dropped
/// inside an async context: the rig lives outside this one.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

async fn http_bytes(server: &TestServer, request: &Value) -> Vec<u8> {
    reqwest::Client::new()
        .post(server.http_url())
        .bearer_auth(TOKEN)
        .json(request)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap()
        .to_vec()
}

async fn ws_bytes(server: &TestServer, request: &Value) -> Vec<u8> {
    let mut ws = connect_ws(&server.ws_url(), TOKEN).await.unwrap();
    ws.send(Message::Text(request.to_string().into()))
        .await
        .unwrap();
    let reply = ws.next().await.unwrap().unwrap();
    reply.into_text().unwrap().as_bytes().to_vec()
}

/// One `fix.list` call over both transports: the bytes must match.
async fn call(server: &TestServer, params: Value) -> Value {
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "fix.list", "params": params});
    let over_http = http_bytes(server, &request).await;
    let over_ws = ws_bytes(server, &request).await;
    assert_eq!(
        String::from_utf8_lossy(&over_http),
        String::from_utf8_lossy(&over_ws),
        "HTTP and WebSocket differ for {params}"
    );
    serde_json::from_slice(&over_http).unwrap()
}

async fn serve(rig: &Rig) -> TestServer {
    spawn_with_token_sources_blocks_and_fixes(
        MockGraph::new(),
        MockSources::new(),
        UnsupportedBlockIds,
        ArcFixes(rig.engine.clone()),
        TOKEN,
    )
    .await
}

#[test]
fn fix_list_answers_the_log_newest_first_with_the_documented_shape() {
    let rig = rig();
    runtime().block_on(fix_list_answers_the_log_newest_first_with_the_documented_shape_body(&rig));
}

async fn fix_list_answers_the_log_newest_first_with_the_documented_shape_body(rig: &Rig) {
    let server = serve(rig).await;

    let all = call(&server, json!({})).await;
    let rows = all["result"].as_array().unwrap_or_else(|| panic!("{all}"));
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows.iter()
            .map(|r| r["time"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [3000, 2000, 1000]
    );
    assert_eq!(
        rows[2],
        json!({
            "id": rows[2]["id"], "time": 1000, "kind": "rename",
            "sources": ["notes"], "old": "Training Plan.md", "new": "Plan.md",
            "files_changed": 3, "status": "applied"
        })
    );
    assert_eq!(rows[1]["files_changed"], 0);
    assert_eq!(
        rows[0],
        json!({
            "id": rows[0]["id"], "time": 3000, "kind": "uncertain",
            "sources": ["notes"], "old": "Gone.md", "new": null,
            "files_changed": 0, "status": "uncertain",
            "message": "vanished, no certain pair: links broken"
        })
    );

    // `since` is inclusive; `limit` cuts from the newest; null is absent.
    let since = call(&server, json!({"since": 2000})).await;
    assert_eq!(since["result"].as_array().unwrap().len(), 2);
    let since = call(&server, json!({"since": 3001})).await;
    assert_eq!(since["result"], json!([]));
    let limited = call(&server, json!({"limit": 1})).await;
    assert_eq!(limited["result"].as_array().unwrap().len(), 1);
    assert_eq!(limited["result"][0]["time"], 3000);
    let nulls = call(&server, json!({"since": null, "limit": null})).await;
    assert_eq!(nulls["result"].as_array().unwrap().len(), 3);
    let huge = call(&server, json!({"limit": 5000})).await;
    assert_eq!(
        huge["result"].as_array().unwrap().len(),
        3,
        "clamped, not refused"
    );

    // A later fix shows up at once and sorts first.
    rig.clock.set(4_000);
    rig.engine.record_uncertain("notes", "Other.md").unwrap();
    let again = call(&server, json!({"limit": 1})).await;
    assert_eq!(again["result"][0]["old"], "Other.md");
}

fn empty_rig() -> Rig {
    let world = FixWorld::new(&GP1_FILES);
    let refactor = tephrad::refactor::RefactorEngine::new(
        Arc::new(tephrad::rpc::backend::SharedGraph::new(world.graph())),
        Arc::new(NoSources),
        Arc::new(NoPause),
    );
    let engine = Arc::new(FixEngine::new(
        &world.cache_path,
        Arc::new(FixWriter::new(&refactor)),
    ));
    Rig {
        _world: world,
        engine,
        clock: Arc::new(ManualClock::new(0)),
    }
}

#[test]
fn an_empty_log_answers_an_empty_list() {
    let rig = empty_rig();
    runtime().block_on(async {
        let server = serve(&rig).await;
        assert_eq!(call(&server, json!({})).await["result"], json!([]));
    });
}

#[test]
fn fix_list_clamps_limit_5000_to_1000_entries() {
    let rig = rig();
    runtime().block_on(fix_list_clamps_limit_5000_to_1000_entries_body(&rig));
}

async fn fix_list_clamps_limit_5000_to_1000_entries_body(rig: &Rig) {
    for i in 0..1_010 {
        rig.clock.set(5_000 + i);
        rig.engine.record_uncertain("notes", "x.md").unwrap();
    }
    let server = serve(rig).await;
    let all = call(&server, json!({"limit": 5000})).await;
    assert_eq!(all["result"].as_array().unwrap().len(), 1_000);
    let default = call(&server, json!({})).await;
    assert_eq!(default["result"].as_array().unwrap().len(), 50);
}

#[test]
fn fix_list_refuses_bad_parameters_the_same_way_on_both_transports() {
    let rig = rig();
    runtime().block_on(fix_list_refuses_bad_parameters_the_same_way_on_both_transports_body(&rig));
}

async fn fix_list_refuses_bad_parameters_the_same_way_on_both_transports_body(rig: &Rig) {
    let server = serve(rig).await;
    for params in [
        json!({"limit": 0}),
        json!({"limit": -1}),
        json!({"limit": 1.5}),
        json!({"limit": "5"}),
        json!({"since": -1}),
        json!({"since": "yesterday"}),
        json!({"since": 1.5}),
    ] {
        let reply = call(&server, params.clone()).await;
        assert_eq!(reply["error"]["code"], -32602, "{params}: {reply}");
        assert!(reply.get("result").is_none(), "{params}");
    }
}

#[test]
fn fix_list_needs_the_bearer_token_on_both_transports() {
    let rig = rig();
    runtime().block_on(fix_list_needs_the_bearer_token_on_both_transports_body(
        &rig,
    ));
}

async fn fix_list_needs_the_bearer_token_on_both_transports_body(rig: &Rig) {
    let server = serve(rig).await;
    let request = json!({"jsonrpc": "2.0", "id": 1, "method": "fix.list", "params": {}});
    for token in [None, Some("wrong".repeat(13))] {
        let mut http = reqwest::Client::new()
            .post(server.http_url())
            .json(&request);
        if let Some(t) = &token {
            http = http.bearer_auth(t);
        }
        assert_eq!(http.send().await.unwrap().status(), 401, "{token:?}");
        assert!(
            connect_ws(&server.ws_url(), token.as_deref().unwrap_or(""))
                .await
                .is_err(),
            "{token:?}: the upgrade was accepted"
        );
    }
}

#[test]
fn an_unknown_method_is_still_method_not_found() {
    let rig = rig();
    runtime().block_on(an_unknown_method_is_still_method_not_found_body(&rig));
}

async fn an_unknown_method_is_still_method_not_found_body(rig: &Rig) {
    let server = serve(rig).await;
    for method in ["fix.undo", "fix.replay", "fix.apply"] {
        let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": {}});
        let reply: Value = serde_json::from_slice(&http_bytes(&server, &request).await).unwrap();
        assert_eq!(reply["error"]["code"], -32601, "{method}");
        assert_eq!(
            http_bytes(&server, &request).await,
            ws_bytes(&server, &request).await
        );
    }
}

struct ArcFixes(Arc<FixEngine>);
impl tephrad::rpc::backend::FixBackend for ArcFixes {
    fn list(
        &self,
        since: Option<i64>,
        limit: usize,
    ) -> Result<Vec<tephrad::fixes::FixEntryWire>, String> {
        self.0.list(since, limit).map_err(|e| e.to_string())
    }
}

/// G9: a successful `source.rescan` retries that source's pending fixes
/// (through the server, as the daemon wires it); a refused one, and any
/// other method, does not.
#[test]
fn only_a_successful_source_rescan_triggers_the_pending_retry() {
    use std::sync::Mutex;
    use tephrad::rpc::backend::ScanStatsWire;

    struct Recording(Arc<Mutex<Vec<String>>>);
    impl tephrad::rpc::backend::FixBackend for Recording {
        fn list(
            &self,
            _: Option<i64>,
            _: usize,
        ) -> Result<Vec<tephrad::fixes::FixEntryWire>, String> {
            Ok(Vec::new())
        }
        fn after_rescan(&self, source: &str) {
            self.0.lock().unwrap().push(source.to_string());
        }
    }

    let stats = ScanStatsWire {
        files_seen: 0,
        files_ignored: 0,
        units_written: 0,
        links_written: 0,
        tags_written: 0,
        blocks_written: 0,
    };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let rt = runtime();
    rt.block_on(async {
        let server = spawn_with_token_sources_blocks_and_fixes(
            MockGraph::new(),
            MockSources::new()
                .with_rescan("notes", Ok(stats))
                .with_rescan("broken", Err("scan failed".to_string())),
            UnsupportedBlockIds,
            Recording(seen.clone()),
            TOKEN,
        )
        .await;
        for (method, params) in [
            ("source.rescan", json!({"name": "notes"})),
            ("source.rescan", json!({"name": "broken"})),
            ("source.list", json!({})),
            ("fix.list", json!({})),
        ] {
            let request = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
            http_bytes(&server, &request).await;
        }
    });
    assert_eq!(*seen.lock().unwrap(), ["notes"]);
}
