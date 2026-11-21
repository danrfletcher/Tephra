//! G19 / FR17: the rename race tests, on real inotify, against the real
//! `tephrad` binary. Atlas's PR-10 tests reach Tephra over RPC; this file
//! does the same, from outside the process.
//!
//! Each test starts its own `tephrad` (the `TEPHRAD_BIN` environment
//! variable names one to run, else `CARGO_BIN_EXE_tephrad`) with a config
//! file, its own `XDG_DATA_HOME` and `HOME`, and an ephemeral loopback
//! port, over a temp vault with three linking files (`[[Plan]]`,
//! `[[Plan|plan]]`, `![[Plan]]` after the fix). The renames are real
//! `rename(2)` calls on disk, so the watcher side is the real inotify
//! adapter; the exact event is the real `link.rename_event` RPC (no
//! test-only method exists, F12). Answers are read over HTTP with the
//! bearer token; pushes over a WebSocket.
//!
//! Linux only: FSEvents, Finder and `git pull` renames cannot be proved
//! here and stay with a person (G19). On macOS this file compiles to
//! nothing and `cargo test -p tephrad --test rename_races` runs zero
//! tests. Run it where it counts with `scripts/linux-tephrad.sh test`.
//!
//! Timing: nothing sleeps for a fixed time to wait for the daemon. Waits
//! poll with a deadline ([`WAIT`]). The only quiet periods are the ones
//! that prove a push or a fix did NOT happen, and they are derived from
//! the watcher's own constants ([`QUIET`], [`HOLD_WITH_MARGIN`]).
//!
//! `RACE_REPEAT=<n>` runs the body of every test `n` times, each time on
//! a new daemon and a new vault ([`repeat`]): the flake check.
#![cfg(target_os = "linux")]

use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier, Mutex};
use std::time::{Duration, Instant, SystemTime};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tephrad::watcher::{DEBOUNCE_MAX_WAIT, DEBOUNCE_WINDOW, RENAME_HOLD};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------
// Timing
// ---------------------------------------------------------------

/// How long any wait for the daemon may take before the test fails.
const WAIT: Duration = Duration::from_secs(30);

/// How long a connection or a log has to stay unchanged before a test
/// says "nothing else happened": a whole debounce, so a watcher batch
/// that was going to come has come.
const QUIET: Duration = Duration::from_millis(
    DEBOUNCE_WINDOW.as_millis() as u64 + DEBOUNCE_MAX_WAIT.as_millis() as u64,
);

/// A hash pair is applied only after the hold (G2); this is the hold plus
/// a debounce, the latest a held pair is applied.
const HOLD_WITH_MARGIN: Duration =
    Duration::from_millis(RENAME_HOLD.as_millis() as u64 + DEBOUNCE_MAX_WAIT.as_millis() as u64);

// ---------------------------------------------------------------
// The vault
// ---------------------------------------------------------------

const OLD: &str = "Training Plan.md";
const NEW: &str = "Plan.md";
const PLAN_BODY: &str = "# Training Plan\n\n## Goals\n\nRun.\n";

/// The three linking files, before and after the rename.
const LINKERS: [(&str, &str, &str); 3] = [
    (
        "Weekly Review.md",
        "# Weekly Review\n\nThis week: [[Training Plan]].\n",
        "# Weekly Review\n\nThis week: [[Plan]].\n",
    ),
    (
        "Goals.md",
        "# Goals\n\nSee [[Training Plan|plan]] first.\n",
        "# Goals\n\nSee [[Plan|plan]] first.\n",
    ),
    (
        "Ideas/Rename Notes.md",
        "# Rename notes\n\n![[Training Plan]]\n",
        "# Rename notes\n\n![[Plan]]\n",
    ),
];

/// The file the warm-up rewrites until the watcher is seen to be live. It
/// stays in the vault, so it never vanishes and is never logged.
const WARMUP: &str = "Warmup.md";

/// Filesystems that are a share from the host, on which host-made changes
/// do not reliably raise inotify events. A vault there would make a test
/// pass or fail for reasons that have nothing to do with Tephra.
const HOST_SHARE_FS: [&str; 9] = [
    "virtiofs",
    "9p",
    "grpcfuse",
    "osxfs",
    "vboxsf",
    "nfs",
    "nfs4",
    "cifs",
    "fuse.osxfs",
];

