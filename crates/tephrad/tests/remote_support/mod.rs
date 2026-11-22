//! A daemon wired the way `main.rs` wires it, pointed at the in-process
//! fake GitHub of `tephra-remote` (PR-3): a real `SourceRegistry`, a real
//! graph with its refresh, the real forwarder and the real RPC server on an
//! ephemeral loopback port. No network, no Keychain: the token lives in an
//! in-memory credential store.
//!
//! The registry owns a tokio runtime, so it must not be dropped on a worker
//! thread: tests are plain `#[test]`s, and the server and forwarder run on
//! a runtime this harness owns.
#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephra_remote::credentials::{CredentialError, CredentialStore, Provider, Secret};
use tephra_remote::testing::{FakeGithub, LinkGate};
use tephra_remote::{AdapterOptions, Clock, FakeClock, SystemClock};
use tephrad::fixes::FixEngine;
use tephrad::refactor::fix_write::FixWriter;
use tephrad::refactor::RefactorEngine;
use tephrad::registry::SourceRegistry;
use tephrad::remote_github::RemoteHub;
use tephrad::rpc::backend::{
    FixService, GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio_tungstenite::tungstenite::Message;

pub const WAIT: Duration = Duration::from_secs(30);
/// A token that is a plain, recognisable sentinel: no test may find it
/// anywhere it must not be.
pub const TOKEN: &str = "ghp_SENTINEL_remote_token_do_not_leak";
pub const REPO: &str = "danrfletcher/atlas";

type Ws =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The fake GitHub, on a runtime of its own so that it outlives a daemon
/// restart.
pub struct Fake {
    rt: tokio::runtime::Runtime,
    pub gh: FakeGithub,
    /// A switch in front of the fake (see [`Fake::start_gated`]).
    gate: Option<LinkGate>,
}

impl Fake {
    pub fn start() -> Arc<Fake> {
        Self::start_with_clock(Arc::new(SystemClock))
    }

    /// A fake whose rate-limit windows are read off `clock`.
    pub fn start_with_clock(clock: Arc<dyn Clock>) -> Arc<Fake> {
        Self::build(clock, false)
    }

    /// A fake behind a [`LinkGate`]: `network_down` and `network_up` take
    /// the connection away and bring it back, on the same URL.
    pub fn start_gated(clock: Arc<dyn Clock>) -> Arc<Fake> {
        Self::build(clock, true)
    }

    fn build(clock: Arc<dyn Clock>, gated: bool) -> Arc<Fake> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("fake runtime");
        let gh = rt.block_on(FakeGithub::start_with_clock(clock));
        let gate = gated.then(|| rt.block_on(LinkGate::in_front_of(gh.base_url())));
        Arc::new(Fake { rt, gh, gate })
    }

    /// What a daemon is pointed at: the fake, or its gate.
    pub fn url(&self) -> String {
        match &self.gate {
            Some(gate) => gate.base_url(),
            None => self.gh.base_url().to_string(),
        }
    }

    /// The connection goes away: connect errors, open connections cut.
    pub fn network_down(&self) {
        self.gate.as_ref().expect("a gated fake").down();
    }

    /// The connection is back.
    pub fn network_up(&self) {
        let gate = self.gate.as_ref().expect("a gated fake");
        self.rt.block_on(gate.up());
    }

    /// The usual fixture repo: a README that links into a vault note, a
    /// source file, a binary, and a nested folder.
    pub fn atlas(&self) -> &Self {
        self.gh.add_repo(REPO, "main");
        self.gh.put_file(
            REPO,
            "main",
            "README.md",
            b"# Atlas\n\nSee [[@vault:note]] and [[Guide]].\n\n## Setup\n",
        );
        self.gh.put_file(
            REPO,
            "main",
            "docs/Guide.md",
            b"# Guide\n\nBack to [[README]].\n",
        );
        self.gh.put_file(
            REPO,
            "main",
            "src/lib.rs",
            b"pub fn atlas() -> u32 {\n    1\n}\n",
        );
        self.gh.put_file(
            REPO,
            "main",
            "assets/logo.png",
            &[0x89, b'P', b'N', b'G', 0, 1, 2],
        );
        self
    }

    /// Requests that read file content (a blob or a contents call).
    pub fn reads(&self) -> Vec<String> {
        self.gh
            .request_log()
            .all()
            .iter()
            .map(|r| r.target())
            .filter(|t| t.contains("/git/blobs/") || t.contains("/contents/"))
            .collect()
    }

    /// The polls: requests for the head commit of a branch.
    pub fn polls(&self) -> Vec<tephra_remote::testing::LoggedRequest> {
        self.gh.request_log().matching("/commits/")
    }

    /// Requests that list a tree.
    pub fn tree_reads(&self) -> Vec<String> {
        self.gh
            .request_log()
            .matching("/git/trees/")
            .iter()
            .map(|r| r.target())
            .collect()
    }

    /// Every request that is not a GET.
    pub fn non_get_requests(&self) -> Vec<String> {
        self.gh
            .request_log()
            .all()
            .iter()
            .filter(|r| r.method != "GET")
            .map(|r| format!("{} {}", r.method, r.target()))
            .collect()
    }

    /// How many requests of any kind the fake has seen.
    pub fn requests(&self) -> usize {
        self.gh.request_log().count()
    }
}

