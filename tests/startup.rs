//! Integration tests: spawn the compiled `tephrad` binary with `HOME`
//! pointed at a fresh `tempfile::TempDir`, and assert on real process
//! behaviour (log output, file permissions, exit codes).
//!
//! Tests that don't need to pre-know the daemon's port use
//! `listen_addr = "127.0.0.1:0"`: the kernel picks a free ephemeral port
//! at bind time, so there is no bind-then-release window for another
//! parallel test to steal the same port number (the bug that made this
//! suite flaky — see `listener_failure_reports_clear_error_and_nonzero_exit`
//! for the one test that *does* need a specific, collision-free port:
//! it binds that port itself and holds it open continuously, with no
//! release gap, until the daemon's own bind attempt against it fails).

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener as StdTcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// `listen_addr` for tests that don't care which port the daemon binds,
/// only that it binds *some* port and logs it. Port 0 means "ask the
/// kernel for a free ephemeral port at bind time", which sidesteps any
/// possibility of colliding with another parallel test's daemon: unlike
/// picking a port number ahead of time (bind-then-release, then bind
/// again later), there is no window between choosing the number and the
/// real daemon process claiming it.
const EPHEMERAL_ADDR: &str = "127.0.0.1:0";

fn tephrad_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tephrad")
}

/// Writes a config file ahead of time with a specific `listen_addr` and
/// `log_level`, so the daemon under test binds a collision-free port
/// instead of regenerating defaults on `127.0.0.1:4812`.
fn write_config_with_level(
    config_path: &Path,
    data_dir: &Path,
    listen_addr: &str,
    log_level: &str,
) {
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(
        config_path,
        format!(
            "listen_addr = \"{listen_addr}\"\n\
             data_dir = \"{data_dir}\"\n\
             log_level = \"{log_level}\"\n",
            listen_addr = listen_addr,
            data_dir = data_dir.display(),
            log_level = log_level,
        ),
    )
    .unwrap();
}

/// `write_config_with_level` with `log_level = "info"`, the level used by
/// most tests here (a handful need other levels, e.g. the trace-level
/// token-leak check).
fn write_config(config_path: &Path, data_dir: &Path, listen_addr: &str) {
    write_config_with_level(config_path, data_dir, listen_addr, "info");
}

/// Drains a single pipe (stdout or stderr) into a shared buffer on a
/// background thread for the process's entire lifetime. Draining
/// continuously (rather than stopping once a line of interest is seen)
/// matters: stopping early closes the pipe's read end, and the child's
/// next write to that stream (e.g. its shutdown log line, after SIGTERM)
/// then fails with a broken pipe.
fn spawn_line_drain<R: std::io::Read + Send + 'static>(
    pipe: R,
) -> (Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
    let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let lines_for_thread = Arc::clone(&lines);
    let handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF: process closed the pipe (exited).
                Ok(_) => {
                    lines_for_thread
                        .lock()
                        .unwrap()
                        .push(line.trim_end().to_string());
                }
                Err(_) => break,
            }
        }
    });
    (lines, handle)
}

/// A spawned `tephrad` process plus background threads continuously
/// draining its stdout and stderr into shared buffers.
struct TephradProcess {
    child: Child,
    stderr_lines: Arc<Mutex<Vec<String>>>,
    stdout_lines: Arc<Mutex<Vec<String>>>,
    stderr_thread: Option<std::thread::JoinHandle<()>>,
    stdout_thread: Option<std::thread::JoinHandle<()>>,
}

