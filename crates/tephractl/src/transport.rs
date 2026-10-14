//! The synchronous JSON-RPC 2.0 transport (G8/F5): one
//! [`RpcClient`] per process invocation (per "each CLI invocation is
//! a fresh connection" edge case — this struct owns no persisted
//! state, caches nothing, and nothing in this crate is `static`/
//! global), posting `POST http://<addr>/rpc` with the bearer token
//! attached to every single request (F5 regression check) and
//! decoding the JSON-RPC envelope PR-6's server returns.
//!
//! This module implements no RPC dispatch, no auth enforcement, and
//! no graph/registry logic of its own (PR-7's fence) — it only
//! builds the envelope PR-6 already defined
//! (`tephrad::rpc::protocol`), sends it, and reports back whatever
//! comes back, including the transport-level failure modes (daemon
//! not running, dropped connection, 401) the spec's edge cases name.

use std::fmt;
use std::time::Duration;

use serde_json::{json, Value};

/// Default daemon address (Plan N's `127.0.0.1:4812`), used when no
/// `--addr` override is given.
pub const DEFAULT_ADDR: &str = "127.0.0.1:4812";

/// How long a single HTTP attempt for an ordinary (non-long-running)
/// method may take before `reqwest` gives up, so a hung/unreachable
/// daemon fails fast rather than hanging a CLI invocation forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a single attempt at a *long-running* method
/// ([`is_long_running`]) may take. `source.register`/`source.rescan`
/// run a full synchronous scan on the daemon, and `link.apply_refactor`
/// rewrites every affected file, so the plain 10s budget is routinely
/// shorter than a real call on a large source (E1) — using it here
/// previously caused `send_with_retry` to misclassify a simply-slow
/// daemon as a dropped connection and re-POST a mutating call that may
/// already have been applied (R1).
const LONG_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Methods whose call might have already mutated daemon/on-disk state
/// by the time a transport failure is observed, so they must never be
/// silently retried: a retry could duplicate the effect
/// (`source.register`/`source.unregister`/`source.rescan`) or — far
/// worse for `link.apply_refactor` — re-run an on-disk rewrite whose
/// first attempt in fact already succeeded, which would then be
/// reported as "aborted" by the staleness guard while files were
/// already changed (R1).
const MUTATING_METHODS: &[&str] = &[
    "source.register",
    "source.unregister",
    "source.rescan",
    "link.apply_refactor",
];

/// Methods long/expensive enough on the daemon side that they need
/// more than [`REQUEST_TIMEOUT`] to complete under normal operation
/// (not just under failure).
const LONG_RUNNING_METHODS: &[&str] = &["source.register", "source.rescan", "link.apply_refactor"];

fn is_mutating(method: &str) -> bool {
    MUTATING_METHODS.contains(&method)
}

fn is_long_running(method: &str) -> bool {
    LONG_RUNNING_METHODS.contains(&method)
}

#[derive(Debug)]
pub enum RpcCallError {
    /// Connection refused at `addr` — the daemon isn't running (or
    /// isn't listening on this address).
    DaemonNotRunning,
    /// The server replied 401 Unauthorized: the token was rejected.
    Unauthorized,
    /// A non-connect transport failure (e.g. the connection was reset
    /// mid-request/mid-response) on a method safe to retry, that also
    /// failed on the single automatic retry.
    Transport(String),
    /// A transport failure left it genuinely unknown whether the
    /// daemon received/applied the request: a timeout on a
    /// *mutating* method ([`is_mutating`]) (R1 — a timeout is not the
    /// same thing as a dropped connection, and must not be treated as
    /// one), or the connection dropped on a mutating method, which
    /// this crate will not blindly re-send, since a retry risks
    /// duplicating a mutation (or, for `link.apply_refactor`,
    /// re-running an on-disk rewrite that already succeeded). Callers
    /// must report this distinctly from both success and a clean
    /// abort — never print "applied"/"aborted"/"removed" for it.
    Ambiguous(String),
    /// A timeout on a *read-only* method (R6 — read-only RPCs never
    /// mutate daemon/on-disk state, so a timeout here is just a slow
    /// or hung daemon, not an ambiguous-effect situation the way a
    /// timed-out mutating call is). Reported plainly ("tephrad did
    /// not respond within Ns while calling ...") rather than with
    /// [`RpcCallError::Ambiguous`]'s "don't blindly retry" wording,
    /// which only makes sense for a call that might have mutated
    /// state.
    Timeout(String),
    /// The server's JSON-RPC envelope carried an `"error"` object.
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// The response body wasn't valid JSON (or wasn't a valid
    /// JSON-RPC envelope).
    Decode(String),
}

