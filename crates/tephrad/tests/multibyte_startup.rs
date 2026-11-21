//! E26 end to end, on the real `tephrad` binary: a source holding file
//! names with multi-byte characters close to the extension (`Résumé—v2.docx`,
//! `Todo…ab`, `日本x`, `Plan…ab.md`) registers and scans to completion,
//! answers `source.list`, `graph.get_backlinks` and `source.rescan`, and
//! the process stays up with no `panicked` on its stderr. On `dev` the scan
//! hit a char-boundary panic in `strip_markdown_extension` and the source
//! never finished (or the daemon died).
//!
//! Real daemon (`CARGO_BIN_EXE_tephrad`, an ephemeral port, a throwaway
//! `HOME`), real parser, real graph, real files in a temp dir: no mocks.

mod fixtures;

use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

/// A running `tephrad` with its stderr (and stdout) drained into
/// buffers for its whole life, and its own PID stopped on drop.
struct Daemon {
    child: Child,
    port: u16,
    token: String,
    stderr: Arc<Mutex<Vec<String>>>,
    stdout: Arc<Mutex<Vec<String>>>,
    _home: tempfile::TempDir,
}

fn drain<R: std::io::Read + Send + 'static>(pipe: R) -> Arc<Mutex<Vec<String>>> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => sink.lock().unwrap().push(line.trim_end().to_string()),
            }
        }
    });
    lines
}