/// An in-memory credential store: what a signed-in user has, with no
/// Keychain behind it.
pub struct MemStore {
    items: Mutex<HashMap<Provider, String>>,
}

impl MemStore {
    pub fn signed_in() -> Arc<MemStore> {
        let store = MemStore {
            items: Mutex::new(HashMap::new()),
        };
        store
            .items
            .lock()
            .unwrap()
            .insert(Provider::Github, TOKEN.to_string());
        Arc::new(store)
    }

    pub fn signed_out() -> Arc<MemStore> {
        Arc::new(MemStore {
            items: Mutex::new(HashMap::new()),
        })
    }
}

impl CredentialStore for MemStore {
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError> {
        Ok(self
            .items
            .lock()
            .unwrap()
            .get(&provider)
            .map(|v| Secret::new(v.clone())))
    }

    fn set(&self, provider: Provider, secret: &Secret) -> Result<(), CredentialError> {
        self.items
            .lock()
            .unwrap()
            .insert(provider, secret.expose().to_string());
        Ok(())
    }

    fn delete(&self, provider: Provider) -> Result<(), CredentialError> {
        self.items.lock().unwrap().remove(&provider);
        Ok(())
    }
}

pub struct Daemon {
    pub registry: Arc<SourceRegistry>,
    pub addr: SocketAddr,
    pub token: String,
    pub rt: Arc<tokio::runtime::Runtime>,
    stop: tokio::sync::watch::Sender<bool>,
    forwarder: Option<tokio::task::JoinHandle<()>>,
    server: tokio::task::JoinHandle<()>,
    pub data_dir: PathBuf,
    pub cache_path: PathBuf,
    pub fake: Arc<Fake>,
    _own_dir: Option<tempfile::TempDir>,
}

impl Daemon {
    /// A signed-in daemon with a data dir of its own.
    pub fn start(fake: &Arc<Fake>) -> Daemon {
        Self::start_with(fake, MemStore::signed_in(), None)
    }

    pub fn start_signed_out(fake: &Arc<Fake>) -> Daemon {
        Self::start_with(fake, MemStore::signed_out(), None)
    }

    /// A daemon on an existing data dir: a restart.
    pub fn restart(fake: &Arc<Fake>, data_dir: &Path) -> Daemon {
        Self::start_with(fake, MemStore::signed_in(), Some(data_dir.to_path_buf()))
    }

    pub fn start_with(fake: &Arc<Fake>, store: Arc<MemStore>, data_dir: Option<PathBuf>) -> Daemon {
        Self::start_full(
            fake,
            store,
            data_dir,
            Arc::new(SystemClock),
            AdapterOptions::default(),
        )
    }

    /// A signed-in daemon whose clock is `clock`: its pollers are due when
    /// the clock says so, and look at it every 10 ms.
    pub fn start_clocked(fake: &Arc<Fake>, clock: Arc<dyn Clock>) -> Daemon {
        Self::start_full(
            fake,
            MemStore::signed_in(),
            None,
            clock,
            AdapterOptions::default(),
        )
    }

    /// A restart of a daemon on `data_dir`, with a clock.
    pub fn restart_clocked(fake: &Arc<Fake>, data_dir: &Path, clock: Arc<dyn Clock>) -> Daemon {
        Self::start_full(
            fake,
            MemStore::signed_in(),
            Some(data_dir.to_path_buf()),
            clock,
            AdapterOptions::default(),
        )
    }

