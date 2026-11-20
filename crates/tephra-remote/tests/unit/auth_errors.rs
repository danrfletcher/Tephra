//! E2 and the status-to-error mapping, end to end through the real
//! adapters against the fakes: 401, 403 kinds, 404, 5xx; the latch and
//! its recovery when the stored token changes; a missing, locked or
//! denied credential store; and the sentinel-token leak check (F9).

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tephra_remote::clock::SystemClock;
use tephra_remote::credentials::{CredentialError, CredentialStore, Provider, Secret};
use tephra_remote::drive::{DriveAdapter, DriveClient};
use tephra_remote::error::UnreachableCause;
use tephra_remote::github::{GithubAdapter, GithubClient};
use tephra_remote::testing::{DriveLimit, FakeDrive, FakeGithub, GithubLimit};
use tephra_remote::{AdapterOptions, RemoteError};
use tracing_subscriber::fmt::MakeWriter;

use crate::common::{drive_env, github_env, Creds, GH_SENTINEL, GOOGLE_SENTINEL};

fn status_table() -> Vec<(u16, RemoteError)> {
    vec![
        (401, RemoteError::AuthRejected),
        (403, RemoteError::PermissionDenied),
        (404, RemoteError::NotFound),
        (410, RemoteError::NotFound),
        (400, RemoteError::Unexpected { status: 400 }),
        (422, RemoteError::Unexpected { status: 422 }),
        (500, RemoteError::Server { status: 500 }),
        (502, RemoteError::Server { status: 502 }),
        (503, RemoteError::Server { status: 503 }),
        (504, RemoteError::Server { status: 504 }),
    ]
}

#[tokio::test]
async fn github_status_to_error_table() {
    for (status, want) in status_table() {
        let env = github_env("ghp_table").await;
        env.fake.fail_next(1, status);
        let got = env.client.whoami().await.unwrap_err();
        assert_eq!(got, want, "GitHub {status}");
        assert_eq!(
            env.fake.request_log().count(),
            1,
            "GitHub {status}: one request, no retry"
        );
    }
}

#[tokio::test]
async fn drive_status_to_error_table() {
    for (status, want) in status_table() {
        let env = drive_env("ya29.table").await;
        env.fake.fail_next(1, status);
        let got = env.client.whoami().await.unwrap_err();
        assert_eq!(got, want, "Drive {status}");
        assert_eq!(
            env.fake.request_log().count(),
            1,
            "Drive {status}: one request, no retry"
        );
    }
}

#[tokio::test]
async fn a_403_that_is_a_permission_failure_is_neither_rate_limit_nor_auth() {
    let env = github_env("ghp_x").await;
    env.fake.fail_next(1, 403); // body: "Resource not accessible by integration"
    assert_eq!(
        env.client.get_repo(&env.repo).await.unwrap_err(),
        RemoteError::PermissionDenied
    );
    // And it did not latch the token or start a rate limit.
    assert!(env.client.whoami().await.is_ok());

    let env = drive_env("ya29.x").await;
    env.fake.fail_next(1, 403); // reason: insufficientPermissions
    assert_eq!(
        env.client.get_file("f1").await.unwrap_err(),
        RemoteError::PermissionDenied
    );
    assert!(env.client.whoami().await.is_ok());
}

#[tokio::test]
async fn a_404_on_a_private_repo_is_not_found_not_auth() {
    let env = github_env("ghp_x").await;
    env.fake.set_hidden("octo/hello", true);
    assert_eq!(
        env.client.get_repo(&env.repo).await.unwrap_err(),
        RemoteError::NotFound
    );
    assert_eq!(
        env.client.list_tree(&env.repo, "main").await.unwrap_err(),
        RemoteError::NotFound
    );
    // The token is fine: other calls still work, nothing latched.
    assert!(env.client.whoami().await.is_ok());
}

