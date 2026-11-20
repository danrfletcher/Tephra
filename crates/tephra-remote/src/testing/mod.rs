//! In-process fake GitHub and Drive servers, for this crate's tests and
//! for every later PR that needs a remote to talk to. Compiled only with
//! the cargo feature `test-support`.
//!
//! - [`FakeGithub`] and [`FakeDrive`] are `axum` servers on `127.0.0.1:0`
//!   (loopback only; never another address). `base_url()` is what to pass
//!   to the real adapter; a Drive fake's base URL ends in `/drive/v3`.
//! - Each keeps an in-memory repo or Drive that tests fill and change.
//! - Both honour `ETag` / `If-None-Match` (304), and the Drive fake pages
//!   its listings and its changes feed with page tokens.
//! - Both can be scripted: fail the next N requests with a status,
//!   redirect the next one,
//!   rate-limit for a window or for N requests in either provider's
//!   style, revoke the token, add latency, cut a body mid-stream.
//! - Both keep a [`RequestLog`] of every request (method, path, query,
//!   headers) in the order they arrived, rate-limited and refused ones
//!   included.
//! - Time for rate-limit windows comes from the [`Clock`] given at
//!   start, so a test that shares one `FakeClock` with the adapter
//!   advances both at once.
//!
//! The fakes are test code: the request log keeps the headers it saw
//! (the `Authorization` header included) so a test can prove which token
//! went where. Nothing here logs through `tracing`.

use std::collections::{HashSet, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::Response;
use axum::Router;
use tokio::task::JoinHandle;

use crate::clock::Clock;

mod drive;
mod github;

pub use drive::{DriveLimit, FakeDrive};
pub use github::{FakeGithub, GithubLimit};

/// One request the fake saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedRequest {
    pub method: String,
    /// The path as sent, still percent-encoded, without the query.
    pub path: String,
    /// The raw query string, if any.
    pub query: Option<String>,
    /// Header names lowercased, in arrival order.
    pub headers: Vec<(String, String)>,
}

impl LoggedRequest {
    /// The first header with this name (any case).
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, v)| v.as_str())
    }

    /// Path and query as one string, for matching in assertions.
    pub fn target(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{q}", self.path),
            None => self.path.clone(),
        }
    }
}

/// The log of requests a fake has seen. Cheap to clone; clones share the
/// same list.
#[derive(Debug, Clone, Default)]
pub struct RequestLog(Arc<Mutex<Vec<LoggedRequest>>>);

impl RequestLog {
    fn push(&self, request: LoggedRequest) {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request);
    }

    /// Every request so far, oldest first.
    pub fn all(&self) -> Vec<LoggedRequest> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn count(&self) -> usize {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn last(&self) -> Option<LoggedRequest> {
        self.all().pop()
    }

    /// Forgets everything logged so far.
    pub fn clear(&self) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clear();
    }

    /// The requests whose path and query contain `needle`.
    pub fn matching(&self, needle: &str) -> Vec<LoggedRequest> {
        self.all()
            .into_iter()
            .filter(|r| r.target().contains(needle))
            .collect()
    }
}

/// Which tokens the fake accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AuthMode {
    /// Any non-empty bearer token.
    Any,
    /// Only these. An empty set is a revoked token: everything is a 401.
    Only(HashSet<String>),
}

/// What the next requests should do instead of the normal thing.
pub(crate) struct Script<S> {
    pub fail_next: VecDeque<u16>,
    /// `Location` values: the next requests get a 301 to each in turn.
    pub redirect_next: VecDeque<String>,
    pub limit_until: Option<(SystemTime, S)>,
    pub limit_requests: Option<(u32, S)>,
    pub latency: Duration,
    pub truncate_next: u32,
    pub empty_next: u32,
}

impl<S> Default for Script<S> {
    fn default() -> Self {
        Script {
            fail_next: VecDeque::new(),
            redirect_next: VecDeque::new(),
            limit_until: None,
            limit_requests: None,
            latency: Duration::ZERO,
            truncate_next: 0,
            empty_next: 0,
        }
    }
}

/// How a provider says "unauthorised", "failed" and "slow down".
pub(crate) struct Flavor<S> {
    pub unauthorized: fn() -> Response,
    pub error: fn(u16) -> Response,
    /// `(style, whole seconds left in the window, now as unix seconds)`.
    pub limited: fn(&S, u64, u64) -> Response,
}

pub(crate) struct Common<S> {
    pub log: RequestLog,
    pub script: Mutex<Script<S>>,
    pub auth: Mutex<AuthMode>,
    pub clock: Arc<dyn Clock>,
    pub flavor: Flavor<S>,
}

