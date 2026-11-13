//! `unit.list` (G1) end to end: the real `SourceRegistry`, a real
//! on-disk tree, and the real RPC server on an ephemeral loopback
//! port (never the product's port) with a throwaway token.
//!
//! Covers the online listing (folders from disk, files from the cache,
//! ignore rules, empty folders, recursion), the offline listing (cache
//! only, `offline: true`), and the refusals: `..`, absolute paths,
//! symlinks out of the source, files given as folders, unknown sources.

mod fixtures;

use std::os::unix::fs::symlink;
use std::sync::Arc;

use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;

struct Daemon {
    /// Owned when `start` made the data dir; `None` when the caller
    /// keeps it across a restart (`start_in`).
    _data_dir: Option<tempfile::TempDir>,
    data_path: std::path::PathBuf,
    registry: Arc<SourceRegistry>,
    client: reqwest::Client,
    url: String,
    token: String,
    server: tokio::task::JoinHandle<()>,
    next_id: std::sync::atomic::AtomicU64,
}

impl Daemon {
    async fn start() -> Daemon {
        let data_dir = tempfile::tempdir().expect("data dir");
        let mut daemon = Daemon::start_in(data_dir.path()).await;
        daemon._data_dir = Some(data_dir);
        daemon
    }

    /// Starts on `data_dir`, the way `main` does: a registry over the
    /// cache file there (so a restart sees what an earlier daemon
    /// cached), the graph, and the RPC router on an ephemeral port.
    async fn start_in(data_dir: &std::path::Path) -> Daemon {
        let cache_path = data_dir.join("cache.db");
        let token_path = data_dir.join("token");
        let token = "b".repeat(64);
        std::fs::write(&token_path, &token).expect("write token");

        let registry = Arc::new(
            SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry"),
        );
        let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
        let shared_graph = Arc::new(SharedGraph::new(
            UnitGraph::build_from_cache(&graph_cache).expect("graph"),
        ));
        let graph_refresh = GraphRefresh::new(shared_graph.clone(), cache_path);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let state = Arc::new(AppState::new(
            token_path,
            shared_graph.clone() as Arc<dyn GraphBackend>,
            registry.clone() as Arc<dyn SourceBackend>,
            Arc::new(tephrad::refactor::RefactorEngine::new(
                shared_graph as Arc<dyn GraphBackend>,
                registry.clone() as Arc<dyn SourceBackend>,
                registry.clone() as Arc<dyn tephrad::refactor::WatcherPauseControl>,
            )) as Arc<dyn RefactorBackend>,
            Some(graph_refresh),
        ));
        let router = build_router(state);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Daemon {
            _data_dir: None,
            data_path: data_dir.to_path_buf(),
            registry,
            client: reqwest::Client::new(),
            url: format!("http://{addr}/rpc"),
            token,
            server,
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    /// The full JSON-RPC response envelope.
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .send()
            .await
            .expect("send")
            .json::<Value>()
            .await
            .expect("json")
    }

    async fn register(&self, name: &str, root: &std::path::Path) {
        let reply = self
            .rpc(
                "source.register",
                json!({"name": name, "root": root.to_string_lossy()}),
            )
            .await;
        assert!(reply.get("error").is_none(), "register failed: {reply}");
    }

    async fn list(&self, source: &str, params: Value) -> Value {
        let mut p = json!({"source": source});
        for (k, v) in params.as_object().unwrap() {
            p[k] = v.clone();
        }
        let reply = self.rpc("unit.list", p).await;
        assert!(reply.get("error").is_none(), "unit.list failed: {reply}");
        reply["result"].clone()
    }

    async fn list_error(&self, params: Value) -> (i64, String) {
        let reply = self.rpc("unit.list", params).await;
        let err = reply
            .get("error")
            .unwrap_or_else(|| panic!("expected an error, got {reply}"));
        (
            err["code"].as_i64().unwrap(),
            err["message"].as_str().unwrap().to_string(),
        )
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// A runtime for a test that must drop a `Daemon` outside async code.
fn new_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn paths(result: &Value) -> Vec<String> {
    result["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["rel_path"].as_str().unwrap().to_string())
        .collect()
}

fn entry<'a>(result: &'a Value, rel: &str) -> &'a Value {
    result["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["rel_path"] == rel)
        .unwrap_or_else(|| panic!("no entry {rel} in {result}"))
}

fn project() -> fixtures::TempSource {
    let src = fixtures::empty_source();
    src.write("README.md", "# Project\n\nIntro.\n");
    src.write(
        "notes/titled.md",
        "---\ntitle: A Titled Note\n---\n\n# Heading\n",
    );
    src.write("notes/headed.md", "# Only A Heading\n\nbody\n");
    src.write("notes/plain.md", "no heading at all\n");
    src.write("src/main.rs", "fn main() {}\n");
    src.write("src/rpc/server.rs", "// server\n");
    src.write_bytes("Attachments/doc.pdf", b"%PDF-1.4\n\0\x01 binary\0");
    src.write_bytes(
        "Attachments/logo.png",
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0],
    );
    src.mkdir("empty-folder");
    // Ignored by the defaults: must never appear.
    src.write("node_modules/pkg/index.js", "x\n");
    src.write(".git/config", "[core]\n");
    src
}

#[tokio::test]
async fn online_root_listing_has_folders_from_disk_then_files_from_the_cache() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    let result = daemon.list("proj", json!({})).await;

    assert_eq!(result["source"], "proj");
    assert_eq!(result["folder"], "");
    assert_eq!(result["recursive"], false);
    assert_eq!(result["offline"], false);
    assert_eq!(result["needs_rescan"], false);
    assert_eq!(
        paths(&result),
        ["Attachments", "empty-folder", "notes", "src", "README.md"]
    );

    let notes = entry(&result, "notes");
    assert_eq!(notes["kind"], "folder");
    assert_eq!(notes["id"], "@proj:notes");
    assert_eq!(notes["name"], "notes");
    assert_eq!(notes["size"], 3);
    assert!(
        notes["mtime"].as_i64().unwrap() > 1_000_000_000,
        "real folder mtime"
    );
    assert!(notes.get("title").is_none(), "folders carry no title");

    let readme = entry(&result, "README.md");
    assert_eq!(readme["kind"], "prose");
    assert_eq!(readme["id"], "@proj:README.md");
    assert_eq!(readme["size"], 18);
    assert!(readme["mtime"].as_i64().is_some());
    assert_eq!(readme["title"], "Project", "first heading is the title");
}

#[tokio::test]
async fn an_empty_folder_shows_with_size_zero_and_ignored_folders_never_do() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    let result = daemon.list("proj", json!({"recursive": true})).await;
    let all = paths(&result);