impl Daemon {
    fn start() -> Self {
        let home = tempfile::tempdir().expect("home dir");
        let config_path = home.path().join(".config/tephra/config.toml");
        let data_dir = home.path().join(".local/share/tephra");
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::write(
            &config_path,
            format!(
                "listen_addr = \"127.0.0.1:0\"\ndata_dir = \"{}\"\nlog_level = \"info\"\n",
                data_dir.display()
            ),
        )
        .unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_tephrad"))
            .arg("--config")
            .arg(&config_path)
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tephrad");
        let stdout = drain(child.stdout.take().unwrap());
        let stderr = drain(child.stderr.take().unwrap());

        let deadline = Instant::now() + Duration::from_secs(20);
        let port = loop {
            let found = stderr.lock().unwrap().iter().find_map(|l| {
                let at = l.find("listening on 127.0.0.1:")?;
                l[at + "listening on 127.0.0.1:".len()..]
                    .split(|c: char| !c.is_ascii_digit())
                    .next()?
                    .parse::<u16>()
                    .ok()
            });
            if let Some(port) = found {
                break port;
            }
            assert!(
                Instant::now() < deadline,
                "tephrad never logged its port: {:?}",
                stderr.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let token = fs::read_to_string(data_dir.join("token"))
            .expect("token file")
            .trim()
            .to_string();
        Daemon {
            child,
            port,
            token,
            stderr,
            stdout,
            _home: home,
        }
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> serde_json::Value {
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{}/rpc", self.port))
            .bearer_auth(&self.token)
            .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
            .send()
            .await
            .unwrap_or_else(|e| panic!("{method}: the connection dropped: {e}"))
            .json::<serde_json::Value>()
            .await
            .unwrap_or_else(|e| panic!("{method}: no JSON reply: {e}"))
    }

    /// Polls `source.list` until `name` is `active` with a finished scan.
    async fn wait_for_scan(&self, name: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let list = self.call("source.list", json!({})).await;
            let row = list["result"]
                .as_array()
                .and_then(|rows| rows.iter().find(|r| r["name"] == json!(name)))
                .cloned();
            if let Some(row) = row {
                if row["status"] == json!("active") && !row["last_scanned_at"].is_null() {
                    return row;
                }
            }
            assert!(
                Instant::now() < deadline,
                "source {name} never finished scanning: {list:?}\n{}",
                self.panics_report()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn panics_report(&self) -> String {
        format!("stderr: {:?}", self.stderr.lock().unwrap())
    }

    /// The process is alive and nothing it printed says `panicked`.
    fn assert_healthy(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "tephrad exited: {}",
            self.panics_report()
        );
        for line in self
            .stderr
            .lock()
            .unwrap()
            .iter()
            .chain(self.stdout.lock().unwrap().iter())
        {
            assert!(!line.contains("panicked"), "tephrad panicked: {line}");
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Only our own child.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn multibyte_named_source_registers_scans_and_answers() {
    let mut daemon = Daemon::start();
    let notes = fixtures::multibyte_source();

    let register = daemon
        .call(
            "source.register",
            json!({"name": "notes", "root": notes.root().to_string_lossy()}),
        )
        .await;
    assert!(register.get("error").is_none(), "{register:?}");
    daemon.wait_for_scan("notes").await;
    daemon.assert_healthy();

    // Every crash-class name is a unit, and the linker's link to it counts.
    for (name, _) in fixtures::MULTIBYTE_NAMES {
        let backlinks = daemon
            .call(
                "graph.get_backlinks",
                json!({"id": format!("@notes:{name}")}),
            )
            .await;
        assert!(backlinks.get("error").is_none(), "{name}: {backlinks:?}");
        let from: Vec<&str> = backlinks["result"]
            .as_array()
            .unwrap_or_else(|| panic!("{name}: {backlinks:?}"))
            .iter()
            .map(|e| e["from"].as_str().unwrap())
            .collect();
        assert_eq!(from, vec!["@notes:Linker—.md"], "{name}");
    }

    // A forced rescan walks the same names again.
    let rescan = daemon.call("source.rescan", json!({"name": "notes"})).await;
    assert!(rescan.get("error").is_none(), "{rescan:?}");
    daemon.wait_for_scan("notes").await;
    let backlinks = daemon
        .call("graph.get_backlinks", json!({"id": "@notes:Plan…ab.md"}))
        .await;
    assert_eq!(backlinks["result"].as_array().unwrap().len(), 1);

    daemon.assert_healthy();
}

/// Two sources, one holding the crash-class names: the other still scans
/// and answers `graph.get_backlinks`.
#[tokio::test]
async fn a_multibyte_source_does_not_take_another_source_down() {
    let mut daemon = Daemon::start();
    let multibyte = fixtures::multibyte_source();
    let plain = fixtures::empty_source();
    plain.write("Target.md", "# Target\n");
    plain.write("Hub.md", "Goes to [[Target]].\n");

    for (name, root) in [("multibyte", &multibyte), ("plain", &plain)] {
        let register = daemon
            .call(
                "source.register",
                json!({"name": name, "root": root.root().to_string_lossy()}),
            )
            .await;
        assert!(register.get("error").is_none(), "{name}: {register:?}");
    }
    daemon.wait_for_scan("multibyte").await;
    daemon.wait_for_scan("plain").await;

    let backlinks = daemon
        .call("graph.get_backlinks", json!({"id": "@plain:Target.md"}))
        .await;
    let from: Vec<&str> = backlinks["result"]
        .as_array()
        .unwrap_or_else(|| panic!("{backlinks:?}"))
        .iter()
        .map(|e| e["from"].as_str().unwrap())
        .collect();
    assert_eq!(from, vec!["@plain:Hub.md"]);
    let backlinks = daemon
        .call("graph.get_backlinks", json!({"id": "@multibyte:Todo…ab"}))
        .await;
    assert_eq!(backlinks["result"].as_array().unwrap().len(), 1);

    daemon.assert_healthy();
}

/// E26: invalid UTF-8 on disk is not newly broken. The scan skips a file
/// whose own name is not UTF-8 (`scan::to_rel_string` covers this at unit
/// level) and indexes the rest; the crash-class names around it are
/// indexed as well. APFS refuses to create such a name, so on macOS this
/// test is reported as ignored rather than passing without checking
/// anything; it runs on Linux. The unit tests in `scan.rs`
/// (`to_rel_string_rejects_a_path_with_a_non_utf8_component`) cover the
/// same rule on every platform.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "this filesystem refuses a non-UTF-8 file name; runs on Linux"
)]
fn a_non_utf8_file_name_is_skipped_as_before_and_the_rest_is_indexed() {
    use std::os::unix::ffi::OsStrExt;

    let source = fixtures::multibyte_source();
    let bad = source
        .root()
        .join(std::ffi::OsStr::from_bytes(b"bad\xff.md"));
    if fs::write(&bad, "# bad name\n").is_err() {
        eprintln!("skipped: this filesystem refuses a non-UTF-8 file name");
        return;
    }
    let cache_dir = tempfile::tempdir().unwrap();
    let cache_path = cache_dir.path().join("cache.db");
    let registry =
        tephrad::registry::SourceRegistry::new(tephra_cache::Cache::open(&cache_path).unwrap())
            .unwrap();
    registry.register("notes", source.root()).expect("register");

    let cache = tephra_cache::Cache::open(&cache_path).unwrap();
    let row = cache.get_source("notes").unwrap().expect("source row");
    for (name, _) in fixtures::MULTIBYTE_NAMES {
        assert!(
            cache.get_unit_by_rel_path(row.id, name).unwrap().is_some(),
            "{name} is indexed"
        );
    }
    assert!(cache
        .get_unit_by_rel_path(row.id, "bad\u{fffd}.md")
        .unwrap()
        .is_none());
}