#[tokio::test]
async fn github_401_latches_and_a_changed_stored_token_recovers_without_a_restart() {
    let env = github_env("ghp_old").await;
    env.fake.revoke_token();

    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(env.fake.request_log().count(), 1, "no retry loop");

    // Latched: more calls of every kind send nothing.
    for _ in 0..3 {
        assert_eq!(
            env.client.whoami().await.unwrap_err(),
            RemoteError::AuthRejected
        );
        assert_eq!(
            env.client.get_repo(&env.repo).await.unwrap_err(),
            RemoteError::AuthRejected
        );
        assert_eq!(
            env.client.list_tree(&env.repo, "main").await.unwrap_err(),
            RemoteError::AuthRejected
        );
    }
    assert_eq!(env.fake.request_log().count(), 1);

    // Even if the server would accept the old token again, the adapter
    // does not try it until the stored token changes.
    env.fake.accept_only_token("ghp_old");
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(env.fake.request_log().count(), 1);

    // A different token in the store: the next call goes out and succeeds.
    env.fake.accept_only_token("ghp_new");
    env.creds.set(Provider::Github, "ghp_new");
    assert!(env.client.whoami().await.is_ok());
    assert_eq!(env.fake.request_log().count(), 2);
    assert_eq!(
        env.fake
            .request_log()
            .last()
            .unwrap()
            .header("authorization"),
        Some("Bearer ghp_new")
    );
    assert!(env.client.get_repo(&env.repo).await.is_ok());

    // The new token can be rejected and latched in turn.
    env.fake.revoke_token();
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    let sent = env.fake.request_log().count();
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(env.fake.request_log().count(), sent);
}

#[tokio::test]
async fn drive_401_latches_and_a_changed_stored_token_recovers_without_a_restart() {
    let env = drive_env("ya29.old").await;
    env.fake.revoke_token();

    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    for _ in 0..3 {
        assert_eq!(
            env.client.get_file("f1").await.unwrap_err(),
            RemoteError::AuthRejected
        );
        assert_eq!(
            env.client.list_children("fld", None).await.unwrap_err(),
            RemoteError::AuthRejected
        );
        assert_eq!(
            env.client.start_page_token().await.unwrap_err(),
            RemoteError::AuthRejected
        );
    }
    assert_eq!(env.fake.request_log().count(), 1);

    env.fake.accept_only_token("ya29.new");
    env.creds.set(Provider::Google, "ya29.new");
    assert!(env.client.whoami().await.is_ok());
    assert_eq!(env.fake.request_log().count(), 2);
    assert!(env.client.list_children("fld", None).await.is_ok());
}

#[tokio::test]
async fn a_missing_token_is_auth_rejected_without_any_network_call() {
    let env = github_env("ghp_x").await;
    env.creds.remove(Provider::Github);
    let repo = env.repo.clone();
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.check_token().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.get_repo(&repo).await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.list_tree(&repo, "main").await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.get_blob(&repo, "abcd").await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client
            .get_file(&repo, "README.md", "main")
            .await
            .unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.poll_ref(&repo, "main", None).await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(env.fake.request_log().count(), 0);

    // Setting a token later needs no restart.
    env.creds.set(Provider::Github, "ghp_later");
    assert!(env.client.whoami().await.is_ok());

    let env = drive_env("ya29.x").await;
    env.creds.remove(Provider::Google);
    assert_eq!(
        env.client.whoami().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.get_file("f1").await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.list_children("fld", None).await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.download("f1").await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.export_markdown("d1").await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.start_page_token().await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(
        env.client.list_changes("1").await.unwrap_err(),
        RemoteError::AuthRejected
    );
    assert_eq!(env.fake.request_log().count(), 0);
}

/// A store whose every call fails the way a locked Keychain does.
struct FailingStore {
    error: CredentialError,
    gets: AtomicUsize,
}

impl CredentialStore for FailingStore {
    fn get(&self, _p: Provider) -> Result<Option<Secret>, CredentialError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        Err(self.error.clone())
    }
    fn set(&self, _p: Provider, _s: &Secret) -> Result<(), CredentialError> {
        Err(self.error.clone())
    }
    fn delete(&self, _p: Provider) -> Result<(), CredentialError> {
        Err(self.error.clone())
    }
}

#[tokio::test]
async fn a_locked_or_denied_store_is_auth_rejected_with_no_request_and_no_fallback() {
    for error in [
        CredentialError::Locked,
        CredentialError::Denied,
        CredentialError::Unavailable,
        CredentialError::Backend("status -25308".to_string()),
    ] {
        let gh = FakeGithub::start().await;
        let dr = FakeDrive::start().await;
        let store = Arc::new(FailingStore {
            error: error.clone(),
            gets: AtomicUsize::new(0),
        });
        let gh_client =
            GithubClient::new(Some(gh.base_url()), store.clone(), Arc::new(SystemClock)).unwrap();
        let dr_client =
            DriveClient::new(Some(dr.base_url()), store.clone(), Arc::new(SystemClock)).unwrap();

        assert_eq!(
            gh_client.whoami().await.unwrap_err(),
            RemoteError::AuthRejected,
            "{error}"
        );
        assert_eq!(
            dr_client.whoami().await.unwrap_err(),
            RemoteError::AuthRejected,
            "{error}"
        );
        assert_eq!(gh.request_log().count(), 0, "{error}");
        assert_eq!(dr.request_log().count(), 0, "{error}");
        // Asked again each time; never cached, never answered from elsewhere.
        assert_eq!(
            gh_client.whoami().await.unwrap_err(),
            RemoteError::AuthRejected
        );
        assert_eq!(store.gets.load(Ordering::SeqCst), 3);
    }
}

