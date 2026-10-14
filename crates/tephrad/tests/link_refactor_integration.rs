//! G7/GP7/GP8/E3 end-to-end: `link.preview_refactor` then
//! `link.apply_refactor` over the real RPC transport, against a real
//! [`tephrad::registry::SourceRegistry`]/[`tephra_graph::UnitGraph`]
//! pair and real on-disk files — spanning two registered sources
//! (`notes`, `atlas-repo`), exactly as the spec's golden path and E3
//! staleness scenario describe. Mirrors `rpc_live_graph.rs`'s own
//! "real components, not `rpc_fixtures`'s mocks" rationale: a mocked
//! `RefactorBackend` can't prove the real engine's file reads/writes,
//! staleness gate, or watcher pause/resume actually happen.

mod fixtures;

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use tephra_cache::Cache;
use tephra_graph::UnitGraph;
use tephrad::refactor::{RefactorEngine, WatcherPauseControl};
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{
    GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
};
use tephrad::rpc::server::{build_router, AppState};
use tokio::net::TcpListener;

fn wait_until(timeout: Duration, mut check: impl FnMut() -> bool) -> bool {
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

/// Minimal test-only percent-decode (mirrors
/// `tephrad::refactor`'s own private helper of the same shape):
/// used by the R12/R13 markdown-destination tests below to assert a
/// rewritten link's target decodes to the expected new path,
/// regardless of whether the rewrite happened to leave it bare or
/// percent-encoded it for safety.
fn test_percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            };
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// R14 review finding: this suite is flaky under the default macOS
/// per-process file-descriptor soft limit (256). Every [`TestDaemon`]
/// opens its own tokio runtime kqueue, `notify` watcher, several
/// `cache.db`/`-wal`/`-shm` SQLite handles, and sockets; running the
/// ~20 tests in this binary in parallel (the default `cargo test`
/// behaviour) can peak well past 256 open fds for the process as a
/// whole, so a `source.register` call fails with EMFILE partway
/// through the run — nondeterministically, depending on scheduling —
/// and every test after that point sees a registry that silently
/// never registered its source. Raise the *process*'s own soft limit
/// once (up to the hard limit, or a generous 10,240 ceiling,
/// whichever is lower) before any `TestDaemon` starts, exactly as a
/// real `tephrad` deployment would be run under a raised limit in
/// production rather than a bare interactive shell's default.
fn raise_fd_limit_for_this_process() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut limit: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) == 0 {
            let ceiling: libc::rlim_t = 10_240;
            let want = if limit.rlim_max == libc::RLIM_INFINITY {
                ceiling
            } else {
                limit.rlim_max.min(ceiling)
            };
            if want > limit.rlim_cur {
                limit.rlim_cur = want;
                let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
            }
        }
    });
}

struct TestDaemon {
    addr: std::net::SocketAddr,
    token: String,
    _cache_dir: tempfile::TempDir,
    server: tokio::task::JoinHandle<()>,
}

impl TestDaemon {
    async fn start() -> Self {
        let cache_dir = tempfile::tempdir().expect("cache dir");
        Self::start_in(cache_dir).await
    }

