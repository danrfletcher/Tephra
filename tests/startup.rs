//! Integration tests: spawn the compiled `tephrad` binary with `HOME`
//! pointed at a fresh `tempfile::TempDir`, and assert on real process
//! behaviour (log output, file permissions, exit codes).
//!
//! Each test uses its own ephemeral high port (bound briefly to confirm
//! it's free, then released) written into a config file ahead of time,
//! so parallel test runs never collide on `127.0.0.1:4812`.

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener as StdTcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Picks a free high port by binding briefly and releasing it. Small
/// window for a race with another process, but combined with each test
/// using a *different* randomly-chosen port, collisions in practice are
/// not observed.
fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().unwrap().port()
}

fn tephrad_bin() -> &'static str {
    env!("CARGO_BIN_EXE_tephrad")
}

/// Writes a config file ahead of time with a specific listen_addr, so the
/// daemon under test binds a collision-free port instead of regenerating
/// defaults on 127.0.0.1:4812.
fn write_config(config_path: &Path, data_dir: &Path, listen_addr: &str) {
    fs::create_dir_all(config_path.parent().unwrap()).unwrap();
    fs::write(
        config_path,
        format!(
            "listen_addr = \"{listen_addr}\"\n\
             data_dir = \"{data_dir}\"\n\
             log_level = \"info\"\n",
            listen_addr = listen_addr,
            data_dir = data_dir.display(),
        ),
    )
    .unwrap();
}

/// A spawned `tephrad` process plus a background thread that continuously
/// drains its stderr into a shared buffer for the process's entire
/// lifetime. Draining continuously (rather than stopping once a line of
/// interest is seen) matters: stopping early closes the pipe's read end,
/// and the child's next write to stderr (e.g. its shutdown log line,
/// after SIGTERM) then fails with a broken pipe.
struct TephradProcess {
    child: Child,
    lines: Arc<Mutex<Vec<String>>>,
    reader_thread: Option<std::thread::JoinHandle<()>>,
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

        let stderr = child.stderr.take().expect("stderr piped");
        let lines: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let lines_for_thread = Arc::clone(&lines);

        let reader_thread = std::thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => break, // EOF: process closed stderr (exited).
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

        TephradProcess {
            child,
            lines,
            reader_thread: Some(reader_thread),
        }
    }

    /// Polls the captured lines for one containing `needle`, up to `timeout`.
    fn wait_for_line(&self, needle: &str, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self
                .lines
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

    fn captured_lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    /// Sends SIGTERM, waits (bounded by `timeout`) for exit, then joins
    /// the stderr-draining thread so all output up to process exit is
    /// captured before returning.
    fn terminate_and_wait(mut self, timeout: Duration) -> Option<i32> {
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

        if let Some(handle) = self.reader_thread.take() {
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
    let port = free_port();
    let listen_addr = format!("127.0.0.1:{port}");
    write_config(&config_path, &data_dir, &listen_addr);

    let process = TephradProcess::spawn(home.path(), &config_path);
    let found = process.wait_for_line(
        &format!("listening on 127.0.0.1:{port}"),
        Duration::from_secs(10),
    );
    assert!(
        found,
        "expected a 'listening on 127.0.0.1:{port}' line, got: {:?}",
        process.captured_lines()
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

    // The token is never printed in any captured log line.
    let token_value = fs::read_to_string(&token_path).unwrap();
    for line in &process.captured_lines() {
        assert!(
            !line.contains(token_value.trim()),
            "log line leaked the token value: {line}"
        );
    }

    let code = process.terminate_and_wait(Duration::from_secs(5));
    assert_eq!(code, Some(0), "expected clean exit after SIGTERM");
}

#[test]
fn second_run_reuses_token_unchanged() {
    let home = tempfile::tempdir().expect("tempdir");
    let config_path = home.path().join(".config/tephra/config.toml");
    let data_dir = home.path().join(".local/share/tephra");
    let port = free_port();
    let listen_addr = format!("127.0.0.1:{port}");
    write_config(&config_path, &data_dir, &listen_addr);

    let process = TephradProcess::spawn(home.path(), &config_path);
    let found = process.wait_for_line("listening on", Duration::from_secs(10));
    assert!(found, "first run should reach the listening line");
    process.terminate_and_wait(Duration::from_secs(5));

    let token_path = data_dir.join("token");
    let first_contents = fs::read_to_string(&token_path).unwrap();
    let first_mtime = fs::metadata(&token_path).unwrap().modified().unwrap();

    // Second run against the same HOME and same port (now free again
    // since the first process was terminated).
    let process2 = TephradProcess::spawn(home.path(), &config_path);
    let found2 = process2.wait_for_line("listening on", Duration::from_secs(10));
    assert!(found2, "second run should also reach the listening line");
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