/// Delegates to the file store and counts reads.
struct CountingStore {
    inner: Arc<dyn CredentialStore>,
    gets: AtomicUsize,
}

impl CredentialStore for CountingStore {
    fn get(&self, p: Provider) -> Result<Option<Secret>, CredentialError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(p)
    }
    fn set(&self, p: Provider, s: &Secret) -> Result<(), CredentialError> {
        self.inner.set(p, s)
    }
    fn delete(&self, p: Provider) -> Result<(), CredentialError> {
        self.inner.delete(p)
    }
}

#[tokio::test]
async fn the_token_is_read_from_the_store_on_every_request() {
    let creds = Creds::with_token(Provider::Github, "ghp_first");
    let fake = FakeGithub::start().await;
    fake.add_repo("octo/hello", "main");
    let store = Arc::new(CountingStore {
        inner: creds.store.clone(),
        gets: AtomicUsize::new(0),
    });
    let client =
        GithubClient::new(Some(fake.base_url()), store.clone(), Arc::new(SystemClock)).unwrap();

    client.whoami().await.unwrap();
    client.whoami().await.unwrap();
    assert_eq!(store.gets.load(Ordering::SeqCst), 2);

    creds.set(Provider::Github, "ghp_second");
    client.whoami().await.unwrap();
    assert_eq!(store.gets.load(Ordering::SeqCst), 3);

    let seen: Vec<_> = fake
        .request_log()
        .all()
        .iter()
        .map(|r| r.header("authorization").unwrap().to_string())
        .collect();
    assert_eq!(
        seen,
        ["Bearer ghp_first", "Bearer ghp_first", "Bearer ghp_second"]
    );
}

// ---- F9: a sentinel token never shows in an error, a log or a Debug ----------

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Capture {
        self.clone()
    }
}

fn leaked(sentinel: &str, what: &str, text: &str) {
    assert!(!text.contains(sentinel), "{what} leaks the token: {text}");
    assert!(
        !text.contains("SENTINEL"),
        "{what} leaks part of the token: {text}"
    );
}

#[tokio::test]
async fn a_sentinel_token_is_in_no_error_no_debug_and_no_trace_output_on_any_github_path() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let mut errors: Vec<RemoteError> = Vec::new();
    let sentinel = GH_SENTINEL;

    // 401, then the latch.
    let env = github_env(sentinel).await;
    env.fake.revoke_token();
    errors.push(env.client.whoami().await.unwrap_err());
    errors.push(env.client.whoami().await.unwrap_err());
    leaked(
        sentinel,
        "Debug of the client",
        &format!("{:?}", env.client),
    );
    leaked(
        sentinel,
        "Debug of the options",
        &format!("{:?}", AdapterOptions::default()),
    );

    // 403 permission, 404, 5xx, unexpected.
    for status in [403, 404, 500, 503, 422] {
        let env = github_env(sentinel).await;
        env.fake.fail_next(1, status);
        errors.push(env.client.whoami().await.unwrap_err());
    }
    // Rate limits in every style, then the local refusal.
    for style in [
        GithubLimit::RetryAfter403,
        GithubLimit::RetryAfter429,
        GithubLimit::Reset403,
        GithubLimit::Secondary403,
    ] {
        let env = github_env(sentinel).await;
        env.fake.rate_limit_for(Duration::from_secs(60), style);
        errors.push(env.client.whoami().await.unwrap_err());
        errors.push(env.client.whoami().await.unwrap_err());
    }
    // Moved, bad body, truncated body, bad input, missing token.
    let env = github_env(sentinel).await;
    env.fake
        .move_repo("octo/hello", "http://127.0.0.1:1/elsewhere");
    errors.push(env.client.get_repo(&env.repo).await.unwrap_err());
    let env = github_env(sentinel).await;
    env.fake.empty_next_bodies(1);
    errors.push(env.client.whoami().await.unwrap_err());
    let env = github_env(sentinel).await;
    env.fake.truncate_next_bodies(1);
    errors.push(env.client.list_tree(&env.repo, "main").await.unwrap_err());
    errors.push(
        env.client
            .get_blob(&env.repo, "../../etc")
            .await
            .unwrap_err(),
    );
    env.creds.remove(Provider::Github);
    errors.push(env.client.whoami().await.unwrap_err());
    // Unreachable: the fake is gone.
    let env = github_env(sentinel).await;
    let gone = drop_fake(env);
    let client = &gone.client;
    errors.push(client.whoami().await.unwrap_err());

    assert!(errors.len() >= 20);
    for e in &errors {
        leaked(sentinel, "an error", &format!("{e} | {e:?}"));
    }
    let logged = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
    assert!(
        logged.contains("latched"),
        "the adapter logs, so this check is not vacuous: {logged}"
    );
    leaked(sentinel, "captured tracing output", &logged);
}