    /// Starts a daemon against an already-existing data directory
    /// (reusing its `cache.db`, hence any already-registered
    /// sources) rather than creating a brand-new one — used to
    /// simulate `tephrad` being killed and restarted against the
    /// exact same on-disk state (R8), as opposed to
    /// `start()`'s always-fresh cache.
    ///
    /// Mirrors `main.rs`'s real startup sequence, including the R8
    /// fix: every already-registered source's root is swept for
    /// stale `.{name}.tephra-tmp-{n}` leftovers (see
    /// `tephrad::scan::sweep_stale_temp_files`) before the daemon
    /// starts serving — so a test using this exercises the real fix,
    /// not a hand-rolled approximation of it.
    async fn start_in(cache_dir: tempfile::TempDir) -> Self {
        raise_fd_limit_for_this_process();

        let cache_path = cache_dir.path().join("cache.db");
        let token_path = cache_dir.path().join("token");
        let token = "c".repeat(64);
        std::fs::write(&token_path, &token).expect("write token");

        let registry_cache = Cache::open(&cache_path).expect("open cache for registry");
        let registry = Arc::new(SourceRegistry::new(registry_cache).expect("new registry"));

        if let Ok(rows) = registry.list() {
            for row in rows {
                let _ = tephrad::scan::sweep_stale_temp_files(std::path::Path::new(&row.path));
            }
        }

        let graph_cache = Cache::open(&cache_path).expect("open cache for graph");
        let initial_graph = UnitGraph::build_from_cache(&graph_cache).expect("build initial graph");
        let shared_graph = Arc::new(SharedGraph::new(initial_graph));
        let graph_refresh = GraphRefresh::new(shared_graph.clone(), cache_path.clone());

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr");

        let refactor_engine = RefactorEngine::new(
            shared_graph.clone() as Arc<dyn GraphBackend>,
            registry.clone() as Arc<dyn SourceBackend>,
            registry.clone() as Arc<dyn WatcherPauseControl>,
        );

        let state = Arc::new(AppState::new(
            token_path,
            shared_graph as Arc<dyn GraphBackend>,
            registry as Arc<dyn SourceBackend>,
            Arc::new(refactor_engine) as Arc<dyn RefactorBackend>,
            Some(graph_refresh),
        ));
        let router = build_router(state);
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        TestDaemon {
            addr,
            token,
            _cache_dir: cache_dir,
            server,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let client = reqwest::Client::new();
        let url = format!("http://{}/rpc", self.addr);
        client
            .post(&url)
            .bearer_auth(&self.token)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    }

    async fn call_no_auth(&self, method: &str, params: serde_json::Value) -> reqwest::Response {
        let client = reqwest::Client::new();
        let url = format!("http://{}/rpc", self.addr);
        client
            .post(&url)
            .json(&json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
            .send()
            .await
            .unwrap()
    }

    fn stop(self) {
        self.server.abort();
    }

    /// Like [`Self::stop`], but keeps this daemon's backing data
    /// directory alive and hands it back, instead of dropping (and
    /// so deleting) it — for a test that starts a *second* `TestDaemon`
    /// against the exact same `cache.db`/token via [`Self::start_in`],
    /// simulating `tephrad` being killed and restarted (R8).
    fn stop_keep_cache_dir(self) -> tempfile::TempDir {
        self.server.abort();
        self._cache_dir
    }
}

/// GP7/GP8: a two-source fixture tree (`notes` and `atlas-repo`),
/// seeded with both a same-source wikilink and a cross-source
/// wikilink pointing at `@notes:OldDoc`, is registered, previewed,
/// and applied — the preview lists both occurrences with zero disk
/// writes, and the apply rewrites only the targeted spans, byte for
/// byte, in both files.
#[tokio::test]
async fn preview_then_apply_rewrites_links_across_two_sources_with_exact_byte_diffs() {
    let daemon = TestDaemon::start().await;

    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n\nContent here.\n");
    notes.write(
        "Journal.md",
        "Morning note.\n\nSee [[OldDoc]] for details.\n",
    );

    let atlas_repo = fixtures::empty_source();
    atlas_repo.write(
        "README.md",
        "# atlas-repo\n\nLinked from another source: [[@notes:OldDoc]]\n",
    );

    let reg_notes = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(reg_notes.get("error").is_none(), "{reg_notes:?}");
    let reg_repo = daemon
        .call(
            "source.register",
            json!({"name": "atlas-repo", "root": atlas_repo.root().to_string_lossy()}),
        )
        .await;
    assert!(reg_repo.get("error").is_none(), "{reg_repo:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert!(preview.get("error").is_none(), "{preview:?}");
    let changes = preview["result"]["changes"]
        .as_array()
        .expect("changes array");
    assert_eq!(
        changes.len(),
        2,
        "Journal.md and README.md both reference OldDoc: {changes:?}"
    );

    // GP7 fence: a preview must perform zero disk writes.
    let journal_before = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    let readme_before = std::fs::read_to_string(atlas_repo.root().join("README.md")).unwrap();
    assert!(journal_before.contains("[[OldDoc]]"));
    assert!(readme_before.contains("[[@notes:OldDoc]]"));

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert!(apply.get("error").is_none(), "{apply:?}");
    assert_eq!(apply["result"]["status"], json!("applied"));
    assert_eq!(apply["result"]["files_changed"], json!(2));

    // Exact before/after content: only the targeted link span
    // changed, everything else byte-identical.
    let journal_after = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    assert_eq!(
        journal_after,
        "Morning note.\n\nSee [[NewDoc]] for details.\n"
    );
    let readme_after = std::fs::read_to_string(atlas_repo.root().join("README.md")).unwrap();
    assert_eq!(
        readme_after,
        "# atlas-repo\n\nLinked from another source: [[@notes:NewDoc]]\n"
    );

    daemon.stop();
}

/// T1 review/tester finding, exact live repro: a single file
/// containing both a wikilink and a markdown link to the same
/// renamed target must have *both* rewritten by `apply_refactor`, and
/// the `preview_refactor` response must already list both as
/// changes. Before the T1 fix, `tephra_graph::UnitGraph::get_backlinks`'s
/// query-display dedup silently collapsed both edges into one before
/// `compute_changes` ever saw them, so only one of the two links was
/// ever rewritten, while `apply_refactor` still reported a successful
/// `files_changed` count with no indication anything was left out.
#[tokio::test]
async fn preview_then_apply_rewrites_every_link_style_to_the_same_target_in_one_file() {
    let daemon = TestDaemon::start().await;

    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n\nContent here.\n");
    notes.write(
        "Journal.md",
        "Link to [[OldDoc]] and also [desc](OldDoc.md).\n",
    );

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert!(preview.get("error").is_none(), "{preview:?}");
    let changes = preview["result"]["changes"]
        .as_array()
        .expect("changes array");
    assert_eq!(
        changes.len(),
        2,
        "the preview must list both the wikilink and the markdown link occurrence: {changes:?}"
    );

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert!(apply.get("error").is_none(), "{apply:?}");
    assert_eq!(apply["result"]["status"], json!("applied"));
    assert_eq!(apply["result"]["files_changed"], json!(1));

    let journal_after = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    assert_eq!(
        journal_after, "Link to [[NewDoc]] and also [desc](NewDoc.md).\n",
        "both link styles must be rewritten, not just one"
    );

    daemon.stop();
}

/// T1, same bug but the spec's own unicode/space rename and three
/// mixed link styles (bare markdown, titled markdown, wikilink) all
/// pointing at the same target in one file — every occurrence must
/// be rewritten.
#[tokio::test]
async fn preview_then_apply_rewrites_three_mixed_link_styles_to_a_unicode_space_rename() {
    let daemon = TestDaemon::start().await;

    let notes = fixtures::empty_source();
    notes.write("Café Notes.md", "# Café Notes\n\nContent here.\n");
    notes.write(
        "Journal.md",
        "Wiki: [[Café Notes]]\n\
         Bare: [b](Caf%C3%A9%20Notes.md)\n\
         Titled: [t](Caf%C3%A9%20Notes.md \"Notes\")\n",
    );

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "Café Notes.md", "new_path": "Café Notes (2).md"}),
        )
        .await;
    assert!(preview.get("error").is_none(), "{preview:?}");
    let changes = preview["result"]["changes"]
        .as_array()
        .expect("changes array");
    assert_eq!(
        changes.len(),
        3,
        "all three link occurrences (wikilink, bare markdown, titled markdown) must be listed: {changes:?}"
    );

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "Café Notes.md", "new_path": "Café Notes (2).md"}),
        )
        .await;
    assert!(apply.get("error").is_none(), "{apply:?}");
    assert_eq!(apply["result"]["status"], json!("applied"));
    assert_eq!(apply["result"]["files_changed"], json!(1));

    let journal_after = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    let reparsed =
        tephra_parser::parse_unit(std::path::Path::new("@notes:Journal.md"), &journal_after);
    assert_eq!(reparsed.wikilinks.len(), 1, "{journal_after:?}");
    assert_eq!(reparsed.wikilinks[0].raw, "Café Notes (2)");
    assert_eq!(reparsed.markdown_links.len(), 2, "{journal_after:?}");
    for m in &reparsed.markdown_links {
        assert_eq!(
            test_percent_decode(&m.target),
            "Café Notes (2).md",
            "{:?}",
            reparsed.markdown_links
        );
    }

    daemon.stop();
}

