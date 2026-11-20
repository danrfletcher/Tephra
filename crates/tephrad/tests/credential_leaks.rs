//! F9: a stored secret reaches nowhere it should not.
//!
//! - The sweep (with `--features test-credential-store`): a sentinel
//!   token is stored through the file store, the real `tephrad` binary
//!   runs at trace level against a data dir that holds the credential
//!   file, and afterwards the sentinel is in none of `config.toml`,
//!   `cache.db` and its WAL/SHM (raw bytes), the daemon's token and
//!   access files, or its captured stdout and stderr. The credential file
//!   itself is the one place it is allowed to be.
//! - Captured `tracing` output of the store calls themselves, including
//!   failing ones, and the `Display` and `Debug` of every error.
//! - A source-tree scan of `tephra-remote/src/credentials/`: no `println!`,
//!   `eprintln!` or `tracing` call takes a secret value, and nothing
//!   there reads the environment.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tephrad::credentials::{CredentialError, CredentialStore, Provider, Secret, UnavailableStore};

const SENTINEL: &str = "ghp_SENTINEL_do_not_leak";

#[cfg(feature = "test-credential-store")]
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---- captured tracing -----------------------------------------------------------

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

/// Runs `f` with a TRACE-level subscriber on this thread and returns what
/// it logged.
fn captured_log(f: impl FnOnce()) -> String {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(capture.clone())
        .with_ansi(false)
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let bytes = capture.0.lock().unwrap().clone();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// A store that fails every call with `err` and logs, as the real stores
/// do, through `tracing`.
struct LoudFailingStore(CredentialError);

impl CredentialStore for LoudFailingStore {
    fn get(&self, p: Provider) -> Result<Option<Secret>, CredentialError> {
        tracing::debug!(provider = p.as_str(), error = %self.0, "get failed");
        Err(self.0.clone())
    }
    fn set(&self, p: Provider, _s: &Secret) -> Result<(), CredentialError> {
        tracing::debug!(provider = p.as_str(), error = %self.0, "set failed");
        Err(self.0.clone())
    }
    fn delete(&self, p: Provider) -> Result<(), CredentialError> {
        tracing::debug!(provider = p.as_str(), error = %self.0, "delete failed");
        Err(self.0.clone())
    }
}

#[test]
fn errors_and_the_unavailable_store_leak_nothing_through_display_debug_or_tracing() {
    let errors = [
        CredentialError::Locked,
        CredentialError::Denied,
        CredentialError::Unavailable,
        CredentialError::Backend("keychain status -25299".into()),
    ];
    let secret = Secret::new(SENTINEL);
    let log = captured_log(|| {
        for err in &errors {
            let store = LoudFailingStore(err.clone());
            for p in Provider::ALL {
                let _ = store.get(p);
                let _ = store.set(p, &secret);
                let _ = store.delete(p);
            }
        }
        let u = UnavailableStore;
        for p in Provider::ALL {
            let _ = u.set(p, &secret);
        }
        // Logging the secret by its own formatting prints the placeholder.
        tracing::info!(secret = %secret, other = ?secret, "stored");
    });
    assert!(log.contains("get failed"), "the capture works: {log:?}");
    assert!(log.contains("redacted"));
    assert!(!log.contains(SENTINEL), "{log}");
    assert!(!log.contains("SENTINEL"), "{log}");

    for err in &errors {
        for rendered in [format!("{err}"), format!("{err:?}"), format!("{err:#?}")] {
            assert!(!rendered.contains(SENTINEL), "{rendered}");
        }
    }
    for rendered in [
        format!("{secret}"),
        format!("{secret:?}"),
        format!("{:?}", vec![&secret]),
    ] {
        assert!(!rendered.contains(SENTINEL), "{rendered}");
    }
}

// ---- the sweep (file store, real daemon) ----------------------------------------

#[cfg(feature = "test-credential-store")]
mod sweep {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};
    use tephrad::credentials::FileStore;

    /// Every regular file under `dir`, recursively.
    fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                files_under(&path, out);
            } else if meta.is_file() {
                out.push(path);
            }
        }
    }

    #[test]
    fn the_sentinel_is_in_no_config_cache_log_or_error_after_a_daemon_run() {
        let home = tempfile::tempdir().unwrap();
        let config_path = home.path().join(".config/tephra/config.toml");
        let data_dir = home.path().join(".local/share/tephra");
        let logs = home.path().join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::create_dir_all(config_path.parent().unwrap()).unwrap();
        fs::create_dir_all(&data_dir).unwrap();

        // Store the sentinel through the store, as `tephractl auth` will.
        let cred_file = data_dir.join("credentials.json");
        let store = FileStore::new(&data_dir, &cred_file).unwrap();
        store.set(Provider::Github, &Secret::new(SENTINEL)).unwrap();
        store
            .set(Provider::Google, &Secret::new(format!("{SENTINEL}_google")))
            .unwrap();
        assert!(contains(
            &fs::read(&cred_file).unwrap(),
            SENTINEL.as_bytes()
        ));
        assert_eq!(
            fs::metadata(&cred_file).unwrap().permissions().mode() & 0o777,
            0o600
        );

        fs::write(
            &config_path,
            format!(
                "listen_addr = \"127.0.0.1:0\"\n\
                 data_dir = \"{}\"\n\
                 log_level = \"trace\"\n\
                 cache_limit = \"1 GB\"\n\
                 test_credential_file = \"{}\"\n",
                data_dir.display(),
                cred_file.display()
            ),
        )
        .unwrap();

        let out_log = logs.join("stdout.log");
        let err_log = logs.join("stderr.log");
        let mut child = Command::new(env!("CARGO_BIN_EXE_tephrad"))
            .arg("--config")
            .arg(&config_path)
            .env("HOME", home.path())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .stdout(Stdio::from(fs::File::create(&out_log).unwrap()))
            .stderr(Stdio::from(fs::File::create(&err_log).unwrap()))
            .spawn()
            .expect("spawn tephrad");

        let deadline = Instant::now() + Duration::from_secs(15);
        let mut listening = false;
        while Instant::now() < deadline {
            if fs::read_to_string(&err_log)
                .map(|t| t.contains("listening on 127.0.0.1:"))
                .unwrap_or(false)
            {
                listening = true;
                break;
            }
            if let Ok(Some(_)) = child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        let status = child.wait().unwrap();
        assert!(
            listening,
            "daemon did not start: {}",
            fs::read_to_string(&err_log).unwrap_or_default()
        );
        assert!(status.success(), "clean exit after SIGTERM: {status:?}");

        // The cache is there and has a WAL/SHM pair or at least the db.
        assert!(data_dir.join("cache.db").exists());

        let mut swept = Vec::new();
        files_under(home.path(), &mut swept);
        let mut checked = Vec::new();
        for file in &swept {
            if file == &cred_file {
                continue;
            }
            let bytes = fs::read(file).unwrap();
            for needle in [SENTINEL, "SENTINEL_do_not_leak"] {
                assert!(
                    !contains(&bytes, needle.as_bytes()),
                    "{} contains the sentinel",
                    file.display()
                );
            }
            checked.push(file.file_name().unwrap().to_string_lossy().into_owned());
        }
        // The sweep really covered the files the spec names.
        for expected in [
            "config.toml",
            "cache.db",
            "token",
            "stdout.log",
            "stderr.log",
        ] {
            assert!(
                checked.iter().any(|n| n == expected),
                "{expected} was swept; saw {checked:?}"
            );
        }
        let log = fs::read_to_string(&err_log).unwrap();
        assert!(log.contains("credential store selected"), "{log}");
    }

    #[test]
    fn store_calls_including_failing_ones_log_no_secret() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.json");
        let store = FileStore::new(dir.path(), &file).unwrap();
        let log = captured_log(|| {
            store.set(Provider::Github, &Secret::new(SENTINEL)).unwrap();
            let _ = store.get(Provider::Github);
            // Break the file so reads and writes fail, with the secret in it.
            fs::write(&file, format!("junk {SENTINEL}")).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
            let e1 = store.get(Provider::Github).unwrap_err();
            let e2 = store
                .set(Provider::Github, &Secret::new(SENTINEL))
                .unwrap_err();
            for e in [e1, e2] {
                assert!(!format!("{e} {e:?}").contains(SENTINEL));
            }
            fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
            let e = store.get(Provider::Github).unwrap_err();
            assert!(!format!("{e} {e:?}").contains(SENTINEL));
        });
        assert!(!log.contains(SENTINEL), "{log}");
    }
}

