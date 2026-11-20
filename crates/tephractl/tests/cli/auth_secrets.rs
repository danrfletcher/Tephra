//! F9: credentials never leave the credential store. A sentinel token
//! is run through every outcome of `auth github`, `auth google`,
//! `auth status` and `auth logout` (G1 to G3, E2, E17, and the failures
//! of the Google flow), success and failure. After each, stdout,
//! stderr, the `--json` output, `config.toml`, a `cache.db`, the data
//! dir, a tracing log and every other file under the run's directories
//! are scanned: none may hold the sentinel or an 8-character piece of
//! it. (The store itself is the one place a secret may be, so it is not
//! scanned: it stands in for the Keychain.)
//!
//! Table-driven: one `Case` per outcome. The scan also holds for the
//! failing-store fake, which implements the PR-1 trait.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tephra_remote::credentials::{CredentialError, CredentialStore, Provider};

use crate::auth_support::{
    contains_bytes, files_under, http, FailOn, FailingStore, Fakes, Harness, MemStore, Ran,
    ScriptedInput,
};

const GITHUB_TOKEN: &str = "ghp_SENTINEL0123456789abcdefghijklmnopqrstuv";
const GOOGLE_ACCESS: &str = "ya29.SENTINEL-access-0123456789abcdef";
const GOOGLE_REFRESH: &str = "1//SENTINEL-refresh-0123456789abcdefghij";
const GOOGLE_CLIENT_SECRET: &str = "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";
const STORED_GITHUB: &str = "ghp_SENTINELSTORED9876543210zyxwvutsrqponm";

/// Every secret that must stay out of every output and file.
fn secrets() -> Vec<&'static str> {
    vec![
        GITHUB_TOKEN,
        GOOGLE_ACCESS,
        GOOGLE_REFRESH,
        GOOGLE_CLIENT_SECRET,
        STORED_GITHUB,
    ]
}

/// The text holds the secret, or an 8-character piece of it. The shared
/// `SENTINEL` marker is not counted as a piece (every sentinel has it,
/// so it would match the others); the part after it is.
fn leaks(haystack: &[u8], secret: &str) -> Option<String> {
    if contains_bytes(haystack, secret) {
        return Some(secret.to_string());
    }
    let tail = secret.split_once("SENTINEL").map_or(secret, |(_, t)| t);
    let chars: Vec<char> = tail.chars().collect();
    (0..=chars.len().saturating_sub(8)).find_map(|i| {
        let piece: String = chars[i..i + 8].iter().collect();
        contains_bytes(haystack, &piece).then_some(piece)
    })
}

/// One run's directories, which the scan reads.
struct World {
    fakes: Fakes,
    config: tempfile::TempDir,
    data: tempfile::TempDir,
    inputs: tempfile::TempDir,
}

impl World {
    fn new() -> World {
        let fakes = Fakes::start();
        fakes.github.accept_only_token(GITHUB_TOKEN);
        fakes
            .google
            .set_client_credentials("sentinel.apps.googleusercontent.com", GOOGLE_CLIENT_SECRET);
        fakes.google.set_tokens(GOOGLE_ACCESS, GOOGLE_REFRESH);
        let world = World {
            fakes,
            config: tempfile::tempdir().unwrap(),
            data: tempfile::tempdir().unwrap(),
            inputs: tempfile::tempdir().unwrap(),
        };
        // Files a real run would leave beside the credentials: a config
        // that names the fakes, an (empty) cache and a tracing log.
        std::fs::write(
            world.config.path().join("config.toml"),
            format!("github_api_url = \"{}\"\n", world.fakes.github.base_url()),
        )
        .unwrap();
        std::fs::write(world.data.path().join("cache.db"), b"SQLite format 3\0").unwrap();
        std::fs::write(world.data.path().join("tephra.log"), b"started\n").unwrap();
        world
    }