    assert_eq!(entry(&result, "empty-folder")["size"], 0);
    assert!(
        all.iter()
            .all(|p| !p.contains("node_modules") && !p.starts_with(".git")),
        "ignored paths leaked: {all:?}"
    );
    // And asking for an ignored folder directly is "no such folder".
    let (_, message) = daemon
        .list_error(json!({"source": "proj", "folder": "node_modules"}))
        .await;
    assert!(message.contains("no such folder"), "{message}");
}

#[tokio::test]
async fn a_folder_listing_gives_kinds_and_titles_for_every_file_type() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    let notes = daemon.list("proj", json!({"folder": "notes"})).await;
    assert_eq!(notes["folder"], "notes");
    assert_eq!(
        paths(&notes),
        ["notes/headed.md", "notes/plain.md", "notes/titled.md"]
    );
    assert_eq!(entry(&notes, "notes/titled.md")["title"], "A Titled Note");
    // No frontmatter title: the scan cached the first heading, which
    // the listing falls back to. No heading at all: no title.
    assert_eq!(entry(&notes, "notes/headed.md")["title"], "Only A Heading");
    assert!(entry(&notes, "notes/plain.md").get("title").is_none());

    let attachments = daemon.list("proj", json!({"folder": "Attachments"})).await;
    assert_eq!(
        entry(&attachments, "Attachments/doc.pdf")["kind"],
        "document"
    );
    assert_eq!(
        entry(&attachments, "Attachments/logo.png")["kind"],
        "binary"
    );
    assert!(
        entry(&attachments, "Attachments/logo.png")["size"]
            .as_i64()
            .unwrap()
            > 0
    );
    assert!(entry(&attachments, "Attachments/doc.pdf")
        .get("title")
        .is_none());

    let src_dir = daemon.list("proj", json!({"folder": "src/"})).await;
    assert_eq!(src_dir["folder"], "src", "trailing slash normalized");
    assert_eq!(paths(&src_dir), ["src/rpc", "src/main.rs"]);
    assert_eq!(entry(&src_dir, "src/main.rs")["kind"], "code");
}