impl TephradProcess {
    fn spawn(home: &Path, config_path: &Path) -> Self {
        let mut child = Command::new(tephrad_bin())
            .arg("--config")
            .arg(config_path)
            .env("HOME", home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn tephrad");

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let (stdout_lines, stdout_thread) = spawn_line_drain(stdout);
        let (stderr_lines, stderr_thread) = spawn_line_drain(stderr);

        TephradProcess {
            child,
            stderr_lines,
            stdout_lines,
            stderr_thread: Some(stderr_thread),
            stdout_thread: Some(stdout_thread),
        }
    }

    /// Polls the captured stderr lines for one containing `needle`, up
    /// to `timeout`. All of `tephrad`'s startup-stage logging goes to
    /// stderr regardless of level (see `init_tracing` in `main.rs`).
    fn wait_for_line(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self
                .stderr_lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains(needle))
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn captured_stderr_lines(&self) -> Vec<String> {
        self.stderr_lines.lock().unwrap().clone()
    }

    fn captured_stdout_lines(&self) -> Vec<String> {
        self.stdout_lines.lock().unwrap().clone()
    }

    /// Sends SIGTERM, waits (bounded by `timeout`) for exit, then joins
    /// both draining threads so all output up to process exit is
    /// captured before returning. Takes `&mut self` (rather than
    /// consuming `self`) so callers can still inspect `captured_*_lines`
    /// afterwards, e.g. to assert on output produced during shutdown.
    fn terminate_and_wait(&mut self, timeout: Duration) -> Option<i32> {
        let pid = self.child.id() as i32;
        unsafe {
            extern "C" {
                fn kill(pid: i32, sig: i32) -> i32;
            }
            kill(pid, 15 /* SIGTERM */);
        }

        let deadline = Instant::now() + timeout;
        let code = loop {
            if let Ok(Some(status)) = self.child.try_wait() {
                break status.code();
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                break None;
            }
            std::thread::sleep(Duration::from_millis(20));
        };

        if let Some(handle) = self.stderr_thread.take() {
            let _ = handle.join();
        }
        if let Some(handle) = self.stdout_thread.take() {
            let _ = handle.join();
        }

        code
    }
}

#[test]
fn first_run_binds_listener_and_creates_token_with_mode_0600() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let data_dir = home.path().join(".local/share/tephra");
    write_config(&config_path, &data_dir, EPHEMERAL_ADDR);

    let mut process = TephradProcess::spawn(home.path(), &config_path);
    let found = process.wait_for_line("listening on 127.0.0.1:", Duration::from_secs(10));
    assert!(
        found,
        "expected a 'listening on 127.0.0.1:<port>' line, got: {:?}",
        process.captured_stderr_lines()
    );

    // Config file exists with the expected fields.
    assert!(config_path.exists());
    let raw = fs::read_to_string(&config_path).unwrap();
    assert!(raw.contains("listen_addr"));
    assert!(raw.contains("data_dir"));
    assert!(raw.contains("log_level"));

    // Token file exists with mode 0600.
    let token_path = data_dir.join("token");
    assert!(token_path.exists(), "token file should have been created");
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);

    // The token is never printed in any captured stderr log line.
    let token_value = fs::read_to_string(&token_path).unwrap();
    for line in &process.captured_stderr_lines() {
        assert!(
            !line.contains(token_value.trim()),
            "log line leaked the token value: {line}"
        );
    }

    let code = process.terminate_and_wait(Duration::from_secs(5));
    assert_eq!(code, Some(0), "expected clean exit after SIGTERM");
}

#[test]
fn token_never_leaks_on_stdout_or_stderr_at_trace_level() {
    // C11 requires the token never appear on either stream at any log
    // level; trace is the most verbose, so it's the strictest check.
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let data_dir = home.path().join(".local/share/tephra");
    write_config_with_level(&config_path, &data_dir, EPHEMERAL_ADDR, "trace");

    let mut process = TephradProcess::spawn(home.path(), &config_path);
    let found = process.wait_for_line("listening on 127.0.0.1:", Duration::from_secs(10));
    assert!(
        found,
        "expected a 'listening on 127.0.0.1:<port>' line, got: {:?}",
        process.captured_stderr_lines()
    );

    let token_path = data_dir.join("token");
    let token_value = fs::read_to_string(&token_path).unwrap();
    let token_value = token_value.trim();

    let code = process.terminate_and_wait(Duration::from_secs(5));
    assert_eq!(code, Some(0), "expected clean exit after SIGTERM");

    // Re-fetch lines after terminate_and_wait, which joins the draining
    // threads and so guarantees all output (including any logged during
    // shutdown) has been captured.
    for line in &process.captured_stderr_lines() {
        assert!(
            !line.contains(token_value),
            "stderr log line leaked the token value at trace level: {line}"
        );
    }
    for line in &process.captured_stdout_lines() {
        assert!(
            !line.contains(token_value),
            "stdout line leaked the token value at trace level: {line}"
        );
    }
}

