//! `tephractl auth github|status|logout` (PR-4 G1, G2, G3, E2): the real
//! parser and command code, run in process against the PR-3 fake GitHub
//! on an ephemeral port and an in-memory credential store. (Not
//! `auth.rs`: that file is the RPC bearer-token test.)
//!
//! Usage errors are also run through the real binary: they stop in the
//! parser, before any store is opened, so they never touch a Keychain.

use std::sync::Arc;

use tephra_remote::credentials::{CredentialError, Provider};
use tephra_remote::testing::GithubLimit;

use crate::auth_support::{
    closed_port, parse, FailOn, FailingStore, Fakes, Harness, MemStore, ScriptedInput,
};

const TOKEN: &str = "ghp_TESTTOKEN0123456789abcdefghijklmnopq";

fn harness(fakes: &Fakes, store: Arc<MemStore>, dir: &tempfile::TempDir) -> Harness {
    let mut h = Harness::new(store, dir.path());
    h.github_base_url = Some(fakes.github.base_url().to_string());
    h
}

// ---- G1: validate, then store ----------------------------------------

#[test]
fn a_200_stores_the_token_and_prints_who_signed_in() {
    let fakes = Fakes::start();
    fakes.github.set_login("octocat");
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    h.input = ScriptedInput::typed(TOKEN);

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 0, "{ran:?}");
    assert_eq!(ran.out, "signed in as octocat\n");
    assert_eq!(store.get_text(Provider::Github).as_deref(), Some(TOKEN));
    assert!(h.input.asked_hidden && !h.input.asked_line);
    // One request, GET /user, with the token as the bearer.
    let log = fakes.github.request_log().all();
    assert_eq!(log.len(), 1, "{log:?}");
    assert_eq!(
        (log[0].method.as_str(), log[0].path.as_str()),
        ("GET", "/user")
    );
    assert_eq!(
        log[0].header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}

#[test]
fn the_dash_form_reads_one_line_from_stdin() {
    let fakes = Fakes::start();
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    h.input = ScriptedInput::piped(&format!("{TOKEN}\nsecond line is never read\n"));

    let ran = h.run_plain(&["auth", "github", "-"]);

    assert_eq!(ran.code, 0, "{ran:?}");
    assert_eq!(store.get_text(Provider::Github).as_deref(), Some(TOKEN));
    assert!(h.input.asked_line && !h.input.asked_hidden);
}

#[test]
fn stdin_with_crlf_or_a_trailing_newline_stores_the_trimmed_token() {
    for text in [
        format!("{TOKEN}\r\n"),
        format!("{TOKEN}\n"),
        TOKEN.to_string(),
    ] {
        let fakes = Fakes::start();
        let store = Arc::new(MemStore::default());
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(&fakes, store.clone(), &dir);
        h.input = ScriptedInput::piped(&text);
        let ran = h.run_plain(&["auth", "github", "-"]);
        assert_eq!(ran.code, 0, "{text:?}: {ran:?}");
        assert_eq!(
            store.get_text(Provider::Github).as_deref(),
            Some(TOKEN),
            "{text:?}"
        );
        let sent = fakes.github.request_log().last().unwrap();
        assert_eq!(
            sent.header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
    }
}

#[test]
fn without_the_dash_and_without_a_terminal_it_exits_1_and_does_not_read_stdin() {
    let fakes = Fakes::start();
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    // Stdin holds a token, but the command must not take it without `-`.
    h.input = ScriptedInput::piped(&format!("{TOKEN}\n"));

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("auth github -"), "{}", ran.err);
    assert!(!h.input.asked_line && !h.input.asked_hidden);
    assert_eq!(store.len(), 0);
    assert_eq!(fakes.github.request_log().count(), 0);
}

#[test]
fn a_401_exits_1_names_the_reason_and_stores_nothing() {
    let fakes = Fakes::start();
    fakes.github.revoke_token();
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    h.input = ScriptedInput::typed(TOKEN);

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("rejected the token (401)"), "{}", ran.err);
    assert!(ran.out.is_empty());
    assert_eq!(store.len(), 0);
    assert_eq!(store.set_count(), 0);
}

