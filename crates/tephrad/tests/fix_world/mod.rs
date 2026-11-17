//! A whole daemon in a test, for `fix_push.rs` and `fix_own_writes.rs`
//! (PR-7): a real `SourceRegistry` with its real watchers over two temp
//! sources (`notes` and `other`), a real cache, a real `SharedGraph` and
//! `GraphRefresh`, the real `FixEngine` wired the way `main.rs` wires it
//! (rename fixer, sink), the real forwarder task (`tephrad::forward::run`)
//! and a real RPC server on an ephemeral loopback port. Clients are real
//! WebSocket connections. No mocked graph.
//!
//! The runtime that runs the server and the forwarder is owned here, so a
//! test is a plain `#[test]`: the registry (which has a runtime of its own)
//! is called and dropped on the test's own thread, as in `main.rs`.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::fixes::{FileWriter, FixEngine, FixEntryWire};
use tephrad::refactor::fix_write::{FixWriter, Sleeper};
use tephrad::refactor::{AtomicFs, RealFs, RefactorEngine};
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    FixService, GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

use crate::fixtures::{self, TempSource};

/// The files of the GP1 `@notes` set the push tests start from.
#[allow(unused_imports)]
pub use crate::fixtures::GP1_FILES;

/// How long a wait for the daemon may take.
pub const WAIT: Duration = Duration::from_secs(20);

/// The debounce window of the watcher with room to spare: how long a
/// test is quiet before it says "no further push came".
pub const QUIET: Duration = Duration::from_millis(1800);

pub fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if check() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A sleeper that does not: a write that fails every attempt would
/// otherwise wait out its back-off.
pub struct NoSleep;

impl Sleeper for NoSleep {
    fn sleep(&self, _d: Duration) {}
}

pub fn real_writer() -> Arc<dyn FileWriter> {
    writer_over(Arc::new(RealFs))
}

pub fn writer_over(fs: Arc<dyn AtomicFs>) -> Arc<dyn FileWriter> {
    Arc::new(FixWriter::with_parts(
        Arc::new(Mutex::new(())),
        fs,
        Arc::new(NoSleep),
    ))
}