    pub fn start_full(
        fake: &Arc<Fake>,
        store: Arc<MemStore>,
        data_dir: Option<PathBuf>,
        clock: Arc<dyn Clock>,
        options: AdapterOptions,
    ) -> Daemon {
        let (own_dir, data_dir) = match data_dir {
            Some(d) => (None, d),
            None => {
                let dir = tempfile::tempdir().expect("data dir");
                let path = dir.path().to_path_buf();
                (Some(dir), path)
            }
        };
        let cache_path = data_dir.join("cache.db");
        let token_path = data_dir.join("token");
        let token = "c".repeat(64);
        std::fs::write(&token_path, &token).expect("write token");

        let registry = Arc::new(
            SourceRegistry::new(Cache::open(&cache_path).expect("registry cache"))
                .expect("registry"),
        );
        let hub =
            RemoteHub::with_options(store, Some(&fake.url()), clock, data_dir.clone(), options)
                .expect("hub")
                .with_poll_tick(Duration::from_millis(10));
        registry.set_remote_hub(Arc::new(hub));

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
        // Start-up, as `main.rs` does it: remote sources answer from the
        // cache; a first index that was cut short is read again from scratch.
        registry.resume_remote_sources();
        Daemon {
            registry,
            addr,
            token,
            rt,
            stop,
            forwarder,
            server,
            data_dir,
            cache_path,
            fake: fake.clone(),
            _own_dir: own_dir,
        }
    }

    pub fn rpc(&self, method: &str, params: Value) -> Value {
        self.rt.block_on(async {
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
        })
    }

    /// `result` of a call that must succeed.
    pub fn ok(&self, method: &str, params: Value) -> Value {
        let reply = self.rpc(method, params);
        assert!(reply.get("error").is_none(), "{method}: {reply}");
        reply["result"].clone()
    }