#[test]
fn a_plain_403_exits_1_names_the_reason_and_stores_nothing() {
    let fakes = Fakes::start();
    fakes.github.fail_next(1, 403);
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    h.input = ScriptedInput::typed(TOKEN);

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("403"), "{}", ran.err);
    assert_eq!(store.len(), 0);
}

#[test]
fn a_403_rate_limit_exits_1_says_so_with_a_reset_time_and_stores_nothing() {
    for style in [
        GithubLimit::RetryAfter403,
        GithubLimit::Reset403,
        GithubLimit::Secondary403,
    ] {
        let fakes = Fakes::start();
        fakes.github.rate_limit_requests(1, style);
        let store = Arc::new(MemStore::default());
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(&fakes, store.clone(), &dir);
        h.input = ScriptedInput::typed(TOKEN);

        let ran = h.run_plain(&["auth", "github"]);

        assert_eq!(ran.code, 1, "{ran:?}");
        assert!(ran.err.contains("rate limit"), "{}", ran.err);
        assert!(ran.err.contains("UTC"), "{}", ran.err);
        assert_eq!(store.len(), 0);
    }
}

#[test]
fn a_5xx_exits_1_and_stores_nothing() {
    let fakes = Fakes::start();
    fakes.github.fail_next(1, 500);
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    h.input = ScriptedInput::typed(TOKEN);

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("server error (500)"), "{}", ran.err);
    assert_eq!(store.len(), 0);
}

#[test]
fn connection_refused_exits_1_names_the_reason_and_stores_nothing() {
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(store.clone(), dir.path());
    h.github_base_url = Some(format!("http://127.0.0.1:{}", closed_port()));
    h.input = ScriptedInput::typed(TOKEN);

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("could not reach GitHub"), "{}", ran.err);
    assert_eq!(store.len(), 0);
}

#[test]
fn empty_and_whitespace_tokens_exit_1_send_nothing_and_store_nothing() {
    for bad in [
        "",
        "   ",
        "ghp_with space",
        "ghp_with\ttab",
        " ghp_leading",
        "ghp_trailing ",
        "ghp_ctl\u{1}char",
    ] {
        let fakes = Fakes::start();
        let store = Arc::new(MemStore::default());
        let dir = tempfile::tempdir().unwrap();
        let mut h = harness(&fakes, store.clone(), &dir);
        h.input = ScriptedInput::typed(bad);

        let ran = h.run_plain(&["auth", "github"]);

        assert_eq!(ran.code, 1, "{bad:?}: {ran:?}");
        assert!(
            ran.err.contains("empty")
                || ran.err.contains("whitespace")
                || ran.err.contains("characters"),
            "{bad:?}: {}",
            ran.err
        );
        assert!(!ran.err.contains("ghp_with"), "{}", ran.err);
        assert_eq!(store.len(), 0, "{bad:?}");
        assert_eq!(
            fakes.github.request_log().count(),
            0,
            "{bad:?}: nothing sent"
        );
    }
}

#[test]
fn a_hostile_login_is_printed_without_control_characters() {
    let fakes = Fakes::start();
    fakes.github.set_login("evil\u{1b}[2Jname");
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    h.input = ScriptedInput::typed(TOKEN);

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(!ran.out.contains('\u{1b}'), "{:?}", ran.out);
}

#[test]
fn a_github_api_url_that_is_not_loopback_is_refused_before_any_send() {
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(store.clone(), dir.path());
    h.github_base_url = Some("https://evil.example".to_string());
    h.input = ScriptedInput::typed(TOKEN);

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("github_api_url"), "{}", ran.err);
    assert_eq!(store.len(), 0);
}

// ---- G1: a token is never an argument ---------------------------------

#[test]
fn a_token_argument_is_a_usage_error_that_does_not_echo_it() {
    for args in [
        vec!["auth", "github", TOKEN],
        vec!["auth", "github", "-", TOKEN],
        vec!["auth", "github", TOKEN, "-"],
        vec!["--json", "auth", "github", TOKEN],
    ] {
        let err = tephractl::cli::parse_args(args.iter().copied()).unwrap_err();
        assert!(err.0.contains("no token argument"), "{args:?}: {err}");
        assert!(!err.0.contains("ghp_"), "{args:?}: {err}");
    }
}