#[test]
fn second_run_reuses_token_unchanged() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let data_dir = home.path().join(".local/share/tephra");
    // Port 0 both times: each run independently asks the kernel for a
    // free ephemeral port at bind time, so there is no gap during which
    // a specific port number needs to stay free between the two runs.
    write_config(&config_path, &data_dir, EPHEMERAL_ADDR);

    let mut process = TephradProcess::spawn(home.path(), &config_path);
    let found = process.wait_for_line("listening on", Duration::from_secs(10));
    assert!(
        found,
        "first run should reach the listening line, got stderr: {:?}",
        process.captured_stderr_lines()
    );
    process.terminate_and_wait(Duration::from_secs(5));

    let token_path = data_dir.join("token");
    let first_contents = fs::read_to_string(&token_path).unwrap();
    let first_mtime = fs::metadata(&token_path).unwrap().modified().unwrap();

    // Second run against the same HOME. Binding port 0 again picks a
    // fresh free ephemeral port (which may or may not be the same
    // number as before); nothing here depends on it being the same.
    let mut process2 = TephradProcess::spawn(home.path(), &config_path);
    let found2 = process2.wait_for_line("listening on", Duration::from_secs(10));
    assert!(
        found2,
        "second run should also reach the listening line, got stderr: {:?}",
        process2.captured_stderr_lines()
    );
    process2.terminate_and_wait(Duration::from_secs(5));

    let second_contents = fs::read_to_string(&token_path).unwrap();
    let second_mtime = fs::metadata(&token_path).unwrap().modified().unwrap();

    assert_eq!(
        first_contents, second_contents,
        "token must not be regenerated on second startup"
    );
    assert_eq!(
        first_mtime, second_mtime,
        "token file mtime must be unchanged on second startup"
    );
}

#[test]
fn startup_log_lines_are_in_the_documented_order() {
    // GP1's documented order: config -> directory creation -> token
    // ensure -> listener bind -> ready log.
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let data_dir = home.path().join(".local/share/tephra");
    write_config(&config_path, &data_dir, EPHEMERAL_ADDR);

    let mut process = TephradProcess::spawn(home.path(), &config_path);
    let found = process.wait_for_line("listening on 127.0.0.1:", Duration::from_secs(10));
    assert!(
        found,
        "expected the listening line to appear, got stderr: {:?}",
        process.captured_stderr_lines()
    );
    process.terminate_and_wait(Duration::from_secs(5));

    let lines = process.captured_stderr_lines();
    let index_of =
        |needle: &str| -> Option<usize> { lines.iter().position(|l| l.contains(needle)) };

    let config_idx = index_of("loaded config").or_else(|| index_of("wrote default config"));
    let dir_idx = index_of("data directory ready");
    let token_idx = index_of("generated new auth token")
        .or_else(|| index_of("reusing existing auth token"))
        .or_else(|| index_of("token file permissions were loosened"));
    let listening_idx = index_of("listening on");

    let (config_idx, dir_idx, token_idx, listening_idx) = (
        config_idx.expect("config-stage log line"),
        dir_idx.expect("directory-stage log line"),
        token_idx.expect("token-stage log line"),
        listening_idx.expect("listening log line"),
    );

    assert!(
        config_idx < dir_idx && dir_idx < token_idx && token_idx < listening_idx,
        "expected config < directory < token < listening, got indices {config_idx}, {dir_idx}, {token_idx}, {listening_idx} in: {lines:?}"
    );
}

#[test]
fn listener_failure_reports_clear_error_and_nonzero_exit() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let data_dir = home.path().join(".local/share/tephra");

    // Bind the port we're going to hand to tephrad ourselves, and read
    // back its *actual* address from the listener we just created —
    // there is no bind-then-release gap (unlike picking a port number
    // first and binding it again later), so no other parallel test can
    // ever steal it between "choosing" the port and holding it open.
    let held = StdTcpListener::bind("127.0.0.1:0").expect("bind a port to hold");
    let listen_addr = held.local_addr().unwrap().to_string();
    write_config(&config_path, &data_dir, &listen_addr);

    let output = Command::new(tephrad_bin())
        .arg("--config")
        .arg(&config_path)
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run tephrad");

    // Keep `held` alive (and thus the port occupied) across the whole
    // run above; drop it explicitly now that we're done with it.
    drop(held);

    assert!(
        !output.status.success(),
        "startup against an already-bound port should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tephrad: listener failed:"),
        "expected a listener-stage error message, got: {stderr}"
    );
}

#[test]
fn directory_stage_failure_reports_clear_error() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let readonly_parent = home.path().join("readonly-parent");
    fs::create_dir_all(&readonly_parent).unwrap();
    let data_dir = readonly_parent.join("data");
    // This test's startup never reaches the listener stage (it fails
    // earlier, at directory creation), so the port value is irrelevant;
    // 0 means the kernel picks one if we ever get that far.
    write_config(&config_path, &data_dir, EPHEMERAL_ADDR);

    // Make the parent directory read-only so data_dir creation fails.
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&readonly_parent, fs::Permissions::from_mode(0o555)).unwrap();

    let output = Command::new(tephrad_bin())
        .arg("--config")
        .arg(&config_path)
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run tephrad");

    // Restore permissions so the TempDir can clean itself up, regardless
    // of the assertions below.
    fs::set_permissions(&readonly_parent, fs::Permissions::from_mode(0o755)).unwrap();

    if nix_root_can_bypass_permissions() {
        eprintln!("skipping assertions: running as a user that can bypass directory permissions (e.g. root)");
        return;
    }

    assert!(
        !output.status.success(),
        "startup against an unwritable data_dir parent should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tephrad: directory failed:"),
        "expected a directory-stage error message, got: {stderr}"
    );
}

