//! `block.ensure_id` (G11, G12) end to end: real temp-dir sources scanned
//! through the real `SourceRegistry`, the real RPC server on an ephemeral
//! loopback port (never the product's port) with a throwaway token, the
//! real cache, and the real `BlockIdEngine` writing real files.
//!
//! Every refusal is checked to have written nothing (the file's bytes
//! and mtime are read back), and every write to have changed the file by
//! exactly one ` ^id` insertion.

mod fixtures;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::block_id::BlockIdEngine;
use tephrad::refactor::{AtomicFs, RealFs, WatcherPauseControl};
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    BlockBackend, GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;

struct Daemon {
    registry: Arc<SourceRegistry>,
    client: reqwest::Client,
    url: String,
    token: String,
    server: tokio::task::JoinHandle<()>,
    _data_dir: tempfile::TempDir,
}

impl Daemon {
    async fn start() -> Daemon {
        let data_dir = tempfile::tempdir().expect("data dir");
        let cache_path = data_dir.path().join("cache.db");
        let token_path = data_dir.path().join("token");
        let token = "d".repeat(64);
        std::fs::write(&token_path, &token).expect("write token");

        let registry = Arc::new(
            SourceRegistry::new(Cache::open(&cache_path).expect("open cache")).expect("registry"),
        );
        let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
        let shared_graph = Arc::new(SharedGraph::new(
            UnitGraph::build_from_cache(&graph_cache).expect("graph"),
        ));
        let graph_refresh = GraphRefresh::new(shared_graph.clone(), cache_path.clone());
        // No live-watcher settle wait: the cache is updated before the
        // verb returns, which is what these tests read back.
        let engine = Arc::new(BlockIdEngine::new(registry.clone()).with_settle(Duration::ZERO));

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let state = Arc::new(
            AppState::new(
                token_path,
                shared_graph.clone() as Arc<dyn GraphBackend>,
                registry.clone() as Arc<dyn SourceBackend>,
                Arc::new(tephrad::refactor::RefactorEngine::new(
                    shared_graph as Arc<dyn GraphBackend>,
                    registry.clone() as Arc<dyn SourceBackend>,
                    registry.clone() as Arc<dyn tephrad::refactor::WatcherPauseControl>,
                )) as Arc<dyn RefactorBackend>,
                Some(graph_refresh),
            )
            .with_blocks(engine.clone() as Arc<dyn BlockBackend>),
        );
        let router = build_router(state);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        Daemon {
            registry,
            client: reqwest::Client::new(),
            url: format!("http://{addr}/rpc"),
            token,
            server,
            _data_dir: data_dir,
        }
    }

    async fn rpc(&self, method: &str, params: Value) -> Value {
        self.client
            .post(&self.url)
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .expect("send")
            .json::<Value>()
            .await
            .expect("json")
    }

    async fn register(&self, name: &str, root: &Path) {
        let reply = self
            .rpc(
                "source.register",
                json!({"name": name, "root": root.to_string_lossy()}),
            )
            .await;
        assert!(reply.get("error").is_none(), "register failed: {reply}");
    }

    async fn ensure(&self, unit: &str, line: u64) -> Value {
        self.rpc("block.ensure_id", json!({"unit": unit, "line": line}))
            .await
    }

    /// A successful `block.ensure_id`'s result.
    async fn ensure_ok(&self, unit: &str, line: u64) -> Value {
        let reply = self.ensure(unit, line).await;
        assert!(reply.get("error").is_none(), "{unit}:{line}: {reply}");
        reply["result"].clone()
    }

    /// A refused `block.ensure_id`'s `(code, message)`.
    async fn ensure_err(&self, unit: &str, line: u64) -> (i64, String) {
        let reply = self.ensure(unit, line).await;
        let err = reply
            .get("error")
            .unwrap_or_else(|| panic!("{unit}:{line} was not refused: {reply}"));
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

/// `(bytes, mtime)` of a file: what "nothing was written" means.
fn snapshot(path: &Path) -> (Vec<u8>, std::time::SystemTime) {
    (
        std::fs::read(path).unwrap(),
        std::fs::metadata(path).unwrap().modified().unwrap(),
    )
}

/// A refusal's expected code: a valid request Tephra declines.
const BLOCK_ERROR: i64 = -32013;
const INVALID_PARAMS: i64 = -32602;

fn six_char_id(id: &str) -> bool {
    id.len() == 6
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// `content` with every `^id` anchor this verb wrote removed: the
/// original file, byte for byte.
fn without_id(after: &str, id: &str) -> String {
    after.replacen(&format!(" ^{id}"), "", 1)
}

#[tokio::test]
async fn ensure_id_writes_one_marker_and_returns_the_link_for_every_prose_extension() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    let files = [
        (
            "Tephra test.md",
            "# Title\n\nfirst line\nsecond line\n\nother\n",
            3,
            "Tephra test",
        ),
        ("log.txt", "alpha\nbeta\n\ngamma\n", 1, "log.txt"),
        ("n.org", "* Head\nbody line\n\nnext\n", 2, "n.org"),
        ("n.rst", "Title\n=====\n\nbody text\n", 4, "n.rst"),
        ("n.adoc", "== Title\n\nparagraph\n", 3, "n.adoc"),
        (
            "a/b/Café Notes.markdown",
            "para\n",
            1,
            "a/b/Café Notes.markdown",
        ),
    ];
    for (path, content, _, _) in &files {
        src.write(path, content);
    }
    daemon.register("notes", src.root()).await;

    for (path, original, line, shown) in files {
        let unit = format!("@notes:{path}");
        let result = daemon.ensure_ok(&unit, line).await;
        let id = result["id"].as_str().unwrap().to_string();
        assert!(six_char_id(&id), "{path}: {id}");
        assert_eq!(result["created"], json!(true), "{path}");
        assert_eq!(
            result["link"],
            json!(format!("[[@notes:{shown}#^{id}]]")),
            "{path}"
        );
        let after = std::fs::read_to_string(src.root().join(path)).unwrap();
        assert_eq!(
            without_id(&after, &id),
            original,
            "{path}: exactly one insertion"
        );
        assert_eq!(after.matches(&format!("^{id}")).count(), 1, "{path}");

        // Idempotent: asking again for any line of that block returns
        // the same ID, writes nothing.
        let (bytes, mtime) = snapshot(&src.root().join(path));
        let again = daemon.ensure_ok(&unit, line).await;
        assert_eq!(again["id"], json!(id), "{path}");
        assert_eq!(again["created"], json!(false), "{path}");
        assert_eq!(again["link"], result["link"], "{path}");
        assert_eq!((bytes, mtime), snapshot(&src.root().join(path)), "{path}");
    }
}

#[tokio::test]
async fn the_cache_knows_the_new_id_before_the_call_returns() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "first paragraph\n\nthe second paragraph here\n");
    daemon.register("notes", src.root()).await;