#[test]
fn the_binary_exits_2_on_a_token_argument_and_sends_nothing() {
    let fakes = Fakes::start();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    std::fs::create_dir_all(config.join("tephra")).unwrap();
    std::fs::write(
        config.join("tephra/config.toml"),
        format!("github_api_url = \"{}\"\n", fakes.github.base_url()),
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
        .args(["auth", "github", TOKEN])
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", &config)
        .env("XDG_DATA_HOME", home.path().join("data"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();

    assert_eq!(out.status.code(), Some(2));
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(
        !stdout.contains("ghp_") && !stderr.contains("ghp_"),
        "{stdout}{stderr}"
    );
    assert_eq!(fakes.github.request_log().count(), 0, "nothing sent");
    assert!(!home.path().join("data").exists(), "nothing written");
}

#[test]
fn the_binary_exits_2_on_every_auth_usage_error() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        vec!["auth"],
        vec!["auth", "bogus"],
        vec!["auth", "google", "extra"],
        vec!["auth", "status", "extra"],
        vec!["auth", "logout"],
        vec!["auth", "logout", "all"],
        vec!["auth", "logout", "gitlab"],
        vec!["auth", "logout", "github", "google"],
        vec!["--json", "auth", "google"],
        vec!["--addr", "127.0.0.1:1", "auth", "status"],
    ] {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
            .args(&args)
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join("c"))
            .env("XDG_DATA_HOME", home.path().join("d"))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    assert!(!home.path().join("d").exists());
}

#[test]
fn logout_with_another_provider_name_is_a_usage_error_that_does_not_echo_it() {
    for bad in ["all", "gitlab", "ghp_SOMETHING"] {
        let err = tephractl::cli::parse_args(["auth", "logout", bad]).unwrap_err();
        assert!(err.0.contains("github or google"), "{err}");
        assert!(!err.0.contains(bad), "{err}");
    }
    assert!(tephractl::cli::parse_args(["auth", "logout"]).is_err());
}

// ---- G2: status -------------------------------------------------------

#[test]
fn status_prints_one_line_per_provider_from_the_store_only() {
    let fakes = Fakes::start();
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);

    let none = h.run_plain(&["auth", "status"]);
    assert_eq!(none.code, 0);
    assert_eq!(none.out, "github: auth_required\ngoogle: auth_required\n");

    store.put(Provider::Github, TOKEN);
    let one = h.run_plain(&["auth", "status"]);
    assert_eq!(one.code, 0);
    assert_eq!(one.out, "github: signed in\ngoogle: auth_required\n");

    store.put(Provider::Google, "{\"refresh_token\":\"1//SECRETREFRESH\"}");
    let both = h.run_plain(&["auth", "status"]);
    assert_eq!(both.out, "github: signed in\ngoogle: signed in\n");
    for shown in [&none, &one, &both] {
        assert!(!shown.out.contains(TOKEN) && !shown.out.contains("SECRETREFRESH"));
        assert!(!shown.out.contains("ghp_") && shown.err.is_empty());
    }
    // No network: the fake saw nothing.
    assert_eq!(fakes.github.request_log().count(), 0);
}

#[test]
fn status_json_has_the_two_providers_and_no_secret() {
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(store.clone(), dir.path());
    store.put(Provider::Github, TOKEN);

    for args in [
        &["auth", "status", "--json"][..],
        &["--json", "auth", "status"][..],
    ] {
        let ran = h.run_plain(args);
        assert_eq!(ran.code, 0);
        let v: serde_json::Value = serde_json::from_str(ran.out.trim()).unwrap();
        assert_eq!(
            v,
            serde_json::json!({
                "github": {"state": "signed in"},
                "google": {"state": "auth_required"},
            })
        );
        assert!(!ran.out.contains("ghp_"));
    }
}

// ---- G3: logout ---------------------------------------------------------

