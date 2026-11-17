//! `link.rename_event` over JSON-RPC on HTTP (G3, G4, G21): one case per
//! reply value, the refusal reasons, bad parameters and auth. The server
//! is the real router (`rpc_fixtures`), the sources are real temp dirs
//! in a real registry and cache, the write loop is the real one. Only the
//! clock is faked.

use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tephra_cache::Cache;
use tephrad::fixes::{FixEngine, ManualClock};
use tephrad::refactor::fix_write::{FixWriter, Sleeper};
use tephrad::refactor::RealFs;
use tephrad::registry::SourceRegistry;
use tephrad::rpc::backend::{FixService, UnsupportedBlockIds};

use crate::fixtures::{self, TempSource, GP1_FILES};
use crate::rpc_fixtures::{spawn_with_token_sources_blocks_and_fixes, MockGraph, MockSources};

const TOKEN: &str = "e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1e1";

struct NoSleep;
impl Sleeper for NoSleep {
    fn sleep(&self, _d: Duration) {}
}

struct Rig {
    notes: TempSource,
    _other: TempSource,
    outside: TempSource,
    cache_path: std::path::PathBuf,
    registry: Arc<SourceRegistry>,
    service: Arc<FixService>,
    _dirs: tempfile::TempDir,
}

impl Rig {
    fn new(notes_files: &[(&str, &str)]) -> Rig {
        let notes = fixtures::empty_source();
        for (rel, text) in notes_files {
            notes.write(rel, text);
        }
        let other = fixtures::empty_source();
        other.write("Elsewhere.md", "# Elsewhere\n");
        let outside = fixtures::empty_source();
        let dirs = tempfile::tempdir().unwrap();
        let cache_path = dirs.path().join("cache.db");
        let registry = Arc::new(SourceRegistry::new(Cache::open(&cache_path).unwrap()).unwrap());
        registry.register("notes", notes.root()).unwrap();
        registry.register("other", other.root()).unwrap();
        let writer = Arc::new(FixWriter::with_parts(
            Arc::new(Mutex::new(())),
            Arc::new(RealFs),
            Arc::new(NoSleep),
        ));
        let clock = Arc::new(ManualClock::new(1_000));
        let engine = Arc::new(FixEngine::new(&cache_path, writer).with_clock(clock.clone()));
        engine.set_sink(registry.clone());
        let service = Arc::new(FixService::new(engine, registry.clone()));
        Rig {
            notes,
            _other: other,
            outside,
            cache_path,
            registry,
            service,
            _dirs: dirs,
        }
    }

    fn notes_path(&self, rel: &str) -> String {
        self.notes.root().join(rel).display().to_string()
    }

    fn mv(&self, old: &str, new: &str) {
        let to = self.notes.root().join(new);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(self.notes.root().join(old), to).unwrap();
    }

    fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.notes.root().join(rel)).unwrap()
    }

    fn log(&self) -> Vec<tephra_cache::FixLogRow> {
        Cache::open(&self.cache_path)
            .unwrap()
            .list_fix_log(None, 100)
            .unwrap()
    }
}

/// The rig's registry owns a runtime of its own, which must not be
/// dropped inside an async context: the rig lives outside this one.
fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

async fn serve(rig: &Rig) -> crate::rpc_fixtures::TestServer {
    spawn_with_token_sources_blocks_and_fixes(
        MockGraph::new(),
        MockSources::new(),
        UnsupportedBlockIds,
        ArcService(rig.service.clone()),
        TOKEN,
    )
    .await
}

/// `FixBackend` over a shared service.
struct ArcService(Arc<FixService>);

impl tephrad::rpc::backend::FixBackend for ArcService {
    fn list(
        &self,
        since: Option<i64>,
        limit: usize,
    ) -> Result<Vec<tephrad::fixes::FixEntryWire>, String> {
        self.0.list(since, limit)
    }
    fn after_rescan(&self, source: &str) {
        self.0.after_rescan(source)
    }
    fn rename_event(
        &self,
        old_path: &str,
        new_path: &str,
    ) -> Result<tephrad::fixes::event::RenameEventWire, tephrad::fixes::event::RenameEventError>
    {
        self.0.rename_event(old_path, new_path)
    }
}