#[tokio::test]
async fn recursive_listing_is_depth_first_and_deterministic() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    let first = daemon.list("proj", json!({"recursive": true})).await;
    let second = daemon.list("proj", json!({"recursive": true})).await;

    assert_eq!(first, second);
    assert_eq!(first["recursive"], true);
    assert_eq!(
        paths(&first),
        [
            "Attachments",
            "Attachments/doc.pdf",
            "Attachments/logo.png",
            "empty-folder",
            "notes",
            "notes/headed.md",
            "notes/plain.md",
            "notes/titled.md",
            "src",
            "src/rpc",
            "src/rpc/server.rs",
            "src/main.rs",
            "README.md",
        ]
    );
}

#[tokio::test]
async fn a_tephraignore_pattern_hides_files_and_folders_from_the_listing() {
    let src = project();
    src.write(".tephraignore", "Attachments/\n*.rs\n");
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    let result = daemon.list("proj", json!({"recursive": true})).await;
    let all = paths(&result);

    assert!(!all.iter().any(|p| p.starts_with("Attachments")), "{all:?}");
    assert!(!all.iter().any(|p| p.ends_with(".rs")), "{all:?}");
    assert!(
        all.contains(&"src".to_string()),
        "the folder itself is not ignored"
    );
    assert_eq!(
        entry(&result, "src")["size"],
        1,
        "size counts non-ignored children only"
    );
}

#[tokio::test]
async fn a_file_added_after_the_scan_is_absent_until_indexed_not_half_listed() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;
    src.write("notes/late.md", "---\ntitle: Late\n---\n");

    let before = daemon.list("proj", json!({"folder": "notes"})).await;
    assert!(!paths(&before).contains(&"notes/late.md".to_string()));

    let rescan = daemon.rpc("source.rescan", json!({"name": "proj"})).await;
    assert!(rescan.get("error").is_none(), "{rescan}");
    let after = daemon.list("proj", json!({"folder": "notes"})).await;
    assert_eq!(entry(&after, "notes/late.md")["title"], "Late");
}

#[tokio::test]
async fn an_offline_source_is_listed_from_the_cache_alone() {
    let src = project();
    let daemon = Daemon::start().await;
    let root = src.root().to_path_buf();
    daemon.register("proj", &root).await;

    // Take the source's root away entirely (an unmounted volume).
    let gone = root.with_extension("gone");
    std::fs::rename(&root, &gone).expect("move root away");

    let result = daemon.list("proj", json!({})).await;
    assert_eq!(result["offline"], true);
    assert_eq!(
        paths(&result),
        ["Attachments", "notes", "src", "README.md"],
        "the cache knows no empty folders"
    );
    let notes = entry(&result, "notes");
    assert_eq!(notes["kind"], "folder");
    assert_eq!(notes["size"], 3);
    assert!(notes["mtime"].is_null(), "no folder mtime without the disk");
    assert_eq!(entry(&result, "README.md")["kind"], "prose");

    let nested = daemon
        .list("proj", json!({"folder": "src", "recursive": true}))
        .await;
    assert_eq!(nested["offline"], true);
    assert_eq!(
        paths(&nested),
        ["src/rpc", "src/rpc/server.rs", "src/main.rs"]
    );

    let (_, message) = daemon
        .list_error(json!({"source": "proj", "folder": "nope"}))
        .await;
    assert!(message.contains("no such folder"), "{message}");

    // Put it back so the tempdir cleans up.
    std::fs::rename(&gone, &root).expect("restore root");
}