    /// Like [`Daemon::ok`], but retries (bounded by [`WAIT`]) while the
    /// answer is an error or `ready` says no. For graph queries made right
    /// after an add: the graph is refreshed by the forwarder and then
    /// pushed, as for a local `source.register`, so it may lag the RPC that
    /// changed the cache by a moment. The last answer is returned (and an
    /// error asserted) when the bound runs out.
    pub fn ok_when(&self, method: &str, params: Value, ready: impl Fn(&Value) -> bool) -> Value {
        let start = Instant::now();
        loop {
            let reply = self.rpc(method, params.clone());
            let done = reply.get("error").is_none() && ready(&reply["result"]);
            if done || start.elapsed() >= WAIT {
                assert!(reply.get("error").is_none(), "{method}: {reply}");
                return reply["result"].clone();
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    pub fn add_github(&self, name: &str, repo: &str) -> Value {
        self.rpc("source.add_github", json!({ "name": name, "repo": repo }))
    }

    pub fn source_row(&self, name: &str) -> Option<Value> {
        self.ok("source.list", json!({}))
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == json!(name))
            .cloned()
    }

    /// Waits until `source.list` says the source is `active`.
    pub fn wait_active(&self, name: &str) -> Value {
        self.wait_for(&format!("{name} active"), || {
            self.source_row(name)
                .filter(|r| r["status"] == json!("active"))
        })
    }

    /// Polls `f` (bounded by [`WAIT`]) until it answers `Some`.
    pub fn wait_for<T>(&self, what: &str, f: impl Fn() -> Option<T>) -> T {
        let start = Instant::now();
        loop {
            if let Some(v) = f() {
                return v;
            }
            assert!(start.elapsed() < WAIT, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// Every `unit.list` entry of `source`, recursively.
    pub fn entries(&self, source: &str) -> Vec<Value> {
        self.ok(
            "unit.list",
            json!({ "source": source, "folder": "", "recursive": true }),
        )["entries"]
            .as_array()
            .unwrap()
            .clone()
    }

    pub fn connect(&self) -> Client {
        let ws = self
            .rt
            .block_on(connect(&self.addr, &self.token))
            .expect("websocket handshake");
        let mut client = Client {
            rt: self.rt.clone(),
            ws,
            next_id: 1,
        };
        // A reply proves the server has subscribed to the pushes.
        client.request("graph.get_edges", json!({}));
        client
    }

    /// Stops the daemon and hands back its data dir, for a restart.
    pub fn close_keeping_data(mut self) -> tempfile::TempDir {
        let dir = self
            ._own_dir
            .take()
            .expect("a daemon with its own data dir");
        self.shutdown();
        dir
    }

    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let _ = self.stop.send(true);
        if let Some(handle) = self.forwarder.take() {
            let _ = self.rt.block_on(handle);
        }
        self.server.abort();
        let _ = self.rt.block_on(&mut self.server);
        for row in self.registry.list().unwrap_or_default() {
            if !row.is_remote() {
                let _ = self.registry.unregister(&row.name);
            }
        }
    }
}

pub async fn connect(
    addr: &SocketAddr,
    token: &str,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = format!("ws://{addr}/rpc").into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        format!("Bearer {token}").parse().expect("header"),
    );
    let (ws, _) = tokio_tungstenite::connect_async(request).await?;
    Ok(ws)
}

pub struct Client {
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

    pub fn request(&mut self, method: &str, params: Value) -> Value {
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

    /// The next `graph:changed` notification of any kind, or `None` when
    /// none arrives within `timeout`.
    pub fn next_graph_changed(&mut self, timeout: Duration) -> Option<Value> {
        let start = Instant::now();
        loop {
            let left = timeout.checked_sub(start.elapsed())?;
            let frame = self.frame(left)?;
            if frame["method"] == json!("graph:changed") {
                return Some(frame["params"].clone());
            }
        }
    }

    /// Reads and drops every `graph:changed` that arrives until none has for
    /// `quiet`; how many there were.
    pub fn drain_graph_changed(&mut self, quiet: Duration) -> usize {
        let mut n = 0;
        while self.next_graph_changed(quiet).is_some() {
            n += 1;
        }
        n
    }

    /// The next `graph:changed` notification carrying `sources_changed`,
    /// skipping the ordinary ones (the index's progress pushes).
    pub fn next_sources_changed(&mut self, timeout: Duration) -> Option<Value> {
        let start = Instant::now();
        loop {
            let left = timeout.checked_sub(start.elapsed())?;
            let frame = self.frame(left)?;
            if frame["params"].get("sources_changed").is_some() {
                return Some(frame["params"]["sources_changed"].clone());
            }
        }
    }
}

/// Every regular file below `dir`, relative, never following a symlink.
pub fn files_under(dir: &Path) -> Vec<PathBuf> {
    fn walk(base: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let Ok(t) = e.file_type() else { continue };
            if t.is_dir() {
                walk(base, &e.path(), out);
            } else if t.is_file() {
                out.push(e.path().strip_prefix(base).unwrap().to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out
}

/// How long the tests wait to see that NOTHING happens: a bounded quiet
/// window. A poller looks at its clock every 10 ms in these tests, so this
/// is many looks.
pub const QUIET: Duration = Duration::from_millis(400);
/// The poll interval.
pub const INTERVAL: Duration = Duration::from_secs(60);

/// A signed-in daemon on a fake clock, in front of a gated fake GitHub that
/// holds the PR-5 `atlas` repository, with the source `g` added and its
/// first index finished. Time moves only when a test says so.
pub struct Rig {
    pub fake: Arc<Fake>,
    pub clock: Arc<FakeClock>,
    pub d: Daemon,
    /// The daemon's credential store: a test signs in again through it.
    pub store: Arc<MemStore>,
    /// The data dir once a restart has taken it from the first daemon.
    dir: Option<tempfile::TempDir>,
}

impl Rig {
    pub fn start() -> Rig {
        Self::start_with(AdapterOptions::default())
    }

    /// Like [`Rig::start`] with the adapter's timeouts given.
    pub fn start_with(options: AdapterOptions) -> Rig {
        let clock = Arc::new(FakeClock::at_epoch_2026());
        let fake = Fake::start_gated(clock.clone());
        fake.atlas();
        let store = MemStore::signed_in();
        let d = Daemon::start_full(&fake, store.clone(), None, clock.clone(), options);
        d.add_github("g", REPO);
        d.wait_active("g");
        let rig = Rig {
            fake,
            clock,
            d,
            store,
            dir: None,
        };
        rig.let_pushes_settle();
        rig
    }

    /// The first index and its graph refresh may still be pushing when the
    /// source turns `active`: wait until a client sees no more, so a test's
    /// count of pushes is the poll's alone.
    fn let_pushes_settle(&self) {
        let mut client = self.d.connect();
        client.drain_graph_changed(QUIET);
    }

    /// A fresh start of the daemon on the same data dir, clock and fake.
    pub fn restart(self) -> Rig {
        let Rig {
            fake,
            clock,
            d,
            store: _,
            dir,
        } = self;
        let dir = match dir {
            Some(dir) => {
                d.close();
                dir
            }
            None => d.close_keeping_data(),
        };
        let store = MemStore::signed_in();
        let d = Daemon::start_full(
            &fake,
            store.clone(),
            Some(dir.path().to_path_buf()),
            clock.clone(),
            AdapterOptions::default(),
        );
        Rig {
            fake,
            clock,
            d,
            store,
            dir: Some(dir),
        }
    }

    /// The user signs in again with another token.
    pub fn sign_in_again(&self, token: &str) {
        self.store
            .set(Provider::Github, &Secret::new(token.to_string()))
            .expect("store the new token");
    }

    /// Moves the clock.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// Waits (bounded) until the fake has seen `n` polls of the branch.
    pub fn wait_polls(&self, n: usize) {
        self.d.wait_for(&format!("{n} polls"), || {
            (self.fake.polls().len() >= n).then_some(())
        });
    }

    /// Waits until `source.list` shows `g` with this status.
    pub fn wait_status(&self, status: &str) -> Value {
        self.d.wait_for(&format!("g is {status}"), || {
            self.d
                .source_row("g")
                .filter(|r| r["status"] == json!(status))
        })
    }

    /// Lets a poller that would act do so, and waits for nothing.
    pub fn quiet(&self) {
        std::thread::sleep(QUIET);
    }
}

/// Everything the index holds for a remote source, as one string: the
/// units (with their hashes), the links, the items, the cursor, the queue
/// and the pins. The source's own row (status, `last_sync_at`) is NOT in it:
/// that is what a poll is allowed to change. Two equal snapshots mean "no
/// index row was written".
pub fn index_snapshot(cache_path: &Path) -> String {
    let cache = Cache::open_read_only(cache_path).expect("open the cache");
    let one = |sql: &str| -> String {
        cache
            .connection()
            .query_row(sql, [], |r| r.get::<_, Option<String>>(0))
            .unwrap_or_else(|e| panic!("{sql}: {e}"))
            .unwrap_or_default()
    };
    [
        one("SELECT group_concat(rel_path || '|' || COALESCE(hash, '-') || '|' || id, ';') FROM (SELECT * FROM units ORDER BY id)"),
        one("SELECT group_concat(from_unit_id || '>' || to_raw || '>' || COALESCE(to_unit_id, '-') || '@' || COALESCE(line, '-'), ';') FROM (SELECT * FROM links ORDER BY id)"),
        one("SELECT group_concat(source || '|' || path || '|' || COALESCE(kind, '-') || '|' || COALESCE(sha, '-') || '|' || COALESCE(size, '-') || '|' || state || '|' || COALESCE(reason, '-'), ';') FROM (SELECT * FROM remote_items ORDER BY source, path)"),
        one("SELECT group_concat(source || '|' || COALESCE(cursor, '-') || '|' || index_complete || '|' || COALESCE(updated_at, '-'), ';') FROM remote_cursors"),
        one("SELECT group_concat(source || '|' || path || '|' || COALESCE(base_revision, '-') || '|' || attempts || '|' || created_at, ';') FROM upload_queue"),
        one("SELECT group_concat(source || '|' || path || '|' || is_folder, ';') FROM remote_pins"),
        one("SELECT group_concat(rel_path, ';') FROM (SELECT rel_path FROM units u JOIN headings h ON h.unit_id = u.id ORDER BY h.id)"),
    ]
    .join("\n")
}

/// Puts a row in the upload queue, as PR-12's edit path will: an edit of
/// `path` waiting to be pushed.
pub fn seed_queued_upload(cache_path: &Path, source: &str, path: &str) {
    let cache = Cache::open(cache_path).expect("open the cache");
    cache
        .connection()
        .execute(
            "INSERT INTO upload_queue (source, path, base_revision, attempts, created_at) \
             VALUES (?1, ?2, 'base-rev', 2, 1795132000)",
            [source, path],
        )
        .expect("seed an upload-queue row");
}

/// Writes a cached copy where PR-7's fetch will: `remote/<source>/clone/<path>`.
pub fn seed_cached_copy(data_dir: &Path, source: &str, path: &str, bytes: &[u8]) -> PathBuf {
    let file = data_dir
        .join("remote")
        .join(source)
        .join("clone")
        .join(path);
    std::fs::create_dir_all(file.parent().unwrap()).expect("clone dir");
    std::fs::write(&file, bytes).expect("a cached copy");
    file
}

/// The kind and state of a source's items, read from the cache.
pub fn item_rows(cache_path: &Path, source: &str) -> Vec<tephra_cache::RemoteItemRow> {
    Cache::open_read_only(cache_path)
        .expect("open the cache")
        .remote_items(source)
        .expect("items")
}
