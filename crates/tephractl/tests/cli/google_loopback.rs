//! `tephractl auth google` (PR-4 G1, GP1, F10 and the Google edge
//! cases): the whole flow against the PR-3 fake Google authorize and
//! token endpoints. The "browser" is a test opener that follows the
//! consent redirect over a plain socket, so the real loopback listener,
//! PKCE and token exchange all run. No real network, no Keychain.

use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tephra_remote::credentials::{CredentialError, Provider};
use tephractl::auth::pkce;

use crate::auth_support::{
    consent_url_in, files_under, http, port_in, query_param, FailOn, FailingStore, Fakes, Harness,
    MemStore, Ran, Reply,
};

/// What the test "browser" does after it is handed the consent URL.
#[derive(Clone, Copy, Debug)]
enum Plan {
    /// Follow the redirect to the callback.
    Normal,
    /// Do nothing at all (the user walked away).
    Nothing,
    /// A callback with the wrong `state`, then the right one.
    WrongStateThenRight,
    /// A callback with the wrong `state`, then nothing.
    WrongStateOnly,
    /// A callback without `state`, then nothing.
    MissingStateOnly,
    /// Stray requests, the real callback, then a second callback.
    StraysThenRealThenReplay,
    /// Stray requests only.
    StraysOnly,
}

type Seen = Vec<(String, Result<Reply, String>)>;

/// The test opener.
struct Browser {
    plan: Plan,
    seen: Arc<Mutex<Seen>>,
    handle: Mutex<Option<JoinHandle<()>>>,
    /// Fails the "open" (after still following the URL) when set.
    open_fails: bool,
}

impl Browser {
    fn new(plan: Plan) -> Browser {
        Browser {
            plan,
            seen: Arc::new(Mutex::new(Vec::new())),
            handle: Mutex::new(None),
            open_fails: false,
        }
    }

    fn opener(&self) -> impl Fn(&str) -> std::io::Result<()> + Sync + '_ {
        move |url| {
            self.start(url);
            if self.open_fails {
                Err(std::io::Error::other("no browser here"))
            } else {
                Ok(())
            }
        }
    }

    fn start(&self, url: &str) {
        let (url, plan, seen) = (url.to_string(), self.plan, self.seen.clone());
        let handle = std::thread::spawn(move || drive(&url, plan, &seen));
        *self.handle.lock().unwrap() = Some(handle);
    }

    /// Waits for the follower and returns what it saw.
    fn finish(&self) -> Seen {
        if let Some(h) = self.handle.lock().unwrap().take() {
            h.join().unwrap();
        }
        self.seen.lock().unwrap().clone()
    }
}

fn note(seen: &Arc<Mutex<Seen>>, label: &str, r: std::io::Result<Reply>) -> Option<Reply> {
    let ok = r.as_ref().ok().cloned();
    seen.lock()
        .unwrap()
        .push((label.to_string(), r.map_err(|e| e.to_string())));
    ok
}

fn replace_param(url: &str, key: &str, value: &str) -> String {
    let (base, query) = url.split_once('?').unwrap();
    let rebuilt: Vec<String> = query
        .split('&')
        .map(|p| match p.split_once('=') {
            Some((k, _)) if k == key => format!("{k}={value}"),
            _ => p.to_string(),
        })
        .collect();
    format!("{base}?{}", rebuilt.join("&"))
}

fn without_param(url: &str, key: &str) -> String {
    let (base, query) = url.split_once('?').unwrap();
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| p.split_once('=').map(|(k, _)| k) != Some(key))
        .collect();
    format!("{base}?{}", kept.join("&"))
}

fn authority(url: &str) -> &str {
    let rest = url.strip_prefix("http://").unwrap();
    rest.split('/').next().unwrap()
}