impl fmt::Display for RpcCallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcCallError::DaemonNotRunning => write!(f, "tephrad is not running"),
            RpcCallError::Unauthorized => write!(f, "authentication failed"),
            RpcCallError::Transport(msg) => write!(f, "transport error: {msg}"),
            RpcCallError::Ambiguous(msg) => write!(f, "{msg}"),
            RpcCallError::Timeout(msg) => write!(f, "{msg}"),
            RpcCallError::Rpc { message, .. } => write!(f, "{message}"),
            RpcCallError::Decode(msg) => write!(f, "invalid response from daemon: {msg}"),
        }
    }
}

impl std::error::Error for RpcCallError {}

fn ambiguous_timeout_message(method: &str, timeout: Duration) -> String {
    format!(
        "no response from tephrad within {}s while calling \"{method}\"; it is unknown whether the request completed on the daemon — run `tephractl status`/`source list` (or check the affected files) before retrying rather than re-running this command blindly",
        timeout.as_secs()
    )
}

/// R6: the plain-timeout message for a *read-only* method — no
/// "don't retry blindly" wording, since a read-only call that timed
/// out never mutated anything and re-running it (or any other
/// read-only command) is perfectly safe.
fn plain_timeout_message(method: &str, timeout: Duration) -> String {
    format!(
        "tephrad did not respond within {}s while calling \"{method}\"",
        timeout.as_secs()
    )
}

/// R6: builds the right [`RpcCallError`] for a timeout on `method` —
/// [`RpcCallError::Ambiguous`] (with the "don't retry blindly"
/// wording) for a mutating method, whose effect on the daemon is
/// genuinely unknown, or a plain [`RpcCallError::Timeout`] for a
/// read-only one, which never mutated anything regardless of whether
/// the daemon ever replied.
fn timeout_error(method: &str, timeout: Duration) -> RpcCallError {
    if is_mutating(method) {
        RpcCallError::Ambiguous(ambiguous_timeout_message(method, timeout))
    } else {
        RpcCallError::Timeout(plain_timeout_message(method, timeout))
    }
}

fn ambiguous_drop_message(method: &str, source: &reqwest::Error) -> String {
    format!(
        "the connection to tephrad was interrupted while calling \"{method}\"; it is unknown whether the request reached the daemon, so it was not automatically retried — run `tephractl status`/`source list` (or check the affected files) before retrying rather than re-running this command blindly: {source}"
    )
}

/// One JSON-RPC client bound to a single daemon address and bearer
/// token. Built fresh by `main.rs` for each process invocation (no
/// cross-invocation state anywhere in this type).
pub struct RpcClient {
    addr: String,
    token: String,
    http: reqwest::blocking::Client,
    request_timeout: Duration,
    long_request_timeout: Duration,
}

impl RpcClient {
    pub fn new(addr: impl Into<String>, token: impl Into<String>) -> Self {
        Self::with_timeouts(
            addr,
            token,
            REQUEST_TIMEOUT,
            LONG_REQUEST_TIMEOUT,
            CONNECT_TIMEOUT,
        )
    }