    let result = daemon.ensure_ok("@notes:n.md", 3).await;
    let id = result["id"].as_str().unwrap();

    // Straight away, with no wait for any watcher: a block search finds
    // that paragraph carrying the new ID.
    let reply = daemon
        .rpc(
            "unit.search",
            json!({"query": "second paragraph", "scope": "all_blocks", "limit": 10}),
        )
        .await;
    let hits = reply["result"]
        .as_array()
        .unwrap_or_else(|| panic!("{reply}"));
    assert!(
        hits.iter()
            .any(|h| h["unit"] == "@notes:n.md" && h["block_id"] == json!(id)),
        "no hit carrying ^{id}: {hits:#?}"
    );
}

#[tokio::test]
async fn a_second_block_in_the_same_file_gets_a_different_id_and_keeps_the_first() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "one\n\ntwo\n\n- three\n- four\n");
    daemon.register("notes", src.root()).await;

    let mut ids = Vec::new();
    for line in [1, 3, 5, 6] {
        let r = daemon.ensure_ok("@notes:n.md", line).await;
        assert_eq!(r["created"], json!(true), "line {line}");
        ids.push(r["id"].as_str().unwrap().to_string());
    }
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 4, "{ids:?}");
    let after = std::fs::read_to_string(src.root().join("n.md")).unwrap();
    assert_eq!(
        after,
        format!(
            "one ^{}\n\ntwo ^{}\n\n- three ^{}\n- four ^{}\n",
            ids[0], ids[1], ids[2], ids[3]
        )
    );
}

#[tokio::test]
async fn an_existing_id_is_returned_as_typed_and_nothing_is_written() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "intro\n\nthe point ^Summary-1\n");
    daemon.register("notes", src.root()).await;
    let before = snapshot(&src.root().join("n.md"));

    let r = daemon.ensure_ok("@notes:n.md", 3).await;
    assert_eq!(r["id"], json!("Summary-1"));
    assert_eq!(r["created"], json!(false));
    assert_eq!(r["link"], json!("[[@notes:n#^Summary-1]]"));
    assert_eq!(before, snapshot(&src.root().join("n.md")));
}

#[tokio::test]
async fn a_heading_is_linked_by_its_text_and_never_written() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "## My Heading\n\ntext\n");
    src.write("n.rst", "Rst Title\n=========\n\ntext\n");
    daemon.register("notes", src.root()).await;
    let before_md = snapshot(&src.root().join("n.md"));
    let before_rst = snapshot(&src.root().join("n.rst"));

    let r = daemon.ensure_ok("@notes:n.md", 1).await;
    assert_eq!(r["id"], Value::Null);
    assert_eq!(r["created"], json!(false));
    assert_eq!(r["link"], json!("[[@notes:n#My Heading]]"));
    let r = daemon.ensure_ok("@notes:n.rst", 2).await;
    assert_eq!(r["link"], json!("[[@notes:n.rst#Rst Title]]"));

    assert_eq!(before_md, snapshot(&src.root().join("n.md")));
    assert_eq!(before_rst, snapshot(&src.root().join("n.rst")));
}

#[tokio::test]
async fn lines_that_cannot_carry_an_id_are_refused_and_nothing_is_written() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    let md = "---\ntitle: T\n---\n\n```rust\nlet x = 1;\n\nlet y = 2;\n```\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n    indented\n\nok\n";
    src.write("n.md", md);
    src.write("n.org", "* H\n\n#+BEGIN_SRC rust\ncode\n#+END_SRC\n");
    daemon.register("notes", src.root()).await;
    let before_md = snapshot(&src.root().join("n.md"));
    let before_org = snapshot(&src.root().join("n.org"));

    for (line, word) in [
        (1, "frontmatter"),
        (3, "frontmatter"),
        (4, "blank"),
        (5, "code"),
        (7, "code"),
        (9, "code"),
        (11, "table"),
        (13, "table"),
        (15, "code"),
        (99, "out of range"),
    ] {
        let (code, msg) = daemon.ensure_err("@notes:n.md", line).await;
        assert_eq!(code, BLOCK_ERROR, "line {line}: {msg}");
        assert!(msg.contains(word), "line {line}: {msg}");
    }
    let (code, msg) = daemon.ensure_err("@notes:n.org", 4).await;
    assert_eq!(code, BLOCK_ERROR);
    assert!(msg.contains("code"), "{msg}");

    assert_eq!(before_md, snapshot(&src.root().join("n.md")));
    assert_eq!(before_org, snapshot(&src.root().join("n.org")));
}