#[test]
fn logout_deletes_that_provider_only_and_status_follows() {
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(store.clone(), dir.path());
    store.put(Provider::Github, TOKEN);
    store.put(Provider::Google, "google-set");

    let ran = h.run_plain(&["auth", "logout", "github"]);

    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(ran.out.contains("signed out of github"), "{}", ran.out);
    assert!(ran.out.contains("local only"), "{}", ran.out);
    assert_eq!(store.get_text(Provider::Github), None);
    assert_eq!(
        store.get_text(Provider::Google).as_deref(),
        Some("google-set")
    );
    let status = h.run_plain(&["auth", "status"]);
    assert_eq!(status.out, "github: auth_required\ngoogle: signed in\n");

    let ran = h.run_plain(&["auth", "logout", "google"]);
    assert_eq!(ran.code, 0);
    assert_eq!(store.len(), 0);
}

#[test]
fn logout_when_not_signed_in_exits_0_and_deletes_nothing() {
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(store.clone(), dir.path());

    for provider in ["github", "google"] {
        let ran = h.run_plain(&["auth", "logout", provider]);
        assert_eq!(ran.code, 0, "{ran:?}");
        assert!(ran.out.contains("not signed in"), "{}", ran.out);
    }
    // Twice in a row is the same.
    let again = h.run_plain(&["auth", "logout", "github"]);
    assert_eq!(again.code, 0);
    assert_eq!(*store.deletes.lock().unwrap(), 0);
}

#[test]
fn logout_makes_no_network_call() {
    let fakes = Fakes::start();
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    store.put(Provider::Github, TOKEN);
    store.put(Provider::Google, "g");
    h.run_plain(&["auth", "logout", "github"]);
    h.run_plain(&["auth", "logout", "google"]);
    assert_eq!(fakes.github.request_log().count(), 0);
    assert_eq!(fakes.google.request_log().count(), 0);
}

// ---- E2: a revoked credential --------------------------------------------

#[test]
fn status_does_not_probe_and_a_new_token_replaces_the_item() {
    let fakes = Fakes::start();
    let store = Arc::new(MemStore::default());
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    h.input = ScriptedInput::typed(TOKEN);
    assert_eq!(h.run_plain(&["auth", "github"]).code, 0);
    let after_signin = fakes.github.request_log().count();

    // GitHub now rejects the stored token; status still just reads the store.
    fakes.github.revoke_token();
    let status = h.run_plain(&["auth", "status"]);
    assert_eq!(status.out, "github: signed in\ngoogle: auth_required\n");
    assert_eq!(fakes.github.request_log().count(), after_signin, "no probe");

    // A fresh token replaces the item: one item, the new token.
    let fresh = "github_pat_FRESH0123456789";
    fakes.github.accept_only_token(fresh);
    h.input = ScriptedInput::typed(fresh);
    let ran = h.run_plain(&["auth", "github"]);
    assert_eq!(ran.code, 0, "{ran:?}");
    assert_eq!(store.len(), 1);
    assert_eq!(store.get_text(Provider::Github).as_deref(), Some(fresh));
}

#[test]
fn a_rejected_new_token_leaves_the_old_item_alone() {
    let fakes = Fakes::start();
    let store = Arc::new(MemStore::default());
    store.put(Provider::Github, "old-token");
    let dir = tempfile::tempdir().unwrap();
    let mut h = harness(&fakes, store.clone(), &dir);
    fakes.github.accept_only_token("something-else");
    h.input = ScriptedInput::typed(TOKEN);

    let ran = h.run_plain(&["auth", "github"]);

    assert_eq!(ran.code, 1);
    assert_eq!(
        store.get_text(Provider::Github).as_deref(),
        Some("old-token")
    );
}

// ---- E17: the store is unavailable ---------------------------------------

#[test]
fn a_failing_store_gives_keychain_unavailable_and_nothing_is_written_elsewhere() {
    for error in [
        CredentialError::Locked,
        CredentialError::Denied,
        CredentialError::Unavailable,
        CredentialError::Backend("keychain status -1".to_string()),
    ] {
        for on in [FailOn::All, FailOn::SetOnly] {
            let fakes = Fakes::start();
            let store = Arc::new(FailingStore::new(error.clone(), on));
            let dir = tempfile::tempdir().unwrap();
            let mut h = Harness::new(store, dir.path());
            h.github_base_url = Some(fakes.github.base_url().to_string());
            h.input = ScriptedInput::typed(TOKEN);

            let ran = h.run_plain(&["auth", "github"]);

            assert_eq!(ran.code, 1, "{error:?}/{on:?}: {ran:?}");
            assert!(ran.err.contains("Keychain unavailable"), "{}", ran.err);
            assert!(!ran.err.contains(TOKEN) && !ran.out.contains(TOKEN));
            // No plaintext fallback: the config dir stays empty.
            assert_eq!(
                std::fs::read_dir(dir.path()).unwrap().count(),
                0,
                "{error:?}/{on:?}"
            );
            // Status shows auth_required, not an error.
            let status = h.run_plain(&["auth", "status"]);
            assert_eq!(status.code, 0);
            assert!(
                status.out.starts_with("github: auth_required"),
                "{}",
                status.out
            );
        }
    }
}