/// Panics when `path` is on a host share (a bind mount of the host).
fn assert_not_a_host_share(path: &Path) {
    let mountinfo = fs::read_to_string("/proc/self/mountinfo").expect("/proc/self/mountinfo");
    let path = path.canonicalize().expect("canonical vault path");
    let mut best: Option<(usize, String)> = None;
    for line in mountinfo.lines() {
        // `id parent maj:min root mountpoint opts [optional...] - fstype source superopts`
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let mountpoint = before.split(' ').nth(4).unwrap_or("");
        let fstype = after.split(' ').next().unwrap_or("").to_string();
        if path.starts_with(mountpoint) && best.as_ref().is_none_or(|(n, _)| mountpoint.len() >= *n)
        {
            best = Some((mountpoint.len(), fstype));
        }
    }
    let fstype = best.map(|(_, f)| f).unwrap_or_default();
    assert!(
        !HOST_SHARE_FS.contains(&fstype.as_str()) && !fstype.starts_with("fuse"),
        "{} is on a {fstype} mount, a share from the host: inotify does not reliably see \
         host-made changes there. Point TMPDIR at the container's own filesystem (for example /tmp).",
        path.display()
    );
}

// ---------------------------------------------------------------
// A real daemon
// ---------------------------------------------------------------

fn tephrad_bin() -> PathBuf {
    match std::env::var_os("TEPHRAD_BIN") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(env!("CARGO_BIN_EXE_tephrad")),
    }
}

/// A running `tephrad` and everything it was given. Dropping it stops
/// exactly this child, by its own handle, and the temp dir goes with it
/// (also when the test panics).
struct Daemon {
    child: Child,
    port: u16,
    token: String,
    stderr: Arc<Mutex<Vec<String>>>,
    rt: Arc<tokio::runtime::Runtime>,
    http: reqwest::Client,
    home: tempfile::TempDir,
    config: PathBuf,
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

/// What a launched `tephrad` gives back.
struct Launched {
    child: Child,
    port: u16,
    token: String,
    stderr: Arc<Mutex<Vec<String>>>,
}

/// Starts `tephrad` over `config` with its own `HOME`, `XDG_DATA_HOME`
/// and `XDG_CONFIG_HOME` under `home`, and waits for its `listening on`
/// line, which names the ephemeral port it was given.
fn launch(home: &Path, config: &Path) -> Launched {
    let data_home = home.join("xdg-data");
    let mut child = Command::new(tephrad_bin())
        .arg("--config")
        .arg(config)
        .env("HOME", home)
        .env("XDG_DATA_HOME", &data_home)
        .env("XDG_CONFIG_HOME", home.join("xdg-config"))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn {}: {e}", tephrad_bin().display()));
    let stderr = drain(child.stderr.take().unwrap());
    let deadline = Instant::now() + WAIT;
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
        if let Some(status) = child.try_wait().expect("try_wait") {
            panic!(
                "tephrad exited early ({status}): {:?}",
                stderr.lock().unwrap()
            );
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "tephrad never logged its port: {:?}",
                stderr.lock().unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let token = fs::read_to_string(data_home.join("tephra/token"))
        .expect("token file")
        .trim()
        .to_string();
    Launched {
        child,
        port,
        token,
        stderr,
    }
}

impl Daemon {
    /// A new daemon over a new, empty home. The vault is made inside it.
    fn start() -> Daemon {
        let home = tempfile::tempdir().expect("home dir");
        assert_not_a_host_share(home.path());
        let config = home.path().join("xdg-config/tephra/config.toml");
        let data_home = home.path().join("xdg-data");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            format!(
                "listen_addr = \"127.0.0.1:0\"\ndata_dir = \"{}\"\nlog_level = \"info\"\n",
                data_home.join("tephra").display()
            ),
        )
        .unwrap();
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime"),
        );
        let launched = launch(home.path(), &config);
        Daemon {
            child: launched.child,
            port: launched.port,
            token: launched.token,
            stderr: launched.stderr,
            rt,
            http: reqwest::Client::new(),
            home,
            config,
        }
    }

    /// Starts the process again over the same config, data dir and vault
    /// (after [`Daemon::stop`]).
    fn spawn(&mut self) {
        let launched = launch(self.home.path(), &self.config);
        self.child = launched.child;
        self.port = launched.port;
        self.token = launched.token;
        self.stderr = launched.stderr;
    }