#[tokio::test]
async fn dot_dot_and_absolute_folders_are_invalid_params() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    for bad in ["..", "../..", "src/../..", "/etc", "/", "notes/../../x"] {
        let reply = daemon
            .rpc("unit.list", json!({"source": "proj", "folder": bad}))
            .await;
        if bad == "/" {
            // "/" is the root spelled oddly, not an escape.
            assert!(reply.get("error").is_none(), "{reply}");
            continue;
        }
        assert_eq!(
            reply["error"]["code"], -32602,
            "folder {bad:?} should be INVALID_PARAMS, got {reply}"
        );
    }
}

#[tokio::test]
async fn a_symlink_out_of_the_source_is_refused_and_never_followed() {
    let outside = fixtures::empty_source();
    outside.write("secret.txt", "top secret\n");
    outside.write("deeper/more.txt", "more\n");

    let src = project();
    symlink(outside.root(), src.root().join("escape")).expect("symlink");
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    // Asking for the link as a folder leaves the root: refused.
    let reply = daemon
        .rpc("unit.list", json!({"source": "proj", "folder": "escape"}))
        .await;
    assert_eq!(reply["error"]["code"], -32602, "{reply}");
    let reply = daemon
        .rpc(
            "unit.list",
            json!({"source": "proj", "folder": "escape/deeper"}),
        )
        .await;
    assert_eq!(reply["error"]["code"], -32602, "{reply}");

    // And a recursive listing from the root doesn't descend into it.
    let all = paths(&daemon.list("proj", json!({"recursive": true})).await);
    assert!(
        !all.iter()
            .any(|p| p.contains("secret.txt") || p.contains("more.txt")),
        "followed a symlink out of the source: {all:?}"
    );
    assert!(
        !all.contains(&"escape".to_string()),
        "a symlinked directory is not a folder entry: {all:?}"
    );
}

/// The filesystem (APFS on a Mac) can resolve a folder spelled with
/// another case or Unicode normalization, or through a symlink that
/// stays inside the root. Ids and the cache are byte-exact, so any
/// spelling that isn't the real name is "no such folder", never a
/// listing under ids that don't exist. (On a case-sensitive filesystem
/// the spellings don't resolve at all; the answer is the same.)
#[tokio::test]
async fn a_folder_spelled_differently_from_the_disk_or_through_an_in_root_symlink_is_no_such_folder(
) {
    let src = project();
    src.write("notes/sub/b.txt", "b\n");
    let nfc = "caf\u{e9}";
    let nfd = "cafe\u{301}";
    src.write(&format!("{nfc}/inner.txt"), "i\n");
    symlink(src.root().join("notes"), src.root().join("alias")).expect("symlink to a folder");
    symlink(src.root().join("README.md"), src.root().join("readme-link"))
        .expect("symlink to a file");
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    for bad in [
        "NOTES",
        "Notes",
        "notes/SUB",
        "NOTES/SUB",
        "alias",
        "alias/sub",
        nfd,
    ] {
        for recursive in [false, true] {
            let (code, message) = daemon
                .list_error(json!({"source": "proj", "folder": bad, "recursive": recursive}))
                .await;
            assert_eq!(code, -32011, "{bad:?}: SOURCE_ERROR");
            assert_eq!(message, format!("no such folder: {bad}"));
        }
    }

    // The real spellings still list, with ids that match the cache.
    let sub = daemon.list("proj", json!({"folder": "notes/sub"})).await;
    assert_eq!(paths(&sub), ["notes/sub/b.txt"]);
    assert_eq!(
        entry(&sub, "notes/sub/b.txt")["id"],
        "@proj:notes/sub/b.txt"
    );
    let cafe = daemon.list("proj", json!({"folder": nfc})).await;
    assert_eq!(paths(&cafe), [format!("{nfc}/inner.txt")]);

    // A symlink to a file is not a folder, not a missing one.
    let (code, message) = daemon
        .list_error(json!({"source": "proj", "folder": "readme-link"}))
        .await;
    assert_eq!(code, -32011);
    assert_eq!(message, "not a folder: readme-link");

    // An in-root symlinked directory is never a folder entry either.
    let all = paths(&daemon.list("proj", json!({"recursive": true})).await);
    assert!(
        !all.iter().any(|p| p == "alias" || p.starts_with("alias/")),
        "{all:?}"
    );
}