/// A client whose fake is gone; the temp dir with its token stays alive.
struct GhGone {
    client: GithubClient,
    _creds: Creds,
}

fn drop_fake(env: crate::common::GhEnv) -> GhGone {
    let crate::common::GhEnv {
        fake,
        client,
        creds,
        ..
    } = env;
    drop(fake);
    GhGone {
        client,
        _creds: creds,
    }
}

#[tokio::test]
async fn a_sentinel_token_is_in_no_error_no_debug_and_no_trace_output_on_any_drive_path() {
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_max_level(tracing::Level::TRACE)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let sentinel = GOOGLE_SENTINEL;
    let mut errors: Vec<RemoteError> = Vec::new();

    let env = drive_env(sentinel).await;
    env.fake.revoke_token();
    errors.push(env.client.whoami().await.unwrap_err());
    errors.push(env.client.whoami().await.unwrap_err());
    leaked(
        sentinel,
        "Debug of the client",
        &format!("{:?}", env.client),
    );

    for status in [403, 404, 500, 503, 400] {
        let env = drive_env(sentinel).await;
        env.fake.fail_next(1, status);
        errors.push(env.client.get_file("f1").await.unwrap_err());
    }
    for style in [
        DriveLimit::Status429 { retry_after: true },
        DriveLimit::Status429 { retry_after: false },
        DriveLimit::Reason403 {
            reason: "rateLimitExceeded",
            retry_after: false,
        },
        DriveLimit::Reason403 {
            reason: "userRateLimitExceeded",
            retry_after: true,
        },
        DriveLimit::Reason403 {
            reason: "sharingRateLimitExceeded",
            retry_after: false,
        },
    ] {
        let env = drive_env(sentinel).await;
        env.fake.rate_limit_for(Duration::from_secs(60), style);
        errors.push(env.client.get_file("f1").await.unwrap_err());
        errors.push(env.client.get_file("f1").await.unwrap_err());
    }
    let env = drive_env(sentinel).await;
    env.fake.empty_next_bodies(1);
    errors.push(env.client.get_file("f1").await.unwrap_err());
    let env = drive_env(sentinel).await;
    env.fake.truncate_next_bodies(1);
    errors.push(env.client.download("f1").await.unwrap_err());
    errors.push(env.client.get_file("a'b").await.unwrap_err());
    env.creds.remove(Provider::Google);
    errors.push(env.client.whoami().await.unwrap_err());

    assert!(errors.len() >= 20);
    for e in &errors {
        leaked(sentinel, "an error", &format!("{e} | {e:?}"));
    }
    let logged = String::from_utf8_lossy(&capture.0.lock().unwrap()).into_owned();
    assert!(logged.contains("latched"), "{logged}");
    leaked(sentinel, "captured tracing output", &logged);
}

#[tokio::test]
async fn unreachable_causes_are_distinct_from_auth_and_rate_limit() {
    // Refused: a fake that is gone.
    let env = github_env("ghp_x").await;
    let gone = drop_fake(env);
    let client = &gone.client;
    assert_eq!(
        client.whoami().await.unwrap_err(),
        RemoteError::Unreachable(UnreachableCause::Connect)
    );

    // Timeout: slower than the adapter's limit.
    let creds = Creds::with_token(Provider::Google, "ya29.x");
    let fake = FakeDrive::start().await;
    fake.set_latency(Duration::from_millis(600));
    let client = DriveClient::with_options(
        Some(fake.base_url()),
        creds.store.clone(),
        Arc::new(SystemClock),
        AdapterOptions {
            request_timeout: Duration::from_millis(150),
            ..AdapterOptions::default()
        },
    )
    .unwrap();
    assert_eq!(
        client.whoami().await.unwrap_err(),
        RemoteError::Unreachable(UnreachableCause::Timeout)
    );
    // An outage is not an auth failure: nothing latched, so once the
    // server answers in time the same adapter works.
    fake.set_latency(Duration::ZERO);
    assert!(client.whoami().await.is_ok());
}