fn drive(url: &str, plan: Plan, seen: &Arc<Mutex<Seen>>) {
    if matches!(plan, Plan::Nothing) {
        return;
    }
    let Some(consent) = note(seen, "consent", http("GET", url, None)) else {
        return;
    };
    let Some(loc) = consent.location else {
        return;
    };
    let base = format!("http://{}", authority(&loc));
    let strays = |seen: &Arc<Mutex<Seen>>| {
        note(
            seen,
            "favicon",
            http("GET", &format!("{base}/favicon.ico"), None),
        );
        note(seen, "root", http("GET", &format!("{base}/"), None));
        note(seen, "post", http("POST", &loc, None));
        note(seen, "head", http("HEAD", &loc, None));
        note(
            seen,
            "bare callback",
            http("GET", &format!("{base}/callback"), None),
        );
        note(
            seen,
            "empty code",
            http(
                "GET",
                &format!(
                    "{base}/callback?code=&state={}",
                    query_param(&loc, "state").unwrap_or_default()
                ),
                None,
            ),
        );
        note(
            seen,
            "foreign host",
            http("GET", &loc, Some("evil.example")),
        );
    };
    match plan {
        Plan::Normal => {
            note(seen, "callback", http("GET", &loc, None));
        }
        Plan::WrongStateThenRight => {
            note(
                seen,
                "wrong state",
                http("GET", &replace_param(&loc, "state", "WRONGSTATE"), None),
            );
            note(seen, "callback", http("GET", &loc, None));
        }
        Plan::WrongStateOnly => {
            note(
                seen,
                "wrong state",
                http("GET", &replace_param(&loc, "state", "WRONGSTATE"), None),
            );
        }
        Plan::MissingStateOnly => {
            note(
                seen,
                "missing state",
                http("GET", &without_param(&loc, "state"), None),
            );
        }
        Plan::StraysOnly => strays(seen),
        Plan::StraysThenRealThenReplay => {
            strays(seen);
            note(seen, "callback", http("GET", &loc, None));
            note(seen, "replay", http("GET", &loc, None));
        }
        Plan::Nothing => unreachable!(),
    }
}

fn status_of(seen: &Seen, label: &str) -> Vec<u16> {
    seen.iter()
        .filter(|(l, _)| l == label)
        .filter_map(|(_, r)| r.as_ref().ok().map(|r| r.status))
        .collect()
}

/// A temp dir holding the client JSON (kept apart from the dir the
/// command may write into).
struct Setup {
    fakes: Fakes,
    store: Arc<MemStore>,
    inputs: tempfile::TempDir,
    config: tempfile::TempDir,
}

impl Setup {
    fn new() -> Setup {
        Setup {
            fakes: Fakes::start(),
            store: Arc::new(MemStore::default()),
            inputs: tempfile::tempdir().unwrap(),
            config: tempfile::tempdir().unwrap(),
        }
    }

