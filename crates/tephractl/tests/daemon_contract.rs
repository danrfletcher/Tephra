//! The strings `tephractl` and `tephrad` must agree on, though they
//! share no code (tephrad is a dev-dependency here only).

#[test]
fn the_fix_line_and_error_kind_match_what_tephrad_writes() {
    assert_eq!(
        tephractl::daemon::FIX_LINE,
        tephrad::access::fix_line("~/Documents")
    );
    assert_eq!(
        tephractl::daemon::ERROR_KIND_PERMISSION_DENIED,
        tephrad::access::ERROR_KIND_PERMISSION_DENIED
    );
}

/// `access.json` as tephrad really writes it is what status reads.
#[test]
fn tephractl_reads_the_access_json_tephrad_writes() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap(); // no Documents inside
    let report = tephrad::access::probe_and_record(dir.path(), Some(home.path())).unwrap();
    let read = tephractl::daemon::status::read_access(dir.path()).expect("readable");
    assert_eq!(read.pid, report.pid);
    assert!(!read.documents_readable);
    assert_eq!(read.error_kind.as_deref(), Some("not_found"));
}

// ---- `source remove` against a real tephrad with a GitHub source (atlas-27 PR-13) -------

mod remove_contract {
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command as Process;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::{json, Value};
    use tephra_cache::Cache;
    use tephra_graph::UnitGraph;
    use tephra_remote::credentials::{CredentialError, CredentialStore, Provider, Secret};
    use tephra_remote::testing::FakeGithub;
    use tephra_remote::{AdapterOptions, SystemClock};
    use tephractl::cli::Command;
    use tephractl::commands;
    use tephractl::transport::RpcClient;
    use tephrad::refactor::{RefactorEngine, WatcherPauseControl};
    use tephrad::registry::SourceRegistry;
    use tephrad::remote_github::RemoteHub;
    use tephrad::rpc::backend::{
        GraphBackend, GraphRefresh, RefactorBackend, SharedGraph, SourceBackend,
    };
    use tephrad::rpc::server::{build_router, AppState};

    const REPO: &str = "danrfletcher/atlas";

    struct Store(Mutex<HashMap<Provider, String>>);