/// Edge case: moving a file with zero incoming links returns an
/// empty preview, and the matching apply performs no writes and
/// still reports success.
#[tokio::test]
async fn zero_incoming_links_previews_empty_and_applies_as_a_successful_no_op() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("Lonely.md", "# Lonely\n\nNo one links here.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "Lonely.md", "new_path": "StillLonely.md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"], json!([]));

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "Lonely.md", "new_path": "StillLonely.md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("applied"));
    assert_eq!(apply["result"]["files_changed"], json!(0));

    daemon.stop();
}

/// Edge case: `old_path == new_path` — `preview_refactor` returns an
/// empty diff, `apply_refactor` is a no-op, neither errors.
#[tokio::test]
async fn identical_old_and_new_path_is_a_no_op_on_both_preview_and_apply() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("Doc.md", "# Doc\n");
    notes.write("Journal.md", "See [[Doc]].\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "Doc.md", "new_path": "Doc.md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"], json!([]));

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "Doc.md", "new_path": "Doc.md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("applied"));
    assert_eq!(apply["result"]["files_changed"], json!(0));
    let journal = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    assert_eq!(journal, "See [[Doc]].\n", "content must be untouched");

    daemon.stop();
}

/// Edge case: a case-only rename (`OldDoc.md` -> `olddoc.md`) is
/// detected and rewritten without a duplicate or orphaned entry.
#[tokio::test]
async fn case_only_rename_rewrites_exactly_once() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "olddoc.md"}),
        )
        .await;
    // link.apply_refactor requires a prior preview; this call alone
    // must be rejected (RPC error), then the real golden path below
    // re-does it correctly.
    assert!(apply.get("error").is_some());

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "olddoc.md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 1);

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "olddoc.md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("applied"));
    assert_eq!(apply["result"]["files_changed"], json!(1));
    let journal = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    assert_eq!(
        journal, "See [[olddoc]] here.\n",
        "exactly one rewrite, no duplicate/orphan"
    );

    daemon.stop();
}

/// Edge case: `new_path` already exists as a distinct file in the
/// target source — `preview_refactor` still returns the link diff,
/// and `apply_refactor` proceeds (it rewrites references only; it
/// never moves/renames the file on disk).
#[tokio::test]
async fn new_path_already_existing_does_not_block_the_refactor_or_move_any_file() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("NewDoc.md", "# A distinct, pre-existing file\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 1);

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("applied"));

    // Neither OldDoc.md nor the pre-existing NewDoc.md was moved,
    // renamed, or overwritten — only the referring link text changed.
    assert_eq!(
        std::fs::read_to_string(notes.root().join("OldDoc.md")).unwrap(),
        "# Old Doc\n"
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("NewDoc.md")).unwrap(),
        "# A distinct, pre-existing file\n"
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("Journal.md")).unwrap(),
        "See [[NewDoc]] here.\n"
    );

    daemon.stop();
}

/// Edge case: unicode and space-containing filenames round-trip
/// through preview and apply without corrupting the link text.
#[tokio::test]
async fn unicode_and_space_filenames_round_trip_through_preview_and_apply() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("Café Notes.md", "# Caf\u{e9} Notes\n");
    notes.write("Journal.md", "See [[Café Notes]] for details.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "Café Notes.md", "new_path": "Café Notes (2).md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 1);

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "Café Notes.md", "new_path": "Café Notes (2).md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("applied"));
    let journal = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    assert_eq!(journal, "See [[Café Notes (2)]] for details.\n");

    daemon.stop();
}