    fn client_file(&self, text: &str) -> std::path::PathBuf {
        let path = self.inputs.path().join("client.json");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn harness(&self) -> Harness {
        let mut h = Harness::new(self.store.clone(), self.config.path());
        h.client_json = Some(self.client_file(&self.fakes.google.client_json()));
        h.timeout = Duration::from_secs(1);
        h
    }

    fn run(&self, h: &mut Harness, browser: &Browser) -> (Ran, Seen) {
        let ran = h.run(&["auth", "google"], &browser.opener());
        let seen = browser.finish();
        (ran, seen)
    }

    /// The config dir stays empty: nothing is written anywhere.
    fn assert_nothing_written(&self) {
        assert!(files_under(self.config.path()).is_empty());
    }
}

fn port_is_closed(port: u16) -> bool {
    TcpStream::connect(("127.0.0.1", port)).is_err()
}

// ---- G1, GP1: the full flow ------------------------------------------------

#[test]
fn the_full_flow_stores_the_token_set_and_closes_the_port() {
    let s = Setup::new();
    s.fakes
        .google
        .set_tokens("ya29.ACCESSTOKEN", "1//REFRESHTOKEN");
    let mut h = s.harness();
    h.timeout = Duration::from_secs(20);
    let browser = Browser::new(Plan::Normal);

    let (ran, seen) = s.run(&mut h, &browser);

    assert_eq!(ran.code, 0, "{ran:?} {seen:?}");
    assert!(ran.out.contains("signed in to Google"), "{}", ran.out);
    assert_eq!(status_of(&seen, "callback"), vec![200]);

    // The stored item is the token set, refresh token included.
    let stored: serde_json::Value =
        serde_json::from_str(&s.store.get_text(Provider::Google).expect("stored")).unwrap();
    assert_eq!(stored["access_token"], "ya29.ACCESSTOKEN");
    assert_eq!(stored["refresh_token"], "1//REFRESHTOKEN");
    assert_eq!(stored["scope"], "https://www.googleapis.com/auth/drive");
    assert_eq!(stored["token_type"], "Bearer");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expires_at = stored["expires_at"].as_u64().unwrap();
    assert!(
        expires_at > now + 3000 && expires_at <= now + 3700,
        "{expires_at}"
    );
    assert_eq!(s.store.len(), 1);
    assert_eq!(s.store.get_text(Provider::Github), None);

    // The consent request: what Google needs for a refresh token, PKCE
    // S256, a fresh state, a loopback redirect.
    let authorize = s.fakes.google.authorize_requests();
    assert_eq!(authorize.len(), 1);
    let a = &authorize[0];
    assert_eq!(a.get("response_type"), Some("code"));
    assert_eq!(a.get("access_type"), Some("offline"));
    assert_eq!(a.get("prompt"), Some("consent"));
    assert_eq!(
        a.get("scope"),
        Some("https://www.googleapis.com/auth/drive")
    );
    assert_eq!(a.get("code_challenge_method"), Some("S256"));
    assert_eq!(
        a.get("client_id"),
        Some(s.fakes.google.client_id().as_str())
    );
    let redirect = a.get("redirect_uri").unwrap();
    let port = port_in(&ran.out);
    assert_eq!(redirect, format!("http://127.0.0.1:{port}/callback"));
    assert!(a.get("state").unwrap().len() >= 43);
    // The URL printed is the URL opened.
    assert_eq!(
        query_param(&consent_url_in(&ran.out), "state").as_deref(),
        a.get("state")
    );

    // The exchange: the verifier hashes to the challenge sent earlier.
    let tokens = s.fakes.google.token_requests();
    assert_eq!(tokens.len(), 1);
    let t = &tokens[0];
    assert_eq!(t.get("grant_type"), Some("authorization_code"));
    assert_eq!(t.get("redirect_uri"), Some(redirect));
    assert_eq!(
        t.get("client_secret"),
        Some(s.fakes.google.client_secret().as_str())
    );
    let verifier = t.get("code_verifier").unwrap();
    assert!((43..=128).contains(&verifier.len()));
    assert_eq!(
        pkce::challenge_for(verifier),
        a.get("code_challenge").unwrap()
    );

    // The listener is gone, and the output holds no token.
    assert!(port_is_closed(port));
    for text in [&ran.out, &ran.err] {
        assert!(!text.contains("ACCESSTOKEN") && !text.contains("REFRESHTOKEN"));
        assert!(!text.contains(verifier));
    }
    s.assert_nothing_written();
}

#[test]
fn a_browser_that_will_not_open_is_not_fatal() {
    let s = Setup::new();
    let mut h = s.harness();
    h.timeout = Duration::from_secs(20);
    let mut browser = Browser::new(Plan::Normal);
    browser.open_fails = true;

    let (ran, _) = s.run(&mut h, &browser);

    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(ran.err.contains("could not open a browser"), "{}", ran.err);
    assert!(s.store.get_text(Provider::Google).is_some());
}

#[test]
fn every_run_has_its_own_verifier_and_state() {
    let s = Setup::new();
    for _ in 0..3 {
        let mut h = s.harness();
        h.timeout = Duration::from_secs(20);
        let (ran, _) = s.run(&mut h, &Browser::new(Plan::Normal));
        assert_eq!(ran.code, 0, "{ran:?}");
    }
    let tokens = s.fakes.google.token_requests();
    let authorize = s.fakes.google.authorize_requests();
    assert_eq!(tokens.len(), 3);
    let mut seen = std::collections::HashSet::new();
    for t in &tokens {
        assert!(seen.insert(t.get("code_verifier").unwrap().to_string()));
    }
    for a in &authorize {
        assert!(seen.insert(a.get("state").unwrap().to_string()));
        assert!(seen.insert(a.get("code_challenge").unwrap().to_string()));
    }
}

// ---- G1: the client JSON ---------------------------------------------------------

fn assert_setup_help(ran: &Ran, s: &Setup, browser: &Browser) {
    assert_eq!(ran.code, 1, "{ran:?}");
    for part in [
        "Desktop app",
        "In production",
        "7 days",
        "google_client_json",
    ] {
        assert!(ran.err.contains(part), "{part} missing: {}", ran.err);
    }
    assert!(ran.out.is_empty(), "no URL was printed: {}", ran.out);
    // Before any listener: no opener call and no request to Google.
    assert!(browser.finish().is_empty());
    assert!(browser.handle.lock().unwrap().is_none());
    assert_eq!(s.fakes.google.request_log().count(), 0);
    assert!(s.store.get_text(Provider::Google).is_none());
}

#[test]
fn an_unset_client_json_explains_the_setup_before_any_listener() {
    let s = Setup::new();
    let mut h = s.harness();
    h.client_json = None;
    let browser = Browser::new(Plan::Nothing);
    let (ran, _) = s.run(&mut h, &browser);
    assert!(ran.err.contains("is not set"), "{}", ran.err);
    assert_setup_help(&ran, &s, &browser);
}

#[test]
fn a_missing_client_file_explains_the_setup_before_any_listener() {
    let s = Setup::new();
    let mut h = s.harness();
    h.client_json = Some(s.inputs.path().join("nope.json"));
    let browser = Browser::new(Plan::Nothing);
    let (ran, _) = s.run(&mut h, &browser);
    assert!(ran.err.contains("cannot read"), "{}", ran.err);
    assert_setup_help(&ran, &s, &browser);
}

#[test]
fn malformed_client_json_explains_the_setup_and_never_quotes_the_file() {
    let secret_text = "{\"installed\": {\"client_secret\": \"GOCSPX-LEAKCHECK\", ";
    for text in [
        secret_text,
        "not json at all GOCSPX-LEAKCHECK",
        "{}",
        "[]",
        "{\"installed\": {\"client_id\": \"x\", \"client_secret\": \"GOCSPX-LEAKCHECK\"}}",
        "{\"installed\": {\"client_id\": \"\", \"client_secret\": \"GOCSPX-LEAKCHECK\", \
         \"auth_uri\": \"https://accounts.google.com/o/oauth2/auth\", \
         \"token_uri\": \"https://oauth2.googleapis.com/token\"}}",
    ] {
        let s = Setup::new();
        let mut h = s.harness();
        h.client_json = Some(s.client_file(text));
        let browser = Browser::new(Plan::Nothing);
        let (ran, _) = s.run(&mut h, &browser);
        assert!(
            ran.err.contains("not a Google Desktop-app client JSON"),
            "{text}: {}",
            ran.err
        );
        assert!(!ran.err.contains("LEAKCHECK"), "{}", ran.err);
        assert_setup_help(&ran, &s, &browser);
    }
}

#[test]
fn a_web_application_client_is_named_as_the_wrong_kind() {
    let s = Setup::new();
    let mut h = s.harness();
    h.client_json = Some(s.client_file(
        "{\"web\": {\"client_id\": \"x\", \"client_secret\": \"y\", \"auth_uri\": \"a\", \"token_uri\": \"b\"}}",
    ));
    let browser = Browser::new(Plan::Nothing);
    let (ran, _) = s.run(&mut h, &browser);
    assert!(ran.err.contains("Web application client"), "{}", ran.err);
    assert_setup_help(&ran, &s, &browser);
}

#[test]
fn a_client_json_that_points_the_secret_at_another_host_is_refused() {
    let s = Setup::new();
    let mut h = s.harness();
    h.client_json = Some(s.client_file(
        "{\"installed\": {\"client_id\": \"x\", \"client_secret\": \"y\", \
         \"auth_uri\": \"https://accounts.google.com/o/oauth2/auth\", \
         \"token_uri\": \"https://evil.example/token\"}}",
    ));
    let browser = Browser::new(Plan::Nothing);
    let (ran, _) = s.run(&mut h, &browser);
    assert!(
        ran.err.contains("somewhere other than Google"),
        "{}",
        ran.err
    );
    assert_setup_help(&ran, &s, &browser);
}

// ---- failures of the flow -------------------------------------------------------------

#[test]
fn access_denied_exits_1_stores_nothing_and_closes_the_listener() {
    let s = Setup::new();
    s.fakes.google.deny_next_consent();
    let mut h = s.harness();
    h.timeout = Duration::from_secs(20);
    let browser = Browser::new(Plan::Normal);

    let started = Instant::now();
    let (ran, seen) = s.run(&mut h, &browser);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("consent denied"), "{}", ran.err);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "ended at once, not at the timeout"
    );
    assert_eq!(status_of(&seen, "callback"), vec![200]);
    assert!(s.store.get_text(Provider::Google).is_none());
    assert!(s.fakes.google.token_requests().is_empty(), "no exchange");
    assert!(port_is_closed(port_in(&ran.out)));
}