#[tokio::test]
async fn malformed_requests_are_invalid_params() {
    let daemon = Daemon::start().await;
    for params in [
        json!({}),
        json!({"unit": "@notes:n.md"}),
        json!({"line": 1}),
        json!({"unit": "@notes:n.md", "line": 0}),
        json!({"unit": "@notes:n.md", "line": -1}),
        json!({"unit": "@notes:n.md", "line": 1.5}),
        json!({"unit": "@notes:n.md", "line": "3"}),
        json!({"unit": "", "line": 1}),
        json!({"unit": "no-at-sign.md", "line": 1}),
        json!({"unit": "@notes", "line": 1}),
    ] {
        let reply = daemon.rpc("block.ensure_id", params.clone()).await;
        assert_eq!(
            reply["error"]["code"],
            json!(INVALID_PARAMS),
            "{params}: {reply}"
        );
    }
}

#[tokio::test]
async fn files_that_are_not_prose_are_refused_by_kind() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("data.json", "{\"a\": 1}\n");
    src.write_bytes("pic.png", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3, 0, 0, 0]);
    src.write_bytes("doc.pdf", b"%PDF-1.4\n%%EOF\n");
    daemon.register("notes", src.root()).await;

    for (path, word) in [
        ("data.json", "text file of another type"),
        ("pic.png", "binary"),
        ("doc.pdf", "document"),
    ] {
        let before = snapshot(&src.root().join(path));
        let (code, msg) = daemon.ensure_err(&format!("@notes:{path}"), 1).await;
        assert_eq!(code, BLOCK_ERROR, "{path}: {msg}");
        assert!(msg.contains(word), "{path}: {msg}");
        assert_eq!(before, snapshot(&src.root().join(path)), "{path}");
    }
}

#[tokio::test]
async fn unknown_sources_and_units_are_refused() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "text\n");
    daemon.register("notes", src.root()).await;

    let (code, msg) = daemon.ensure_err("@nope:n.md", 1).await;
    assert_eq!(code, BLOCK_ERROR);
    assert!(msg.contains("no such unit"), "{msg}");
    let (code, msg) = daemon.ensure_err("@notes:missing.md", 1).await;
    assert_eq!(code, BLOCK_ERROR);
    assert!(msg.contains("no such unit"), "{msg}");
    // A path that climbs out of the source is never a unit.
    let (code, _) = daemon.ensure_err("@notes:../escape.md", 1).await;
    assert_eq!(code, BLOCK_ERROR);
}

#[tokio::test]
async fn a_path_a_wikilink_cannot_hold_is_refused_before_any_write() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("C#/notes.md", "text\n");
    src.write("a|b.md", "text\n");
    daemon.register("notes", src.root()).await;

    for path in ["C#/notes.md", "a|b.md"] {
        let before = snapshot(&src.root().join(path));
        let (code, msg) = daemon.ensure_err(&format!("@notes:{path}"), 1).await;
        assert_eq!(code, BLOCK_ERROR, "{path}: {msg}");
        assert!(msg.contains("wikilink"), "{path}: {msg}");
        assert_eq!(before, snapshot(&src.root().join(path)), "{path}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_file_is_refused_and_the_link_survives() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("real.md"), "text\n").unwrap();
    src.write("n.md", "text\n");
    daemon.register("notes", src.root()).await;

    // Replace the indexed file with a symlink to a file elsewhere.
    std::fs::remove_file(src.root().join("n.md")).unwrap();
    std::os::unix::fs::symlink(outside.path().join("real.md"), src.root().join("n.md")).unwrap();

    let (code, msg) = daemon.ensure_err("@notes:n.md", 1).await;
    assert_eq!(code, BLOCK_ERROR);
    assert!(msg.contains("symlink"), "{msg}");
    assert!(std::fs::symlink_metadata(src.root().join("n.md"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read_to_string(outside.path().join("real.md")).unwrap(),
        "text\n"
    );
}

#[tokio::test]
async fn a_file_edited_since_the_scan_is_refused_as_changed_and_not_overwritten() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "original\n");
    daemon.register("notes", src.root()).await;

    // The user edits the file after Tephra indexed it, without a rescan.
    // The watcher is paused, so it cannot reindex the edit before the
    // call: the cache is stale however long the request takes to arrive.
    daemon.registry.pause_all_watchers();
    src.write("n.md", "original\nthen the user typed a lot more here\n");
    let before = snapshot(&src.root().join("n.md"));

    let (code, msg) = daemon.ensure_err("@notes:n.md", 1).await;
    assert_eq!(code, BLOCK_ERROR);
    assert!(msg.contains("file changed, try again"), "{msg}");
    assert_eq!(before, snapshot(&src.root().join("n.md")));
    daemon.registry.resume_all_watchers();

    // After a rescan the same request works, on the user's new text.
    daemon.registry.rescan("notes").unwrap();
    let r = daemon.ensure_ok("@notes:n.md", 1).await;
    let id = r["id"].as_str().unwrap();
    assert_eq!(
        std::fs::read_to_string(src.root().join("n.md")).unwrap(),
        format!("original\nthen the user typed a lot more here ^{id}\n")
    );
}

#[tokio::test]
async fn crlf_bom_and_unicode_files_keep_every_other_byte() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    let crlf = "one\r\ntwo\r\n\r\nthree\r\n";
    let bom = "\u{FEFF}héllo wörld 🎉\n\nnext\n";
    let hard_break = "line  \nbreak\n";
    src.write("crlf.md", crlf);
    src.write("bom.md", bom);
    src.write("hard.md", hard_break);
    src.write("noeol.md", "last line without a newline");
    daemon.register("notes", src.root()).await;

    for (path, original, line) in [
        ("crlf.md", crlf, 2),
        ("bom.md", bom, 1),
        ("hard.md", hard_break, 1),
        ("noeol.md", "last line without a newline", 1),
    ] {
        let r = daemon.ensure_ok(&format!("@notes:{path}"), line).await;
        let id = r["id"].as_str().unwrap();
        let after = std::fs::read(src.root().join(path)).unwrap();
        let marker = format!(" ^{id}").into_bytes();
        let at = after
            .windows(marker.len())
            .position(|w| w == marker.as_slice())
            .unwrap_or_else(|| panic!("{path}: no marker"));
        let mut restored = after.clone();
        restored.drain(at..at + marker.len());
        assert_eq!(restored, original.as_bytes(), "{path}");
        assert_eq!(after.len(), original.len() + marker.len(), "{path}");
    }
    // The CRLF file's marker went before the \r\n, not after it.
    let after = std::fs::read_to_string(src.root().join("crlf.md")).unwrap();
    assert!(after.starts_with("one\r\ntwo ^"), "{after:?}");
    assert!(after.contains("\r\n\r\nthree\r\n"), "{after:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn file_permissions_survive_the_rewrite() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "text\n");
    std::fs::set_permissions(
        src.root().join("n.md"),
        std::fs::Permissions::from_mode(0o640),
    )
    .unwrap();
    daemon.register("notes", src.root()).await;

    daemon.ensure_ok("@notes:n.md", 1).await;
    let mode = std::fs::metadata(src.root().join("n.md"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o640);
    // And no temp file is left beside it.
    let names: Vec<_> = std::fs::read_dir(src.root())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["n.md".to_string()], "{names:?}");
}

#[tokio::test]
async fn a_utf16_markdown_file_is_refused_not_mangled() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    let mut bytes = vec![0xFF, 0xFE];
    for u in "hello\n".encode_utf16() {
        bytes.extend(u.to_le_bytes());
    }
    src.write_bytes("n.md", &bytes);
    daemon.register("notes", src.root()).await;
    let before = snapshot(&src.root().join("n.md"));

    let (code, _) = daemon.ensure_err("@notes:n.md", 1).await;
    assert_eq!(code, BLOCK_ERROR);
    assert_eq!(before, snapshot(&src.root().join("n.md")));
}