/// R12 review finding, end-to-end: a markdown link renamed to a
/// unicode- and space-containing path round-trips through the real
/// daemon's preview and apply, across a bare, a titled, and a
/// relative (cross-directory) destination, without turning any of
/// them into plain text — asserted by reparsing the rewritten files
/// and checking each one's link target decodes to the new path.
#[tokio::test]
async fn markdown_links_unicode_and_space_filenames_round_trip_through_preview_and_apply() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write(
        "Journal.md",
        "Bare: [doc](OldDoc.md) end.\nTitled: [doc](OldDoc.md \"The title\")\n",
    );
    notes.write("sub/Deep.md", "[doc](../OldDoc.md)\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "Café Notes (2).md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 3);

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "Café Notes (2).md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("applied"));
    assert_eq!(apply["result"]["files_changed"], json!(2));

    let journal = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    assert!(
        journal.contains("\"The title\""),
        "the title must survive untouched: {journal:?}"
    );
    let reparsed_journal =
        tephra_parser::parse_unit(std::path::Path::new("@notes:Journal.md"), &journal);
    assert_eq!(reparsed_journal.markdown_links.len(), 2);
    for link in &reparsed_journal.markdown_links {
        assert_eq!(
            test_percent_decode(&link.target),
            "Café Notes (2).md",
            "a renamed markdown link must still resolve to the new path: {link:?}"
        );
    }

    let deep = std::fs::read_to_string(notes.root().join("sub/Deep.md")).unwrap();
    let reparsed_deep =
        tephra_parser::parse_unit(std::path::Path::new("@notes:sub/Deep.md"), &deep);
    assert_eq!(reparsed_deep.markdown_links.len(), 1);
    assert_eq!(
        test_percent_decode(&reparsed_deep.markdown_links[0].target),
        "../Café Notes (2).md"
    );

    daemon.stop();
}

/// R13 review finding, end-to-end: a markdown destination written
/// with a backslash escape (`Old\(2\).md`) shows up in the preview
/// (because the parser's decoded target still matches the old path)
/// and must actually apply cleanly, rather than coming back
/// "partial" because the recorded span's raw bytes never equalled
/// the decoded `before` text.
#[tokio::test]
async fn markdown_link_with_backslash_escaped_destination_applies_end_to_end() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("Old(2).md", "# Old doc\n");
    notes.write("Journal.md", "[doc](Old\\(2\\).md)\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "Old(2).md", "new_path": "New(3).md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 1);

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "Old(2).md", "new_path": "New(3).md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("applied"), "{apply:?}");

    let journal = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    let reparsed = tephra_parser::parse_unit(std::path::Path::new("@notes:Journal.md"), &journal);
    assert_eq!(reparsed.markdown_links.len(), 1);
    assert_eq!(
        test_percent_decode(&reparsed.markdown_links[0].target),
        "New(3).md"
    );

    daemon.stop();
}

/// apply, `apply_refactor` aborts with a descriptive error before
/// writing to any file — including files that were not stale.
#[tokio::test]
async fn staleness_between_preview_and_apply_aborts_before_any_write() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");
    notes.write("Index.md", "Also [[OldDoc]] here.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 2);

    // Touch Journal.md's mtime without changing its content — this
    // alone must be enough to trip the staleness gate (E3 is an
    // mtime check, not a content-hash check).
    let journal_path = notes.root().join("Journal.md");
    let original_content = std::fs::read_to_string(&journal_path).unwrap();
    let future =
        filetime::FileTime::from_unix_time(filetime::FileTime::now().unix_seconds() + 3600, 0);
    filetime::set_file_mtime(&journal_path, future).expect("set_file_mtime");

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("aborted"));
    let reason = apply["result"]["reason"].as_str().unwrap_or("");
    assert!(reason.contains("modified since preview"), "{reason}");
    assert!(reason.contains("Journal.md"), "{reason}");

    // Zero bytes changed on disk anywhere — including Index.md, which
    // was not itself stale (E3: "aborts ... including files that
    // were not stale").
    assert_eq!(
        std::fs::read_to_string(&journal_path).unwrap(),
        original_content
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("Index.md")).unwrap(),
        "Also [[OldDoc]] here.\n"
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("OldDoc.md")).unwrap(),
        "# Old Doc\n"
    );

    daemon.stop();
}

/// Edge case: calling `apply_refactor` without a prior matching
/// `preview_refactor` call is rejected, preventing apply against a
/// stale, unseen diff.
#[tokio::test]
async fn apply_without_a_prior_preview_is_rejected() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]].\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert!(apply.get("error").is_some(), "{apply:?}");
    let message = apply["error"]["message"].as_str().unwrap_or("");
    assert!(message.contains("preview"), "{message}");
    assert_eq!(
        std::fs::read_to_string(notes.root().join("Journal.md")).unwrap(),
        "See [[OldDoc]].\n",
        "a rejected apply must not write anything"
    );

    daemon.stop();
}

/// Edge case: a target file becomes unreadable (permission denied)
/// between preview and apply — that file is reported as failed,
/// already-written files are not rolled back, and the response
/// enumerates exactly which files succeeded vs failed.
#[tokio::test]
#[cfg(unix)]
async fn a_file_that_becomes_unreadable_between_preview_and_apply_is_reported_not_rolled_back() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");
    notes.write("Index.md", "Also [[OldDoc]] here.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 2);

    // Index.md becomes unreadable (0000) between preview and apply.
    // Its mtime is untouched, so this exercises the write-time I/O
    // failure path, not the staleness gate.
    let index_path = notes.root().join("Index.md");
    let mut perms = std::fs::metadata(&index_path).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&index_path, perms).expect("chmod 000");

    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;

    // Restore permissions immediately so the fixture tempdir can be
    // cleaned up regardless of the assertion outcome below.
    let mut restore = std::fs::metadata(&index_path).unwrap().permissions();
    restore.set_mode(0o644);
    std::fs::set_permissions(&index_path, restore).expect("restore permissions");

    // Root running as uid 0 (some CI containers) bypasses the 0000
    // permission bits entirely; skip the assertion in that
    // environment rather than fail on an untestable precondition.
    if unsafe { libc::geteuid() } == 0 {
        daemon.stop();
        return;
    }

    assert_eq!(apply["result"]["status"], json!("partial"), "{apply:?}");
    assert_eq!(apply["result"]["files_changed"], json!(1));
    let failed = apply["result"]["failed"].as_array().expect("failed array");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["file"], json!("@notes:Index.md"));

    // Journal.md (succeeded before the failure) is not rolled back.
    assert_eq!(
        std::fs::read_to_string(notes.root().join("Journal.md")).unwrap(),
        "See [[NewDoc]] here.\n"
    );

    daemon.stop();
}