#[test]
fn the_timeout_exits_1_closes_the_port_and_stores_nothing() {
    let s = Setup::new();
    let mut h = s.harness();
    h.timeout = Duration::from_secs(1);
    let browser = Browser::new(Plan::Nothing);

    let started = Instant::now();
    let (ran, _) = s.run(&mut h, &browser);
    let took = started.elapsed();

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("timed out after 1 second"), "{}", ran.err);
    assert!(
        took >= Duration::from_millis(900) && took < Duration::from_secs(5),
        "{took:?}"
    );
    assert!(port_is_closed(port_in(&ran.out)));
    assert!(s.store.get_text(Provider::Google).is_none());
    assert!(s.fakes.google.token_requests().is_empty());
}

#[test]
fn a_callback_with_the_wrong_state_is_rejected_and_the_flow_carries_on() {
    let s = Setup::new();
    let mut h = s.harness();
    h.timeout = Duration::from_secs(20);
    let browser = Browser::new(Plan::WrongStateThenRight);

    let (ran, seen) = s.run(&mut h, &browser);

    assert_eq!(status_of(&seen, "wrong state"), vec![400]);
    assert_eq!(status_of(&seen, "callback"), vec![200]);
    assert_eq!(ran.code, 0, "{ran:?}");
    assert_eq!(s.fakes.google.token_requests().len(), 1);
}