/// Many requests for different blocks of one file at once: every ID
/// lands, none is lost to a racing rewrite, and the IDs are distinct.
#[test]
fn concurrent_ensure_id_calls_on_one_file_lose_no_write() {
    let data_dir = tempfile::tempdir().unwrap();
    let cache_path = data_dir.path().join("cache.db");
    let registry =
        Arc::new(SourceRegistry::new(Cache::open(&cache_path).unwrap()).expect("registry"));
    let src = fixtures::empty_source();
    let paragraphs = 12;
    let content: String = (0..paragraphs)
        .map(|i| format!("paragraph {i}\n\n"))
        .collect();
    src.write("n.md", &content);
    registry
        .register("notes", src.root())
        .expect("register source");

    let engine = Arc::new(BlockIdEngine::new(registry.clone()).with_settle(Duration::ZERO));
    let handles: Vec<_> = (0..paragraphs)
        .map(|i| {
            let engine = engine.clone();
            std::thread::spawn(move || engine.ensure_id("@notes:n.md", (i * 2 + 1) as u64))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    let after = std::fs::read_to_string(src.root().join("n.md")).unwrap();
    let mut ids = Vec::new();
    for (i, r) in results.iter().enumerate() {
        let wire = r.as_ref().unwrap_or_else(|e| panic!("request {i}: {e}"));
        ids.push(wire.id.clone().unwrap());
    }
    let unique: std::collections::HashSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), paragraphs, "{ids:?}");
    for id in &ids {
        assert_eq!(
            after.matches(&format!("^{id}")).count(),
            1,
            "{id} in {after}"
        );
    }
    let stripped = ids
        .iter()
        .fold(after.clone(), |acc, id| without_id(&acc, id));
    assert_eq!(stripped, content);
}

// -- offline and permission_denied sources ----------------------------

#[tokio::test]
async fn a_unit_in_an_offline_source_is_refused() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "text\n");
    daemon.register("notes", src.root()).await;
    let root = src.root().to_path_buf();
    let kept = std::fs::read(root.join("n.md")).unwrap();

    // The root vanishes (an unplugged drive): the rescan marks the
    // source offline and keeps its rows.
    let moved = root.with_extension("gone");
    std::fs::rename(&root, &moved).unwrap();
    daemon.registry.rescan("notes").unwrap();

    let (code, msg) = daemon.ensure_err("@notes:n.md", 1).await;
    assert_eq!(code, BLOCK_ERROR);
    assert!(msg.contains("offline"), "{msg}");

    std::fs::rename(&moved, &root).unwrap();
    assert_eq!(std::fs::read(root.join("n.md")).unwrap(), kept);
}

