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
use tephra_remote::testing::{FakeDrive, FakeGithub, LinkGate};
use tephra_remote::{AdapterOptions, Clock, FakeClock, SystemClock};
use tephrad::fixes::FixEngine;
use tephrad::refactor::fix_write::FixWriter;
use tephrad::refactor::RefactorEngine;
use tephrad::registry::SourceRegistry;
use tephrad::remote::fetch::{Disk, RealDisk, SinkFile};
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
/// The Google token of a signed-in test store.
pub const GOOGLE_TOKEN: &str = "ya29.SENTINEL_google_token_do_not_leak";
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
        store
            .items
            .lock()
            .unwrap()
            .insert(Provider::Google, GOOGLE_TOKEN.to_string());
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

/// What a test sets of the cache cap (PR-8).
#[derive(Debug, Clone, Copy)]
pub struct Tuning {
    /// The limit as `cache_limit` is written; `None`: what a former run of
    /// this data dir saved, else 10 GB.
    pub limit: Option<&'static str>,
    /// Seconds a file must be unused before it may be dropped. The
    /// product's is 600; tests that do not move the clock keep it, so
    /// nothing is dropped under them.
    pub idle_secs: u64,
    /// The fake sits behind a switch, so `network_down` works.
    pub gated: bool,
    /// How often a poller looks at the (fake) clock, in real time. The
    /// default 10 ms makes a poll that the clock has made due run at once;
    /// a test that moves the clock far and then counts requests gives a
    /// tick longer than the test, so no poll can land in between.
    pub poll_tick: Duration,
    /// The git program a clone's pass runs (PR-9); `None`: `git` on the path.
    /// A test gives a wrapper that records what it is asked.
    pub git: Option<&'static str>,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            limit: None,
            idle_secs: 600,
            gated: false,
            poll_tick: Duration::from_millis(10),
            git: None,
        }
    }
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
        Self::start_full_with_disk(fake, store, data_dir, clock, options, None)
    }

    /// [`Daemon::start_full`] with the disk a fetch writes to given (a
    /// [`ProbeDisk`] makes a write or an fsync fail).
    pub fn start_full_with_disk(
        fake: &Arc<Fake>,
        store: Arc<MemStore>,
        data_dir: Option<PathBuf>,
        clock: Arc<dyn Clock>,
        options: AdapterOptions,
        disk: Option<Arc<dyn Disk>>,
    ) -> Daemon {
        Self::start_full_with_drive(fake, store, data_dir, clock, options, disk, None)
    }

    /// [`Daemon::start_full_with_disk`] with the Drive API at `drive_url`
    /// (a `FakeDrive`'s `base_url()`).
    pub fn start_full_with_drive(
        fake: &Arc<Fake>,
        store: Arc<MemStore>,
        data_dir: Option<PathBuf>,
        clock: Arc<dyn Clock>,
        options: AdapterOptions,
        disk: Option<Arc<dyn Disk>>,
        drive_url: Option<&str>,
    ) -> Daemon {
        Self::start_tuned(
            fake,
            store,
            data_dir,
            clock,
            options,
            disk,
            drive_url,
            Tuning::default(),
        )
    }

    /// [`Daemon::start_full_with_drive`] with the cache cap's settings
    /// given (PR-8).
    #[allow(clippy::too_many_arguments)]
    pub fn start_tuned(
        fake: &Arc<Fake>,
        store: Arc<MemStore>,
        data_dir: Option<PathBuf>,
        clock: Arc<dyn Clock>,
        options: AdapterOptions,
        disk: Option<Arc<dyn Disk>>,
        drive_url: Option<&str>,
        tuning: Tuning,
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
                .with_drive_api_url(drive_url)
                .expect("the Drive adapter")
                .with_poll_tick(tuning.poll_tick);
        let hub = match disk {
            Some(disk) => hub.with_disk(disk),
            None => hub,
        };
        let hub = match tuning.git {
            Some(program) => hub.with_git_program(PathBuf::from(program)),
            None => hub,
        };
        // The cap (PR-8). The limit is kept in a file of the data dir, as
        // the daemon keeps it in its config file: a restart on the same
        // data dir starts with what `cache set-limit` saved. A test that
        // names a limit starts with that one.
        let limit_file = data_dir.join("cache_limit.saved");
        let limit_text = tuning
            .limit
            .map(str::to_string)
            .or_else(|| std::fs::read_to_string(&limit_file).ok())
            .unwrap_or_else(|| "10 GB".to_string());
        let limit_bytes = tephrad::cache_limit::parse_cache_limit(&limit_text).expect("a limit");
        let hub = hub
            .with_cache_limit(&limit_text, limit_bytes)
            .with_limit_saver(Arc::new(move |text| {
                std::fs::write(&limit_file, text).map_err(|e| e.to_string())
            }))
            .with_cache_idle_secs(tuning.idle_secs)
            .with_evict_tick(Duration::from_millis(20));
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
        self.reply(method, params)["result"].clone()
    }

    /// The whole reply to a request (an error's `code` and `data` too).
    pub fn reply(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.rt
            .block_on(self.ws.send(Message::Text(body.to_string().into())))
            .expect("send");
        loop {
            let frame = self.frame(WAIT).expect("a reply");
            if frame.get("id") == Some(&json!(id)) {
                return frame;
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

/// A disk that does what the real one does, counts what is written, and can
/// be told to fail the way a full disk does (E16).
pub struct ProbeDisk {
    /// How many `write_all` calls reached the disk.
    pub writes: std::sync::atomic::AtomicUsize,
    /// The largest single write.
    pub largest_write: std::sync::atomic::AtomicUsize,
    /// Fail the write with this number (1 is the first) with `ENOSPC`.
    fail_write_at: Mutex<Option<usize>>,
    /// Fail the file's fsync with `ENOSPC`.
    fail_file_sync: std::sync::atomic::AtomicBool,
    /// Fail the directory's fsync with `ENOSPC`.
    fail_dir_sync: std::sync::atomic::AtomicBool,
    /// Every path a delete was asked for (PR-8's eviction), and whether
    /// deletes fail (as a read-only folder would).
    pub removes: Mutex<Vec<PathBuf>>,
    fail_remove: std::sync::atomic::AtomicBool,
}

impl ProbeDisk {
    pub fn new() -> Arc<ProbeDisk> {
        Arc::new(ProbeDisk {
            writes: Default::default(),
            largest_write: Default::default(),
            fail_write_at: Mutex::new(None),
            fail_file_sync: Default::default(),
            fail_dir_sync: Default::default(),
            removes: Mutex::new(Vec::new()),
            fail_remove: Default::default(),
        })
    }

    /// Makes every delete fail with `EPERM`.
    pub fn fail_remove(&self, on: bool) {
        self.fail_remove
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn fail_write_number(&self, n: usize) {
        *self.fail_write_at.lock().unwrap() = Some(n);
    }

    pub fn fail_file_sync(&self, on: bool) {
        self.fail_file_sync
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn fail_dir_sync(&self, on: bool) {
        self.fail_dir_sync
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn heal(&self) {
        *self.fail_write_at.lock().unwrap() = None;
        self.fail_file_sync(false);
        self.fail_dir_sync(false);
    }
}

fn enospc() -> std::io::Error {
    std::io::Error::from_raw_os_error(28)
}

struct ProbeFile {
    inner: Box<dyn SinkFile>,
    probe: Arc<ProbeDisk>,
}

impl SinkFile for ProbeFile {
    fn write_all(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        use std::sync::atomic::Ordering::SeqCst;
        let n = self.probe.writes.fetch_add(1, SeqCst) + 1;
        self.probe.largest_write.fetch_max(bytes.len(), SeqCst);
        if *self.probe.fail_write_at.lock().unwrap() == Some(n) {
            return Err(enospc());
        }
        self.inner.write_all(bytes)
    }

    fn sync_all(&mut self) -> std::io::Result<()> {
        if self
            .probe
            .fail_file_sync
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(enospc());
        }
        self.inner.sync_all()
    }
}

/// `Disk` for a `ProbeDisk` held in an `Arc` (the daemon owns one clone, the
/// test keeps the other).
pub struct ProbeHandle(pub Arc<ProbeDisk>);

impl Disk for ProbeHandle {
    fn create_new(&self, path: &Path) -> std::io::Result<Box<dyn SinkFile>> {
        Ok(Box::new(ProbeFile {
            inner: RealDisk.create_new(path)?,
            probe: self.0.clone(),
        }))
    }

    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.0
            .removes
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(path.to_path_buf());
        if self.0.fail_remove.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        RealDisk.remove_file(path)
    }

    fn sync_dir(&self, dir: &Path) -> std::io::Result<()> {
        if self
            .0
            .fail_dir_sync
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(enospc());
        }
        RealDisk.sync_dir(dir)
    }
}

/// A daemon on a fake clock in front of a fake GitHub: `build` puts files in
/// the fake, then the source `g` is added and indexed. No poll is due unless
/// the test moves the clock.
pub struct FetchRig {
    pub fake: Arc<Fake>,
    pub clock: Arc<FakeClock>,
    pub d: Daemon,
    pub probe: Arc<ProbeDisk>,
    /// The data dir once a restart has taken it from the first daemon.
    dir: Option<tempfile::TempDir>,
}

impl FetchRig {
    pub fn start(build: impl FnOnce(&Fake)) -> FetchRig {
        Self::start_with_drive(None, build)
    }

    /// [`FetchRig::start`] with the Drive API at `drive_url`.
    pub fn start_with_drive(drive_url: Option<&str>, build: impl FnOnce(&Fake)) -> FetchRig {
        Self::start_tuned(drive_url, Tuning::default(), build)
    }

    /// [`FetchRig::start`] with the cache cap's settings given (PR-8).
    pub fn start_capped(tuning: Tuning, build: impl FnOnce(&Fake)) -> FetchRig {
        Self::start_tuned(None, tuning, build)
    }

    fn start_tuned(drive_url: Option<&str>, tuning: Tuning, build: impl FnOnce(&Fake)) -> FetchRig {
        let clock = Arc::new(FakeClock::at_epoch_2026());
        let fake = if tuning.gated {
            Fake::start_gated(clock.clone())
        } else {
            Fake::start_with_clock(clock.clone())
        };
        fake.atlas();
        build(&fake);
        let probe = ProbeDisk::new();
        let d = Daemon::start_tuned(
            &fake,
            MemStore::signed_in(),
            None,
            clock.clone(),
            AdapterOptions::default(),
            Some(Arc::new(ProbeHandle(probe.clone()))),
            drive_url,
            tuning,
        );
        d.add_github("g", REPO);
        d.wait_active("g");
        let rig = FetchRig {
            fake,
            clock,
            d,
            probe,
            dir: None,
        };
        rig.d.connect().drain_graph_changed(QUIET);
        rig
    }

    /// A fresh start of the daemon on the same data dir, clock and fake
    /// (the disk probe is the same one). `tuning.limit` of `None` starts
    /// with the limit a former run saved.
    pub fn restart(self, tuning: Tuning) -> FetchRig {
        let FetchRig {
            fake,
            clock,
            d,
            probe,
            dir,
        } = self;
        let dir = match dir {
            Some(dir) => {
                d.close();
                dir
            }
            None => d.close_keeping_data(),
        };
        let d = Daemon::start_tuned(
            &fake,
            MemStore::signed_in(),
            Some(dir.path().to_path_buf()),
            clock.clone(),
            AdapterOptions::default(),
            Some(Arc::new(ProbeHandle(probe.clone()))),
            None,
            tuning,
        );
        FetchRig {
            fake,
            clock,
            d,
            probe,
            dir: Some(dir),
        }
    }

    /// `unit.fetch` of `path` of `g`, the whole reply.
    pub fn fetch(&self, path: &str) -> Value {
        self.d
            .rpc("unit.fetch", json!({ "unit": format!("@g:{path}") }))
    }

    /// The row of `path` of `g`.
    pub fn row(&self, path: &str) -> tephra_cache::RemoteFetchRow {
        Cache::open_read_only(&self.d.cache_path)
            .expect("open the cache")
            .remote_fetch_row("g", path)
            .expect("read the row")
            .unwrap_or_else(|| panic!("{path} has a row"))
    }

    /// Where `path` of `g` is fetched to.
    pub fn cached_path(&self, path: &str) -> PathBuf {
        self.d
            .data_dir
            .join("remote")
            .join("g")
            .join("clone")
            .join(path)
    }

    /// Everything under the content cache of `g`.
    pub fn cache_files(&self) -> Vec<PathBuf> {
        files_under(&self.d.data_dir.join("remote").join("g"))
    }

    /// Content requests the fake has seen (index reads included).
    pub fn reads(&self) -> usize {
        self.fake.reads().len()
    }
}

/// A source of kind `drive` with items, written straight into the remote
/// tables: PR-10 adds the real thing. `items` are
/// `(path, kind, file_id, size)`.
pub fn seed_drive_source(cache_path: &Path, name: &str, items: &[(&str, &str, &str, i64)]) {
    let cache = Cache::open(cache_path).expect("open the cache");
    cache
        .connection()
        .execute(
            "INSERT INTO remote_sources (name, kind, target, created_at, status) \
             VALUES (?1, 'drive', 'folder-id', 1795132000, 'active')",
            [name],
        )
        .expect("seed a drive source");
    for (path, kind, file_id, size) in items {
        cache
            .connection()
            .execute(
                "INSERT INTO remote_items (source, path, kind, file_id, size, state) \
                 VALUES (?1, ?2, ?3, ?4, ?5, 'remote')",
                rusqlite::params![name, path, kind, file_id, size],
            )
            .expect("seed a drive item");
    }
}

/// Runs `sql` on the cache database: seeds what later PRs will write (a
/// pin, an edit waiting to be pushed, a state).
pub fn exec_sql(cache_path: &Path, sql: &str) {
    let cache = Cache::open(cache_path).expect("open the cache");
    cache.connection().execute_batch(sql).expect("seed sql");
}

/// `cache.status` of a daemon, unwrapped.
pub fn cache_status(d: &Daemon) -> Value {
    d.ok("cache.status", json!({}))
}

/// The `n`-th (0-based) file of a fixture repo, `size` bytes, named
/// `big/<n>.bin`. A distinct byte per file so a mix-up shows.
pub fn big_file(n: u8, size: usize) -> (String, Vec<u8>) {
    (format!("big/{n}.bin"), vec![b'a' + n; size])
}

/// Hex-free stable stamp: seconds since the fake clock's epoch.
pub fn stamp(clock: &FakeClock) -> i64 {
    clock
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs() as i64
}

/// A daemon in front of a fake GitHub (unused) and a fake Drive that holds
/// `files` (`id`, bytes); the Drive source `gdrive` has the rows `items`
/// (`path`, kind, file id, listed size). Its files are fetched by the real
/// `unit.fetch`, so each copy gets the stamp eviction compares it with.
pub struct DriveRig {
    pub rig: FetchRig,
    pub drive: FakeDrive,
    // The fake Drive's server runs on this runtime.
    _rt: tokio::runtime::Runtime,
}

impl DriveRig {
    pub fn start(files: &[(&str, &[u8])], items: &[(&str, &str, &str, i64)]) -> DriveRig {
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let drive = rt.block_on(FakeDrive::start());
        for (id, bytes) in files {
            drive.add_file(id, id, "application/octet-stream", None, bytes);
        }
        let rig = FetchRig::start_with_drive(Some(drive.base_url()), |_| {});
        seed_drive_source(&rig.d.cache_path, "gdrive", items);
        DriveRig {
            rig,
            drive,
            _rt: rt,
        }
    }

    pub fn fetch(&self, path: &str) -> Value {
        self.rig
            .d
            .rpc("unit.fetch", json!({ "unit": format!("@gdrive:{path}") }))
    }

    /// The content root of the source.
    pub fn root(&self) -> PathBuf {
        self.rig.d.data_dir.join("remote").join("gdrive")
    }

    pub fn state(&self, path: &str) -> String {
        item_rows(&self.rig.d.cache_path, "gdrive")
            .into_iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("{path} has a row"))
            .state
    }
}

// ---- Google Drive sources (PR-10) ----------------------------------------------------

/// A signed-in daemon on a fake clock in front of the fake Drive of PR-3:
/// `build` puts files in the Drive, then the test adds a Drive source. No
/// poll is due unless the test moves the clock.
pub struct DriveDaemon {
    pub drive: FakeDrive,
    pub clock: Arc<FakeClock>,
    pub d: Daemon,
    pub store: Arc<MemStore>,
    // The fake Drive's server runs on this runtime.
    _rt: tokio::runtime::Runtime,
    /// The data dir once a restart has taken it from the first daemon.
    dir: Option<tempfile::TempDir>,
}

impl DriveDaemon {
    pub fn start(build: impl FnOnce(&FakeDrive)) -> DriveDaemon {
        Self::start_with(MemStore::signed_in(), build)
    }

    pub fn start_with(store: Arc<MemStore>, build: impl FnOnce(&FakeDrive)) -> DriveDaemon {
        Self::start_custom(store, Tuning::default(), None, build)
    }

    /// [`DriveDaemon::start_with`] with the cache cap's settings and the
    /// disk a fetch writes to given (PR-12's pin tests).
    pub fn start_custom(
        store: Arc<MemStore>,
        tuning: Tuning,
        disk: Option<Arc<dyn Disk>>,
        build: impl FnOnce(&FakeDrive),
    ) -> DriveDaemon {
        let rt = tokio::runtime::Runtime::new().expect("a runtime");
        let clock = Arc::new(FakeClock::at_epoch_2026());
        let drive = rt.block_on(FakeDrive::start_with_clock(clock.clone()));
        build(&drive);
        let fake = Fake::start_with_clock(clock.clone());
        let d = Daemon::start_tuned(
            &fake,
            store.clone(),
            None,
            clock.clone(),
            AdapterOptions::default(),
            disk,
            Some(drive.base_url()),
            tuning,
        );
        DriveDaemon {
            drive,
            clock,
            d,
            store,
            _rt: rt,
            dir: None,
        }
    }

    /// `source.add_drive`, the whole reply.
    pub fn add_raw(&self, name: &str, folder: &str) -> Value {
        self.d.rpc(
            "source.add_drive",
            json!({ "name": name, "folder": folder }),
        )
    }

    /// `source.add_drive` that must succeed.
    pub fn add(&self, name: &str, folder: &str) -> Value {
        self.d.ok(
            "source.add_drive",
            json!({ "name": name, "folder": folder }),
        )
    }

    /// Adds the source and waits until its first index is done.
    pub fn add_active(&self, name: &str, folder: &str) -> Value {
        self.add(name, folder);
        let row = self.d.wait_active(name);
        self.d.connect().drain_graph_changed(QUIET);
        row
    }

    /// Moves the clock one poll interval and waits until the poller has
    /// finished one poll of the source.
    pub fn poll(&self) {
        let before = self.d.registry.remote_polls_done();
        self.clock.advance(INTERVAL);
        self.d.wait_for("a poll", || {
            (self.d.registry.remote_polls_done() > before).then_some(())
        });
    }

    /// [`DriveDaemon::poll`] for a poll that changes the index, returning
    /// once the daemon's in-memory graph shows it. The poller commits the
    /// index and announces; the daemon then rebuilds the graph and only
    /// then pushes `graph:changed`, so `graph.*` and `unit.resolve_link`
    /// answer from the old graph until that push. A test that asks the
    /// graph right after `poll` races the rebuild.
    pub fn poll_applied(&self) {
        let mut client = self.d.connect();
        self.poll();
        client
            .next_graph_changed(WAIT)
            .expect("a graph:changed after a poll that changed the index");
    }

    /// The user signs in to Google again with another token.
    pub fn sign_in_again(&self, token: &str) {
        self.store
            .set(Provider::Google, &Secret::new(token.to_string()))
            .expect("store the new token");
    }

    /// The items of `source`: `(path, kind)`, sorted.
    pub fn listing(&self, source: &str) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = item_rows(&self.d.cache_path, source)
            .into_iter()
            .map(|r| (r.path, r.kind.unwrap_or_default()))
            .collect();
        out.sort();
        out
    }

    pub fn paths(&self, source: &str) -> Vec<String> {
        self.listing(source).into_iter().map(|(p, _)| p).collect()
    }

    pub fn media_reads(&self) -> usize {
        self.drive.request_log().matching("alt=media").len()
    }

    pub fn exports(&self) -> usize {
        self.drive.request_log().matching("/export").len()
    }

    /// A fresh start of the daemon on the same data dir, clock and Drive.
    pub fn restart(self) -> DriveDaemon {
        self.restart_after(|_| {})
    }

    /// [`DriveDaemon::restart`] that runs `between` on the cache file once
    /// the first daemon is stopped and before the next one starts: a test
    /// leaves the state a kill would have left.
    pub fn restart_after(self, between: impl FnOnce(&Path)) -> DriveDaemon {
        let DriveDaemon {
            drive,
            clock,
            d,
            store,
            _rt,
            dir,
        } = self;
        let cache_path = d.cache_path.clone();
        let dir = match dir {
            Some(dir) => {
                d.close();
                dir
            }
            None => d.close_keeping_data(),
        };
        between(&cache_path);
        let fake = Fake::start_with_clock(clock.clone());
        let d = Daemon::start_tuned(
            &fake,
            store.clone(),
            Some(dir.path().to_path_buf()),
            clock.clone(),
            AdapterOptions::default(),
            None,
            Some(drive.base_url()),
            Tuning::default(),
        );
        DriveDaemon {
            drive,
            clock,
            d,
            store,
            _rt,
            dir: Some(dir),
        }
    }
}

// ---- Drive edits go back to Drive (PR-11) ---------------------------------------------

/// The Drive of the upload tests: a folder `fld` with `Plan.md` (id `p1`),
/// `Other.md` (`o1`) and a Google Doc (`d1`).
pub fn edits_drive(drive: &FakeDrive) {
    drive.add_folder("fld", "Notes", None);
    drive.add_file(
        "p1",
        "Plan.md",
        "text/markdown",
        Some("fld"),
        b"# Plan\nfirst\n",
    );
    drive.add_file("o1", "Other.md", "text/markdown", Some("fld"), b"# Other\n");
    drive.add_doc("d1", "Doc", Some("fld"), "# Doc\n");
}

/// What a person saves into the content cache: the bytes, in place.
pub fn save(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).expect("save a file of the cache");
}