#[test]
fn a_wrong_or_missing_state_alone_never_completes_the_flow() {
    for plan in [Plan::WrongStateOnly, Plan::MissingStateOnly] {
        let s = Setup::new();
        let mut h = s.harness();
        h.timeout = Duration::from_secs(1);
        let browser = Browser::new(plan);

        let (ran, seen) = s.run(&mut h, &browser);

        assert_eq!(
            seen.len(),
            2,
            "{plan:?}: consent then the bad callback: {seen:?}"
        );
        assert_eq!(seen[1].1.as_ref().unwrap().status, 400, "{plan:?}");
        assert_eq!(ran.code, 1, "{plan:?}: {ran:?}");
        assert!(ran.err.contains("timed out"), "{plan:?}: {}", ran.err);
        assert!(s.fakes.google.token_requests().is_empty(), "{plan:?}");
        assert!(s.store.get_text(Provider::Google).is_none());
    }
}

#[test]
fn a_forged_denial_without_the_state_cannot_end_the_flow() {
    let s = Setup::new();
    let mut h = s.harness();
    h.timeout = Duration::from_secs(20);
    // The follower sends `?error=access_denied` with no state first.
    let browser = Browser::new(Plan::Normal);
    let opener = |url: &str| -> std::io::Result<()> {
        let url = url.to_string();
        let seen = browser.seen.clone();
        *browser.handle.lock().unwrap() = Some(std::thread::spawn(move || {
            let loc = http("GET", &url, None).unwrap().location.unwrap();
            let base = format!("http://{}", authority(&loc));
            let forged = http("GET", &format!("{base}/callback?error=access_denied"), None);
            note(&seen, "forged denial", forged);
            note(&seen, "callback", http("GET", &loc, None));
        }));
        Ok(())
    };
    let ran = h.run(&["auth", "google"], &opener);
    let seen = browser.finish();

    assert_eq!(status_of(&seen, "forged denial"), vec![400]);
    assert_eq!(ran.code, 0, "{ran:?}");
}