#[cfg(unix)]
#[tokio::test]
async fn a_unit_in_a_permission_denied_source_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    // File modes are ignored for root.
    if std::fs::metadata("/proc/self").is_err() && std::env::var("USER").as_deref() == Ok("root") {
        return;
    }
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "text\n");
    daemon.register("notes", src.root()).await;
    let root = src.root().to_path_buf();

    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o000)).unwrap();
    let restore = || std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755));
    let rescanned = daemon.registry.rescan("notes");
    let reply = daemon.ensure("@notes:n.md", 1).await;
    restore().unwrap();
    rescanned.unwrap();

    let err = reply
        .get("error")
        .unwrap_or_else(|| panic!("not refused: {reply}"));
    assert_eq!(err["code"], json!(BLOCK_ERROR));
    let msg = err["message"].as_str().unwrap();
    assert!(msg.contains("permission denied"), "{msg}");
    assert_eq!(
        std::fs::read_to_string(root.join("n.md")).unwrap(),
        "text\n"
    );
}

// -- real RPC auth ----------------------------------------------------

#[tokio::test]
async fn calls_without_or_with_a_wrong_token_are_401_and_the_file_is_untouched() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "text\n");
    daemon.register("notes", src.root()).await;
    let before = snapshot(&src.root().join("n.md"));
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "block.ensure_id",
                      "params": {"unit": "@notes:n.md", "line": 1}});

    let none = daemon
        .client
        .post(&daemon.url)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(none.status(), reqwest::StatusCode::UNAUTHORIZED);
    for wrong in ["wrong", &format!("{}x", daemon.token)] {
        let r = daemon
            .client
            .post(&daemon.url)
            .bearer_auth(wrong)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), reqwest::StatusCode::UNAUTHORIZED, "{wrong}");
    }
    assert_eq!(before, snapshot(&src.root().join("n.md")));
}

// -- the filesystem and the watcher ----------------------------------

/// An [`AtomicFs`] that records every call and then does the real thing.
#[derive(Default)]
struct RecordingFs {
    calls: Mutex<Vec<String>>,
}

impl RecordingFs {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn count_prefix(&self, prefix: &str) -> usize {
        self.calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }
}

impl AtomicFs for RecordingFs {
    fn write_new(&self, path: &Path, content: &[u8]) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("write_new {}", path.display()));
        RealFs.write_new(path, content)
    }
    fn set_permissions(&self, path: &Path, perms: std::fs::Permissions) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("set_permissions {}", path.display()));
        RealFs.set_permissions(path, perms)
    }
    fn sync(&self, path: &Path) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("sync {}", path.display()));
        RealFs.sync(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("rename {}", to.display()));
        RealFs.rename(from, to)
    }
}

/// A [`WatcherPauseControl`] that records pause and resume, with the
/// paths the resume was told to forget.
#[derive(Default)]
struct RecordingWatcher {
    log: Mutex<Vec<String>>,
}

impl RecordingWatcher {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl WatcherPauseControl for RecordingWatcher {
    fn pause_all(&self) {
        self.log.lock().unwrap().push("pause".to_string());
    }
    fn resume_all(&self, written: &HashSet<PathBuf>) {
        let mut paths: Vec<String> = written.iter().map(|p| p.display().to_string()).collect();
        paths.sort();
        self.log
            .lock()
            .unwrap()
            .push(format!("resume:{}", paths.join(",")));
    }
}

struct Rig {
    registry: Arc<SourceRegistry>,
    fs: Arc<RecordingFs>,
    watcher: Arc<RecordingWatcher>,
    engine: Arc<BlockIdEngine>,
    src: fixtures::TempSource,
    _data: tempfile::TempDir,
}

/// A real registry and cache over a real temp source, an engine whose
/// filesystem and watcher are the recorders above.
fn rig(files: &[(&str, &[u8])]) -> Rig {
    let data = tempfile::tempdir().unwrap();
    let registry =
        Arc::new(SourceRegistry::new(Cache::open(data.path().join("cache.db")).unwrap()).unwrap());
    let src = fixtures::empty_source();
    for (path, bytes) in files {
        src.write_bytes(path, bytes);
    }
    registry.register("notes", src.root()).unwrap();
    let fs = Arc::new(RecordingFs::default());
    let watcher = Arc::new(RecordingWatcher::default());
    let engine = Arc::new(
        BlockIdEngine::with_parts(registry.clone(), fs.clone(), watcher.clone())
            .with_settle(Duration::ZERO),
    );
    Rig {
        registry,
        fs,
        watcher,
        engine,
        src,
        _data: data,
    }
}

fn temp_files_in(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in walk(dir) {
        let name = entry.file_name().unwrap().to_string_lossy().into_owned();
        if name.contains(".tephra-tmp-") {
            found.push(name);
        }
    }
    found
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(walk(&path));
        } else {
            out.push(path);
        }
    }
    out
}

#[test]
fn a_write_goes_through_the_atomic_fs_once_and_the_watcher_is_told_to_forget_exactly_that_path() {
    let rig = rig(&[("Tephra test.md", b"# T\n\nfirst line\nsecond\n")]);
    // The engine works on canonical paths (macOS tempdirs sit behind
    // the /var -> /private/var link).
    let abs = std::fs::canonicalize(rig.src.root().join("Tephra test.md")).unwrap();
    let wire = rig.engine.ensure_id("@notes:Tephra test.md", 3).unwrap();
    assert!(wire.created);

    // Temp file, permissions, fsync, then one rename onto the target.
    let calls = rig.fs.calls();
    assert_eq!(rig.fs.count_prefix("write_new"), 1, "{calls:?}");
    assert_eq!(rig.fs.count_prefix("sync"), 1, "{calls:?}");
    assert_eq!(
        calls
            .iter()
            .filter(|c| **c == format!("rename {}", abs.display()))
            .count(),
        1,
        "{calls:?}"
    );
    assert!(
        calls[0].contains(".tephra-tmp-"),
        "the content goes to a temp file first: {calls:?}"
    );

    // Paused for the write, resumed telling the watcher this very path.
    assert_eq!(
        rig.watcher.log(),
        vec!["pause".to_string(), format!("resume:{}", abs.display())]
    );
    assert!(temp_files_in(rig.src.root()).is_empty());
}