async fn rpc(server: &crate::rpc_fixtures::TestServer, params: Value) -> Value {
    reqwest::Client::new()
        .post(server.http_url())
        .bearer_auth(TOKEN)
        .json(&json!({"jsonrpc": "2.0", "id": 7, "method": "link.rename_event", "params": params}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn event(old: &str, new: &str) -> Value {
    json!({"old_path": old, "new_path": new})
}

fn exists(root: &Path, rel: &str) -> bool {
    root.join(rel).exists()
}

#[test]
fn applied_is_answered_with_the_files_rewritten_and_a_repeat_is_already_applied() {
    let rig = Rig::new(&GP1_FILES);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Training Plan.md", "Plan.md");
        let params = event(
            &rig.notes_path("Training Plan.md"),
            &rig.notes_path("Plan.md"),
        );

        let reply = rpc(&server, params.clone()).await;
        assert_eq!(reply["id"], json!(7));
        assert_eq!(
            reply["result"],
            json!({"status": "applied", "files_changed": 3}),
            "{reply}"
        );
        assert!(rig.text("Goals.md").contains("[[Plan|plan]]"));
        assert!(rig.text("Weekly Review.md").contains("[[Plan]]"));
        assert!(rig.text("Ideas/Rename Notes.md").contains("![[Plan]]"));
        assert_eq!(rig.log().len(), 1);

        // The same event again, and once more: answered from the log.
        for _ in 0..2 {
            let again = rpc(&server, params.clone()).await;
            assert_eq!(
                again["result"],
                json!({"status": "already_applied"}),
                "{again}"
            );
        }
        assert_eq!(rig.log().len(), 1, "one fix, one log entry");
    });
}

#[test]
fn a_chain_sent_in_order_applies_in_order_and_a_late_first_event_is_already_applied() {
    // E1: A to B, then B to C, each event sent right after its rename.
    let rig = Rig::new(&GP1_FILES);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Training Plan.md", "Plan B.md");
        let first = event(
            &rig.notes_path("Training Plan.md"),
            &rig.notes_path("Plan B.md"),
        );
        let r1 = rpc(&server, first.clone()).await;
        assert_eq!(
            r1["result"],
            json!({"status": "applied", "files_changed": 3}),
            "{r1}"
        );
        assert!(rig.text("Goals.md").contains("[[Plan B|plan]]"));

        rig.mv("Plan B.md", "Plan C.md");
        let second = event(&rig.notes_path("Plan B.md"), &rig.notes_path("Plan C.md"));
        let r2 = rpc(&server, second.clone()).await;
        assert_eq!(
            r2["result"],
            json!({"status": "applied", "files_changed": 3}),
            "{r2}"
        );
        assert!(rig.text("Goals.md").contains("[[Plan C|plan]]"));
        assert_eq!(rig.log().len(), 2);

        // The first event, late: answered from the log, nothing rewritten.
        let late = rpc(&server, first).await;
        assert_eq!(
            late["result"],
            json!({"status": "already_applied"}),
            "{late}"
        );
        let late2 = rpc(&server, second).await;
        assert_eq!(
            late2["result"],
            json!({"status": "already_applied"}),
            "{late2}"
        );
        assert_eq!(rig.log().len(), 2);
        assert!(rig.text("Goals.md").contains("[[Plan C|plan]]"));
    });
}

#[test]
fn a_late_a_to_b_when_b_has_since_moved_to_c_resolves_through_the_log_to_c() {
    // E1, E23: the file went A to B (the cache saw B), then B to C. The
    // event for B to C is the first to arrive; the one for A to B is late.
    let rig = Rig::new(&GP1_FILES);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Training Plan.md", "Plan B.md");
        rig.registry.rescan("notes").unwrap();
        rig.mv("Plan B.md", "Plan C.md");
        let b_to_c = rpc(
            &server,
            event(&rig.notes_path("Plan B.md"), &rig.notes_path("Plan C.md")),
        )
        .await;
        assert_eq!(b_to_c["result"]["status"], json!("applied"), "{b_to_c}");
        assert_eq!(rig.log().len(), 1);

        let late = rpc(
            &server,
            event(
                &rig.notes_path("Training Plan.md"),
                &rig.notes_path("Plan B.md"),
            ),
        )
        .await;
        assert_eq!(
            late["result"],
            json!({"status": "applied", "files_changed": 3}),
            "{late}"
        );
        // The links name where the file is now, not the stop on the way.
        assert!(rig.text("Goals.md").contains("[[Plan C|plan]]"));
        assert!(rig.text("Weekly Review.md").contains("[[Plan C]]"));
        assert!(!rig.text("Goals.md").contains("Plan B"));
        let log = rig.log();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].old_path.as_deref(), Some("Training Plan.md"));
        assert_eq!(log[0].new_path.as_deref(), Some("Plan C.md"));

        // And the same late event once more: nothing is written twice.
        let again = rpc(
            &server,
            event(
                &rig.notes_path("Training Plan.md"),
                &rig.notes_path("Plan B.md"),
            ),
        )
        .await;
        assert_eq!(
            again["result"],
            json!({"status": "already_applied"}),
            "{again}"
        );
        assert_eq!(rig.log().len(), 2);
    });
}