    /// Builds a client with explicit timeouts. `tephractl`'s own
    /// binary always uses [`RpcClient::new`]'s production defaults;
    /// this constructor exists so tests can exercise the
    /// timeout-handling paths in [`RpcCallError::Ambiguous`] without
    /// waiting out the real multi-second/multi-minute production
    /// durations.
    pub fn with_timeouts(
        addr: impl Into<String>,
        token: impl Into<String>,
        request_timeout: Duration,
        long_request_timeout: Duration,
        connect_timeout: Duration,
    ) -> Self {
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(connect_timeout)
            .build()
            .expect("building a plain-HTTP reqwest client never fails in practice");
        RpcClient {
            addr: addr.into(),
            token: token.into(),
            http,
            request_timeout,
            long_request_timeout,
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn url(&self) -> String {
        format!("http://{}/rpc", self.addr)
    }

    /// The per-attempt timeout for `method`: [`LONG_REQUEST_TIMEOUT`]
    /// (configurable via [`RpcClient::with_timeouts`]) for methods
    /// that run a full synchronous scan or on-disk rewrite on the
    /// daemon ([`is_long_running`]), the ordinary budget otherwise.
    fn timeout_for(&self, method: &str) -> Duration {
        if is_long_running(method) {
            self.long_request_timeout
        } else {
            self.request_timeout
        }
    }

    fn send_once(
        &self,
        body: &Value,
        timeout: Duration,
    ) -> Result<reqwest::blocking::Response, reqwest::Error> {
        // F5 regression check: every single request this crate ever
        // sends carries the bearer token — there is no code path in
        // this module that posts without it.
        self.http
            .post(self.url())
            .timeout(timeout)
            .bearer_auth(&self.token)
            .json(body)
            .send()
    }

    /// Sends `body` for `method`, retrying exactly once on a non-connect,
    /// non-timeout transport failure (a dropped connection mid-request/
    /// mid-response) — but only for a method safe to retry
    /// (`!is_mutating(method)`), per the spec's edge case. Three failure
    /// modes are reported without ever retrying:
    ///   - connection refused (the daemon isn't running at all): reported
    ///     immediately, since a second attempt cannot fix that.
    ///   - a timeout (R1) on a *mutating* method: a slow daemon, not a
    ///     dropped connection — it is unknown whether the call completed,
    ///     so this is surfaced as [`RpcCallError::Ambiguous`], never
    ///     blindly retried.
    ///   - a timeout on a *read-only* method (R6): also never retried,
    ///     but reported as a plain [`RpcCallError::Timeout`] rather than
    ///     [`RpcCallError::Ambiguous`] — a read-only call cannot have
    ///     mutated anything, so there is nothing ambiguous about its
    ///     effect, and the "don't retry blindly" wording would mislead
    ///     the user into thinking a harmless read is unsafe to re-run.
    ///   - a dropped connection on a mutating method (R1): also surfaced
    ///     as [`RpcCallError::Ambiguous`] rather than retried, since the
    ///     original request may already have taken effect on the daemon
    ///     (duplicate `source.register`, or — worse — re-running an
    ///     on-disk rewrite `link.apply_refactor` already applied).
    fn send_with_retry(
        &self,
        method: &str,
        body: &Value,
    ) -> Result<reqwest::blocking::Response, RpcCallError> {
        let timeout = self.timeout_for(method);
        match self.send_once(body, timeout) {
            Ok(response) => Ok(response),
            Err(e) if e.is_connect() => Err(RpcCallError::DaemonNotRunning),
            Err(e) if e.is_timeout() => Err(timeout_error(method, timeout)),
            Err(e) if is_mutating(method) => {
                Err(RpcCallError::Ambiguous(ambiguous_drop_message(method, &e)))
            }
            Err(_first_attempt_err) => match self.send_once(body, timeout) {
                Ok(response) => Ok(response),
                Err(e2) if e2.is_connect() => Err(RpcCallError::DaemonNotRunning),
                Err(e2) if e2.is_timeout() => Err(timeout_error(method, timeout)),
                Err(e2) => Err(RpcCallError::Transport(e2.to_string())),
            },
        }
    }

    /// Issues one JSON-RPC 2.0 call (`method`, `params`) and returns
    /// the decoded `result` value, or a typed [`RpcCallError`]
    /// covering every failure mode named in the spec's edge cases.
    pub fn call(&self, method: &str, params: Value) -> Result<Value, RpcCallError> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });

        let response = self.send_with_retry(method, &body)?;

        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(RpcCallError::Unauthorized);
        }

        let envelope: Value = response
            .json()
            .map_err(|e| RpcCallError::Decode(e.to_string()))?;

        if let Some(error) = envelope.get("error") {
            let code = error.get("code").and_then(Value::as_i64).unwrap_or(0);
            let message = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_string();
            let data = error.get("data").cloned();
            return Err(RpcCallError::Rpc {
                code,
                message,
                data,
            });
        }

        Ok(envelope.get("result").cloned().unwrap_or(Value::Null))
    }
}