#[test]
fn nothing_is_opened_for_write_and_the_watcher_is_not_touched_for_anything_refused_or_unchanged() {
    let docx = std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../tephra-parser/tests/fixtures/documents/headings.docx"),
    )
    .unwrap();
    let rig = rig(&[
        ("doc.docx", &docx),
        ("pic.png", &[0x89, b'P', b'N', b'G', 0, 1, 2, 3, 0, 0]),
        ("data.json", b"{\"a\": 1}\n"),
        ("code.rs", b"const A: u8 = 1;\n\nfn main() {}\n"),
        (
            "n.md",
            b"---\nk: v\n---\n\n```\ncode\n```\n\n| a |\n|---|\n| 1 |\n\nreal ^kept\n# Head\n",
        ),
        ("big.txt", b"hello\n"),
    ]);
    let all_before: Vec<(PathBuf, Vec<u8>)> = walk(rig.src.root())
        .into_iter()
        .map(|p| {
            let b = std::fs::read(&p).unwrap();
            (p, b)
        })
        .collect();

    let attempts: &[(&str, u64)] = &[
        ("@notes:doc.docx", 1),
        ("@notes:pic.png", 1),
        ("@notes:data.json", 1),
        ("@notes:code.rs", 2), // no symbol on a blank line
        ("@notes:n.md", 1),    // frontmatter
        ("@notes:n.md", 4),    // blank
        ("@notes:n.md", 6),    // code
        ("@notes:n.md", 10),   // table
        ("@notes:n.md", 13),   // existing id
        ("@notes:n.md", 14),   // heading
        ("@notes:n.md", 99),
        ("@notes:n.md", 0),
        ("@notes:missing.md", 1),
        ("@nope:n.md", 1),
        ("bad", 1),
    ];
    for (unit, line) in attempts {
        let _ = rig.engine.ensure_id(unit, *line);
    }

    assert_eq!(rig.fs.calls(), Vec::<String>::new(), "AtomicFs was used");
    assert_eq!(rig.watcher.log(), Vec::<String>::new(), "watcher touched");
    let all_after: Vec<(PathBuf, Vec<u8>)> = walk(rig.src.root())
        .into_iter()
        .map(|p| {
            let b = std::fs::read(&p).unwrap();
            (p, b)
        })
        .collect();
    assert_eq!(all_before, all_after);
    assert!(temp_files_in(rig.src.root()).is_empty());
}

#[test]
fn a_failed_write_reports_the_error_and_leaves_the_file_and_no_temp_file() {
    struct FailingRename(RecordingFs);
    impl AtomicFs for FailingRename {
        fn write_new(&self, p: &Path, c: &[u8]) -> std::io::Result<()> {
            self.0.write_new(p, c)
        }
        fn set_permissions(&self, p: &Path, m: std::fs::Permissions) -> std::io::Result<()> {
            self.0.set_permissions(p, m)
        }
        fn sync(&self, p: &Path) -> std::io::Result<()> {
            self.0.sync(p)
        }
        fn rename(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::Error::other("disk on fire"))
        }
    }
    let rig = rig(&[("n.md", b"text\n")]);
    let engine = BlockIdEngine::with_parts(
        rig.registry.clone(),
        Arc::new(FailingRename(RecordingFs::default())),
        rig.watcher.clone(),
    )
    .with_settle(Duration::ZERO);
    let before = snapshot(&rig.src.root().join("n.md"));

    let err = engine.ensure_id("@notes:n.md", 1).unwrap_err();
    assert!(err.to_string().contains("disk on fire"), "{err}");
    assert_eq!(before, snapshot(&rig.src.root().join("n.md")));
    assert!(temp_files_in(rig.src.root()).is_empty());
    // The watcher was paused and is resumed (never left paused), with
    // nothing to forget because nothing was written.
    assert_eq!(
        rig.watcher.log(),
        vec!["pause".to_string(), "resume:".to_string()]
    );
}

// -- concurrency --------------------------------------------------------

#[test]
fn concurrent_calls_on_one_paragraph_return_one_id_and_make_one_write() {
    let rig = rig(&[("n.md", b"the paragraph\nspans two lines\n\nother\n")]);
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let engine = rig.engine.clone();
            // Both lines of the same paragraph, alternating.
            std::thread::spawn(move || engine.ensure_id("@notes:n.md", 1 + (i % 2)))
        })
        .collect();
    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().unwrap().expect("every call succeeds"))
        .collect();

    let id = results[0].id.clone().unwrap();
    assert!(results.iter().all(|r| r.id.as_deref() == Some(id.as_str())));
    assert_eq!(results.iter().filter(|r| r.created).count(), 1);
    assert_eq!(
        std::fs::read_to_string(rig.src.root().join("n.md")).unwrap(),
        format!("the paragraph\nspans two lines ^{id}\n\nother\n")
    );
    assert_eq!(rig.fs.count_prefix("rename"), 1, "{:?}", rig.fs.calls());
}