/// Best-effort detection of whether the current process can bypass
/// directory permission checks (true for root). Avoids a false test
/// failure in a container that runs tests as root.
fn nix_root_can_bypass_permissions() -> bool {
    unsafe {
        extern "C" {
            fn geteuid() -> u32;
        }
        geteuid() == 0
    }
}

#[test]
fn data_dir_is_created_non_world_writable_under_permissive_umask() {
    // Runs the real binary under umask 000 via a shell wrapper, so only
    // the child process's umask is affected (umask is process-wide, and
    // cargo test runs multiple tests in parallel threads of the same
    // process, so mutating this test's own umask would race with every
    // other concurrently-running test).
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let data_dir = home.path().join(".local/share/tephra");
    write_config(&config_path, &data_dir, EPHEMERAL_ADDR);

    let shell_cmd = format!(
        "umask 000 && exec {bin} --config {config}",
        bin = shell_quote(tephrad_bin()),
        config = shell_quote(config_path.to_str().unwrap()),
    );

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&shell_cmd)
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tephrad under umask 000");

    let stderr = child.stderr.take().expect("stderr piped");
    let (lines, thread) = spawn_line_drain(stderr);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while Instant::now() < deadline {
        if lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("listening on"))
        {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        found,
        "expected the listening line to appear, got: {:?}",
        lines.lock().unwrap().clone()
    );

    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;

    // Terminate and clean up regardless of the assertion outcome.
    let pid = child.id() as i32;
    unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        kill(pid, 15 /* SIGTERM */);
    }
    let _ = child.wait();
    let _ = thread.join();

    assert_eq!(
        mode, 0o700,
        "data dir should be 0700 even when created under umask 000"
    );
}

/// Minimal single-quote shell escaping for building the `sh -c` command
/// line in the umask test above.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[test]
fn startup_with_no_existing_config_writes_defaults_and_logs_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    assert!(!config_path.exists());

    // Hold tephrad's real default port open so its listener bind fails
    // once startup reaches that stage, keeping this test deterministic
    // and avoiding any chance of this test actually serving traffic on
    // tephra's real default port (127.0.0.1:4812, which could collide
    // with an actual running daemon).
    let _held = match StdTcpListener::bind("127.0.0.1:4812") {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "skipping: could not hold 127.0.0.1:4812 ({e}); likely in use by another process"
            );
            return;
        }
    };

    let output = Command::new(tephrad_bin())
        .arg("--config")
        .arg(&config_path)
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run tephrad");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("wrote default config (file was absent)"),
        "expected a 'wrote default config' line, got: {stderr}"
    );
    assert!(
        !output.status.success(),
        "expected a listener-stage failure since port 4812 is held, got: {stderr}"
    );
    assert!(
        stderr.contains("tephrad: listener failed:"),
        "expected a listener-stage error message, got: {stderr}"
    );

    assert!(
        config_path.exists(),
        "the binary itself should have generated the default config"
    );
    let raw = fs::read_to_string(&config_path).unwrap();
    assert!(raw.contains("listen_addr = \"127.0.0.1:4812\""));
    assert!(raw.contains("data_dir"));
    assert!(raw.contains("log_level = \"info\""));
}

#[test]
fn corrupt_config_exits_non_zero_and_is_not_overwritten() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    let corrupt = "this [ is not valid toml {{{";
    fs::write(&config_path, corrupt).unwrap();

    let output = Command::new(tephrad_bin())
        .arg("--config")
        .arg(&config_path)
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run tephrad");

    assert!(
        !output.status.success(),
        "startup against corrupt config should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tephrad: config failed:"),
        "expected a config-stage error message, got: {stderr}"
    );

    // The user's corrupt file must not have been overwritten.
    let raw = fs::read_to_string(&config_path).unwrap();
    assert_eq!(raw, corrupt);
}

