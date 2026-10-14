//! Shared test harness for the RPC/WebSocket server tests
//! (`tests/unit/rpc_*.rs`, `tests/integration/rpc_*.rs`): an
//! in-memory [`MockGraph`]/[`MockSources`] pair implementing PR-6's
//! [`GraphBackend`]/[`SourceBackend`] seams with pre-canned
//! responses, and [`spawn`], which binds a real `tephrad::rpc` server
//! to an ephemeral loopback port against those mocks and a disposable
//! tempdir token file.
//!
//! No real `tephra-cache` SQLite database or filesystem watcher is
//! created anywhere in this module (per the spec's "PR-5's resolver
//! and graph functions are mocked/stubbed at the trait or
//! function-pointer boundary ... real SQLite cache and filesystem
//! watcher (PR-2/PR-4) are not exercised here"), and no real user
//! `~/.local/share/tephra` directory or `token` file is ever touched
//! — every [`spawn`] call gets its own fresh [`tempfile::TempDir`].
#![allow(dead_code)]

use std::collections::HashMap;
use std::fs;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;
use tephra_graph::{Edge, ResolveError, ResolveResult};
use tephrad::rpc::backend::{GraphBackend, ScanStatsWire, SourceBackend, SourceRowWire};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// A canned [`GraphBackend`]: every response is pre-registered by
/// exact key via the `with_*` builders; an unregistered key gets a
/// deterministic "not found" style answer rather than panicking, so
/// a test only needs to configure the keys it actually exercises.
#[derive(Default)]
pub struct MockGraph {
    resolve: Mutex<HashMap<String, Result<ResolveResult, ResolveError>>>,
    backlinks: Mutex<HashMap<String, Vec<Edge>>>,
    forward_links: Mutex<HashMap<String, Vec<Edge>>>,
    edges: Mutex<Vec<Edge>>,
}

impl MockGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_resolve(self, link: &str, result: Result<ResolveResult, ResolveError>) -> Self {
        self.resolve
            .lock()
            .unwrap()
            .insert(link.to_string(), result);
        self
    }

    pub fn with_backlinks(self, id: &str, edges: Vec<Edge>) -> Self {
        self.backlinks.lock().unwrap().insert(id.to_string(), edges);
        self
    }

    pub fn with_forward_links(self, id: &str, edges: Vec<Edge>) -> Self {
        self.forward_links
            .lock()
            .unwrap()
            .insert(id.to_string(), edges);
        self
    }

    pub fn with_edges(self, edges: Vec<Edge>) -> Self {
        *self.edges.lock().unwrap() = edges;
        self
    }
}

impl GraphBackend for MockGraph {
    fn resolve_link(
        &self,
        raw: &str,
        _default_source: Option<&str>,
    ) -> Result<ResolveResult, ResolveError> {
        self.resolve
            .lock()
            .unwrap()
            .get(raw)
            .cloned()
            .unwrap_or_else(|| {
                Err(ResolveError::Unresolved {
                    source: "mock".to_string(),
                    target: raw.to_string(),
                })
            })
    }

    fn get_backlinks(&self, id: &str) -> Vec<Edge> {
        self.backlinks
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    fn get_forward_links(&self, id: &str) -> Vec<Edge> {
        self.forward_links
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .unwrap_or_default()
    }

    fn get_edges(&self) -> Vec<Edge> {
        self.edges.lock().unwrap().clone()
    }
}

/// A canned [`SourceBackend`], the PR-4 registry-verb counterpart to
/// [`MockGraph`].
#[derive(Default)]
pub struct MockSources {
    register: Mutex<HashMap<String, Result<ScanStatsWire, String>>>,
    unregister: Mutex<HashMap<String, Result<(), String>>>,
    list: Mutex<Option<Result<Vec<SourceRowWire>, String>>>,
    rescan: Mutex<HashMap<String, Result<ScanStatsWire, String>>>,
}

impl MockSources {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_register(self, name: &str, result: Result<ScanStatsWire, String>) -> Self {
        self.register
            .lock()
            .unwrap()
            .insert(name.to_string(), result);
        self
    }