    /// Stops the process with SIGTERM (its own graceful path) and waits
    /// for it: the daemon is down, as when a person quits it.
    fn stop(&mut self) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: the pid is our own child's, which has not been reaped.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + WAIT;
        loop {
            if self.child.try_wait().expect("try_wait").is_some() {
                return;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "tephrad did not stop on SIGTERM: {:?}",
                    self.stderr.lock().unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn vault(&self) -> PathBuf {
        self.home.path().join("vault")
    }

    /// Writes the vault: the target, the three linking files, the warm-up
    /// file. Not yet registered.
    fn write_vault(&self) {
        let vault = self.vault();
        let mut files: Vec<(&str, &str)> = vec![(OLD, PLAN_BODY), (WARMUP, "# Warm-up 0\n")];
        files.extend(LINKERS.iter().map(|(rel, before, _)| (*rel, *before)));
        for (rel, text) in files {
            let full = vault.join(rel);
            fs::create_dir_all(full.parent().unwrap()).unwrap();
            fs::write(full, text).unwrap();
        }
        assert_not_a_host_share(&vault);
    }

    fn rpc(&self) -> Rpc {
        Rpc {
            rt: self.rt.clone(),
            http: self.http.clone(),
            url: format!("http://127.0.0.1:{}/rpc", self.port),
            token: self.token.clone(),
        }
    }

    fn call(&self, method: &str, params: Value) -> Value {
        self.rpc().call(method, params)
    }

    /// `link.rename_event` for `old` to `new` in the vault; the reply.
    fn event(&self, old: &str, new: &str) -> Value {
        self.rpc().event(&self.vault(), old, new)
    }

    /// The WebSocket, connected and known to be subscribed to pushes.
    fn connect(&self) -> Ws {
        let mut request = format!("ws://127.0.0.1:{}/rpc", self.port)
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
        let mut ws = Ws {
            rt: self.rt.clone(),
            ws,
            pending: VecDeque::new(),
            next_id: 1,
        };
        // A reply proves the server is in its loop, so subscribed.
        let reply = ws.call("graph.get_edges", json!({}));
        assert!(reply.get("error").is_none(), "{reply}");
        ws
    }

    /// Registers the vault as source `notes`, waits for its scan and for
    /// the watcher to be seen live (a rewritten file raises a push), and
    /// returns the WebSocket, which saw all of that.
    fn register_and_warm_up(&self) -> Ws {
        let ws = self.connect();
        let reply = self.call(
            "source.register",
            json!({"name": "notes", "root": self.vault().to_string_lossy()}),
        );
        assert!(reply.get("error").is_none(), "{reply}\n{}", self.report());
        self.wait_for_scan();
        self.warm_up(ws)
    }

    fn wait_for_scan(&self) {
        let deadline = Instant::now() + WAIT;
        loop {
            let list = self.call("source.list", json!({}));
            let row = list["result"]
                .as_array()
                .and_then(|rows| rows.iter().find(|r| r["name"] == json!("notes")))
                .cloned();
            if let Some(row) = row {
                if row["status"] == json!("active") && !row["last_scanned_at"].is_null() {
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "notes never finished scanning: {list}\n{}",
                self.report()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A watcher that has only just started can miss a change made at
    /// once, so a file is rewritten until its push is seen.
    fn warm_up(&self, mut ws: Ws) -> Ws {
        let deadline = Instant::now() + WAIT;
        let mut round = 0;
        while Instant::now() < deadline {
            round += 1;
            fs::write(self.vault().join(WARMUP), format!("# Warm-up {round}\n")).unwrap();
            if ws.next_push(Duration::from_millis(1500)).is_some() {
                // Let the batch that carried it finish before the test
                // starts: nothing it does shares a debounce with this.
                let _ = ws.drain(QUIET);
                return ws;
            }
        }
        panic!("the watcher never reported a change\n{}", self.report());
    }

    fn report(&self) -> String {
        let lines = self.stderr.lock().unwrap();
        let text = lines.join("\n");
        if text.contains("No space left on device") || text.contains("inotify") {
            format!(
                "the inotify limit may be too low: raise fs.inotify.max_user_watches \
                 (and fs.inotify.max_user_instances)\ntephrad stderr:\n{text}"
            )
        } else {
            format!("tephrad stderr:\n{text}")
        }
    }

    // --- the vault on disk ---

    /// `rename(2)` inside the vault, nothing else: what a shell `mv` or
    /// Finder does. The watcher sees it; Atlas's event is separate.
    fn mv(&self, old: &str, new: &str) {
        let to = self.vault().join(new);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(self.vault().join(old), to).expect("rename(2)");
    }

    fn text(&self, rel: &str) -> String {
        fs::read_to_string(self.vault().join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"))
    }

    fn mtimes(&self) -> Vec<(String, SystemTime)> {
        LINKERS
            .iter()
            .map(|(rel, _, _)| {
                let meta = fs::metadata(self.vault().join(rel)).unwrap();
                (rel.to_string(), meta.modified().unwrap())
            })
            .collect()
    }

    /// Every linking file reads as `after` (or as before, when `fixed` is
    /// false), exactly.
    fn assert_links(&self, target: &str, fixed: bool, context: &str) {
        for (rel, before, after) in LINKERS {
            let expected = if !fixed {
                before.to_string()
            } else {
                after.replace("Plan", target)
            };
            assert_eq!(self.text(rel), expected, "{context}: {rel}");
        }
    }

    // --- the log ---

    fn entries(&self) -> Vec<Value> {
        let reply = self.call("fix.list", json!({}));
        reply["result"]
            .as_array()
            .unwrap_or_else(|| panic!("fix.list: {reply}"))
            .clone()
    }

    fn entries_of(&self, kind: &str) -> Vec<Value> {
        self.entries()
            .into_iter()
            .filter(|e| e["kind"] == json!(kind))
            .collect()
    }

    /// Polls `fix.list` until `check` holds; the entries it held for.
    fn wait_for_entries(&self, what: &str, mut check: impl FnMut(&[Value]) -> bool) -> Vec<Value> {
        let deadline = Instant::now() + WAIT;
        loop {
            let entries = self.entries();
            if check(&entries) {
                return entries;
            }
            assert!(
                Instant::now() < deadline,
                "waited for {what}; the log is {entries:?}\n{}",
                self.report()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// The fix log is still exactly `before` after a quiet period: no
    /// late signal added or changed an entry.
    fn assert_log_stays(&self, before: &[Value], context: &str) {
        std::thread::sleep(QUIET);
        assert_eq!(self.entries(), before, "{context}: the log changed");
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Only our own child, by its own handle.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ---------------------------------------------------------------
// RPC and pushes
// ---------------------------------------------------------------

/// A cloneable HTTP JSON-RPC caller with the bearer token, so a second
/// thread can call at the same time.
#[derive(Clone)]
struct Rpc {
    rt: Arc<tokio::runtime::Runtime>,
    http: reqwest::Client,
    url: String,
    token: String,
}

impl Rpc {
    fn call(&self, method: &str, params: Value) -> Value {
        self.rt.block_on(async {
            self.http
                .post(&self.url)
                .bearer_auth(&self.token)
                .json(&json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}))
                .send()
                .await
                .unwrap_or_else(|e| panic!("{method}: the connection dropped: {e}"))
                .json::<Value>()
                .await
                .unwrap_or_else(|e| panic!("{method}: no JSON reply: {e}"))
        })
    }

    fn event(&self, vault: &Path, old: &str, new: &str) -> Value {
        let root = vault.canonicalize().expect("vault root");
        self.call(
            "link.rename_event",
            json!({
                "old_path": root.join(old).display().to_string(),
                "new_path": root.join(new).display().to_string(),
            }),
        )
    }
}

/// A fix-log entry whose work is finished and wrote all three linking
/// files. The entry is stored before its files are written (with the
/// work list), so seeing it in `fix.list` is not yet seeing the files
/// fixed: a test waits for this.
fn is_applied(entry: &Value) -> bool {
    entry["kind"] == json!("rename")
        && entry["status"] == json!("applied")
        && entry["files_changed"] == json!(3)
}

/// The label of a `link.rename_event` reply: `applied`, `partial`,
/// `already_applied` or `nothing_to_fix`. Panics on an error reply.
fn label(reply: &Value) -> String {
    assert!(reply.get("error").is_none(), "an error reply: {reply}");
    reply["result"]["status"]
        .as_str()
        .unwrap_or_else(|| panic!("no status in {reply}"))
        .to_string()
}

/// One authenticated WebSocket connection.
struct Ws {
    rt: Arc<tokio::runtime::Runtime>,
    ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    /// Pushes that came while a call waited for its reply.
    pending: VecDeque<Value>,
    next_id: u64,
}

impl Ws {
    fn frame(&mut self, timeout: Duration) -> Option<Value> {
        let next = self
            .rt
            .block_on(async { tokio::time::timeout(timeout, self.ws.next()).await })
            .ok()??
            .expect("not a transport error");
        let text = next.into_text().expect("a text frame").to_string();
        Some(serde_json::from_str(&text).expect("json"))
    }

    fn call(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        self.rt
            .block_on(self.ws.send(Message::Text(body.to_string().into())))
            .expect("send");
        loop {
            let frame = self.frame(WAIT).expect("a reply");
            if frame.get("id") == Some(&json!(id)) {
                return frame;
            }
            self.pending.push_back(frame);
        }
    }

    /// The next `graph:changed` push, or `None` when none comes in time.
    fn next_push(&mut self, timeout: Duration) -> Option<Value> {
        if let Some(frame) = self.pending.pop_front() {
            return Some(frame);
        }
        let frame = self.frame(timeout)?;
        assert_eq!(frame["method"], json!("graph:changed"), "{frame}");
        assert!(frame.get("id").is_none(), "a push is a notification");
        Some(frame)
    }

    /// Every push until the connection has been quiet for `quiet`.
    fn drain(&mut self, quiet: Duration) -> Vec<Value> {
        let mut pushes = Vec::new();
        while let Some(push) = self.next_push(quiet) {
            pushes.push(push);
        }
        pushes
    }

    /// Every `fixes` entry pushed until the connection is quiet.
    fn fixes(&mut self) -> Vec<Value> {
        self.drain(QUIET)
            .iter()
            .filter_map(|p| p["params"]["fixes"].as_array())
            .flatten()
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------
// Assertions every test shares
// ---------------------------------------------------------------

/// What every race test ends with: the files, the log and the push all
/// say one rename was fixed once.
fn assert_fixed_once(d: &Daemon, ws: &mut Ws, context: &str) {
    d.assert_links("Plan", true, context);
    let renames = d.entries_of("rename");
    assert_eq!(
        renames.len(),
        1,
        "{context}: one fix-log entry: {:?}",
        d.entries()
    );
    assert_eq!(d.entries().len(), 1, "{context}: nothing else was logged");
    let entry = &renames[0];
    assert_eq!(entry["old"], json!(OLD), "{context}");
    assert_eq!(entry["new"], json!(NEW), "{context}");
    assert_eq!(entry["sources"], json!(["notes"]), "{context}");
    assert_eq!(entry["files_changed"], json!(3), "{context}");
    assert_eq!(entry["status"], json!("applied"), "{context}");

    let pushed = ws.fixes();
    assert_eq!(
        pushed.len(),
        1,
        "{context}: one fixes entry pushed: {pushed:?}"
    );
    assert_eq!(
        pushed[0],
        json!({
            "kind": "rename", "source": "notes", "old": OLD, "new": NEW,
            "files_changed": 3, "status": "applied", "deferred": 0
        }),
        "{context}"
    );
}

/// Runs `body` as many times as `RACE_REPEAT` says (default once), each
/// time on a fresh daemon and vault: a flake shows as a failure of one
/// round, named.
fn repeat(body: impl Fn(usize)) {
    let rounds: usize = std::env::var("RACE_REPEAT")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n| *n >= 1)
        .unwrap_or(1);
    for round in 1..=rounds {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(round)));
        if let Err(cause) = result {
            eprintln!("round {round} of {rounds} failed");
            std::panic::resume_unwind(cause);
        }
    }
}

/// A fresh daemon over the vault, registered, scanned and warm.
fn world() -> (Daemon, Ws) {
    let d = Daemon::start();
    d.write_vault();
    let ws = d.register_and_warm_up();
    (d, ws)
}

// ---------------------------------------------------------------
// The six race tests (FR17)
// ---------------------------------------------------------------

/// Test 1, event first: the exact event lands before the watcher has
/// reported the rename. It answers `applied`; the watcher's later
/// pairing is a no-op.
#[test]
fn race_1_event_first_then_the_watchers_pairing_does_nothing() {
    repeat(|round| {
        let (d, mut ws) = world();
        d.mv(OLD, NEW);
        // Sent at once: the watcher's batch is a debounce away.
        let reply = d.event(OLD, NEW);
        assert_eq!(
            reply["result"],
            json!({"status": "applied", "files_changed": 3}),
            "round {round}: {reply}"
        );
        // The watcher reports the rename; a pairing for it is a no-op.
        let after_event = d.entries();
        d.assert_log_stays(&after_event, "event first");
        assert_fixed_once(&d, &mut ws, &format!("event first, round {round}"));
    });
}

/// Test 2, watcher first: the watcher fixes the rename with the OS pair
/// and no hold; the event that comes later is `already_applied`.
#[test]
fn race_2_watcher_first_then_the_event_is_already_applied() {
    repeat(|round| {
        let (d, mut ws) = world();
        let renamed_at = Instant::now();
        d.mv(OLD, NEW);
        d.wait_for_entries("the watcher's fix", |e| e.iter().any(is_applied));
        // An OS pair has no hold (G2): it lands well inside the hold that
        // a hash-only pair waits.
        assert!(
            renamed_at.elapsed() < RENAME_HOLD,
            "round {round}: the OS-paired rename took {:?}",
            renamed_at.elapsed()
        );
        let reply = d.event(OLD, NEW);
        assert_eq!(label(&reply), "already_applied", "round {round}: {reply}");
        assert_fixed_once(&d, &mut ws, &format!("watcher first, round {round}"));
    });
}

/// Test 3, both at once: the rename and the event are released from one
/// barrier. Whichever lands first, the links are rewritten once, there is
/// one entry, and the other signal is a no-op.
#[test]
fn race_3_both_at_once_rewrite_and_log_once() {
    repeat(|round| {
        let (d, mut ws) = world();
        let gate = Arc::new(Barrier::new(2));
        let renamer = {
            let (gate, from, to) = (gate.clone(), d.vault().join(OLD), d.vault().join(NEW));
            std::thread::spawn(move || {
                gate.wait();
                fs::rename(from, to).expect("rename(2)");
            })
        };
        let rpc = d.rpc();
        let vault = d.vault();
        gate.wait();
        let reply = rpc.event(&vault, OLD, NEW);
        renamer.join().unwrap();

        // If the event got in before the rename reached the disk there
        // was nothing for it to fix yet: a no-op, and the watcher fixes
        // the rename. Otherwise the event applied it.
        let first = label(&reply);
        assert!(
            ["applied", "already_applied", "nothing_to_fix"].contains(&first.as_str()),
            "round {round}: {reply}"
        );
        d.wait_for_entries("the fix", |e| e.iter().any(is_applied));
        let settled = d.entries();
        d.assert_log_stays(&settled, "both at once");
        // The event again, now that the dust has settled.
        let again = d.event(OLD, NEW);
        assert_eq!(label(&again), "already_applied", "round {round}: {again}");
        assert_fixed_once(&d, &mut ws, &format!("both at once, round {round}"));
    });
}

/// Test 4, event after the fix: a repeated event after a finished fix
/// answers `already_applied` and writes nothing.
#[test]
fn race_4_an_event_after_the_fix_writes_nothing() {
    repeat(|round| {
        let (d, mut ws) = world();
        d.mv(OLD, NEW);
        let reply = d.event(OLD, NEW);
        assert_eq!(label(&reply), "applied", "round {round}: {reply}");
        let fixed = d.entries();
        d.assert_log_stays(&fixed, "after the first event");
        let mtimes = d.mtimes();
        for resend in 1..=2 {
            let again = d.event(OLD, NEW);
            assert_eq!(
                label(&again),
                "already_applied",
                "round {round}, resend {resend}: {again}"
            );
        }
        assert_eq!(
            d.mtimes(),
            mtimes,
            "round {round}: a repeated event wrote a file"
        );
        assert_fixed_once(&d, &mut ws, &format!("event after the fix, round {round}"));
    });
}

/// Test 5, event queued while down (the client's own queue is replayed
/// after the restart). Case a: catch-up pairs the rename by itself and
/// the replayed event is `already_applied`. Case b: the file was edited
/// as well, catch-up cannot pair it (F5), the links stay broken, and the
/// replayed exact event fixes it.
#[test]
fn race_5_an_event_queued_while_down_is_replayed_after_the_restart() {
    repeat(|round| {
        // Case a: a plain rename while tephrad is down.
        {
            let (mut d, ws) = world();
            drop(ws);
            d.stop();
            d.mv(OLD, NEW);
            d.spawn();
            let mut ws = d.connect();
            d.wait_for_entries("the catch-up's fix", |e| e.iter().any(is_applied));
            let reply = d.event(OLD, NEW);
            assert_eq!(
                label(&reply),
                "already_applied",
                "round {round}, case a: {reply}"
            );
            let fixed = d.entries();
            d.assert_log_stays(&fixed, "case a");
            d.assert_links("Plan", true, "case a");
            assert_eq!(d.entries_of("rename").len(), 1, "case a: {:?}", d.entries());
            assert_eq!(
                d.entries().len(),
                1,
                "case a: nothing else logged: {:?}",
                d.entries()
            );
            assert_eq!(d.entries()[0]["files_changed"], json!(3), "case a");
            assert_eq!(d.entries()[0]["status"], json!("applied"), "case a");
            let pushed = ws.fixes();
            assert_eq!(
                pushed.len(),
                1,
                "case a: one fixes entry pushed: {pushed:?}"
            );
            assert_eq!(pushed[0]["old"], json!(OLD), "case a");
            assert_eq!(pushed[0]["new"], json!(NEW), "case a");
            assert_eq!(pushed[0]["files_changed"], json!(3), "case a");
        }
        // Case b: the same rename plus an edit: no certain pair.
        {
            let (mut d, ws) = world();
            drop(ws);
            d.stop();
            // A new inode, new content: rename + edit with no OS pair.
            fs::remove_file(d.vault().join(OLD)).unwrap();
            fs::write(
                d.vault().join(NEW),
                format!("{PLAN_BODY}\nEdited while down.\n"),
            )
            .unwrap();
            d.spawn();
            let mut ws = d.connect();
            d.wait_for_entries("the catch-up to give the vanished file up", |e| {
                e.iter().any(|x| x["kind"] == json!("uncertain"))
            });
            d.assert_links("Plan", false, "case b, before the event");
            assert!(
                d.entries_of("rename").is_empty(),
                "case b: catch-up guessed: {:?}",
                d.entries()
            );
            let reply = d.event(OLD, NEW);
            assert_eq!(
                reply["result"],
                json!({"status": "applied", "files_changed": 3}),
                "round {round}, case b: {reply}"
            );
            let fixed = d.entries();
            d.assert_log_stays(&fixed, "case b");
            d.assert_links("Plan", true, "case b");
            assert_eq!(d.entries_of("rename").len(), 1, "case b: {:?}", d.entries());
            let again = d.event(OLD, NEW);
            assert_eq!(
                label(&again),
                "already_applied",
                "round {round}, case b: {again}"
            );
            let pushed: Vec<Value> = ws
                .fixes()
                .into_iter()
                .filter(|e| e["kind"] == json!("rename"))
                .collect();
            assert_eq!(pushed.len(), 1, "case b: one rename pushed: {pushed:?}");
            assert_eq!(pushed[0]["files_changed"], json!(3), "case b");
        }
    });
}

/// Test 6, conflicting event vs hash guess (E6). The old file is removed
/// and two new files appear with no OS pair: `Guess.md` with the old
/// content (a unique hash pair, a guess) and `Plan.md` with other content
/// (where the exact event says it went). The exact event wins, in both
/// orders, and the log shows the correction.
#[test]
fn race_6_the_exact_event_beats_a_hash_guess_in_both_orders() {
    const GUESS: &str = "Guess.md";
    // Makes the vault the way the conflict needs it.
    fn arrange(d: &Daemon) {
        fs::remove_file(d.vault().join(OLD)).unwrap();
        fs::write(d.vault().join(GUESS), PLAN_BODY).unwrap();
        fs::write(
            d.vault().join(NEW),
            format!("{PLAN_BODY}\nRenamed and edited.\n"),
        )
        .unwrap();
    }
    repeat(|round| {
        // Order 1: the guess lands first (after its hold); the event then
        // corrects it.
        {
            let (d, mut ws) = world();
            arrange(&d);
            d.wait_for_entries("the hash guess", |e| {
                e.iter().any(|x| is_applied(x) && x["new"] == json!(GUESS))
            });
            d.assert_links("Guess", true, "order 1, after the guess");
            let reply = d.event(OLD, NEW);
            assert_eq!(label(&reply), "applied", "round {round}, order 1: {reply}");
            d.assert_links("Plan", true, "order 1, after the event");
            let settled = d.entries();
            d.assert_log_stays(&settled, "order 1");
            let renames = d.entries_of("rename");
            assert_eq!(
                renames.len(),
                2,
                "order 1: the guess and its correction: {:?}",
                d.entries()
            );
            // The log shows the correction: the guess's entry
            // (OLD -> Guess.md, applied, N files) stays, and the exact
            // event's own entry (OLD -> Plan.md) is applied and counts
            // the same N files, the ones it pointed at Plan.md instead.
            let guess = renames
                .iter()
                .find(|e| e["old"] == json!(OLD) && e["new"] == json!(GUESS))
                .unwrap_or_else(|| panic!("order 1, the guess entry: {renames:?}"));
            let correction = renames
                .iter()
                .find(|e| e["old"] == json!(OLD) && e["new"] == json!(NEW))
                .unwrap_or_else(|| panic!("order 1, the correction entry: {renames:?}"));
            assert_eq!(guess["status"], json!("applied"), "order 1: {guess}");
            assert_eq!(
                correction["status"],
                json!("applied"),
                "order 1: {correction}"
            );
            assert!(
                guess["files_changed"].as_u64().unwrap_or(0) > 0,
                "order 1: the guess wrote files: {guess}"
            );
            assert_eq!(
                correction["files_changed"], guess["files_changed"],
                "order 1: the correction repoints every file the guess wrote"
            );
            let again = d.event(OLD, NEW);
            assert_eq!(
                label(&again),
                "already_applied",
                "round {round}, order 1: {again}"
            );
            let pushed = ws.fixes();
            assert_eq!(pushed.len(), 2, "order 1: a push per entry: {pushed:?}");
        }
        // Order 2: the event lands first, inside the guess's hold; the
        // guess that comes after it changes nothing.
        {
            let (d, mut ws) = world();
            arrange(&d);
            let reply = d.event(OLD, NEW);
            assert_eq!(label(&reply), "applied", "round {round}, order 2: {reply}");
            d.assert_links("Plan", true, "order 2, after the event");
            // Let the hold pass: the guess is due now.
            std::thread::sleep(HOLD_WITH_MARGIN);
            d.assert_links("Plan", true, "order 2, after the hold");
            let renames = d.entries_of("rename");
            assert!(
                renames.iter().all(|e| e["new"] != json!(GUESS)),
                "order 2: the guess was applied after the event: {:?}",
                d.entries()
            );
            assert_eq!(renames.len(), 1, "order 2: {:?}", d.entries());
            let again = d.event(OLD, NEW);
            assert_eq!(
                label(&again),
                "already_applied",
                "round {round}, order 2: {again}"
            );
            let pushed = ws.fixes();
            assert_eq!(pushed.len(), 1, "order 2: one push: {pushed:?}");
        }
    });
}

// ---------------------------------------------------------------
// Edge cases and fence regressions
// ---------------------------------------------------------------

/// E24: the same event sent twice at once answers `applied` once and
/// `already_applied` for the other (or waits and then answers so); the
/// files are rewritten once and there is one entry.
#[test]
fn e24_an_event_resent_while_the_first_runs_never_rewrites_twice() {
    repeat(|round| {
        let (d, mut ws) = world();
        d.mv(OLD, NEW);
        let gate = Arc::new(Barrier::new(2));
        let other = {
            let (rpc, gate, vault) = (d.rpc(), gate.clone(), d.vault());
            std::thread::spawn(move || {
                gate.wait();
                rpc.event(&vault, OLD, NEW)
            })
        };
        gate.wait();
        let first = d.event(OLD, NEW);
        let second = other.join().unwrap();
        let mut labels = vec![label(&first), label(&second)];
        labels.sort();
        assert_eq!(
            labels,
            ["already_applied", "applied"],
            "round {round}: {first} / {second}"
        );
        let settled = d.entries();
        d.assert_log_stays(&settled, "a resent event");
        assert_fixed_once(&d, &mut ws, &format!("a resent event, round {round}"));
    });
}

/// F5: a rename plus an edit with no OS pair and no event is uncertain
/// and stays unfixed. Similarity is never a basis.
#[test]
fn f5_a_rename_plus_edit_with_no_pair_and_no_event_stays_unfixed() {
    repeat(|round| {
        let (d, mut ws) = world();
        fs::remove_file(d.vault().join(OLD)).unwrap();
        fs::write(
            d.vault().join(NEW),
            format!("{PLAN_BODY}\nEdited as well.\n"),
        )
        .unwrap();
        d.wait_for_entries("the vanished file to be given up", |e| {
            e.iter().any(|x| x["kind"] == json!("uncertain"))
        });
        // Past the hold, in case anything was waiting on it.
        std::thread::sleep(HOLD_WITH_MARGIN);
        d.assert_links("Plan", false, &format!("round {round}"));
        assert!(
            d.entries_of("rename").is_empty(),
            "round {round}: {:?}",
            d.entries()
        );
        assert_eq!(
            d.entries_of("uncertain").len(),
            1,
            "round {round}: {:?}",
            d.entries()
        );
        let pushed: Vec<Value> = ws
            .fixes()
            .into_iter()
            .filter(|e| e["kind"] == json!("rename"))
            .collect();
        assert!(pushed.is_empty(), "round {round}: {pushed:?}");
    });
}

/// F12: this PR adds no method. The dispatch table is the same eighteen
/// verbs, and a test or injection method is not answered by a real daemon.
#[test]
fn f12_the_method_list_is_unchanged_and_there_is_no_injection_method() {
    let expected = [
        "unit.resolve_link",
        "graph.get_backlinks",
        "graph.get_forward_links",
        "graph.get_edges",
        "source.register",
        "source.unregister",
        "source.list",
        "source.rescan",
        "source.promote",
        "source.unpromote",
        "source.rename",
        "link.preview_refactor",
        "link.apply_refactor",
        "unit.list",
        "unit.search",
        "block.ensure_id",
        "fix.list",
        "link.rename_event",
    ];
    assert_eq!(tephrad::rpc::dispatch::METHOD_NAMES, expected);

    let d = Daemon::start();
    for method in [
        "test.rename",
        "test.inject_event",
        "link.inject_rename",
        "fix.replay",
    ] {
        let reply = d.call(method, json!({}));
        assert_eq!(reply["error"]["code"], json!(-32601), "{method}: {reply}");
    }
}