/// `block.ensure_id` racing `link.apply_refactor` on the same file:
/// whichever way each round falls, neither silently overwrites the
/// other. Every round ends with the file equal to the original plus
/// exactly the changes whose calls reported success.
#[tokio::test]
async fn ensure_id_racing_apply_refactor_on_one_file_never_loses_either_change() {
    const ROUNDS: usize = 12;
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    for i in 0..ROUNDS {
        src.write(&format!("Old{i}.md"), "target\n");
        src.write(
            &format!("j{i}.md"),
            &format!("See [[Old{i}]] here.\nand more\n\nSecond paragraph.\n"),
        );
    }
    daemon.register("notes", src.root()).await;

    let (mut ensured, mut applied, mut both) = (0, 0, 0);
    for i in 0..ROUNDS {
        let original = format!("See [[Old{i}]] here.\nand more\n\nSecond paragraph.\n");
        let preview = daemon
            .rpc(
                "link.preview_refactor",
                json!({"source": "notes", "old_path": format!("Old{i}.md"),
                       "new_path": format!("New{i}.md")}),
            )
            .await;
        assert!(preview.get("error").is_none(), "{preview}");

        let unit = format!("@notes:j{i}.md");
        let (apply, ensure) = tokio::join!(
            daemon.rpc(
                "link.apply_refactor",
                json!({"source": "notes", "old_path": format!("Old{i}.md"),
                       "new_path": format!("New{i}.md")}),
            ),
            daemon.ensure(&unit, 1),
        );

        let after = std::fs::read_to_string(src.root().join(format!("j{i}.md"))).unwrap();
        let ensure_ok = ensure.get("error").is_none();
        let apply_ok = apply["result"]["status"] == json!("applied");
        let mut expected = original.clone();
        if ensure_ok {
            let id = ensure["result"]["id"].as_str().unwrap();
            assert!(
                after.contains(&format!(" ^{id}")),
                "round {i}: ^{id} lost: {after:?}"
            );
            expected = expected.replacen("and more", &format!("and more ^{id}"), 1);
            ensured += 1;
        } else {
            let msg = ensure["error"]["message"].as_str().unwrap();
            assert!(msg.contains("file changed"), "round {i}: {msg}");
        }
        if apply_ok {
            assert!(
                after.contains(&format!("[[New{i}]]")) && !after.contains(&format!("[[Old{i}]]")),
                "round {i}: link change lost: {after:?}"
            );
            expected = expected.replace(&format!("[[Old{i}]]"), &format!("[[New{i}]]"));
            applied += 1;
        }
        if ensure_ok && apply_ok {
            both += 1;
        }
        assert_eq!(after, expected, "round {i}: apply {apply}, ensure {ensure}");
    }
    eprintln!("race rounds: {ROUNDS}, ensure ok {ensured}, apply ok {applied}, both {both}");
}

/// `link.apply_refactor` and `block.ensure_id` share one watcher pause:
/// an `ensure_id` finishing while another writer still holds the pause
/// must not resume the watchers under it. Pausing and resuming go
/// through the registry's `WatcherPauseControl`, as `apply_refactor`
/// does, and the state is read from the real watcher.
#[tokio::test]
async fn ensure_id_does_not_resume_watchers_another_writer_still_holds() {
    let daemon = Daemon::start().await;
    let src = fixtures::empty_source();
    src.write("n.md", "para\n");
    daemon.register("notes", src.root()).await;
    let control = daemon.registry.clone() as Arc<dyn WatcherPauseControl>;
    assert!(!daemon.registry.watchers_paused());

    // Another writer (an `apply_refactor`) is mid-write.
    control.pause_all();
    let r = daemon.ensure_ok("@notes:n.md", 1).await;
    assert_eq!(r["created"], json!(true));
    assert!(
        daemon.registry.watchers_paused(),
        "ensure_id resumed the watchers while another writer was still writing"
    );

    // The other writer finishes last, so it is the one that resumes.
    control.resume_all(&HashSet::new());
    assert!(!daemon.registry.watchers_paused());

    // And alone, ensure_id leaves the watchers running.
    src.write_bytes("m.md", b"para\n");
    daemon.registry.rescan("notes").unwrap();
    daemon.ensure_ok("@notes:m.md", 1).await;
    assert!(!daemon.registry.watchers_paused());
}

// -- the on-disk fixtures (tests/fixtures/block_id/) ----------------------

/// A source holding a copy of every file in `tests/fixtures/block_id/`.
fn fixture_source() -> fixtures::TempSource {
    let src = fixtures::empty_source();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/block_id");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap().to_string();
        src.write_bytes(&name, &std::fs::read(&path).unwrap());
    }
    src
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/block_id")
            .join(name),
    )
    .unwrap()
}

#[tokio::test]
async fn the_golden_path_on_the_fixture_adds_one_marker_to_line_3_then_is_idempotent() {
    let daemon = Daemon::start().await;
    let src = fixture_source();
    daemon.register("notes", src.root()).await;
    let path = src.root().join("Tephra test.md");
    let original = fixture("Tephra test.md");

    let r = daemon.ensure_ok("@notes:Tephra test.md", 3).await;
    let id = r["id"].as_str().unwrap();
    assert!(six_char_id(id));
    assert_eq!(r["link"], json!(format!("[[@notes:Tephra test#^{id}]]")));
    let after = std::fs::read(&path).unwrap();
    // Line 3 is a one-paragraph line: the marker is at the end of it.
    let mut expected = original.clone();
    let at = original
        .windows(18)
        .position(|w| w == b"Intro paragraph.\n\n")
        .unwrap()
        + 16;
    expected.splice(at..at, format!(" ^{id}").into_bytes());
    assert_eq!(
        String::from_utf8(after.clone()).unwrap().lines().nth(2),
        Some(format!("Intro paragraph. ^{id}").as_str())
    );
    assert_eq!(after, expected);

    let before = snapshot(&path);
    let again = daemon.ensure_ok("@notes:Tephra test.md", 3).await;
    assert_eq!(again["link"], r["link"]);
    assert_eq!(again["created"], json!(false));
    assert_eq!(before, snapshot(&path));

    // The fixture's two-line paragraph (lines 5-6) takes its marker at
    // the end of line 6, whichever of the two lines is asked about.
    let multi = daemon.ensure_ok("@notes:Tephra test.md", 5).await;
    let multi_id = multi["id"].as_str().unwrap();
    assert_eq!(multi["created"], json!(true));
    let text = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        text.lines().nth(5),
        Some(format!("two lines. ^{multi_id}").as_str())
    );
    assert_eq!(text.lines().nth(4), Some("This paragraph runs over"));
    let multi_again = daemon.ensure_ok("@notes:Tephra test.md", 6).await;
    assert_eq!(multi_again["link"], multi["link"]);
    assert_eq!(multi_again["created"], json!(false));
}