    impl CredentialStore for Store {
        fn get(&self, p: Provider) -> Result<Option<Secret>, CredentialError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(&p)
                .map(|v| Secret::new(v.clone())))
        }
        fn set(&self, p: Provider, s: &Secret) -> Result<(), CredentialError> {
            self.0.lock().unwrap().insert(p, s.expose().to_string());
            Ok(())
        }
        fn delete(&self, p: Provider) -> Result<(), CredentialError> {
            self.0.lock().unwrap().remove(&p);
            Ok(())
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Process::new("git")
            .args(["-c", "user.name=Dan", "-c", "user.email=dan@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "protocol.file.allow=always",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The GitHub token the daemon holds; it must be in nothing a removal prints.
    const SENTINEL: &str = "ghp_SENTINEL_contract_token_do_not_leak";

    #[test]
    fn a_github_source_with_an_edit_is_refused_then_removed_with_force() {
        // The fake GitHub, on a runtime of its own.
        let fake_rt = tokio::runtime::Runtime::new().unwrap();
        let gh = fake_rt.block_on(FakeGithub::start());
        gh.add_repo(REPO, "main");
        gh.put_file(REPO, "main", "README.md", b"# Atlas\n");
        let data = tempfile::tempdir().unwrap();
        let data_dir = data.path().to_path_buf();
        let token_path = data_dir.join("token");
        let token = "d".repeat(64);
        std::fs::write(&token_path, &token).unwrap();

        let store = Arc::new(Store(Mutex::new(HashMap::from([(
            Provider::Github,
            SENTINEL.to_string(),
        )]))));
        let registry =
            Arc::new(SourceRegistry::new(Cache::open(data_dir.join("cache.db")).unwrap()).unwrap());
        let hub = RemoteHub::with_options(
            store,
            Some(gh.base_url()),
            Arc::new(SystemClock),
            data_dir.clone(),
            AdapterOptions::default(),
        )
        .expect("hub");
        registry.set_remote_hub(Arc::new(hub));

        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = {
            let registry = registry.clone();
            let cache_path = data_dir.join("cache.db");
            let token_path = token_path.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async move {
                    let graph_cache = Cache::open(&cache_path).unwrap();
                    let shared = Arc::new(SharedGraph::new(
                        UnitGraph::build_from_cache(&graph_cache).unwrap(),
                    ));
                    let refresh = GraphRefresh::new(shared.clone(), cache_path);
                    let engine = RefactorEngine::new(
                        shared.clone() as Arc<dyn GraphBackend>,
                        registry.clone() as Arc<dyn SourceBackend>,
                        registry.clone() as Arc<dyn WatcherPauseControl>,
                    );
                    let state = Arc::new(AppState::new(
                        token_path,
                        shared as Arc<dyn GraphBackend>,
                        registry as Arc<dyn SourceBackend>,
                        Arc::new(engine) as Arc<dyn RefactorBackend>,
                        Some(refresh),
                    ));
                    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                    addr_tx.send(listener.local_addr().unwrap()).unwrap();
                    let task = tokio::spawn(async move {
                        let _ = axum::serve(listener, build_router(state)).await;
                    });
                    let _ = stop_rx.await;
                    task.abort();
                });
                rt.shutdown_background();
            })
        };
        let addr = addr_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let client = RpcClient::new(addr.to_string(), token);

        let run = |command: Command, json: bool| {
            commands::execute(&command, json, &client, &mut std::io::empty())
        };
        let remove = |force: bool, json: bool| {
            run(
                Command::SourceRemove {
                    name: "g".to_string(),
                    force,
                },
                json,
            )
        };

        let added = run(
            Command::SourceAddGithub {
                name: "g".to_string(),
                repo: REPO.to_string(),
                branch: None,
            },
            true,
        );
        assert_eq!(added.exit_code, 0, "{}", added.stderr);
        let start = Instant::now();
        loop {
            let list: Value = client.call("source.list", json!({})).unwrap();
            if list[0]["status"] == json!("active") {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(30),
                "never active: {list}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }

        // A real clone with Dan's edit in it.
        let origin = tempfile::tempdir().unwrap();
        let seed = tempfile::tempdir().unwrap();
        git(origin.path(), &["init", "--bare", "-b", "main", "."]);
        git(seed.path(), &["init", "-b", "main", "."]);
        std::fs::write(seed.path().join("README.md"), "# Atlas\n").unwrap();
        git(seed.path(), &["add", "-A"]);
        git(seed.path(), &["commit", "-m", "first"]);
        git(
            seed.path(),
            &[
                "remote",
                "add",
                "origin",
                &origin.path().display().to_string(),
            ],
        );
        git(seed.path(), &["push", "origin", "main"]);
        let dir = data_dir.join("remote").join("g");
        std::fs::create_dir_all(&dir).unwrap();
        let clone = dir.join("clone");
        git(
            &data_dir,
            &[
                "clone",
                "-b",
                "main",
                &origin.path().display().to_string(),
                &clone.display().to_string(),
            ],
        );
        std::fs::write(clone.join("README.md"), "# Atlas\nmy edit\n").unwrap();
        let requests_before = gh.request_log().count();

        // Refused: exit 1, the count, the --force hint; nothing is gone.
        let refused = remove(false, false);
        assert_eq!(refused.exit_code, 1, "{}", refused.stderr);
        assert_eq!(refused.stdout, "");
        assert!(
            refused.stderr.contains("1 not pushed"),
            "{}",
            refused.stderr
        );
        assert!(refused.stderr.contains("--force"), "{}", refused.stderr);
        assert!(clone.join("README.md").is_file());
        assert_eq!(
            std::fs::read(clone.join("README.md")).unwrap(),
            b"# Atlas\nmy edit\n"
        );
        let list: Value = client.call("source.list", json!({})).unwrap();
        assert_eq!(list[0]["name"], json!("g"));

        // The --json form is one object, same exit code.
        let refused_json = remove(false, true);
        assert_eq!(refused_json.exit_code, 1);
        let object: Value = serde_json::from_str(&refused_json.stdout).unwrap();
        assert_eq!(object["reason"], json!("unsent_work"));
        assert_eq!(object["not_pushed"], json!(1));

        // Forced: removed, everything gone.
        let forced = remove(true, false);
        assert_eq!(forced.exit_code, 0, "{}", forced.stderr);
        assert_eq!(forced.stdout, "Removed source \"g\"\n");
        let list: Value = client.call("source.list", json!({})).unwrap();
        assert_eq!(list, json!([]));
        assert!(!dir.exists(), "remote/g/ is gone");
        assert_eq!(
            gh.request_log().count(),
            requests_before,
            "neither the refusal nor the removal asked GitHub anything"
        );

        // F9: the token is in no output of the command, rendered or `--json`.
        for output in [&refused, &refused_json, &forced] {
            for text in [&output.stdout, &output.stderr] {
                assert!(!text.contains(SENTINEL), "{text}");
                assert!(!text.contains("Bearer"), "{text}");
            }
        }

        let _ = stop_tx.send(());
        server.join().unwrap();
    }
}
