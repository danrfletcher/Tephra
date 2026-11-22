//! PR-11 (G16): every change to the source list sends exactly one
//! `graph:changed` push with a `sources_changed` marker, checked through
//! the real path. A real `SourceRegistry` (real watchers over temp
//! sources), a real `SharedGraph` with `GraphRefresh`, the real forwarder
//! task (`tephrad::forward::run`), the real RPC server on an ephemeral
//! loopback port, and WebSocket clients built with `connect_ws` from
//! `tests/rpc_fixtures`. Nothing is mocked. Every cache, token and source
//! lives in a tempdir of its own; nothing under `~/.local/share/tephra`.
//!
//! No fixed sleeps wait for the daemon. Every wait ends as soon as the
//! frame it waits for arrives, with a bounded timeout. A "no push for
//! this" check is made without waiting for silence: a later call that DOES
//! push is made, and the first push the client reads must be that one,
//! because pushes arrive in the order they were sent. The only fixed wait
//! is the 50 ms subscribe grace `rpc_server_ws.rs` uses, and it is not
//! needed here: `Client::connect` proves the subscription with a request.
//!
//! The registry owns a tokio runtime, so it must not be dropped on a
//! worker thread: each test is a plain `#[test]` and the server and the
//! forwarder run on a runtime the harness owns (as `fix_world` does).
#![allow(dead_code)]