#[tokio::test]
async fn a_file_path_unknown_source_and_bad_param_types_are_errors() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    let (code, message) = daemon
        .list_error(json!({"source": "proj", "folder": "README.md"}))
        .await;
    assert_eq!(code, -32011, "SOURCE_ERROR");
    assert!(message.contains("not a folder"), "{message}");

    let (code, message) = daemon
        .list_error(json!({"source": "ghost", "folder": ""}))
        .await;
    assert_eq!(code, -32011);
    assert!(message.contains("no such source"), "{message}");

    let (code, message) = daemon
        .list_error(json!({"source": "proj", "folder": "nope"}))
        .await;
    assert_eq!(code, -32011);
    assert!(message.contains("no such folder"), "{message}");

    for params in [
        json!({}),
        json!({"source": ""}),
        json!({"source": 5}),
        json!({"source": "proj", "folder": 5}),
        json!({"source": "proj", "recursive": "yes"}),
        json!({"source": "proj", "folder": "a\u{0}b"}),
    ] {
        let reply = daemon.rpc("unit.list", params.clone()).await;
        assert_eq!(
            reply["error"]["code"], -32602,
            "{params} should be INVALID_PARAMS, got {reply}"
        );
    }

    // null counts as absent for the optional params.
    let ok = daemon
        .list("proj", json!({"folder": null, "recursive": null}))
        .await;
    assert_eq!(ok["folder"], "");
    assert_eq!(ok["recursive"], false);
}

#[tokio::test]
async fn unit_list_is_read_only_and_leaves_the_registry_usable() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;

    let before = daemon.registry.list_units("proj", "", true).unwrap();
    for _ in 0..5 {
        daemon.list("proj", json!({"recursive": true})).await;
    }
    let after = daemon.registry.list_units("proj", "", true).unwrap();
    assert_eq!(before, after);

    // Writers still work after many listings.
    src.write("added.md", "# Added\n");
    let rescan = daemon.rpc("source.rescan", json!({"name": "proj"})).await;
    assert!(rescan.get("error").is_none(), "{rescan}");
}

#[tokio::test]
async fn unicode_and_space_names_round_trip_byte_exact() {
    // NFC and NFD spellings of the same name live in separate folders:
    // APFS is normalization-insensitive, so two files differing only by
    // normalization can't coexist in one folder on a Mac. That they are
    // never merged is covered at the cache level (tephra-cache's
    // list_folder tests), where the bytes are the whole identity.
    let src = fixtures::empty_source();
    let nfc = "caf\u{e9} notes.md";
    let nfd = "cafe\u{301} notes.md";
    src.write(&format!("nfc/{nfc}"), "nfc\n");
    src.write(&format!("nfd/{nfd}"), "nfd\n");
    src.write("with space/inner file.txt", "x\n");
    src.write("日本語.md", "x\n");
    let daemon = Daemon::start().await;
    daemon.register("uni", src.root()).await;

    let result = daemon.list("uni", json!({"recursive": true})).await;

    for rel in [
        format!("nfc/{nfc}"),
        format!("nfd/{nfd}"),
        "with space/inner file.txt".to_string(),
        "日本語.md".to_string(),
        "with space".to_string(),
    ] {
        let e = entry(&result, &rel);
        assert_eq!(e["id"], format!("@uni:{rel}"));
        assert_eq!(
            e["name"].as_str().unwrap().as_bytes(),
            rel.rsplit('/').next().unwrap().as_bytes(),
            "{rel}"
        );
        assert_eq!(e["rel_path"].as_str().unwrap().as_bytes(), rel.as_bytes());
    }
}