    pub fn with_unregister(self, name: &str, result: Result<(), String>) -> Self {
        self.unregister
            .lock()
            .unwrap()
            .insert(name.to_string(), result);
        self
    }

    pub fn with_list(self, result: Result<Vec<SourceRowWire>, String>) -> Self {
        *self.list.lock().unwrap() = Some(result);
        self
    }

    pub fn with_rescan(self, name: &str, result: Result<ScanStatsWire, String>) -> Self {
        self.rescan.lock().unwrap().insert(name.to_string(), result);
        self
    }
}

impl SourceBackend for MockSources {
    fn register(&self, name: &str, _root: &str) -> Result<ScanStatsWire, String> {
        self.register
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_else(|| Err(format!("no mock response configured for register({name})")))
    }

    fn unregister(&self, name: &str) -> Result<(), String> {
        self.unregister
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_else(|| {
                Err(format!(
                    "no mock response configured for unregister({name})"
                ))
            })
    }

    fn list(&self) -> Result<Vec<SourceRowWire>, String> {
        self.list.lock().unwrap().clone().unwrap_or(Ok(Vec::new()))
    }

    fn rescan(&self, name: &str) -> Result<ScanStatsWire, String> {
        self.rescan
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .unwrap_or_else(|| Err(format!("no mock response configured for rescan({name})")))
    }
}

/// A [`SourceBackend`] whose `register` call blocks the calling
/// (OS) thread for `delay` before returning — used only by
/// `rpc_concurrency.rs`'s R6 test, which proves dispatch runs
/// `SourceBackend` calls via `tokio::task::spawn_blocking` (not
/// directly on an async worker thread) by checking that a concurrent
/// `graph.get_edges` call completes long before this slow
/// `source.register` call does, even under a single-worker-thread
/// `#[tokio::test]` runtime — if dispatch ever stopped using
/// `spawn_blocking`, this same test would hang until `delay` elapses,
/// since a current-thread runtime's one worker would be stuck inside
/// `std::thread::sleep` instead of free to poll the other connection.
pub struct SlowSources {
    delay: std::time::Duration,
}

impl SlowSources {
    pub fn new(delay: std::time::Duration) -> Self {
        SlowSources { delay }
    }
}

impl SourceBackend for SlowSources {
    fn register(&self, _name: &str, _root: &str) -> Result<ScanStatsWire, String> {
        std::thread::sleep(self.delay);
        Ok(ScanStatsWire {
            files_seen: 1,
            files_ignored: 0,
            units_written: 1,
            links_written: 0,
            tags_written: 0,
            blocks_written: 0,
        })
    }

    fn unregister(&self, _name: &str) -> Result<(), String> {
        Err("SlowSources only implements register".to_string())
    }

    fn list(&self) -> Result<Vec<SourceRowWire>, String> {
        Ok(Vec::new())
    }

    fn rescan(&self, _name: &str) -> Result<ScanStatsWire, String> {
        Err("SlowSources only implements register".to_string())
    }
}

/// A running RPC server bound to an ephemeral loopback port, backed
/// by mocks, with its own disposable tempdir token file. Dropping
/// this aborts the background server task and deletes the tempdir —
/// nothing persists past one test.
pub struct TestServer {
    pub addr: SocketAddr,
    pub token: String,
    tempdir: TempDir,
    state: Arc<AppState>,
    handle: JoinHandle<()>,
}

impl TestServer {
    pub fn http_url(&self) -> String {
        format!("http://{}/rpc", self.addr)
    }

    pub fn ws_url(&self) -> String {
        format!("ws://{}/rpc", self.addr)
    }

    pub fn token_path(&self) -> PathBuf {
        self.tempdir.path().join("token")
    }