/// Writes `Warmup.md` into `source` until the registry reports a change
/// for `name`: from then on the watcher is known to see what is written.
fn warm_up(registry: &SourceRegistry, name: &str, source: &TempSource) -> bool {
    let mut events = registry.subscribe();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut round = 0;
    while Instant::now() < deadline {
        round += 1;
        source.write("Warmup.md", &format!("# Warm-up {round}\n"));
        let seen = wait_until(Duration::from_millis(1500), || loop {
            match events.try_recv() {
                Ok(tephrad::registry::DaemonEvent::SourceChanged { source, paths, .. })
                    if source == name && !paths.is_empty() =>
                {
                    return true
                }
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        });
        if seen {
            return true;
        }
    }
    false
}

pub struct Daemon {
    pub notes: TempSource,
    pub other: TempSource,
    pub cache_path: PathBuf,
    pub registry: Arc<SourceRegistry>,
    pub engine: Arc<FixEngine>,
    /// What the server answers `fix.list` and `link.rename_event` with.
    pub service: Arc<FixService>,
    pub shared: Arc<SharedGraph>,
    pub state: Arc<AppState>,
    pub addr: SocketAddr,
    pub token: String,
    pub rt: Arc<tokio::runtime::Runtime>,
    stop: tokio::sync::watch::Sender<bool>,
    forwarder: Option<tokio::task::JoinHandle<()>>,
    server: tokio::task::JoinHandle<()>,
    /// Whether the OS watcher was seen to deliver a write at start.
    pub watcher_live: bool,
    _dirs: tempfile::TempDir,
}

impl Daemon {
    /// A daemon over `notes_files` (source `notes`) and `other_files`
    /// (source `other`). With `detect_renames` the watcher hands the
    /// renames it sees to the engine, as `main.rs` does.
    pub fn start(
        notes_files: &[(&str, &str)],
        other_files: &[(&str, &str)],
        writer: Arc<dyn FileWriter>,
        detect_renames: bool,
    ) -> Daemon {
        for attempt in 1..=3 {
            let d = Self::build(notes_files, other_files, writer.clone(), detect_renames);
            if d.watcher_live {
                return d;
            }
            eprintln!("attempt {attempt}: the watcher was deaf; building the daemon again");
            d.close();
        }
        // Still deaf: carry on. A test that needs the watcher fails on its
        // own bounded wait; one that does not need it is not held back.
        Self::build(notes_files, other_files, writer, detect_renames)
    }

    fn build(
        notes_files: &[(&str, &str)],
        other_files: &[(&str, &str)],
        writer: Arc<dyn FileWriter>,
        detect_renames: bool,
    ) -> Daemon {
        let notes = fixtures::empty_source();
        for (rel, text) in notes_files {
            notes.write(rel, text);
        }
        let other = fixtures::empty_source();
        for (rel, text) in other_files {
            other.write(rel, text);
        }
        let dirs = tempfile::tempdir().expect("data dir");
        let cache_path = dirs.path().join("cache.db");
        let token_path = dirs.path().join("token");
        let token = "b".repeat(64);
        std::fs::write(&token_path, &token).expect("write token");

        let registry = Arc::new(
            SourceRegistry::new(Cache::open(&cache_path).expect("registry cache"))
                .expect("registry"),
        );
        let engine = Arc::new(FixEngine::new(&cache_path, writer));
        if detect_renames {
            registry.set_rename_fixer(engine.clone());
        }
        engine.set_sink(registry.clone());
        registry.register("notes", notes.root()).expect("notes");
        registry.register("other", other.root()).expect("other");
        // A watcher that has only just started can miss an event made
        // at once (seen under load), so each source is first shown to
        // be live: a file is written until its event is seen.
        // The OS watcher on this Mac is sometimes deaf for the whole life
        // of a source (the slice-6 `rename_watcher` tests do the same on
        // the base commit); then everything is built again, a few times.
        let watcher_live = [("notes", &notes), ("other", &other)]
            .iter()
            .all(|(name, source)| warm_up(&registry, name, source));

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
        let service = Arc::new(FixService::new(engine.clone(), registry.clone()));
        let state = Arc::new(
            AppState::new(
                token_path,
                shared.clone() as Arc<dyn GraphBackend>,
                registry.clone() as Arc<dyn SourceBackend>,
                refactor as Arc<dyn RefactorBackend>,
                Some(refresh),
            )
            .with_fixes(service.clone()),
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
            engine.clone(),
            registry.subscribe(),
            stop_rx,
        )));
        Daemon {
            notes,
            other,
            cache_path,
            registry,
            engine,
            service,
            shared,
            state,
            addr,
            token,
            rt,
            stop,
            forwarder,
            server,
            watcher_live,
            _dirs: dirs,
        }
    }

    /// A daemon with rename detection on, over `files`, whose watcher has
    /// seen `old` renamed to `new` and fixed it: the daemon, a connected
    /// client, and the push with the fix.
    ///
    /// The OS file watcher on this Mac sometimes delivers nothing at all to
    /// a source for the whole life of a daemon (the slice-6
    /// `rename_watcher` tests do the same on the base commit). A rename
    /// the watcher never reports is not a failure of what is under test,
    /// so a daemon that heard nothing in `PATIENCE` is closed and the
    /// whole thing is tried again on a new one, up to `ATTEMPTS` times.
    pub fn start_and_see_rename_fixed(
        files: &[(&str, &str)],
        old: &str,
        new: &str,
    ) -> (Daemon, Client, Value) {
        const ATTEMPTS: usize = 4;
        const PATIENCE: Duration = Duration::from_secs(8);
        for attempt in 1..=ATTEMPTS {
            let d = Daemon::start(files, &[], real_writer(), true);
            let mut client = d.connect();
            d.mv(old, new);
            if let (Some(push), _) = client.next_fix_push(PATIENCE) {
                return (d, client, push);
            }
            eprintln!("attempt {attempt}: the watcher reported nothing; starting again");
            d.close();
        }
        panic!("the OS watcher reported nothing for the rename in {ATTEMPTS} daemons");
    }

    pub fn connect(&self) -> Client {
        let mut request = format!("ws://{}/rpc", self.addr)
            .into_client_request()
            .expect("request");
        request.headers_mut().insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", self.token)).unwrap(),
        );
        let (ws, _) = self
            .rt
            .block_on(tokio_tungstenite::connect_async(request))
            .expect("websocket handshake");
        let mut client = Client {
            rt: self.rt.clone(),
            ws,
            backlog: VecDeque::new(),
            next_id: 1,
        };
        // A reply proves the server has reached its loop, and so has
        // subscribed to the pushes: nothing sent after this is missed.
        client.request("graph.get_edges", json!({}));
        client
    }

    /// The notes source's root as the registry stores it.
    pub fn notes_root(&self) -> PathBuf {
        self.notes.root().canonicalize().expect("notes root")
    }

    pub fn other_root(&self) -> PathBuf {
        self.other.root().canonicalize().expect("other root")
    }

    /// Renames `old` to `new` inside `notes` on disk only. The watcher
    /// sees it; the cache is not touched.
    pub fn mv(&self, old: &str, new: &str) {
        let to = self.notes.root().join(new);
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::rename(self.notes.root().join(old), to).expect("rename on disk");
    }

    /// `mv`, then the cache is brought up to date by a rescan, so a
    /// fix called by hand finds the graph as the watcher would leave it.
    pub fn mv_and_rescan(&self, old: &str, new: &str) {
        self.mv(old, new);
        self.registry.rescan("notes").expect("rescan");
    }

    /// `link.rename_event` for `old` to `new` in `notes`, called on the
    /// service directly (no socket), as the RPC handler calls it.
    pub fn event(
        &self,
        old: &str,
        new: &str,
    ) -> Result<tephrad::fixes::event::RenameEventWire, tephrad::fixes::event::RenameEventError>
    {
        use tephrad::rpc::backend::FixBackend;
        let root = self.notes_root();
        self.service.rename_event(
            &root.join(old).display().to_string(),
            &root.join(new).display().to_string(),
        )
    }

    pub fn text(&self, rel: &str) -> String {
        std::fs::read_to_string(self.notes.root().join(rel)).expect("read notes file")
    }

    pub fn other_text(&self, rel: &str) -> String {
        std::fs::read_to_string(self.other.root().join(rel)).expect("read other file")
    }

    pub fn entries(&self) -> Vec<FixEntryWire> {
        self.engine.list(None, 100).expect("fix log")
    }

    pub fn backlinks(&self, id: &str) -> usize {
        self.shared.get_backlinks(id).len()
    }

    pub fn close(mut self) {
        let _ = self.stop.send(true);
        if let Some(handle) = self.forwarder.take() {
            let _ = self.rt.block_on(handle);
        }
        // Wait for the aborted server task to be dropped, so that the
        // last reference to the registry (whose own runtime may not be
        // dropped on a worker thread) is the one this thread drops.
        self.server.abort();
        let _ = self.rt.block_on(&mut self.server);
        let _ = self.registry.unregister("notes");
        let _ = self.registry.unregister("other");
    }
}

