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
use tephra_remote::testing::FakeGithub;
use tephra_remote::SystemClock;
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
}

impl Fake {
    pub fn start() -> Arc<Fake> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("fake runtime");
        let gh = rt.block_on(FakeGithub::start());
        Arc::new(Fake { rt, gh })
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
        let hub = RemoteHub::new(
            store,
            Some(fake.gh.base_url()),
            Arc::new(SystemClock),
            data_dir.clone(),
        )
        .expect("hub");
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
        // cache; a first index that was cut short is finished.
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