/// R10 review finding, direct reproduction against the real daemon:
/// a referring file that is unreadable *at preview time* (unlike the
/// test above, which chmods between preview and apply) must still be
/// named, with its error, in `link.preview_refactor`'s own result —
/// not just silently missing from `changes` with the response
/// otherwise looking complete.
#[tokio::test]
async fn preview_refactor_names_an_unreadable_referring_file_as_skipped() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");
    notes.write("Secret.md", "Also see [[OldDoc]].\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let secret_path = notes.root().join("Secret.md");
    let mut perms = std::fs::metadata(&secret_path).unwrap().permissions();
    perms.set_mode(0o000);
    std::fs::set_permissions(&secret_path, perms).expect("chmod 000");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;

    // Restore permissions immediately so the fixture tempdir can be
    // cleaned up regardless of the assertion outcome below.
    let mut restore = std::fs::metadata(&secret_path).unwrap().permissions();
    restore.set_mode(0o644);
    std::fs::set_permissions(&secret_path, restore).expect("restore permissions");

    // Root running as uid 0 (some CI containers) bypasses the 0000
    // permission bits entirely; skip the assertion in that
    // environment rather than fail on an untestable precondition.
    if unsafe { libc::geteuid() } == 0 {
        daemon.stop();
        return;
    }

    let changes = preview["result"]["changes"]
        .as_array()
        .expect("changes array");
    assert_eq!(changes.len(), 1, "{preview:?}");
    assert_eq!(changes[0]["file"], json!("@notes:Journal.md"));

    let skipped = preview["result"]["skipped"]
        .as_array()
        .expect("skipped array");
    assert_eq!(skipped.len(), 1, "{preview:?}");
    assert_eq!(skipped[0]["file"], json!("@notes:Secret.md"));
    assert!(
        !skipped[0]["error"].as_str().unwrap_or_default().is_empty(),
        "{preview:?}"
    );

    daemon.stop();
}

/// F5: both RPC methods reject a request with no/invalid bearer
/// token with the same 401-equivalent error PR-6 uses for every
/// other method — no new unauthenticated code path for disk
/// mutation.
#[tokio::test]
async fn both_refactor_methods_require_authentication_over_http() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview_resp = daemon
        .call_no_auth(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(preview_resp.status(), 401);

    let apply_resp = daemon
        .call_no_auth(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(apply_resp.status(), 401);

    daemon.stop();
}

/// Fence: no file is ever mutated by `link.preview_refactor` under
/// any input, including a malformed/nonexistent `old_path` — proven
/// here by making the entire source root read-only (no file can be
/// created or written inside it) and asserting `preview_refactor`
/// still succeeds, over both a real existing target and a bogus one.
#[tokio::test]
#[cfg(unix)]
async fn preview_refactor_never_writes_even_against_a_read_only_source_root() {
    use std::os::unix::fs::PermissionsExt;

    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    // R6(b) review finding: chmod only the directory to 0500 (as this
    // test originally did) stops *new* files being created inside it
    // but does nothing to stop an in-place write to an *existing*
    // file that's still individually writable — so it could never
    // have caught a preview that (incorrectly) wrote in place rather
    // than only reading. Chmod the existing files themselves to 0400
    // too, and compare their exact bytes and mtimes from before and
    // after preview, rather than relying solely on would-be writes
    // failing.
    let old_doc_path = notes.root().join("OldDoc.md");
    let journal_path = notes.root().join("Journal.md");
    let old_doc_before = std::fs::read(&old_doc_path).unwrap();
    let journal_before = std::fs::read(&journal_path).unwrap();
    let old_doc_mtime_before = std::fs::metadata(&old_doc_path)
        .unwrap()
        .modified()
        .unwrap();
    let journal_mtime_before = std::fs::metadata(&journal_path)
        .unwrap()
        .modified()
        .unwrap();

    let mut file_perms = std::fs::metadata(&journal_path).unwrap().permissions();
    file_perms.set_mode(0o400);
    std::fs::set_permissions(&old_doc_path, file_perms.clone()).expect("chmod OldDoc.md 0400");
    std::fs::set_permissions(&journal_path, file_perms).expect("chmod Journal.md 0400");

    // Make the directory itself read-only + non-executable-for-write
    // (0500): any attempt to create a file (e.g. a temp file for an
    // atomic rewrite) inside it would fail immediately.
    let mut perms = std::fs::metadata(notes.root()).unwrap().permissions();
    perms.set_mode(0o500);
    std::fs::set_permissions(notes.root(), perms).expect("chmod 0500 the source root");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;

    let bogus_preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "does/not/exist.md", "new_path": "also/missing.md"}),
        )
        .await;

    // Restore so the tempdir can be cleaned up.
    let mut restore = std::fs::metadata(notes.root()).unwrap().permissions();
    restore.set_mode(0o700);
    std::fs::set_permissions(notes.root(), restore).expect("restore root permissions");
    let mut file_restore = std::fs::metadata(&journal_path)
        .map(|m| m.permissions())
        .unwrap_or_else(|_| std::fs::Permissions::from_mode(0o644));
    file_restore.set_mode(0o644);
    let _ = std::fs::set_permissions(&old_doc_path, file_restore.clone());
    let _ = std::fs::set_permissions(&journal_path, file_restore);

    if unsafe { libc::geteuid() } == 0 {
        daemon.stop();
        return;
    }

    assert!(preview.get("error").is_none(), "{preview:?}");
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 1);
    assert!(bogus_preview.get("error").is_none(), "{bogus_preview:?}");
    assert_eq!(bogus_preview["result"]["changes"], json!([]));

    // The actual proof R6(b) asked for: every byte and the mtime of
    // both files are exactly as they were before `preview_refactor`
    // ran, not merely "the directory refused a new file".
    assert_eq!(
        std::fs::read(&old_doc_path).unwrap(),
        old_doc_before,
        "preview must never write to OldDoc.md"
    );
    assert_eq!(
        std::fs::read(&journal_path).unwrap(),
        journal_before,
        "preview must never write to Journal.md"
    );
    assert_eq!(
        std::fs::metadata(&old_doc_path)
            .unwrap()
            .modified()
            .unwrap(),
        old_doc_mtime_before,
        "preview must never touch OldDoc.md's mtime"
    );
    assert_eq!(
        std::fs::metadata(&journal_path)
            .unwrap()
            .modified()
            .unwrap(),
        journal_mtime_before,
        "preview must never touch Journal.md's mtime"
    );

    daemon.stop();
}