#[test]
fn rename_back_is_a_new_fix_and_rewrites_the_links_again() {
    // E20: Plan to Training Plan, then back to Plan.
    let rig = Rig::new(&[("Plan.md", "# Plan\n"), ("Home.md", "See [[Plan]].\n")]);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Plan.md", "Training Plan.md");
        let there = rpc(
            &server,
            event(
                &rig.notes_path("Plan.md"),
                &rig.notes_path("Training Plan.md"),
            ),
        )
        .await;
        assert_eq!(
            there["result"],
            json!({"status": "applied", "files_changed": 1}),
            "{there}"
        );
        assert_eq!(rig.text("Home.md"), "See [[Training Plan]].\n");

        rig.mv("Training Plan.md", "Plan.md");
        let back = rpc(
            &server,
            event(
                &rig.notes_path("Training Plan.md"),
                &rig.notes_path("Plan.md"),
            ),
        )
        .await;
        assert_eq!(
            back["result"],
            json!({"status": "applied", "files_changed": 1}),
            "{back}"
        );
        assert_eq!(rig.text("Home.md"), "See [[Plan]].\n");
        assert_eq!(rig.log().len(), 2);
    });
}

#[test]
fn a_name_that_was_reused_is_never_rewritten_to_the_wrong_file() {
    // E22: Goals.md was renamed, and a new file now has the old name.
    let rig = Rig::new(&GP1_FILES);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Training Plan.md", "Plan.md");
        rig.notes
            .write("Training Plan.md", "# A new, different note\n");
        let reply = rpc(
            &server,
            event(
                &rig.notes_path("Training Plan.md"),
                &rig.notes_path("Plan.md"),
            ),
        )
        .await;
        assert_eq!(
            reply["result"],
            json!({"status": "nothing_to_fix"}),
            "{reply}"
        );
        assert!(rig.text("Goals.md").contains("[[Training Plan|plan]]"));
        assert!(rig.log().is_empty());
    });
}

#[test]
fn nothing_to_fix_for_equal_paths_missing_targets_outside_moves_and_ignored_dirs() {
    let rig = Rig::new(&GP1_FILES);
    rig.notes.write("node_modules/pkg/a.md", "# a\n");
    rig.notes.write("Keep.md", "# Keep\n");
    rig.registry.rescan("notes").unwrap();
    runtime().block_on(async {
        let server = serve(&rig).await;

        // Equal paths: nothing is written.
        let same = rpc(
            &server,
            event(&rig.notes_path("Keep.md"), &rig.notes_path("Keep.md")),
        )
        .await;
        assert_eq!(
            same["result"],
            json!({"status": "nothing_to_fix"}),
            "{same}"
        );

        // The new file is not on disk and the log does not say where it went.
        let missing = rpc(
            &server,
            event(
                &rig.notes_path("Training Plan.md"),
                &rig.notes_path("Nowhere.md"),
            ),
        )
        .await;
        assert_eq!(
            missing["result"],
            json!({"status": "nothing_to_fix"}),
            "{missing}"
        );
        assert!(rig.text("Goals.md").contains("[[Training Plan|plan]]"));

        // An ignored directory.
        rig.mv("node_modules/pkg/a.md", "node_modules/pkg/b.md");
        let ignored = rpc(
            &server,
            event(
                &rig.notes_path("node_modules/pkg/a.md"),
                &rig.notes_path("node_modules/pkg/b.md"),
            ),
        )
        .await;
        assert_eq!(
            ignored["result"],
            json!({"status": "nothing_to_fix"}),
            "{ignored}"
        );
        assert!(rig.log().is_empty(), "{:?}", rig.log());

        // Moved outside every source: nothing to fix, and the log says so.
        let to = rig.outside.root().join("Training Plan.md");
        fs::rename(rig.notes.root().join("Training Plan.md"), &to).unwrap();
        let outside = rpc(
            &server,
            event(
                &rig.notes_path("Training Plan.md"),
                &to.display().to_string(),
            ),
        )
        .await;
        assert_eq!(
            outside["result"],
            json!({"status": "nothing_to_fix"}),
            "{outside}"
        );
        let log = rig.log();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].kind, "uncertain");
        assert_eq!(log[0].old_path.as_deref(), Some("Training Plan.md"));
        assert!(
            log[0]
                .detail_json
                .as_deref()
                .unwrap()
                .contains("vanished, moved outside sources"),
            "{:?}",
            log[0].detail_json
        );
        // Sent again: still one entry.
        let _ = rpc(
            &server,
            event(
                &rig.notes_path("Training Plan.md"),
                &to.display().to_string(),
            ),
        )
        .await;
        assert_eq!(rig.log().len(), 1);
    });
}