    fn harness(&self, store: Arc<dyn CredentialStore>) -> Harness {
        let mut h = Harness::new(store, self.config.path());
        h.github_base_url = Some(self.fakes.github.base_url().to_string());
        let client = self.inputs.path().join("client.json");
        std::fs::write(&client, self.fakes.google.client_json()).unwrap();
        h.client_json = Some(client);
        h.timeout = Duration::from_secs(1);
        h
    }

    /// The scan: output of the runs, then every file under the run's
    /// directories, for every secret and for the codes and verifiers the
    /// fake Google saw.
    fn scan(&self, what: &str, outputs: &[&Ran]) {
        let mut needles: Vec<String> = secrets().iter().map(|s| s.to_string()).collect();
        for t in self.fakes.google.token_requests() {
            for key in ["code", "code_verifier"] {
                if let Some(v) = t.get(key) {
                    needles.push(v.to_string());
                }
            }
        }
        for ran in outputs {
            for (stream, text) in [("stdout", &ran.out), ("stderr", &ran.err)] {
                for secret in &needles {
                    assert!(
                        leaks(text.as_bytes(), secret).is_none(),
                        "{what}: {stream} holds a piece of a secret: {text}"
                    );
                }
            }
        }
        for dir in [self.config.path(), self.data.path(), self.inputs.path()] {
            for (path, bytes) in files_under(dir) {
                // The client JSON is the user's own input file: it holds
                // the client secret by design (and nothing else of ours).
                if path.file_name().is_some_and(|n| n == "client.json") {
                    for secret in needles.iter().filter(|s| *s != GOOGLE_CLIENT_SECRET) {
                        assert!(leaks(&bytes, secret).is_none(), "{what}: {path:?}");
                    }
                    continue;
                }
                for secret in &needles {
                    assert!(
                        leaks(&bytes, secret).is_none(),
                        "{what}: {path:?} holds a piece of a secret"
                    );
                }
            }
        }
    }
}

/// Follows the consent redirect, like a browser.
fn follow(url: &str) -> std::io::Result<()> {
    let url = url.to_string();
    std::thread::spawn(move || {
        if let Ok(r) = http("GET", &url, None) {
            if let Some(loc) = r.location {
                let _ = http("GET", &loc, None);
            }
        }
    });
    Ok(())
}

/// A browser that goes to the callback with the wrong state, then the
/// right one.
fn follow_wrong_state_first(url: &str) -> std::io::Result<()> {
    let url = url.to_string();
    std::thread::spawn(move || {
        if let Ok(r) = http("GET", &url, None) {
            if let Some(loc) = r.location {
                let wrong = loc.replacen("state=", "state=WRONG", 1);
                let _ = http("GET", &wrong, None);
                let _ = http("GET", &loc, None);
            }
        }
    });
    Ok(())
}

type Setup = fn(&World, &mut Harness, &Arc<MemStore>);
type Opener = fn(&str) -> std::io::Result<()>;

struct Case {
    name: &'static str,
    args: &'static [&'static str],
    setup: Setup,
    opener: Option<Opener>,
    /// Whether the store the command used is a failing one.
    failing: Option<(CredentialError, FailOn)>,
    expect_code: i32,
}

fn nothing(_: &World, _: &mut Harness, _: &Arc<MemStore>) {}

macro_rules! setup {
    ($body:expr) => {{
        fn f(w: &World, h: &mut Harness, s: &Arc<MemStore>) {
            let body: fn(&World, &mut Harness, &Arc<MemStore>) = $body;
            body(w, h, s)
        }
        f as Setup
    }};
}