impl DriveDaemon {
    /// A daemon with the source `gdrive` on [`edits_drive`], indexed.
    pub fn start_edits() -> DriveDaemon {
        let dd = DriveDaemon::start(edits_drive);
        dd.add_active("gdrive", "fld");
        dd
    }

    /// `unit.fetch` of `@gdrive:<path>`: where the file is.
    pub fn fetch_file(&self, path: &str) -> PathBuf {
        let result = self
            .d
            .ok("unit.fetch", json!({ "unit": format!("@gdrive:{path}") }));
        PathBuf::from(result["path"].as_str().expect("a path"))
    }

    pub fn row(&self, path: &str) -> tephra_cache::RemoteItemRow {
        item_rows(&self.d.cache_path, "gdrive")
            .into_iter()
            .find(|r| r.path == path)
            .unwrap_or_else(|| panic!("{path} has a row"))
    }

    /// Waits until `path` is in `state`; returns the row.
    pub fn wait_state(&self, path: &str, state: &str) -> tephra_cache::RemoteItemRow {
        self.d.wait_for(&format!("{path} to be {state}"), || {
            Some(self.row(path)).filter(|r| r.state == state)
        })
    }

    /// The upload queue of `gdrive`.
    pub fn queue(&self) -> Vec<tephra_cache::UploadEntry> {
        Cache::open_read_only(&self.d.cache_path)
            .expect("open the cache")
            .upload_entries("gdrive")
            .expect("the queue")
    }

    /// Waits until the queue row of `path` has been tried `attempts` times.
    pub fn wait_attempts(&self, path: &str, attempts: u32) -> tephra_cache::UploadEntry {
        self.d.wait_for(&format!("{attempts} tries of {path}"), || {
            self.queue()
                .into_iter()
                .find(|e| e.path == path && e.attempts >= attempts)
        })
    }

    /// The fake clock moves; nothing sleeps.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    pub fn now_secs(&self) -> i64 {
        stamp(&self.clock)
    }

    /// Every request the fake Drive saw that is not a GET, as
    /// `METHOD path`.
    pub fn writes(&self) -> Vec<String> {
        self.drive
            .request_log()
            .all()
            .into_iter()
            .filter(|r| r.method != "GET")
            .map(|r| format!("{} {}", r.method, r.path))
            .collect()
    }

    /// `resolve`, the whole reply.
    pub fn resolve(&self, path: &str, keep: &str) -> Value {
        self.d.rpc(
            "resolve",
            json!({ "unit": format!("@gdrive:{path}"), "keep": keep }),
        )
    }

    /// The content root of `gdrive`.
    pub fn root(&self) -> PathBuf {
        self.d.data_dir.join("remote").join("gdrive")
    }
}