#[test]
fn stray_requests_get_404_or_400_and_neither_complete_nor_abort_the_flow() {
    let s = Setup::new();
    let mut h = s.harness();
    h.timeout = Duration::from_secs(20);
    let browser = Browser::new(Plan::StraysThenRealThenReplay);

    let (ran, seen) = s.run(&mut h, &browser);

    assert_eq!(status_of(&seen, "favicon"), vec![404]);
    assert_eq!(status_of(&seen, "root"), vec![404]);
    assert_eq!(status_of(&seen, "post"), vec![400]);
    assert_eq!(status_of(&seen, "head"), vec![400]);
    assert_eq!(status_of(&seen, "bare callback"), vec![400]);
    assert_eq!(status_of(&seen, "empty code"), vec![400]);
    assert_eq!(status_of(&seen, "foreign host"), vec![400]);
    // The real callback still completed the flow.
    assert_eq!(status_of(&seen, "callback"), vec![200]);
    assert_eq!(ran.code, 0, "{ran:?}");
    // A second callback after success is refused, or the port is shut.
    let replay = &seen.iter().find(|(l, _)| l == "replay").unwrap().1;
    // (A refused connection means the port is already closed: fine.)
    if let Ok(r) = replay {
        assert!(r.status == 400 || r.status == 404, "{r:?}");
    }
    assert_eq!(
        s.fakes.google.token_requests().len(),
        1,
        "one exchange only"
    );
}

#[test]
fn stray_requests_alone_do_not_complete_the_flow() {
    let s = Setup::new();
    let mut h = s.harness();
    h.timeout = Duration::from_secs(1);
    let browser = Browser::new(Plan::StraysOnly);

    let (ran, seen) = s.run(&mut h, &browser);

    assert!(
        seen.iter()
            .skip(1)
            .all(|(_, r)| matches!(r, Ok(r) if r.status == 400 || r.status == 404)),
        "{seen:?}"
    );
    assert_eq!(ran.code, 1);
    assert!(ran.err.contains("timed out"));
    assert!(s.fakes.google.token_requests().is_empty());
}