fn cases() -> Vec<Case> {
    let github = &["auth", "github"][..];
    let google = &["auth", "google"][..];
    vec![
        // ---- auth github (G1)
        Case {
            name: "github 200",
            args: github,
            setup: setup!(|_, h, _| h.input = ScriptedInput::typed(GITHUB_TOKEN)),
            opener: None,
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "github 200 via stdin with CRLF",
            args: &["auth", "github", "-"],
            setup: setup!(|_, h, _| h.input = ScriptedInput::piped(&format!("{GITHUB_TOKEN}\r\n"))),
            opener: None,
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "github 401",
            args: github,
            setup: setup!(|w, h, _| {
                w.fakes.github.revoke_token();
                h.input = ScriptedInput::typed(GITHUB_TOKEN);
            }),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "github 403",
            args: github,
            setup: setup!(|w, h, _| {
                w.fakes.github.fail_next(1, 403);
                h.input = ScriptedInput::typed(GITHUB_TOKEN);
            }),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "github rate limit",
            args: github,
            setup: setup!(|w, h, _| {
                w.fakes
                    .github
                    .rate_limit_requests(1, tephra_remote::testing::GithubLimit::RetryAfter403);
                h.input = ScriptedInput::typed(GITHUB_TOKEN);
            }),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "github 500",
            args: github,
            setup: setup!(|w, h, _| {
                w.fakes.github.fail_next(1, 500);
                h.input = ScriptedInput::typed(GITHUB_TOKEN);
            }),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "github connection refused",
            args: github,
            setup: setup!(|_, h, _| {
                h.github_base_url = Some(format!(
                    "http://127.0.0.1:{}",
                    crate::auth_support::closed_port()
                ));
                h.input = ScriptedInput::typed(GITHUB_TOKEN);
            }),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "github token with whitespace",
            args: github,
            setup: setup!(|_, h, _| h.input = ScriptedInput::typed("ghp_SENTINEL with space")),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "github no dash and no terminal (a token waits on stdin)",
            args: github,
            setup: setup!(|_, h, _| h.input = ScriptedInput::piped(GITHUB_TOKEN)),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "github refused base url",
            args: github,
            setup: setup!(|_, h, _| {
                h.github_base_url = Some("https://evil.example".to_string());
                h.input = ScriptedInput::typed(GITHUB_TOKEN);
            }),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        // ---- E17: a failing store
        Case {
            name: "github, keychain locked",
            args: github,
            setup: setup!(|_, h, _| h.input = ScriptedInput::typed(GITHUB_TOKEN)),
            opener: None,
            failing: Some((CredentialError::Locked, FailOn::All)),
            expect_code: 1,
        },
        Case {
            name: "github, keychain write denied",
            args: github,
            setup: setup!(|_, h, _| h.input = ScriptedInput::typed(GITHUB_TOKEN)),
            opener: None,
            failing: Some((CredentialError::Denied, FailOn::SetOnly)),
            expect_code: 1,
        },
        Case {
            name: "google, keychain locked",
            args: google,
            setup: nothing,
            opener: Some(follow),
            failing: Some((CredentialError::Locked, FailOn::All)),
            expect_code: 1,
        },
        Case {
            name: "google, keychain write denied",
            args: google,
            setup: nothing,
            opener: Some(follow),
            failing: Some((CredentialError::Denied, FailOn::SetOnly)),
            expect_code: 1,
        },
        // ---- auth google (G1 and the failures of the flow)
        Case {
            name: "google success",
            args: google,
            setup: setup!(|_, h, _| h.timeout = Duration::from_secs(20)),
            opener: Some(follow),
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "google wrong state first, then right",
            args: google,
            setup: setup!(|_, h, _| h.timeout = Duration::from_secs(20)),
            opener: Some(follow_wrong_state_first),
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "google denied",
            args: google,
            setup: setup!(|w, h, _| {
                w.fakes.google.deny_next_consent();
                h.timeout = Duration::from_secs(20);
            }),
            opener: Some(follow),
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "google timeout",
            args: google,
            setup: nothing,
            opener: Some(|_| Ok(())),
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "google invalid_grant (the fake echoes code and verifier)",
            args: google,
            setup: setup!(|w, h, _| {
                w.fakes.google.fail_next_token(400, "invalid_grant");
                h.timeout = Duration::from_secs(20);
            }),
            opener: Some(follow),
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "google token endpoint 500",
            args: google,
            setup: setup!(|w, h, _| {
                w.fakes.google.fail_next_token(500, "backend_error");
                h.timeout = Duration::from_secs(20);
            }),
            opener: Some(follow),
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "google no refresh token",
            args: google,
            setup: setup!(|w, h, _| {
                w.fakes.google.omit_refresh_token_next();
                h.timeout = Duration::from_secs(20);
            }),
            opener: Some(follow),
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "google client json unset",
            args: google,
            setup: setup!(|_, h, _| h.client_json = None),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "google client json malformed (holds the client secret)",
            args: google,
            setup: setup!(|w, h, _| {
                let path = w.inputs.path().join("client.json");
                std::fs::write(
                    &path,
                    format!("{{\"installed\": {{\"client_secret\": \"{GOOGLE_CLIENT_SECRET}\", "),
                )
                .unwrap();
                h.client_json = Some(path);
            }),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        Case {
            name: "google client json points the secret at another host",
            args: google,
            setup: setup!(|w, h, _| {
                let path = w.inputs.path().join("client.json");
                std::fs::write(
                    &path,
                    format!(
                        "{{\"installed\": {{\"client_id\": \"x\", \"client_secret\": \
                         \"{GOOGLE_CLIENT_SECRET}\", \"auth_uri\": \
                         \"https://accounts.google.com/o/oauth2/auth\", \"token_uri\": \
                         \"https://evil.example/token\"}}}}"
                    ),
                )
                .unwrap();
                h.client_json = Some(path);
            }),
            opener: None,
            failing: None,
            expect_code: 1,
        },
        // ---- auth status (G2, E2)
        Case {
            name: "status with both signed in",
            args: &["auth", "status"],
            setup: setup!(|_, _, s| {
                s.put(Provider::Github, STORED_GITHUB);
                s.put(
                    Provider::Google,
                    &format!(
                        "{{\"access_token\":\"{GOOGLE_ACCESS}\",\"refresh_token\":\"{GOOGLE_REFRESH}\"}}"
                    ),
                );
            }),
            opener: None,
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "status --json with both signed in",
            args: &["auth", "status", "--json"],
            setup: setup!(|_, _, s| {
                s.put(Provider::Github, STORED_GITHUB);
                s.put(
                    Provider::Google,
                    &format!(
                        "{{\"access_token\":\"{GOOGLE_ACCESS}\",\"refresh_token\":\"{GOOGLE_REFRESH}\"}}"
                    ),
                );
            }),
            opener: None,
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "status, nothing stored",
            args: &["auth", "status"],
            setup: nothing,
            opener: None,
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "status, keychain locked",
            args: &["auth", "status"],
            setup: nothing,
            opener: None,
            failing: Some((CredentialError::Locked, FailOn::All)),
            expect_code: 0,
        },
        Case {
            name: "status --json, keychain denied",
            args: &["auth", "status", "--json"],
            setup: nothing,
            opener: None,
            failing: Some((
                CredentialError::Backend(format!("status -25293 {STORED_GITHUB}")),
                FailOn::All,
            )),
            expect_code: 0,
        },
        // ---- auth logout (G3)
        Case {
            name: "logout github",
            args: &["auth", "logout", "github"],
            setup: setup!(|_, _, s| s.put(Provider::Github, STORED_GITHUB)),
            opener: None,
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "logout google, not signed in",
            args: &["auth", "logout", "google"],
            setup: nothing,
            opener: None,
            failing: None,
            expect_code: 0,
        },
        Case {
            name: "logout, keychain denied",
            args: &["auth", "logout", "github"],
            setup: nothing,
            opener: None,
            failing: Some((CredentialError::Denied, FailOn::All)),
            expect_code: 1,
        },
    ]
}

#[test]
fn no_outcome_leaves_a_secret_in_any_output_or_file() {
    for case in cases() {
        let world = World::new();
        let mem = Arc::new(MemStore::default());
        let store: Arc<dyn CredentialStore> = match &case.failing {
            Some((error, on)) => Arc::new(FailingStore::new(error.clone(), *on)),
            None => mem.clone(),
        };
        let mut h = world.harness(store);
        (case.setup)(&world, &mut h, &mem);

        let ran = match case.opener {
            Some(open) => h.run(case.args, &open),
            None => h.run_plain(case.args),
        };

        assert_eq!(ran.code, case.expect_code, "{}: {ran:?}", case.name);
        world.scan(case.name, &[&ran]);
        // Let a lingering follower thread finish before the dirs go.
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn the_scan_would_catch_a_leak() {
    // Guard against a scan that can never fail: plant each kind of leak.
    let world = World::new();
    let leaky_out = Ran {
        code: 0,
        out: format!("signed in as {}\n", &GITHUB_TOKEN[14..30]),
        err: String::new(),
    };
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        world.scan("planted output leak", &[&leaky_out]);
    }));
    assert!(
        caught.is_err(),
        "an 8-character piece in stdout went unseen"
    );

    let clean = Ran {
        code: 0,
        out: "signed in as octocat\n".into(),
        err: String::new(),
    };
    for (dir, name) in [
        (world.config.path(), "config.toml"),
        (world.data.path(), "cache.db"),
        (world.data.path(), "tephra.log"),
    ] {
        let path = dir.join(name);
        let before = std::fs::read(&path).unwrap();
        let mut leaked = before.clone();
        leaked.extend_from_slice(GOOGLE_REFRESH.as_bytes());
        std::fs::write(&path, leaked).unwrap();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            world.scan("planted file leak", &[&clean]);
        }));
        assert!(caught.is_err(), "a secret in {name} went unseen");
        std::fs::write(&path, before).unwrap();
    }
    world.scan("clean", &[&clean]);
}