#[test]
fn a_path_in_no_source_is_a_refusal_with_a_stable_reason_never_an_acknowledgement() {
    let rig = Rig::new(&GP1_FILES);
    rig.outside.write("Stray.md", "# Stray\n");
    runtime().block_on(async {
        let server = serve(&rig).await;
        let stray = rig.outside.root().join("Stray.md").display().to_string();
        let gone = rig.outside.root().join("Gone.md").display().to_string();
        for params in [
            event(&gone, &stray),
            event(&gone, &rig.notes_path("Goals.md")),
        ] {
            let reply = rpc(&server, params).await;
            assert!(reply.get("result").is_none(), "{reply}");
            assert_eq!(reply["error"]["code"], json!(-32011), "{reply}");
            assert_eq!(
                reply["error"]["data"],
                json!({"reason": "not_inside_a_source"}),
                "{reply}"
            );
        }
        assert!(rig.log().is_empty());
    });
}

#[test]
fn bad_parameters_are_invalid_params_and_an_unauthenticated_call_is_401() {
    let rig = Rig::new(&GP1_FILES);
    runtime().block_on(async {
        let server = serve(&rig).await;
        let good = rig.notes_path("Goals.md");
        for params in [
            json!({}),
            json!({"old_path": good}),
            json!({"new_path": good}),
            json!({"old_path": "", "new_path": good}),
            json!({"old_path": good, "new_path": ""}),
            json!({"old_path": "Goals.md", "new_path": good}),
            json!({"old_path": good, "new_path": "relative/Aims.md"}),
            json!({"old_path": 7, "new_path": good}),
            json!({"old_path": null, "new_path": good}),
            json!({"old_path": good, "new_path": format!("{good}\u{0}")}),
        ] {
            let reply = rpc(&server, params.clone()).await;
            assert_eq!(reply["error"]["code"], json!(-32602), "{params}: {reply}");
        }

        let body = json!({"jsonrpc": "2.0", "id": 1, "method": "link.rename_event",
            "params": event(&good, &good)});
        let none = reqwest::Client::new()
            .post(server.http_url())
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(none.status(), reqwest::StatusCode::UNAUTHORIZED);
        let wrong = reqwest::Client::new()
            .post(server.http_url())
            .bearer_auth("0".repeat(64))
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
        assert!(rig.log().is_empty());
    });
}

#[test]
fn a_folder_rename_counts_every_file_rewritten_once() {
    let rig = Rig::new(&[
        ("Ideas/One.md", "# One\n"),
        ("Ideas/Two.md", "# Two\n\nSee [[One]].\n"),
        ("Home.md", "[[Ideas/One]] and [[Ideas/Two]]\n"),
        ("Log.md", "[[Ideas/One|first]]\n"),
        ("Unrelated.md", "# Unrelated\n"),
    ]);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Ideas", "Notions");
        let params = event(&rig.notes_path("Ideas"), &rig.notes_path("Notions"));
        let reply = rpc(&server, params.clone()).await;
        assert_eq!(reply["result"]["status"], json!("applied"), "{reply}");
        assert_eq!(reply["result"]["files_changed"], json!(2), "{reply}");
        assert_eq!(rig.text("Home.md"), "[[Notions/One]] and [[Notions/Two]]\n");
        assert_eq!(rig.text("Log.md"), "[[Notions/One|first]]\n");
        assert_eq!(rig.log().len(), 1);

        let again = rpc(&server, params).await;
        assert_eq!(
            again["result"],
            json!({"status": "already_applied"}),
            "{again}"
        );
        assert_eq!(rig.log().len(), 1);
        assert!(exists(rig.notes.root(), "Notions/One.md"));
    });
}