#[test]
fn a_silent_connection_does_not_block_the_real_callback() {
    let s = Setup::new();
    let mut h = s.harness();
    h.timeout = Duration::from_secs(20);
    let browser = Browser::new(Plan::Normal);
    // A speculative browser connection that sends nothing, held open.
    let held = Mutex::new(Vec::new());
    let opener = |url: &str| -> std::io::Result<()> {
        let url = url.to_string();
        let seen = browser.seen.clone();
        let loc = http("GET", &url, None).unwrap().location.unwrap();
        held.lock()
            .unwrap()
            .push(TcpStream::connect(authority(&loc)).unwrap());
        *browser.handle.lock().unwrap() = Some(std::thread::spawn(move || {
            note(&seen, "callback", http("GET", &loc, None));
        }));
        Ok(())
    };
    let started = Instant::now();
    let ran = h.run(&["auth", "google"], &opener);
    browser.finish();
    assert_eq!(ran.code, 0, "{ran:?}");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[test]
fn a_failed_exchange_stores_nothing_and_its_text_carries_no_code_or_verifier() {
    // (what the fake's token endpoint answers, text the error must hold)
    type Arm = fn(&Fakes);
    let cases: [(&str, Arm, &str); 4] = [
        (
            "invalid_grant (400)",
            |f| f.google.fail_next_token(400, "invalid_grant"),
            "invalid_grant",
        ),
        (
            "500",
            |f| f.google.fail_next_token(500, "backend_error"),
            "HTTP 500",
        ),
        (
            "no refresh token",
            |f| f.google.omit_refresh_token_next(),
            "no refresh token",
        ),
        (
            "unauthorized client (401)",
            |f| f.google.fail_next_token(401, "invalid_client"),
            "invalid_client",
        ),
    ];
    for (name, arm, expect) in cases {
        let s = Setup::new();
        arm(&s.fakes);
        let mut h = s.harness();
        h.timeout = Duration::from_secs(20);
        let browser = Browser::new(Plan::Normal);

        let (ran, _) = s.run(&mut h, &browser);

        assert_eq!(ran.code, 1, "{name}: {ran:?}");
        assert!(ran.err.contains(expect), "{name}: {}", ran.err);
        assert!(
            ran.err.contains("Nothing was stored"),
            "{name}: {}",
            ran.err
        );
        assert!(s.store.get_text(Provider::Google).is_none(), "{name}");
        let sent = s.fakes.google.token_requests();
        assert_eq!(sent.len(), 1, "{name}");
        let code = sent[0].get("code").unwrap();
        let verifier = sent[0].get("code_verifier").unwrap();
        for text in [&ran.out, &ran.err] {
            assert!(!text.contains(code), "{name}: code leaked: {text}");
            assert!(!text.contains(verifier), "{name}: verifier leaked: {text}");
        }
        assert!(port_is_closed(port_in(&ran.out)), "{name}");
    }
}

#[test]
fn an_unreachable_token_endpoint_stores_nothing() {
    let s = Setup::new();
    let dead = crate::auth_support::closed_port();
    let mut h = s.harness();
    h.client_json = Some(s.client_file(&format!(
        "{{\"installed\": {{\"client_id\": \"{}\", \"client_secret\": \"{}\", \
         \"auth_uri\": \"{}\", \"token_uri\": \"http://127.0.0.1:{dead}/token\"}}}}",
        s.fakes.google.client_id(),
        s.fakes.google.client_secret(),
        s.fakes.google.authorize_url(),
    )));
    h.timeout = Duration::from_secs(20);
    let browser = Browser::new(Plan::Normal);

    let (ran, _) = s.run(&mut h, &browser);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(
        ran.err.contains("could not reach Google's token endpoint"),
        "{}",
        ran.err
    );
    assert!(s.store.get_text(Provider::Google).is_none());
}

// ---- E17: the store ----------------------------------------------------------------------

#[test]
fn a_failing_store_exits_1_with_keychain_unavailable_before_asking_for_consent() {
    for error in [CredentialError::Locked, CredentialError::Denied] {
        let s = Setup::new();
        let store = Arc::new(FailingStore::new(error.clone(), FailOn::All));
        let mut h = Harness::new(store, s.config.path());
        h.client_json = Some(s.client_file(&s.fakes.google.client_json()));
        let browser = Browser::new(Plan::Normal);

        let (ran, _) = s.run(&mut h, &browser);

        assert_eq!(ran.code, 1, "{error:?}: {ran:?}");
        assert!(ran.err.contains("Keychain unavailable"), "{}", ran.err);
        assert!(ran.out.is_empty());
        assert_eq!(
            s.fakes.google.request_log().count(),
            0,
            "no consent was asked for"
        );
        s.assert_nothing_written();
    }
}

#[test]
fn a_store_that_fails_only_on_write_still_stores_nothing_and_says_why() {
    let s = Setup::new();
    let store = Arc::new(FailingStore::new(CredentialError::Denied, FailOn::SetOnly));
    let mut h = Harness::new(store.clone(), s.config.path());
    h.client_json = Some(s.client_file(&s.fakes.google.client_json()));
    h.timeout = Duration::from_secs(20);
    let browser = Browser::new(Plan::Normal);

    let (ran, _) = s.run(&mut h, &browser);

    assert_eq!(ran.code, 1, "{ran:?}");
    assert!(ran.err.contains("Keychain unavailable"), "{}", ran.err);
    assert!(!ran.err.contains("ya29") && !ran.out.contains("ya29"));
    s.assert_nothing_written();
}

// ---- concurrent runs --------------------------------------------------------------------------

#[test]
fn two_runs_at_once_each_get_their_own_port_verifier_and_state() {
    let s = Setup::new();
    let slots: Arc<Mutex<[Option<String>; 2]>> = Arc::new(Mutex::new([None, None]));
    let crossed: Arc<Mutex<Vec<(usize, u16, u16)>>> = Arc::new(Mutex::new(Vec::new()));
    let client = s.client_file(&s.fakes.google.client_json());

    let results: Vec<Ran> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..2usize)
            .map(|me| {
                let slots = slots.clone();
                let crossed = crossed.clone();
                let store = Arc::new(MemStore::default());
                let mut h = Harness::new(store.clone(), s.config.path());
                h.client_json = Some(client.clone());
                h.timeout = Duration::from_secs(20);
                scope.spawn(move || {
                    let follower: Mutex<Option<JoinHandle<()>>> = Mutex::new(None);
                    let opener = |url: &str| -> std::io::Result<()> {
                        let url = url.to_string();
                        let (slots, crossed) = (slots.clone(), crossed.clone());
                        *follower.lock().unwrap() = Some(std::thread::spawn(move || {
                            let loc = http("GET", &url, None).unwrap().location.unwrap();
                            slots.lock().unwrap()[me] = Some(loc.clone());
                            // Wait for the other run's redirect.
                            let theirs = loop {
                                if let Some(t) = slots.lock().unwrap()[1 - me].clone() {
                                    break t;
                                }
                                std::thread::sleep(Duration::from_millis(10));
                            };
                            // Hand the OTHER run's code and state to MY port.
                            let tail = theirs.split_once("/callback").unwrap().1;
                            let mine = authority(&loc);
                            let cross = http("GET", &format!("http://{mine}/callback{tail}"), None)
                                .unwrap();
                            let ports = (
                                mine.rsplit(':').next().unwrap().parse().unwrap(),
                                authority(&theirs)
                                    .rsplit(':')
                                    .next()
                                    .unwrap()
                                    .parse()
                                    .unwrap(),
                            );
                            crossed
                                .lock()
                                .unwrap()
                                .push((cross.status as usize, ports.0, ports.1));
                            let real = http("GET", &loc, None).unwrap();
                            assert_eq!(real.status, 200);
                        }));
                        Ok(())
                    };
                    let ran = h.run(&["auth", "google"], &opener);
                    if let Some(f) = follower.lock().unwrap().take() {
                        f.join().unwrap();
                    }
                    (ran, store.get_text(Provider::Google))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| {
                let (ran, stored) = h.join().unwrap();
                assert!(stored.is_some(), "{ran:?}");
                ran
            })
            .collect()
    });

    for ran in &results {
        assert_eq!(ran.code, 0, "{ran:?}");
    }
    // Each run's port differs, and the other run's code was refused (400).
    let ports: Vec<u16> = results.iter().map(|r| port_in(&r.out)).collect();
    assert_ne!(ports[0], ports[1]);
    let crossed = crossed.lock().unwrap();
    assert_eq!(crossed.len(), 2);
    assert!(
        crossed
            .iter()
            .all(|(status, mine, theirs)| *status == 400 && mine != theirs),
        "{crossed:?}"
    );
    // Distinct verifiers, states, challenges and codes; two exchanges.
    let authorize = s.fakes.google.authorize_requests();
    let tokens = s.fakes.google.token_requests();
    assert_eq!((authorize.len(), tokens.len()), (2, 2));
    assert_ne!(authorize[0].get("state"), authorize[1].get("state"));
    assert_ne!(
        authorize[0].get("code_challenge"),
        authorize[1].get("code_challenge")
    );
    assert_ne!(
        tokens[0].get("code_verifier"),
        tokens[1].get("code_verifier")
    );
    assert_ne!(tokens[0].get("code"), tokens[1].get("code"));
    // Every exchange used the verifier of its own consent request.
    for t in &tokens {
        let challenge = pkce::challenge_for(t.get("code_verifier").unwrap());
        assert!(authorize
            .iter()
            .any(|a| a.get("code_challenge") == Some(challenge.as_str())));
    }
}