/// Fixed text that holds neither a secret nor a piece of one: the
/// words used in messages must not themselves trip the 8-character scan.
#[test]
fn the_sentinels_share_no_eight_characters_with_the_fixed_messages() {
    let words = [
        "signed in as ",
        "Keychain unavailable",
        "auth_required",
        "consent denied",
        "Nothing was stored",
        "rejected the token",
        "Waiting up to",
        "https://www.googleapis.com/auth/drive",
        "tephractl: ",
    ];
    for secret in secrets() {
        for w in words {
            assert!(leaks(w.as_bytes(), secret).is_none(), "{w}");
        }
    }
}

/// F10, by source: the only code in `tephractl` that opens a listening
/// socket is `auth/google.rs`, and it binds `127.0.0.1` on port 0, never
/// a wildcard address. (A runtime check cannot show that a command opens
/// no socket; this one pins where sockets can come from.)
#[test]
fn only_the_google_flow_can_open_a_listener_and_only_on_loopback() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut listeners = Vec::new();
    for (path, bytes) in files_under(&src) {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // Test modules inside a file may mention an address as a string.
        let code = text.split("#[cfg(test)]").next().unwrap_or("");
        for word in ["0.0.0.0", "UNSPECIFIED", "[::]", "Ipv6Addr"] {
            assert!(!code.contains(word), "{path:?} names {word}");
        }
        if code.contains("TcpListener") || code.contains("UnixListener") {
            listeners.push(path);
        }
    }
    assert_eq!(listeners.len(), 1, "{listeners:?}");
    assert!(listeners[0].ends_with("auth/google.rs"), "{listeners:?}");
    let google = std::fs::read_to_string(&listeners[0]).unwrap();
    assert!(google.contains("TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))"));
    assert_eq!(google.matches("::bind(").count(), 1);
}