#[test]
fn logout_with_a_failing_store_exits_1_with_keychain_unavailable() {
    let store = Arc::new(FailingStore::new(CredentialError::Denied, FailOn::All));
    let dir = tempfile::tempdir().unwrap();
    let mut h = Harness::new(store, dir.path());
    let ran = h.run_plain(&["auth", "logout", "github"]);
    assert_eq!(ran.code, 1);
    assert!(ran.err.contains("Keychain unavailable"), "{}", ran.err);
}

#[test]
fn parse_gives_the_command_each_usage_line_names() {
    use tephractl::cli::Command;
    assert_eq!(
        parse(&["auth", "github"]),
        Command::AuthGithub { stdin: false }
    );
    assert_eq!(
        parse(&["auth", "github", "-"]),
        Command::AuthGithub { stdin: true }
    );
    assert_eq!(parse(&["auth", "google"]), Command::AuthGoogle);
    assert_eq!(parse(&["auth", "status"]), Command::AuthStatus);
    assert_eq!(
        parse(&["auth", "logout", "google"]),
        Command::AuthLogout {
            provider: Provider::Google
        }
    );
}

// ---- the real binary with the test file store --------------------------
//
// Only in a build with `--features test-credential-store` (CI's default
// build has no file store, and a default-feature binary would use the real
// Keychain). `auth google` is not run through the binary: its opener is
// the real `open`, which a test must never start.

#[cfg(feature = "test-credential-store")]
#[test]
fn the_binary_signs_in_reports_and_signs_out_through_the_file_store() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    let fakes = Fakes::start();
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config");
    let data = home.path().join("data");
    std::fs::create_dir_all(config.join("tephra")).unwrap();
    std::fs::create_dir_all(data.join("tephra")).unwrap();
    let cred = data.join("tephra/cred.json");
    std::fs::write(
        config.join("tephra/config.toml"),
        format!(
            "github_api_url = \"{}\"\ntest_credential_file = \"{}\"\n",
            fakes.github.base_url(),
            cred.display()
        ),
    )
    .unwrap();
    let run = |args: &[&str], stdin: Option<&str>| {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_tephractl"))
            .args(args)
            .env_clear()
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_DATA_HOME", &data)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin.unwrap_or("").as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();

    fakes.github.set_login("octocat");
    let out = run(&["auth", "github", "-"], Some(&format!("{TOKEN}\n")));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "signed in as octocat\n");
    // The token is in the credential file (mode 0600) and nowhere else.
    assert_eq!(
        std::fs::metadata(&cred).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(String::from_utf8_lossy(&std::fs::read(&cred).unwrap()).contains(TOKEN));
    for (path, bytes) in crate::auth_support::files_under(home.path()) {
        if path != cred {
            assert!(
                !crate::auth_support::contains_bytes(&bytes, TOKEN),
                "{path:?}"
            );
        }
    }

    let out = run(&["auth", "status"], None);
    assert_eq!(
        text(&out.stdout),
        "github: signed in\ngoogle: auth_required\n"
    );
    assert!(!text(&out.stdout).contains(TOKEN) && !text(&out.stderr).contains(TOKEN));

    let out = run(&["auth", "logout", "github"], None);
    assert_eq!(out.status.code(), Some(0));
    let out = run(&["auth", "status", "--json"], None);
    assert_eq!(
        text(&out.stdout).trim(),
        r#"{"github":{"state":"auth_required"},"google":{"state":"auth_required"}}"#
    );

    // `auth google` with no client JSON stops before any browser or port.
    let out = run(&["auth", "google"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("google_client_json"));
}