#[test]
fn non_canonical_and_symlinked_paths_are_canonicalised_like_source_register() {
    let rig = Rig::new(&GP1_FILES);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Training Plan.md", "Plan.md");
        // `.` and `..` segments, and a doubled slash, in both paths.
        let root = rig.notes.root().display().to_string();
        let old = format!("{root}/Ideas/../Training Plan.md");
        let new = format!("{root}//./Plan.md");
        let reply = rpc(&server, event(&old, &new)).await;
        assert_eq!(
            reply["result"],
            json!({"status": "applied", "files_changed": 3}),
            "{reply}"
        );
    });
}

#[cfg(unix)]
#[test]
fn a_symlinked_route_to_the_source_is_the_same_source() {
    let rig = Rig::new(&GP1_FILES);
    let links = tempfile::tempdir().unwrap();
    let alias = links.path().join("alias");
    std::os::unix::fs::symlink(rig.notes.root(), &alias).unwrap();
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Training Plan.md", "Plan.md");
        let reply = rpc(
            &server,
            event(
                &alias.join("Training Plan.md").display().to_string(),
                &alias.join("Plan.md").display().to_string(),
            ),
        )
        .await;
        assert_eq!(
            reply["result"],
            json!({"status": "applied", "files_changed": 3}),
            "{reply}"
        );
    });
}

#[test]
fn a_case_only_rename_is_a_fix_on_a_case_insensitive_disk_and_on_a_case_sensitive_one() {
    let rig = Rig::new(&[
        ("Plan.md", "# Plan\n"),
        ("Home.md", "See [[Plan]], [[Plan|p]] and [[PLAN]].\n"),
    ]);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Plan.md", "plan.md");
        let ev = event(&rig.notes_path("Plan.md"), &rig.notes_path("plan.md"));
        let reply = rpc(&server, ev.clone()).await;
        // Applied whether or not the old spelling still stats (it does
        // on a case-insensitive disk such as this Mac's); the link of
        // the wrong case did not resolve before and is left alone.
        assert_eq!(
            reply["result"],
            json!({"status": "applied", "files_changed": 1}),
            "{reply}"
        );
        assert_eq!(
            rig.text("Home.md"),
            "See [[plan]], [[plan|p]] and [[PLAN]].\n"
        );
        let log = rig.log();
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(log[0].old_path.as_deref(), Some("Plan.md"));
        assert_eq!(log[0].new_path.as_deref(), Some("plan.md"));

        // A repeat is answered from the log, and writes nothing.
        let again = rpc(&server, ev).await;
        assert_eq!(
            again["result"],
            json!({"status": "already_applied"}),
            "{again}"
        );
        assert_eq!(rig.log().len(), 1);
        assert_eq!(
            rig.text("Home.md"),
            "See [[plan]], [[plan|p]] and [[PLAN]].\n"
        );
    });
}

#[test]
fn a_case_only_folder_rename_fixes_the_links_to_its_files() {
    let rig = Rig::new(&[
        ("Notes/Plan.md", "# Plan\n"),
        ("Home.md", "See [[Notes/Plan]].\n"),
    ]);
    runtime().block_on(async {
        let server = serve(&rig).await;
        rig.mv("Notes", "notes");
        let reply = rpc(
            &server,
            event(&rig.notes_path("Notes"), &rig.notes_path("notes")),
        )
        .await;
        assert_eq!(
            reply["result"],
            json!({"status": "applied", "files_changed": 1}),
            "{reply}"
        );
        assert_eq!(rig.text("Home.md"), "See [[notes/Plan]].\n");
        assert_eq!(rig.log().len(), 1);
    });
}