/// Programmatic test for the watcher pause/resume requirement (GP8):
/// while `SourceRegistry::pause_all_watchers` is in effect, a real
/// filesystem edit on a tracked file is not processed into a reindex
/// — the cache row stays at its pre-edit content — until
/// `resume_all_watchers` is called, after which the deferred edit is
/// picked up. Exercises `tephrad::registry::SourceRegistry` directly
/// (PR-4's own watcher/registry test surface), the same mechanism
/// `RefactorEngine::apply` drives around its own writes.
#[test]
fn watcher_pause_defers_a_reindex_until_resume() {
    use tephrad::registry::SourceRegistry;

    let fixture = fixtures::vault_fixture();
    let cache_dir = tempfile::tempdir().expect("cache tempdir");
    let cache_path = cache_dir.path().join("cache.db");
    let cache = Cache::open(&cache_path).expect("open cache");

    let registry = SourceRegistry::new(cache).expect("new registry");
    registry
        .register("notes", fixture.root())
        .expect("register");

    registry.pause_all_watchers();

    fixture.write(
        "one.md",
        "# One\n\nEdited while paused, now links to [[Three]] only.\n",
    );

    // While paused, the edit must not be reflected for a reasonable
    // window — `wait_until` here is expected to time out (we assert
    // it did *not* fire), proving the watcher's batch was deferred
    // rather than raced.
    let verify_cache = Cache::open(&cache_path).expect("reopen cache");
    let source = verify_cache
        .get_source("notes")
        .expect("get_source")
        .expect("source row");
    let reindexed_while_paused = wait_until(Duration::from_millis(600), || {
        let unit = match verify_cache.get_unit_by_rel_path(source.id, "one.md") {
            Ok(Some(u)) => u,
            _ => return false,
        };
        let link_count: i64 = verify_cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1 AND to_raw = ?2",
                rusqlite::params![unit.id, "Three"],
                |r| r.get(0),
            )
            .unwrap_or(0);
        link_count == 1
    });
    assert!(
        !reindexed_while_paused,
        "a filesystem event fired during the pause window must not be processed into a \
         reindex until after resume"
    );

    registry.resume_all_watchers();

    let reindexed_after_resume = wait_until(Duration::from_secs(5), || {
        let unit = match verify_cache.get_unit_by_rel_path(source.id, "one.md") {
            Ok(Some(u)) => u,
            _ => return false,
        };
        let link_count: i64 = verify_cache
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM links WHERE from_unit_id = ?1 AND to_raw = ?2",
                rusqlite::params![unit.id, "Three"],
                |r| r.get(0),
            )
            .unwrap_or(0);
        link_count == 1
    });
    assert!(
        reindexed_after_resume,
        "the deferred edit must be processed once the watcher is resumed"
    );

    registry.unregister("notes").expect("unregister");
}