#[test]
fn config_flag_overrides_xdg_config_home_resolution() {
    // T1 (checklist review): C25 launches tephrad with an explicit
    // `--config /tmp/tephra-test/config.toml`, while C27 cats
    // `$XDG_CONFIG_HOME/tephra/config.toml` — a different file, since
    // `--config` always takes precedence over XDG config-dir resolution
    // (`paths::default_config_path()` is only consulted when no
    // `--config` flag is given — see `cli.rs`). This test locks in that
    // precedence at the binary level: with `XDG_CONFIG_HOME` pointing at
    // one directory and `--config` pointing at a config file outside it,
    // tephrad must load only the `--config` path.
    let home = tempfile::tempdir().expect("tempdir");

    // A config file sitting at the XDG-resolved location, deliberately
    // corrupt: if tephrad read it instead of (or as well as) the
    // `--config` path, startup would fail at the config stage rather
    // than reach "listening on".
    let xdg_config_home = home.path().join("xdg-config");
    let xdg_config_path = xdg_config_home.join("tephra/config.toml");
    fs::create_dir_all(xdg_config_path.parent().unwrap()).unwrap();
    fs::write(&xdg_config_path, "this [ is not valid toml {{{").unwrap();

    // The real config, at an explicit path outside the XDG tree, naming
    // a distinct data_dir so we can tell afterwards which file was used.
    let explicit_config_path = home.path().join("explicit/config.toml");
    let explicit_data_dir = home.path().join("explicit-data");
    write_config(&explicit_config_path, &explicit_data_dir, EPHEMERAL_ADDR);

    let mut child = Command::new(tephrad_bin())
        .arg("--config")
        .arg(&explicit_config_path)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", &xdg_config_home)
        .env_remove("XDG_DATA_HOME")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn tephrad");

    let stderr = child.stderr.take().expect("stderr piped");
    let (lines, thread) = spawn_line_drain(stderr);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found = false;
    while Instant::now() < deadline {
        if lines
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("listening on"))
        {
            found = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    // Terminate and clean up regardless of the assertion outcome below.
    let pid = child.id() as i32;
    unsafe {
        extern "C" {
            fn kill(pid: i32, sig: i32) -> i32;
        }
        kill(pid, 15 /* SIGTERM */);
    }
    let _ = child.wait();
    let _ = thread.join();

    let captured = lines.lock().unwrap().clone();
    assert!(
        found,
        "expected startup to succeed using the --config path, ignoring the corrupt XDG-resolved config; got: {captured:?}"
    );

    // The data_dir named in the --config file was used: its token file
    // exists. This can only be true if tephrad parsed the --config
    // file (the corrupt XDG-resolved one would have aborted startup
    // before reaching the token stage).
    assert!(
        explicit_data_dir.join("token").exists(),
        "expected the token to be created under the --config file's data_dir"
    );

    // The decoy XDG-resolved config must be left untouched: not parsed,
    // and not overwritten as a "default" target either.
    let xdg_raw = fs::read_to_string(&xdg_config_path).unwrap();
    assert_eq!(
        xdg_raw, "this [ is not valid toml {{{",
        "the XDG-resolved config file must not be read or rewritten when --config is given"
    );
}

#[test]
fn help_flag_exits_zero_without_touching_disk_or_binding() {
    let home = tempfile::tempdir().expect("tempdir");

    let output = Command::new(tephrad_bin())
        .arg("--help")
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run tephrad --help");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--config"));

    // Nothing should have been created under HOME.
    let config_dir = home.path().join(".config");
    let data_dir = home.path().join(".local");
    assert!(!config_dir.exists(), "--help must not touch disk");
    assert!(!data_dir.exists(), "--help must not touch disk");
}

#[test]
fn invalid_log_level_fails_startup_with_clear_error_not_silence() {
    // Before this was fixed, an invalid log_level (e.g. a typo like
    // "loud") was handed straight to `tracing_subscriber::EnvFilter`,
    // which happily parsed it as a target directive rather than a
    // level. That silently filtered out *all* output instead of
    // falling back to info, so the process produced a successful,
    // totally silent run with no "listening on" line. It must instead
    // fail loudly at the config stage.
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let data_dir = home.path().join(".local/share/tephra");
    // This run is expected to fail at the config stage, before the
    // listener stage is ever reached, so the port value is irrelevant.
    write_config_with_level(&config_path, &data_dir, EPHEMERAL_ADDR, "loud");

    let output = Command::new(tephrad_bin())
        .arg("--config")
        .arg(&config_path)
        .env("HOME", home.path())
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .expect("run tephrad");

    assert!(
        !output.status.success(),
        "startup with an invalid log_level should exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tephrad: config failed:"),
        "expected a config-stage error message, got stderr: {stderr:?}, stdout: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !stderr.is_empty(),
        "stderr must not be silently empty on an invalid log_level"
    );
}