    /// Simulates `tephractl token rotate` (E11): overwrites the token
    /// file in place with `new_token`, mode 0600, same as
    /// `tephrad::token::write_token_file` would in production.
    pub fn rotate_token(&self, new_token: &str) {
        write_token_file(&self.token_path(), new_token);
    }

    /// Pushes a synthetic `graph:changed` notification to every
    /// currently connected WebSocket client, for the WS test's
    /// "delivery of a synthetic `graph:changed` push" requirement.
    pub fn broadcast_graph_changed(&self, payload: serde_json::Value) {
        self.state.broadcast_graph_changed(payload);
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn write_token_file(path: &Path, token: &str) {
    fs::write(path, token).expect("write token file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("chmod token file");
    }
}

/// Binds a real `tephrad::rpc` server (same `build_router` the
/// production binary uses) to `127.0.0.1:0`, serving against `graph`
/// and `sources`, authenticated against a freshly generated 64-hex
/// token written to a disposable tempdir. Returns once the listener
/// is actually bound and accepting connections.
pub async fn spawn(graph: MockGraph, sources: MockSources) -> TestServer {
    spawn_with_token(graph, sources, &generate_token()).await
}

/// Same as [`spawn`], but with a caller-chosen token — used by tests
/// that need to know the token value up front (e.g. to assert a
/// *different* value is rejected).
pub async fn spawn_with_token(graph: MockGraph, sources: MockSources, token: &str) -> TestServer {
    spawn_with_token_and_any_sources(graph, sources, token).await
}

/// Generic counterpart of [`spawn_with_token`] for any
/// [`SourceBackend`] implementation, not just [`MockSources`] — used
/// by the R6 concurrency test to back a server with [`SlowSources`]
/// while still reusing the exact same `build_router`/token-file setup
/// every other fixture-backed test goes through.
pub async fn spawn_with_token_and_any_sources(
    graph: MockGraph,
    sources: impl SourceBackend + 'static,
    token: &str,
) -> TestServer {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let token_path = tempdir.path().join("token");
    write_token_file(&token_path, token);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local_addr");

    let state = Arc::new(AppState::new(
        token_path,
        Arc::new(graph) as Arc<dyn GraphBackend>,
        Arc::new(sources) as Arc<dyn SourceBackend>,
        // These fixtures always back `AppState` with a canned
        // `MockGraph`, never a real `tephrad::rpc::backend::SharedGraph`
        // — there is nothing for a successful `source.*` mutation to
        // refresh here (R1 is instead covered end-to-end, against the
        // real `SourceRegistry`/`UnitGraph`, by
        // `tests/rpc_live_graph.rs`).
        None,
    ));
    let router = build_router(state.clone());

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });

    TestServer {
        addr,
        token: token.to_string(),
        tempdir,
        state,
        handle,
    }
}

fn generate_token() -> String {
    // Shape (64 hex chars) matches production
    // `tephrad::token::ensure_token`'s real tokens, but this is
    // intentionally not that function: these tests never touch a
    // real `~/.local/share/tephra`, so they generate their own
    // disposable value instead of calling the binary-only `token`
    // module.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    let mut seed = nanos ^ (std::process::id() as u128) << 64;
    let mut out = String::with_capacity(64);
    while out.len() < 64 {
        // xorshift, good enough for a disposable test token (not a
        // security-sensitive value: it never protects anything real).
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.push_str(&format!("{:016x}", seed as u64));
    }
    out.truncate(64);
    out
}

/// Builds a `GET /rpc` WebSocket client request carrying
/// `Authorization: Bearer <token>`, then connects it — the
/// WebSocket-transport equivalent of `reqwest::RequestBuilder::bearer_auth`.
pub async fn connect_ws(
    url: &str,
    token: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    tokio_tungstenite::tungstenite::Error,
> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    let mut request = url.into_client_request()?;
    request.headers_mut().insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
    );

    let (stream, _response) = tokio_tungstenite::connect_async(request).await?;
    Ok(stream)
}