/// Edge case: a very large fan-in (1,000+ files linking to the same
/// target) completes as a single bounded `apply_refactor` call
/// without the daemon blocking RPC handling for other sources during
/// the run — asserted by firing a `graph.get_edges` call against a
/// second, unrelated source concurrently with the large apply and
/// observing it complete promptly rather than waiting for the apply
/// to finish.
#[tokio::test]
async fn large_fan_in_completes_as_one_bounded_apply_without_blocking_other_rpc_calls() {
    let daemon = TestDaemon::start().await;

    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    const FAN_IN: usize = 1_200;
    for i in 0..FAN_IN {
        notes.write(&format!("Linker{i:04}.md"), "See [[OldDoc]] here.\n");
    }

    let other = fixtures::empty_source();
    other.write("Solo.md", "# Solo\n\nNothing to do with notes.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");
    let register = daemon
        .call(
            "source.register",
            json!({"name": "other", "root": other.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(
        preview["result"]["changes"].as_array().unwrap().len(),
        FAN_IN
    );

    // Fire the large apply and a small, unrelated `graph.get_edges`
    // call on `other` concurrently. If the apply blocked the whole
    // daemon, the concurrent call would also stall until the apply
    // finished; measuring that it returns well before the apply does
    // (not merely "eventually") is the actual non-blocking proof.
    let apply_fut = daemon.call(
        "link.apply_refactor",
        json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
    );
    let other_fut = async {
        // Give the apply a moment to actually start before racing it.
        tokio::time::sleep(Duration::from_millis(20)).await;
        let start = Instant::now();
        let result = daemon.call("graph.get_edges", json!({})).await;
        (result, start.elapsed())
    };

    let (apply, (other_result, other_elapsed)) = tokio::join!(apply_fut, other_fut);

    assert!(other_result.get("error").is_none(), "{other_result:?}");
    assert!(
        other_elapsed < Duration::from_secs(2),
        "an unrelated RPC call took {other_elapsed:?} to return — the daemon appears to have \
         blocked all RPC handling on the large apply rather than running it as a bounded, \
         non-blocking operation"
    );

    assert_eq!(apply["result"]["status"], json!("applied"));
    assert_eq!(apply["result"]["files_changed"], json!(FAN_IN));

    daemon.stop();
}

/// Edge case ("process is killed mid-apply_refactor: on restart
/// tephrad does not re-apply a partial refactor automatically; files
/// already rewritten remain rewritten and no temp files are left
/// behind"): `tephrad` keeps no on-disk journal of an in-progress
/// apply to resume (confirmed by inspection — `RefactorEngine::apply`
/// persists nothing beyond each file's own atomic write), so a
/// restart can never "automatically re-apply" anything by
/// construction; this test instead pins the two properties that
/// *are* mechanically checkable without an actual `kill -9`: after a
/// real multi-file apply completes, (a) no `.tephra-tmp-*` temp file
/// is left in any affected directory, and (b) restarting the daemon
/// against the same on-disk files and cache does not alter them
/// again (nothing to "re-apply" — the files already reflect the
/// applied state, and a fresh preview against them now reports zero
/// further changes).
#[tokio::test]
async fn no_leftover_temp_files_and_nothing_to_reapply_after_restart() {
    let daemon = TestDaemon::start().await;
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");
    notes.write("Index.md", "Also [[OldDoc]] here.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");
    daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    let apply = daemon
        .call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(apply["result"]["status"], json!("applied"));
    daemon.stop();

    // (a) no leftover temp files anywhere in the source root.
    for entry in walkdir::WalkDir::new(notes.root()) {
        let entry = entry.expect("walk fixture tree");
        let name = entry.file_name().to_string_lossy();
        assert!(
            !name.contains(".tephra-tmp-"),
            "leftover temp file after a completed apply: {}",
            entry.path().display()
        );
    }

    // (b) a fresh daemon ("restart"), pointed at the same already-
    // rewritten files, has nothing further to reapply: a new preview
    // for the exact same rename reports zero occurrences of the old
    // name (it was already rewritten to the new name everywhere),
    // and the files are untouched by simply starting the daemon.
    let journal_after_restart = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    let index_after_restart = std::fs::read_to_string(notes.root().join("Index.md")).unwrap();

    let daemon2 = TestDaemon::start().await;
    let register = daemon2
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");
    let preview_after_restart = daemon2
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(
        preview_after_restart["result"]["changes"],
        json!([]),
        "nothing left to reapply: OldDoc.md has no remaining references after the restart"
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("Journal.md")).unwrap(),
        journal_after_restart,
        "starting a fresh daemon must not itself rewrite anything"
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("Index.md")).unwrap(),
        index_after_restart
    );

    daemon2.stop();
}

/// R3 review finding, direct reproduction: a process killed between
/// `write_new` and `rename` (see `tephrad::refactor::rewrite_links_atomic_with`)
/// leaves a `.<name>.tephra-tmp-<n>` file sitting in the user's own
/// source tree. Nothing in the normal apply path ever revisits it, so
/// simulate exactly that — plant the stray file directly, without
/// ever calling `apply_refactor` — then "restart" (register on a
/// fresh daemon, the same source root) and confirm `source.register`'s
/// own full walk sweeps it away rather than leaving it forever or
/// indexing it as if it were a real note.
#[tokio::test]
async fn a_temp_file_left_by_a_simulated_kill_is_swept_on_the_next_register() {
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");

    // Plant the stray temp file exactly as a kill between `write_new`
    // and `rename` would leave it: same naming convention, sitting
    // right next to the real file it was about to replace.
    let stray = notes.root().join(".Journal.md.tephra-tmp-424242");
    std::fs::write(&stray, "partially written garbage").unwrap();
    assert!(
        stray.exists(),
        "the stray temp file must exist before register"
    );

    let daemon = TestDaemon::start().await;
    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    assert!(
        !stray.exists(),
        "a stale refactor temp file must be swept away by the next register/rescan walk"
    );
    // The real files are completely unaffected by the sweep.
    assert_eq!(
        std::fs::read_to_string(notes.root().join("Journal.md")).unwrap(),
        "See [[OldDoc]] here.\n"
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("OldDoc.md")).unwrap(),
        "# Old Doc\n"
    );

    daemon.stop();
}