#[tokio::test]
async fn every_kind_lists_with_its_real_kind_and_size_including_empty_nul_and_too_large() {
    use std::io::Write;
    let src = fixtures::empty_source();
    src.write("a.md", "# A\n");
    src.write("lib.rs", "fn x() {}\n");
    src.write("Cargo.toml", "[package]\nname = \"x\"\n");
    src.write_bytes("Attachments/doc.pdf", b"%PDF-1.4\n\0\x01 binary\0");
    src.write_bytes(
        "Attachments/logo.png",
        &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0],
    );
    src.write_bytes("zero", b"");
    src.write_bytes("zero.md", b"");
    src.write_bytes("nul.py", b"print('hi')\n\0\0 [[Target]]\n");
    let big = src.root().join("big.md");
    let mut f = std::fs::File::create(&big).unwrap();
    // Real text well past the NUL sniff window, then a sparse tail.
    f.write_all(&b"# big line of prose\n".repeat(4096)).unwrap();
    f.set_len(tephrad::file_kind::MAX_INDEXED_BYTES + 1)
        .unwrap();
    drop(f);
    let daemon = Daemon::start().await;
    daemon.register("kinds", src.root()).await;

    let result = daemon.list("kinds", json!({"recursive": true})).await;

    let kind = |rel: &str| entry(&result, rel)["kind"].as_str().unwrap().to_string();
    assert_eq!(kind("a.md"), "prose");
    assert_eq!(kind("lib.rs"), "code");
    assert_eq!(kind("Cargo.toml"), "code");
    assert_eq!(kind("Attachments/doc.pdf"), "document");
    assert_eq!(kind("Attachments/logo.png"), "binary");
    assert_eq!(kind("zero.md"), "prose");
    assert_eq!(entry(&result, "zero")["size"], 0);
    assert_eq!(entry(&result, "zero.md")["size"], 0);
    assert_eq!(kind("nul.py"), "binary", "a NUL byte makes it binary");
    // Over the 20 MiB cap: indexed by name only, but its real kind and
    // size are kept.
    assert_eq!(kind("big.md"), "prose");
    assert_eq!(
        entry(&result, "big.md")["size"],
        (tephrad::file_kind::MAX_INDEXED_BYTES + 1) as i64
    );
    // Every entry has one of the six kinds.
    for e in result["entries"].as_array().unwrap() {
        let k = e["kind"].as_str().unwrap();
        assert!(
            [
                "prose",
                "code",
                "document",
                "text-other",
                "binary",
                "folder"
            ]
            .contains(&k),
            "{e}"
        );
        if k != "prose" {
            assert!(e.get("title").is_none(), "only prose carries a title: {e}");
        }
    }
}

#[tokio::test]
async fn listing_changes_nothing_on_disk_or_in_the_cache() {
    use std::collections::BTreeMap;
    fn snapshot(root: &std::path::Path) -> BTreeMap<String, (Vec<u8>, std::time::SystemTime)> {
        let mut out = BTreeMap::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    let mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
                    out.insert(rel, (std::fs::read(&path).unwrap(), mtime));
                }
            }
        }
        out
    }
    fn counts(cache_path: &std::path::Path) -> Vec<i64> {
        let cache = Cache::open(cache_path).unwrap();
        ["units", "links", "tags", "headings", "sources"]
            .iter()
            .map(|t| cache.table_row_count(t).unwrap())
            .collect()
    }

    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;
    let cache_path = daemon.data_path.join("cache.db");

    let disk_before = snapshot(src.root());
    let rows_before = counts(&cache_path);
    for folder in ["", "notes", "src", "Attachments", "empty-folder"] {
        daemon
            .list("proj", json!({"folder": folder, "recursive": true}))
            .await;
        daemon.list("proj", json!({"folder": folder})).await;
    }
    for bad in ["nope", "README.md", "..", "/etc"] {
        let _ = daemon
            .rpc("unit.list", json!({"source": "proj", "folder": bad}))
            .await;
    }

    assert_eq!(
        snapshot(src.root()),
        disk_before,
        "file bytes or mtimes changed"
    );
    assert_eq!(counts(&cache_path), rows_before, "cache row counts changed");
}