// ---- source-tree scan --------------------------------------------------------------

fn credentials_sources() -> Vec<(PathBuf, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tephra-remote/src/credentials");
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).expect("tephra-remote/src/credentials") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let text = fs::read_to_string(&path).unwrap();
            out.push((path, text));
        }
    }
    assert!(out.len() >= 3, "mod.rs, keychain.rs and file_store.rs");
    out
}

const PRINT_MACROS: [&str; 5] = ["println!", "eprintln!", "print!", "eprint!", "dbg!"];
const TRACING_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

/// The argument text of every printing or logging macro call in `src`:
/// from the `(` after the macro name to its matching `)`.
fn logging_calls(src: &str) -> Vec<String> {
    let bytes = src.as_bytes();
    let mut needles: Vec<String> = PRINT_MACROS.iter().map(|s| s.to_string()).collect();
    for level in TRACING_LEVELS {
        needles.push(format!("tracing::{level}!"));
        needles.push(format!("{level}!"));
    }
    needles.push("event!".into());
    needles.push("span!".into());
    let mut calls: std::collections::BTreeMap<usize, String> = Default::default();
    for needle in &needles {
        let mut from = 0;
        while let Some(pos) = src[from..].find(needle.as_str()) {
            let start = from + pos + needle.len();
            from = start;
            let rest = src[start..].trim_start();
            if !rest.starts_with('(') && !rest.starts_with('{') && !rest.starts_with('[') {
                continue;
            }
            let open = start + (src[start..].len() - rest.len());
            let mut depth = 0i32;
            let mut end = open;
            for (i, b) in bytes[open..].iter().enumerate() {
                match b {
                    b'(' | b'{' | b'[' => depth += 1,
                    b')' | b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            calls.insert(open, src[open..=end].to_string());
        }
    }
    calls.into_values().collect()
}

/// Words that mean a secret value is being passed.
const SECRET_WORDS: [&str; 8] = [
    "secret",
    "expose",
    "password",
    "token",
    "bearer",
    "value",
    "refresh",
    "credential_value",
];

fn takes_a_secret(call: &str) -> bool {
    let lower = call.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| lower.contains(w))
}

#[test]
fn no_println_eprintln_or_tracing_call_in_credentials_takes_a_secret() {
    let mut seen = 0;
    let mut offenders = Vec::new();
    for (path, text) in credentials_sources() {
        for call in logging_calls(&text) {
            seen += 1;
            if takes_a_secret(&call) {
                offenders.push(format!("{}: {call}", path.display()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "a logging call in credentials/ mentions a secret: {offenders:#?}"
    );
    // Not vacuous: the stores do log, and the scanner finds those calls.
    assert!(seen >= 3, "the scan found only {seen} logging calls");
}

#[test]
fn the_scanner_flags_what_it_is_meant_to_flag() {
    let bad = [
        r#"fn f(secret: &Secret) { tracing::info!("got {}", secret.expose()); }"#,
        r#"fn f(t: &str) { println!("{t}", t = token); }"#,
        r#"fn f() { eprintln!("pw {}", password); }"#,
        r#"fn f(v: String) { debug!(value = %v, "x"); }"#,
    ];
    for src in bad {
        let calls = logging_calls(src);
        assert!(!calls.is_empty(), "{src}");
        assert!(calls.iter().any(|c| takes_a_secret(c)), "{src}: {calls:?}");
    }
    let fine =
        r#"fn f(p: Provider) { tracing::debug!(provider = p.as_str(), error = %e, "failed"); }"#;
    let calls = logging_calls(fine);
    assert_eq!(calls.len(), 1);
    assert!(!takes_a_secret(&calls[0]));
}

/// Nothing in the credential code reads a token from the environment, a
/// config key or a plain file other than the test store's own file.
#[test]
fn credentials_source_reads_no_environment_and_no_config() {
    for (path, text) in credentials_sources() {
        let code: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for banned in [
            "env::var",
            "env!(",
            "option_env!",
            "std::env",
            "config::",
            "Config",
        ] {
            assert!(!code.contains(banned), "{} uses {banned}", path.display());
        }
        let is_file_store = path.file_name().and_then(|n| n.to_str()) == Some("file_store.rs");
        if !is_file_store {
            for banned in [
                "fs::read",
                "fs::write",
                "File::open",
                "File::create",
                "OpenOptions",
            ] {
                assert!(
                    !code.contains(banned),
                    "{} touches a file with {banned}",
                    path.display()
                );
            }
        }
    }
}