impl<S> Common<S> {
    pub fn new(clock: Arc<dyn Clock>, flavor: Flavor<S>) -> Self {
        Common {
            log: RequestLog::default(),
            script: Mutex::new(Script::default()),
            auth: Mutex::new(AuthMode::Any),
            clock,
            flavor,
        }
    }

    pub fn script(&self) -> MutexGuard<'_, Script<S>> {
        self.script.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn auth(&self) -> MutexGuard<'_, AuthMode> {
        self.auth.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn token_ok(&self, headers: &HeaderMap) -> bool {
        let token = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|t| !t.is_empty());
        match (&*self.auth(), token) {
            (_, None) => false,
            (AuthMode::Any, Some(_)) => true,
            (AuthMode::Only(set), Some(t)) => set.contains(t),
        }
    }
}

fn epoch_secs(t: SystemTime) -> u64 {
    t.duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The middleware every request passes through, in this order: log,
/// latency, token check, scripted failure, scripted rate limit, the
/// route, then (if scripted) a cut body.
async fn control<S: Send + Sync + 'static>(
    State(common): State<Arc<Common<S>>>,
    request: Request,
    next: Next,
) -> Response {
    common.log.push(LoggedRequest {
        method: request.method().to_string(),
        path: request.uri().path().to_string(),
        query: request.uri().query().map(str::to_string),
        headers: request
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_ascii_lowercase(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect(),
    });

    let latency = common.script().latency;
    if !latency.is_zero() {
        tokio::time::sleep(latency).await;
    }

    if !common.token_ok(request.headers()) {
        return (common.flavor.unauthorized)();
    }

    let now = common.clock.now();
    {
        let mut script = common.script();
        if let Some(location) = script.redirect_next.pop_front() {
            drop(script);
            return with_header(
                json_response(301, &serde_json::json!({ "message": "Moved Permanently" })),
                "location",
                &location,
            );
        }
        if let Some(status) = script.fail_next.pop_front() {
            drop(script);
            return if status == 401 {
                (common.flavor.unauthorized)()
            } else {
                (common.flavor.error)(status)
            };
        }
        if let Some((until, style)) = &script.limit_until {
            if now < *until {
                let left = until.duration_since(now).unwrap_or_default();
                let secs = left.as_secs() + u64::from(left.subsec_nanos() > 0);
                let response = (common.flavor.limited)(style, secs, epoch_secs(now));
                return response;
            }
            script.limit_until = None;
        }
        if let Some((remaining, style)) = script.limit_requests.take() {
            if remaining > 0 {
                let response = (common.flavor.limited)(&style, 1, epoch_secs(now));
                if remaining > 1 {
                    script.limit_requests = Some((remaining - 1, style));
                }
                return response;
            }
        }
    }

    let response = next.run(request).await;

    let (cut, empty) = {
        let mut script = common.script();
        let ok = response.status().is_success();
        let cut = ok && script.truncate_next > 0;
        if cut {
            script.truncate_next -= 1;
        }
        let empty = ok && !cut && script.empty_next > 0;
        if empty {
            script.empty_next -= 1;
        }
        (cut, empty)
    };
    if cut {
        return cut_body(response).await;
    }
    if empty {
        let (mut parts, _) = response.into_parts();
        parts.headers.remove(header::CONTENT_LENGTH);
        return Response::from_parts(parts, Body::empty());
    }
    response
}

/// Sends the first half of the body, declares the full length, then fails
/// the stream so the connection is dropped before the body is complete.
async fn cut_body(response: Response) -> Response {
    let (mut parts, body) = response.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, usize::MAX).await else {
        return Response::from_parts(parts, Body::empty());
    };
    if bytes.len() < 2 {
        return Response::from_parts(parts, Body::from(bytes));
    }
    let full = bytes.len();
    let head = bytes.slice(..full / 2);
    parts.headers.insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&full.to_string()).expect("a number is a header value"),
    );
    // The pause lets the server flush the headers and the first half
    // before the stream fails; without it the client can see the failure
    // before any response and report a different error.
    use futures_util::StreamExt as _;
    let stream = futures_util::stream::iter(vec![
        Ok::<Bytes, std::io::Error>(head),
        Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "fake server cut the body",
        )),
    ])
    .then(|item| async move {
        if item.is_err() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        item
    });
    Response::from_parts(parts, Body::from_stream(stream))
}