/// R8 review finding, direct reproduction: `SourceRegistry::new`
/// only re-reads existing cache rows — it never walks the
/// filesystem — and re-registering an already-registered source is
/// refused, so the sweep above (which runs on `source.register`) can
/// never fire on a plain restart. Unlike that test, which simulates
/// a "restart" with a brand-new, never-registered cache, this one
/// registers `notes` for real, plants the stray file afterwards
/// (simulating a kill mid-`apply_refactor` sometime during that
/// daemon's life, not during its initial scan), kills the daemon
/// while keeping its `cache.db` intact, and starts a second daemon
/// against that exact same persisted cache with no RPC call of any
/// kind in between — confirming the fix actually covers a real
/// restart, not just another register.
#[tokio::test]
async fn a_temp_file_left_by_a_kill_is_swept_on_the_next_real_restart() {
    let notes = fixtures::empty_source();
    notes.write("OldDoc.md", "# Old Doc\n");
    notes.write("Journal.md", "See [[OldDoc]] here.\n");

    let daemon1 = TestDaemon::start().await;
    let register = daemon1
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    // Plant the stray temp file *after* register's own initial-scan
    // sweep already ran — this is not the scenario the register-time
    // sweep itself exercises.
    let stray = notes.root().join(".Journal.md.tephra-tmp-777777");
    std::fs::write(&stray, "partially written garbage").unwrap();
    assert!(
        stray.exists(),
        "the stray temp file must exist right after being planted"
    );

    // "Kill" daemon1, but keep its cache.db/token directory intact —
    // a real restart reuses the same on-disk state.
    let cache_dir = daemon1.stop_keep_cache_dir();
    assert!(
        stray.exists(),
        "killing the daemon must not itself sweep anything"
    );

    // Start a second daemon against that exact same persisted cache.
    // No RPC call happens in between: re-registering "notes" would
    // be refused as a duplicate, and there is no rescan command.
    let daemon2 = TestDaemon::start_in(cache_dir).await;

    assert!(
        !stray.exists(),
        "a plain daemon restart must sweep stale refactor temp files for every \
         already-registered source, with no register/rescan call needed"
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("Journal.md")).unwrap(),
        "See [[OldDoc]] here.\n",
        "the startup sweep must not touch real file content"
    );
    assert_eq!(
        std::fs::read_to_string(notes.root().join("OldDoc.md")).unwrap(),
        "# Old Doc\n"
    );

    // The source is still fully usable after the restart: a fresh
    // preview against it works with no re-register needed.
    let preview = daemon2
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldDoc.md", "new_path": "NewDoc.md"}),
        )
        .await;
    assert_eq!(preview["result"]["changes"].as_array().unwrap().len(), 1);

    daemon2.stop();
}

/// R6(c): two `apply_refactor` calls targeting overlapping files
/// (both renames are referenced from the same `Journal.md`) started
/// genuinely concurrently (`tokio::join!`, not one awaited before the
/// other starts) must be serialized by `apply_lock`, not interleaved.
///
/// Because both touch the same `Journal.md`, serialization here
/// doesn't mean both necessarily succeed: `apply_lock` lets exactly
/// one of the two run to completion (its own read, write, and
/// rescan) before the other even starts its own staleness check —
/// and that second call's own `preview_refactor` snapshot of
/// `Journal.md`'s mtime, taken before either apply ran, no longer
/// matches the file the first apply just rewrote, so it correctly
/// aborts rather than either corrupting the file or silently
/// clobbering the first rename. Without `apply_lock` serializing the
/// two calls at all, both could instead read `Journal.md`'s original
/// content concurrently, compute their own single-line rewrite from
/// it, and whichever wrote last would silently clobber the other's
/// rename entirely — the exact race this test rules out.
#[tokio::test]
async fn two_concurrent_applies_targeting_overlapping_files_are_serialized() {
    let daemon = Arc::new(TestDaemon::start().await);
    let notes = fixtures::empty_source();
    notes.write("OldA.md", "# Old A\n");
    notes.write("OldB.md", "# Old B\n");
    notes.write("Journal.md", "See [[OldA]] and [[OldB]] here.\n");

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");

    let preview_a = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldA.md", "new_path": "NewA.md"}),
        )
        .await;
    let preview_b = daemon
        .call(
            "link.preview_refactor",
            json!({"source": "notes", "old_path": "OldB.md", "new_path": "NewB.md"}),
        )
        .await;
    assert_eq!(preview_a["result"]["changes"].as_array().unwrap().len(), 1);
    assert_eq!(preview_b["result"]["changes"].as_array().unwrap().len(), 1);

    let d1 = daemon.clone();
    let d2 = daemon.clone();
    let (apply_a, apply_b) = tokio::join!(
        d1.call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldA.md", "new_path": "NewA.md"}),
        ),
        d2.call(
            "link.apply_refactor",
            json!({"source": "notes", "old_path": "OldB.md", "new_path": "NewB.md"}),
        )
    );

    drop(d1);
    drop(d2);

    let status_a = apply_a["result"]["status"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let status_b = apply_b["result"]["status"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let applied_count = [&status_a, &status_b]
        .iter()
        .filter(|s| s.as_str() == "applied")
        .count();
    let aborted_count = [&status_a, &status_b]
        .iter()
        .filter(|s| s.as_str() == "aborted")
        .count();
    assert_eq!(
        (applied_count, aborted_count),
        (1, 1),
        "apply_lock must let exactly one of the two overlapping applies through to completion \
         and correctly abort the other on staleness — never both succeed (which would require \
         an unserialized, interleaved write) and never both abort (the first one through always \
         has a fresh, matching snapshot): a={apply_a:?} b={apply_b:?}"
    );

    let journal = std::fs::read_to_string(notes.root().join("Journal.md")).unwrap();
    let plausible_after_exactly_one_rename_lands = [
        "See [[NewA]] and [[OldB]] here.\n",
        "See [[OldA]] and [[NewB]] here.\n",
    ];
    assert!(
        plausible_after_exactly_one_rename_lands.contains(&journal.as_str()),
        "Journal.md must reflect exactly the one rename that actually succeeded, with no \
         corruption from the two applies interleaving: {journal:?}"
    );

    match Arc::try_unwrap(daemon) {
        Ok(daemon) => daemon.stop(),
        Err(_) => panic!("daemon Arc still has other owners after both applies completed"),
    }
}