#[tokio::test]
async fn a_folder_of_ten_thousand_files_comes_back_in_one_response_quickly() {
    let src = fixtures::empty_source();
    for i in 0..10_000 {
        src.write(&format!("many/file-{i:05}.txt"), "x\n");
    }
    let daemon = Daemon::start().await;
    daemon.register("big", src.root()).await;

    let started = std::time::Instant::now();
    let result = daemon.list("big", json!({"folder": "many"})).await;
    let elapsed = started.elapsed();

    assert_eq!(result["entries"].as_array().unwrap().len(), 10_000);
    assert_eq!(result["entries"][0]["rel_path"], "many/file-00000.txt");
    assert_eq!(result["entries"][9_999]["rel_path"], "many/file-09999.txt");
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "10,000-file listing took {elapsed:?}"
    );
}

#[tokio::test]
async fn unit_list_without_the_token_or_with_a_wrong_one_is_unauthorized() {
    let src = project();
    let daemon = Daemon::start().await;
    daemon.register("proj", src.root()).await;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "unit.list", "params": {"source": "proj"}
    });

    let missing = daemon
        .client
        .post(&daemon.url)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 401);
    let wrong = daemon
        .client
        .post(&daemon.url)
        .bearer_auth("c".repeat(64))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    for resp in [missing, wrong] {
        let text = resp.text().await.unwrap();
        assert!(
            !text.contains("README.md") && !text.contains("entries"),
            "an unauthenticated reply leaked a listing: {text}"
        );
    }
}

/// A restart: a second daemon on the same data dir, started after the
/// source's root has gone, serves the listing from the cache over HTTP.
///
/// A plain `#[test]`, with one tokio runtime per daemon: a
/// `SourceRegistry` owns a runtime of its own, which panics if it is
/// dropped from inside an async context, and a keep-alive connection
/// task keeps the registry alive after the server task is aborted. So
/// the first daemon is stopped by dropping its whole runtime outside
/// `block_on`, which ends every task it spawned, and only then is the
/// root renamed and the cache reopened.
#[test]
fn a_restarted_daemon_lists_an_offline_source_from_the_cache_over_http() {
    let src = project();
    let root = src.root().to_path_buf();
    let data_dir = tempfile::tempdir().unwrap();

    let first_rt = new_runtime();
    let first = first_rt.block_on(async {
        let first = Daemon::start_in(data_dir.path()).await;
        first.register("proj", &root).await;
        let online = first.list("proj", json!({})).await;
        assert_eq!(online["offline"], false);
        first
    });
    let first_registry = Arc::downgrade(&first.registry);
    drop(first);
    drop(first_rt);
    assert_eq!(
        first_registry.strong_count(),
        0,
        "the first daemon is still alive: a task of its runtime outlived the runtime"
    );

    let gone = root.with_extension("away");
    std::fs::rename(&root, &gone).unwrap();
    let second_rt = new_runtime();
    let (second, result, nested, (code, message)) = second_rt.block_on(async {
        let second = Daemon::start_in(data_dir.path()).await;
        let result = second.list("proj", json!({})).await;
        let nested = second
            .list("proj", json!({"folder": "src", "recursive": true}))
            .await;
        let err = second
            .list_error(json!({"source": "proj", "folder": "nope"}))
            .await;
        (second, result, nested, err)
    });
    // Stopped the same way, outside any async context.
    drop(second);
    drop(second_rt);
    std::fs::rename(&gone, &root).unwrap();

    assert_eq!(result["offline"], true);
    assert_eq!(
        paths(&result),
        ["Attachments", "notes", "src", "README.md"],
        "the cache knows no empty folders"
    );
    assert!(result["entries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["kind"] != "folder" || e["mtime"].is_null()));
    assert_eq!(nested["offline"], true);
    assert_eq!(
        paths(&nested),
        ["src/rpc", "src/rpc/server.rs", "src/main.rs"]
    );
    assert_eq!(code, -32011, "SOURCE_ERROR");
    assert!(message.contains("no such folder: nope"), "{message}");
}