/// A running listener; the task is aborted (and the port closed) on drop.
pub(crate) struct Served {
    pub addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Served {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Binds `127.0.0.1:0` and serves `router` wrapped in the control layer.
pub(crate) async fn serve<S: Send + Sync + 'static>(
    router: Router,
    common: Arc<Common<S>>,
) -> Served {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("local address");
    let app = router.layer(from_fn_with_state(common, control::<S>));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Served { addr, task }
}

/// A 40-hex-digit stand-in for a git sha: deterministic, not
/// cryptographic.
pub(crate) fn fake_sha(parts: &[&[u8]]) -> String {
    use std::hash::{Hash, Hasher};
    let mut out = String::new();
    for round in 0u8..3 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        round.hash(&mut h);
        for p in parts {
            p.len().hash(&mut h);
            p.hash(&mut h);
        }
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out.truncate(40);
    out
}

/// A response with an `ETag` over its body, or a 304 when the request's
/// `If-None-Match` names that tag.
pub(crate) fn etagged(request_headers: &HeaderMap, content_type: &str, body: Vec<u8>) -> Response {
    let tag = format!("\"{}\"", fake_sha(&[&body]));
    let matches = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(|t| t.trim().trim_start_matches("W/"))
                .any(|t| t == tag || t == "*")
        });
    let mut response = Response::new(if matches {
        Body::empty()
    } else {
        Body::from(body)
    });
    *response.status_mut() = if matches {
        StatusCode::NOT_MODIFIED
    } else {
        StatusCode::OK
    };
    let h = response.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&tag).expect("etag"));
    if !matches {
        h.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(content_type).expect("content type"),
        );
    }
    response
}

/// A JSON response with the given status.
pub(crate) fn json_response(status: u16, value: &serde_json::Value) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = StatusCode::from_u16(status).expect("a valid status");
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

/// Adds a header to a response.
pub(crate) fn with_header(mut response: Response, name: &'static str, value: &str) -> Response {
    response.headers_mut().insert(
        header::HeaderName::from_static(name),
        HeaderValue::from_str(value).expect("a header value"),
    );
    response
}

/// Methods every fake has, defined once. `$common` is the field holding
/// the `Arc<Common<..>>`, `$limit` the provider's rate-limit style type.
macro_rules! scripting_methods {
    ($limit:ty) => {
        /// The base URL to give the real adapter, no trailing slash.
        pub fn base_url(&self) -> &str {
            &self.base_url
        }

        /// Every request the fake has seen.
        pub fn request_log(&self) -> $crate::testing::RequestLog {
            self.common.log.clone()
        }

        /// Fail the next `n` requests with `status` (in the provider's
        /// error format). A 401 is the provider's rejected-token answer.
        pub fn fail_next(&self, n: usize, status: u16) {
            let mut script = self.common.script();
            for _ in 0..n {
                script.fail_next.push_back(status);
            }
        }

        /// Answer the next request with a 301 to `location`. Nothing at
        /// `location` is ever contacted by the fake.
        pub fn redirect_next(&self, location: &str) {
            self.common
                .script()
                .redirect_next
                .push_back(location.to_string());
        }

        /// Rate-limit every request until `window` has passed on the
        /// fake's clock.
        pub fn rate_limit_for(&self, window: ::std::time::Duration, style: $limit) {
            let until = self.common.clock.now() + window;
            self.common.script().limit_until = Some((until, style));
        }

        /// Rate-limit the next `n` requests.
        pub fn rate_limit_requests(&self, n: u32, style: $limit) {
            self.common.script().limit_requests = Some((n, style));
        }

        /// Stop rate limiting.
        pub fn clear_rate_limit(&self) {
            let mut script = self.common.script();
            script.limit_until = None;
            script.limit_requests = None;
        }

        /// Make every token a 401, until `accept_only_token` is called.
        pub fn revoke_token(&self) {
            *self.common.auth() = $crate::testing::AuthMode::Only(Default::default());
        }

        /// Accept exactly this token (replacing any earlier choice).
        pub fn accept_only_token(&self, token: &str) {
            *self.common.auth() =
                $crate::testing::AuthMode::Only([token.to_string()].into_iter().collect());
        }

        /// Accept any non-empty token again (the starting mode).
        pub fn accept_any_token(&self) {
            *self.common.auth() = $crate::testing::AuthMode::Any;
        }

        /// Delay every response by `latency` (real time).
        pub fn set_latency(&self, latency: ::std::time::Duration) {
            self.common.script().latency = latency;
        }

        /// Answer the next `n` successful requests with a 200 and an empty
        /// body (what a broken proxy or a bug would send).
        pub fn empty_next_bodies(&self, n: u32) {
            self.common.script().empty_next = n;
        }

        /// Cut the body of the next `n` successful responses halfway:
        /// the headers promise the full length, half arrives, then the
        /// connection drops.
        pub fn truncate_next_bodies(&self, n: u32) {
            self.common.script().truncate_next = n;
        }
    };
}
pub(crate) use scripting_methods;