mod fixtures;
mod rpc_fixtures;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::fixes::FixEngine;
use tephrad::refactor::fix_write::FixWriter;
use tephrad::refactor::RefactorEngine;
use tephrad::registry::{SourceChange, SourceChangeKind, SourceRegistry};
use tephrad::rpc::backend::{
    FixService, GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio_tungstenite::tungstenite::Message;

use fixtures::TempSource;

const WAIT: Duration = Duration::from_secs(20);

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct Daemon {
    registry: Arc<SourceRegistry>,
    state: Arc<AppState>,
    addr: SocketAddr,
    token: String,
    rt: Arc<tokio::runtime::Runtime>,
    stop: tokio::sync::watch::Sender<bool>,
    forwarder: Option<tokio::task::JoinHandle<()>>,
    server: tokio::task::JoinHandle<()>,
    cache_path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Daemon {
    /// Wired the way `main.rs` wires it: registry, graph with its
    /// refresh, the fix engine, the RPC server and the forwarder.
    fn start() -> Daemon {
        let dir = tempfile::tempdir().expect("data dir");
        let cache_path = dir.path().join("cache.db");
        let token_path = dir.path().join("token");
        let token = "c".repeat(64);
        std::fs::write(&token_path, &token).expect("write token");

        let registry = Arc::new(
            SourceRegistry::new(Cache::open(&cache_path).expect("registry cache"))
                .expect("registry"),
        );
        let graph_cache = Cache::open(&cache_path).expect("graph cache");
        let shared = Arc::new(SharedGraph::new(
            UnitGraph::build_from_cache(&graph_cache).expect("graph"),
        ));
        let refresh = GraphRefresh::new(shared.clone(), cache_path.clone());
        let refactor = Arc::new(RefactorEngine::new(
            shared.clone() as Arc<dyn GraphBackend>,
            registry.clone() as Arc<dyn SourceBackend>,
            registry.clone() as Arc<dyn tephrad::refactor::WatcherPauseControl>,
        ));
        let engine = Arc::new(FixEngine::new(
            &cache_path,
            Arc::new(FixWriter::new(&refactor)),
        ));
        engine.set_sink(registry.clone());
        let state = Arc::new(
            AppState::new(
                token_path,
                shared.clone() as Arc<dyn GraphBackend>,
                registry.clone() as Arc<dyn SourceBackend>,
                refactor as Arc<dyn RefactorBackend>,
                Some(refresh),
            )
            .with_fixes(Arc::new(FixService::new(engine.clone(), registry.clone()))),
        );

        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime"),
        );
        let listener = rt
            .block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
            .expect("bind an ephemeral port");
        let addr = listener.local_addr().expect("addr");
        let router = build_router(state.clone());
        let server = rt.spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let forwarder = Some(rt.spawn(tephrad::forward::run(
            state.clone(),
            engine,
            registry.subscribe(),
            stop_rx,
        )));
        Daemon {
            registry,
            state,
            addr,
            token,
            rt,
            stop,
            forwarder,
            server,
            cache_path,
            _dir: dir,
        }
    }

    fn ws_url(&self) -> String {
        format!("ws://{}/rpc", self.addr)
    }

    fn connect(&self) -> Client {
        let ws = self
            .rt
            .block_on(rpc_fixtures::connect_ws(&self.ws_url(), &self.token))
            .expect("websocket handshake");
        let mut client = Client {
            rt: self.rt.clone(),
            ws,
            next_id: 1,
        };
        // A reply proves the server reached its loop and so has
        // subscribed to the pushes: nothing sent after this is missed.
        client.request("graph.get_edges", json!({}));
        client
    }

    /// One JSON-RPC call over HTTP: the whole reply.
    fn rpc(&self, method: &str, params: Value) -> Value {
        self.rt.block_on(self.rpc_async(method, params))
    }

    async fn rpc_async(&self, method: &str, params: Value) -> Value {
        reqwest::Client::new()
            .post(format!("http://{}/rpc", self.addr))
            .bearer_auth(&self.token)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .expect("send")
            .json()
            .await
            .expect("json reply")
    }

    fn register(&self, name: &str, source: &TempSource) -> Value {
        self.rpc(
            "source.register",
            json!({ "name": name, "root": source.root().to_string_lossy() }),
        )
    }

    fn unregister(&self, name: &str) -> Value {
        self.rpc("source.unregister", json!({ "name": name }))
    }

    /// `source.list` names, as a client that re-queries sees them.
    fn listed(&self) -> Vec<String> {
        let reply = self.rpc("source.list", json!({}));
        let mut names: Vec<String> = reply["result"]
            .as_array()
            .unwrap_or_else(|| panic!("a list: {reply}"))
            .iter()
            .map(|row| row["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    /// Whether `[[@name:two]]` resolves, as a client that re-queries sees.
    fn resolves(&self, name: &str) -> bool {
        let reply = self.rpc(
            "unit.resolve_link",
            json!({ "link": format!("[[@{name}:two]]") }),
        );
        reply.get("error").is_none()
            && reply["result"]["unit"]["id"] == json!(format!("@{name}:two.md"))
    }

    fn close(mut self) {
        let _ = self.stop.send(true);
        if let Some(handle) = self.forwarder.take() {
            let _ = self.rt.block_on(handle);
        }
        // Wait for the aborted server task to be dropped, so the last
        // reference to the registry (whose own runtime may not be dropped
        // on a worker thread) is the one this thread drops.
        self.server.abort();
        let _ = self.rt.block_on(&mut self.server);
        for row in self.registry.list().unwrap_or_default() {
            let _ = self.registry.unregister(&row.name);
        }
    }
}

struct Client {
    rt: Arc<tokio::runtime::Runtime>,
    ws: Ws,
    next_id: u64,
}

impl Client {
    fn frame(&mut self, timeout: Duration) -> Option<Value> {
        let next = self
            .rt
            .block_on(async { tokio::time::timeout(timeout, self.ws.next()).await })
            .ok()??
            .expect("not a transport error");
        Some(serde_json::from_str(&next.into_text().expect("text frame")).expect("json"))
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.rt
            .block_on(self.ws.send(Message::Text(body.to_string().into())))
            .expect("send");
        loop {
            let frame = self.frame(WAIT).expect("a reply");
            if frame.get("id") == Some(&json!(id)) {
                return frame["result"].clone();
            }
        }
    }

    /// The next `graph:changed` notification, or `None` in `timeout`.
    fn next_push(&mut self, timeout: Duration) -> Option<Value> {
        let frame = self.frame(timeout)?;
        assert_eq!(frame["method"], json!("graph:changed"), "{frame}");
        assert!(frame.get("id").is_none(), "a push is a notification");
        Some(frame)
    }

    /// The next push, which the caller expects to be there.
    fn push(&mut self) -> Value {
        self.next_push(WAIT).expect("a push within the timeout")
    }

    /// The next push that carries `sources_changed`. An ordinary push for
    /// a path (the source's own watcher reporting a file the test wrote
    /// just before the register, which macOS can deliver late) may come
    /// first and is not what the caller waits for.
    fn push_with_sources_changed(&mut self) -> Value {
        loop {
            let push = self.push();
            if sources_changed(&push).is_some() {
                return push;
            }
        }
    }
}

/// The `sources_changed` list of a push, or `None` for an ordinary one.
fn sources_changed(push: &Value) -> Option<&Value> {
    push["params"].get("sources_changed")
}

fn added(name: &str) -> Value {
    json!([{ "kind": "added", "name": name }])
}

fn removed(name: &str) -> Value {
    json!([{ "kind": "removed", "name": name }])
}

fn notes() -> TempSource {
    let source = fixtures::empty_source();
    source.write("one.md", "# One\n\n[[two]]\n");
    source.write("two.md", "# Two\n");
    source.write("three.md", "# Three\n");
    source
}

// ---------------------------------------------------------------
// G16: register and unregister over RPC
// ---------------------------------------------------------------

#[test]
fn register_then_unregister_over_rpc_send_one_push_each_and_a_requery_agrees_at_once() {
    let d = Daemon::start();
    let mut client = d.connect();
    let src = notes();

    let reply = d.register("notes", &src);
    assert!(reply.get("error").is_none(), "{reply}");
    let push = client.push_with_sources_changed();
    assert_eq!(push["params"]["sources_changed"], added("notes"), "{push}");
    // `source` and `paths` stay, `paths` empty (the whole-source form).
    assert_eq!(push["params"]["source"], json!("notes"));
    assert_eq!(push["params"]["paths"], json!([]));
    let mut keys: Vec<&str> = push["params"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["paths", "source", "sources_changed"]);
    // A client that re-queries on receipt already sees the change.
    assert_eq!(d.listed(), ["notes"]);
    assert!(
        d.resolves("notes"),
        "[[@notes:two]] must resolve on receipt"
    );

    let reply = d.unregister("notes");
    assert!(reply.get("error").is_none(), "{reply}");
    let push = client.push_with_sources_changed();
    assert_eq!(
        push["params"]["sources_changed"],
        removed("notes"),
        "{push}"
    );
    assert!(d.listed().is_empty());
    assert!(!d.resolves("notes"), "the link must not resolve any more");

    // Exactly one push each: the next one is the next call's.
    let reply = d.register("again", &src);
    assert!(reply.get("error").is_none(), "{reply}");
    assert_eq!(
        client.push_with_sources_changed()["params"]["sources_changed"],
        added("again")
    );
    d.close();
}

#[test]
fn a_client_that_ignores_sources_changed_still_reads_source_and_paths() {
    #[derive(serde::Deserialize, Debug)]
    struct Old {
        source: String,
        paths: Vec<String>,
    }
    let d = Daemon::start();
    let mut client = d.connect();
    d.register("notes", &notes());
    let push = client.push_with_sources_changed();
    let old: Old = serde_json::from_value(push["params"].clone()).unwrap();
    assert_eq!(old.source, "notes");
    assert!(old.paths.is_empty());
    d.close();
}

#[test]
fn the_forwarder_refreshes_the_graph_before_it_broadcasts_a_direct_register() {
    // Registered on the registry itself, not over RPC, so the RPC
    // handler's own refresh is not there to cover for the forwarder: the
    // order push-after-refresh is the forwarder's alone.
    let d = Daemon::start();
    let mut client = d.connect();
    let src = notes();
    let registry = d.registry.clone();
    let root = src.root().to_path_buf();
    let worker = std::thread::spawn(move || registry.register("notes", root).map(|_| ()));
    let push = client.push_with_sources_changed();
    worker.join().unwrap().expect("register");
    assert_eq!(push["params"]["sources_changed"], added("notes"));
    assert!(
        d.resolves("notes"),
        "the graph was stale when the push landed"
    );
    assert_eq!(d.listed(), ["notes"]);

    let registry = d.registry.clone();
    let worker = std::thread::spawn(move || registry.unregister("notes"));
    let push = client.push_with_sources_changed();
    worker.join().unwrap().expect("unregister");
    assert_eq!(push["params"]["sources_changed"], removed("notes"));
    assert!(
        !d.resolves("notes"),
        "the graph was stale when the push landed"
    );
    d.close();
}

#[test]
fn the_emitter_carries_renamed_promoted_and_unpromoted_to_a_client() {
    let d = Daemon::start();
    let mut client = d.connect();
    d.registry
        .announce_sources_changed(vec![SourceChange::renamed("notes", "journal")]);
    d.registry
        .announce_sources_changed(vec![SourceChange::new(SourceChangeKind::Promoted, "docs")]);
    d.registry.announce_sources_changed(vec![SourceChange::new(
        SourceChangeKind::Unpromoted,
        "docs",
    )]);
    let renamed = client.push();
    assert_eq!(
        renamed["params"]["sources_changed"],
        json!([{ "kind": "renamed", "name": "journal", "old_name": "notes" }])
    );
    assert_eq!(renamed["params"]["source"], json!("journal"));
    assert!(renamed.get("id").is_none());
    assert_eq!(
        client.push()["params"]["sources_changed"],
        json!([{ "kind": "promoted", "name": "docs" }])
    );
    assert_eq!(
        client.push()["params"]["sources_changed"],
        json!([{ "kind": "unpromoted", "name": "docs" }])
    );
    d.close();
}

// ---------------------------------------------------------------
// Edge and failure cases
// ---------------------------------------------------------------

#[test]
fn a_failed_register_and_an_unknown_unregister_send_no_push() {
    let d = Daemon::start();
    let mut client = d.connect();
    let src = notes();
    assert!(d.register("notes", &src).get("error").is_none());
    assert_eq!(
        client.push_with_sources_changed()["params"]["sources_changed"],
        added("notes")
    );

    // Missing path, not a folder, duplicate name, duplicate root, nested
    // and ancestor roots: all refused.
    let other = notes();
    let inner = src.root().join("inside");
    std::fs::create_dir(&inner).unwrap();
    let a_file = src.root().join("one.md");
    let parent = src.root().parent().unwrap().to_path_buf();
    let refused = [
        ("x", "/no/such/folder/for/tephra".to_string()),
        ("x", a_file.to_string_lossy().to_string()),
        ("notes", other.root().to_string_lossy().to_string()),
        ("copy", src.root().to_string_lossy().to_string()),
        ("inner", inner.to_string_lossy().to_string()),
        ("outer", parent.to_string_lossy().to_string()),
    ];
    for (name, root) in &refused {
        let reply = d.rpc("source.register", json!({ "name": name, "root": root }));
        assert!(reply.get("error").is_some(), "{name} {root}: {reply}");
    }
    // Idempotent unregister of a name that is not registered: Ok, no push.
    let reply = d.unregister("ghost");
    assert!(reply.get("error").is_none(), "{reply}");

    // The first push after all that is the next real change's.
    assert!(d.register("sentinel", &other).get("error").is_none());
    let push = client.push_with_sources_changed();
    assert_eq!(
        push["params"]["sources_changed"],
        added("sentinel"),
        "a refused call sent a push: {push}"
    );
    assert_eq!(d.listed(), ["notes", "sentinel"]);
    d.close();
}

#[test]
fn a_register_that_rolls_back_sends_no_added_push() {
    let d = Daemon::start();
    let mut client = d.connect();
    let broken = notes();
    // An unreadable `.tephraignore` fails the scan after the row exists.
    std::fs::create_dir(broken.root().join(".tephraignore")).unwrap();
    let reply = d.register("broken", &broken);
    assert!(reply.get("error").is_some(), "{reply}");
    assert!(d.listed().is_empty(), "the row was rolled back");

    assert!(d.register("sentinel", &notes()).get("error").is_none());
    assert_eq!(
        client.push_with_sources_changed()["params"]["sources_changed"],
        added("sentinel")
    );
    d.close();
}

#[test]
fn add_then_remove_with_no_read_between_sends_two_pushes_in_that_order() {
    let d = Daemon::start();
    let mut client = d.connect();
    let src = notes();
    assert!(d.register("notes", &src).get("error").is_none());
    assert!(d.unregister("notes").get("error").is_none());

    let first = client.push_with_sources_changed();
    let second = client.push_with_sources_changed();
    assert_eq!(
        first["params"]["sources_changed"],
        added("notes"),
        "{first}"
    );
    assert_eq!(
        second["params"]["sources_changed"],
        removed("notes"),
        "{second}"
    );
    assert!(d.listed().is_empty());
    d.close();
}

#[test]
fn a_client_that_missed_the_first_push_still_converges_on_source_list() {
    let d = Daemon::start();
    let mut first = d.connect();
    let src = notes();

    assert!(d.register("notes", &src).get("error").is_none());
    assert_eq!(first.push()["params"]["sources_changed"], added("notes"));
    // A second client joins between the two changes, once the first push
    // has gone out: it misses it. (Joining earlier would be a race with
    // the forwarder, which sends one push at a time.)
    let mut second = d.connect();
    assert!(d.unregister("notes").get("error").is_none());

    assert_eq!(first.push()["params"]["sources_changed"], removed("notes"));
    assert_eq!(second.push()["params"]["sources_changed"], removed("notes"));
    // Neither missed nor late: `source.list` says what is true now.
    assert!(d.listed().is_empty());
    let mut late = d.connect();
    assert_eq!(
        late.request("source.list", json!({})),
        json!([]),
        "a client that joined after both pushes converges on source.list"
    );
    d.close();
}

#[test]
fn two_concurrent_registers_send_two_pushes_and_both_sources_resolve() {
    let d = Daemon::start();
    let mut client = d.connect();
    let (a, b) = (notes(), notes());

    let (ra, rb) = d.rt.block_on(async {
        tokio::join!(
            d.rpc_async(
                "source.register",
                json!({ "name": "alpha", "root": a.root().to_string_lossy() })
            ),
            d.rpc_async(
                "source.register",
                json!({ "name": "beta", "root": b.root().to_string_lossy() })
            ),
        )
    });
    assert!(
        ra.get("error").is_none() && rb.get("error").is_none(),
        "{ra} {rb}"
    );

    let mut names: Vec<String> = (0..2)
        .map(|_| {
            let push = client.push_with_sources_changed();
            let list = push["params"]["sources_changed"]
                .as_array()
                .unwrap()
                .clone();
            assert_eq!(list.len(), 1, "{push}");
            assert_eq!(list[0]["kind"], json!("added"));
            list[0]["name"].as_str().unwrap().to_string()
        })
        .collect();
    names.sort();
    assert_eq!(names, ["alpha", "beta"]);
    assert_eq!(d.listed(), ["alpha", "beta"]);
    assert!(d.resolves("alpha") && d.resolves("beta"));
    d.close();
}

#[test]
fn with_no_client_connected_register_and_unregister_succeed_and_the_forwarder_carries_on() {
    let d = Daemon::start();
    let src = notes();
    // Nobody is listening: both calls succeed, and the pushes go to no one.
    assert!(d.register("notes", &src).get("error").is_none());
    assert!(d.unregister("notes").get("error").is_none());

    // A client that connects now is served by the same forwarder. The
    // forwarder works through its queue one event at a time (a graph
    // refresh each), so it may still be on the two pushes above when the
    // client connects; those are sent to whoever is connected at that
    // moment, never kept for a client that connects later. So read until
    // the push for the new change, and check it arrives.
    let mut client = d.connect();
    assert!(d.register("later", &src).get("error").is_none());
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let push = client
            .next_push(left)
            .unwrap_or_else(|| panic!("no push for the later register; saw {seen:?}"));
        let list = push["params"]["sources_changed"].clone();
        if list == added("later") {
            break;
        }
        seen.push(list);
    }
    // Everything seen before it is from the unheard pair, in order.
    assert!(seen.len() <= 2, "{seen:?}");
    for list in &seen {
        assert!(
            *list == added("notes") || *list == removed("notes"),
            "{seen:?}"
        );
    }
    d.close();
}

#[test]
fn a_rescan_finding_a_new_file_is_an_ordinary_push_with_no_sources_changed() {
    let d = Daemon::start();
    let mut client = d.connect();
    let src = notes();
    assert!(d.register("notes", &src).get("error").is_none());
    assert_eq!(client.push()["params"]["sources_changed"], added("notes"));

    // E17: a rescan that finds a new file is an ordinary push with paths.
    src.write("four.md", "# Four\n");
    let reply = d.rpc("source.rescan", json!({ "name": "notes" }));
    assert!(reply.get("error").is_none(), "{reply}");
    let ordinary = client.push();
    assert!(sources_changed(&ordinary).is_none(), "{ordinary}");
    assert_eq!(ordinary["params"]["source"], json!("notes"));

    // E18 (a restart announces nothing) is checked in `registry.rs`, with
    // sources really in the cache, and live in the container run.
    d.close();
}

#[test]
fn a_slow_client_and_a_lagging_forwarder_do_not_stop_later_pushes_reaching_others() {
    let d = Daemon::start();
    let src = notes();

    // A client that connects and never reads again.
    let _slow = d.connect();
    // A fast client, read by a task so the test can flood.
    let fast = d.connect();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
    let Client { rt, mut ws, .. } = fast;
    let reader = d.rt.spawn(async move {
        while let Some(Ok(frame)) = ws.next().await {
            if let Ok(text) = frame.into_text() {
                if let Ok(value) = serde_json::from_str::<Value>(&text) {
                    let _ = tx.send(value);
                }
            }
        }
    });
    let _keep = rt;

    // Enough bytes that the slow client's socket stops taking them.
    for i in 0..400 {
        d.state.broadcast_graph_changed(
            json!({ "source": "noise", "paths": [format!("{i}{}", "x".repeat(48 * 1024))] }),
        );
    }
    // More source-list events than the registry's event channel holds,
    // sent faster than the forwarder refreshes the graph for them: the
    // forwarder's receiver lags (its `RecvError::Lagged` branch).
    const BURST: usize = 5000;
    for i in 0..BURST {
        d.registry.announce_sources_changed(vec![SourceChange::new(
            SourceChangeKind::Promoted,
            format!("p{i}"),
        )]);
    }
    // A real change after all of that.
    assert!(d.register("after", &src).get("error").is_none());

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut list_pushes = 0usize;
    let mut saw_after = false;
    while Instant::now() < deadline {
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(Some(push)) =
            d.rt.block_on(async { tokio::time::timeout(left, rx.recv()).await })
        else {
            break;
        };
        if let Some(list) = sources_changed(&push) {
            list_pushes += 1;
            if *list == added("after") {
                saw_after = true;
                break;
            }
        }
    }
    reader.abort();
    assert!(saw_after, "the push for the later register never arrived");
    assert!(
        list_pushes < BURST,
        "the forwarder never lagged ({list_pushes} of {BURST} arrived): the Lagged branch was not exercised"
    );
    // The daemon still answers.
    assert_eq!(d.listed(), ["after"]);
    d.close();
}

// ---------------------------------------------------------------
// One emitter
// ---------------------------------------------------------------

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn broadcast_graph_changed_is_called_from_one_place_only() {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .to_path_buf();
    let mut files = Vec::new();
    for entry in std::fs::read_dir(&crates).expect("crates dir") {
        let src = entry.expect("entry").path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    assert!(!files.is_empty());
    let mut callers = Vec::new();
    for file in &files {
        let text = std::fs::read_to_string(file).expect("read source");
        for (n, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or("");
            if code.contains("broadcast_graph_changed(")
                && !code.contains("fn broadcast_graph_changed")
            {
                callers.push(format!("{}:{}", file.display(), n + 1));
            }
        }
    }
    assert_eq!(callers.len(), 1, "callers: {callers:?}");
    assert!(
        callers[0].contains("tephrad/src/forward.rs"),
        "the one caller is the forwarder: {callers:?}"
    );
}