#[tokio::test]
async fn every_fixture_prose_file_gets_exactly_one_inserted_marker() {
    let daemon = Daemon::start().await;
    let src = fixture_source();
    daemon.register("notes", src.root()).await;

    for (name, line) in [
        ("log.txt", 2),
        ("notes.org", 3),
        ("notes.rst", 4),
        ("notes.adoc", 4),
        ("crlf.md", 1),
        ("no-eol.md", 3),
        ("unicode.md", 1),
        ("unicode.md", 3),
        ("structure.md", 18),
    ] {
        let original = std::fs::read(src.root().join(name)).unwrap();
        let r = daemon.ensure_ok(&format!("@notes:{name}"), line).await;
        let id = r["id"].as_str().unwrap_or_else(|| panic!("{name}: {r}"));
        let after = std::fs::read(src.root().join(name)).unwrap();
        let marker = format!(" ^{id}").into_bytes();
        assert_eq!(after.len(), original.len() + marker.len(), "{name}:{line}");
        let at = (0..=original.len())
            .find(|&i| {
                let mut t = original.clone();
                t.splice(i..i, marker.clone());
                t == after
            })
            .unwrap_or_else(|| panic!("{name}:{line}: not a single insertion"));
        // The marker sits at the end of a line: before \r?\n or at EOF.
        let rest = &after[at + marker.len()..];
        assert!(
            rest.is_empty() || rest.starts_with(b"\n") || rest.starts_with(b"\r\n"),
            "{name}:{line}: {:?}",
            String::from_utf8_lossy(rest)
        );
    }
    // The hard-break line keeps its two trailing spaces; the ID follows them.
    let hard = std::fs::read_to_string(src.root().join("unicode.md")).unwrap();
    assert!(hard.contains("a hard break   ^"), "{hard:?}");
}

#[tokio::test]
async fn typed_ids_are_returned_as_they_are_and_never_replaced() {
    let daemon = Daemon::start().await;
    let src = fixture_source();
    daemon.register("notes", src.root()).await;
    let before = snapshot(&src.root().join("summary.md"));

    let r = daemon.ensure_ok("@notes:summary.md", 3).await;
    assert_eq!(r["id"], json!("summary"));
    assert_eq!(r["created"], json!(false));
    assert_eq!(r["link"], json!("[[@notes:summary#^summary]]"));
    let r = daemon.ensure_ok("@notes:summary.md", 5).await;
    assert_eq!(r["id"], json!("my-note-1"));
    assert_eq!(r["link"], json!("[[@notes:summary#^my-note-1]]"));
    // The intro paragraph has none yet; it gets a fresh one that is
    // neither typed ID.
    assert_eq!(before, snapshot(&src.root().join("summary.md")));
    let r = daemon.ensure_ok("@notes:summary.md", 1).await;
    let id = r["id"].as_str().unwrap();
    assert!(six_char_id(id));
    let after = std::fs::read_to_string(src.root().join("summary.md")).unwrap();
    assert!(after.contains("The key point. ^summary\n"), "{after}");
    assert!(after.contains("Other ^my-note-1\n"), "{after}");
}

#[tokio::test]
async fn a_file_with_duplicate_ids_answers_each_occurrence_and_never_adds_a_third() {
    let daemon = Daemon::start().await;
    let src = fixture_source();
    daemon.register("notes", src.root()).await;
    let path = src.root().join("dupes.md");
    let before = snapshot(&path);

    for line in [1, 3] {
        let r = daemon.ensure_ok("@notes:dupes.md", line).await;
        assert_eq!(r["id"], json!("dup01"), "line {line}");
        assert_eq!(r["created"], json!(false), "line {line}");
    }
    assert_eq!(before, snapshot(&path));

    // The unmarked third paragraph gets a new ID, never "dup01" again.
    let r = daemon.ensure_ok("@notes:dupes.md", 5).await;
    let id = r["id"].as_str().unwrap();
    assert_ne!(id, "dup01");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        format!("First ^dup01\n\nSecond ^dup01\n\nThird ^{id}\n")
    );
}

#[tokio::test]
async fn fence_table_frontmatter_and_indented_code_in_the_fixture_are_refused() {
    let daemon = Daemon::start().await;
    let src = fixture_source();
    daemon.register("notes", src.root()).await;
    let path = src.root().join("structure.md");
    let before = snapshot(&path);

    for (line, word) in [
        (2, "frontmatter"),
        (6, "code"),
        (8, "code"),
        (9, "code"),
        (11, "blank"),
        (12, "table"),
        (14, "table"),
        (16, "code"),
    ] {
        let (code, msg) = daemon.ensure_err("@notes:structure.md", line).await;
        assert_eq!(code, BLOCK_ERROR, "line {line}: {msg}");
        assert!(msg.contains(word), "line {line}: {msg}");
    }
    assert_eq!(before, snapshot(&path));
}