/// One authenticated WebSocket connection.
pub struct Client {
    rt: Arc<tokio::runtime::Runtime>,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    /// Pushes that arrived while a request waited for its reply.
    backlog: VecDeque<Value>,
    next_id: u64,
}

impl Client {
    /// The next frame as JSON, or `None` when none came in `timeout`.
    fn frame(&mut self, timeout: Duration) -> Option<(Value, String)> {
        let next = self
            .rt
            .block_on(async { tokio::time::timeout(timeout, self.ws.next()).await })
            .ok()??
            .expect("not a transport error");
        let text = next.into_text().expect("a text frame").to_string();
        Some((serde_json::from_str(&text).expect("json"), text))
    }

    /// Calls `method` and returns its `result`. Pushes that arrive first
    /// wait in the backlog.
    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.rt
            .block_on(self.ws.send(Message::Text(body.to_string().into())))
            .expect("send");
        loop {
            let (frame, _) = self.frame(WAIT).expect("a reply");
            if frame.get("id") == Some(&json!(id)) {
                return frame["result"].clone();
            }
            self.backlog.push_back(frame);
        }
    }

    /// Calls `method` and returns the whole reply frame (`result` or
    /// `error`).
    pub fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.rt
            .block_on(self.ws.send(Message::Text(body.to_string().into())))
            .expect("send");
        loop {
            let (frame, _) = self.frame(WAIT).expect("a reply");
            if frame.get("id") == Some(&json!(id)) {
                return frame;
            }
            self.backlog.push_back(frame);
        }
    }

    /// The next `graph:changed` push (its whole notification) and the
    /// text it was sent as, or `None` when none comes in `timeout`.
    pub fn next_push_text(&mut self, timeout: Duration) -> Option<(Value, String)> {
        if let Some(frame) = self.backlog.pop_front() {
            let text = frame.to_string();
            return Some((frame, text));
        }
        let (frame, text) = self.frame(timeout)?;
        assert_eq!(frame["method"], json!("graph:changed"), "{frame}");
        assert!(frame.get("id").is_none(), "a push is a notification");
        Some((frame, text))
    }

    pub fn next_push(&mut self, timeout: Duration) -> Option<Value> {
        self.next_push_text(timeout).map(|(v, _)| v)
    }

    /// The next push that carries `fixes`; pushes without are returned in
    /// the second place (ordinary change pushes, e.g. of the rename
    /// itself).
    pub fn next_fix_push(&mut self, timeout: Duration) -> (Option<Value>, Vec<Value>) {
        let deadline = Instant::now() + timeout;
        let mut ordinary = Vec::new();
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(push) = self.next_push(left) else {
                return (None, ordinary);
            };
            if push["params"].get("fixes").is_some() {
                return (Some(push), ordinary);
            }
            ordinary.push(push);
        }
    }

    /// Every push that arrives until the connection has been quiet for
    /// `quiet`.
    pub fn drain(&mut self, quiet: Duration) -> Vec<Value> {
        let mut pushes = Vec::new();
        while let Some(push) = self.next_push(quiet) {
            pushes.push(push);
        }
        pushes
    }
}

pub fn fixes_of(push: &Value) -> &Vec<Value> {
    push["params"]["fixes"].as_array().expect("a fixes list")
}

/// A drained list's pushes that carry `fixes`.
pub fn with_fixes(pushes: &[Value]) -> Vec<&Value> {
    pushes
        .iter()
        .filter(|p| p["params"].get("fixes").is_some())
        .collect()
}
