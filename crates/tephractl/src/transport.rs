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

/// How long a single HTTP attempt (connect + response) may take
/// before `reqwest` gives up, so a hung/unreachable daemon fails
/// fast rather than hanging a CLI invocation forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum RpcCallError {
    /// Connection refused at `addr` — the daemon isn't running (or
    /// isn't listening on this address).
    DaemonNotRunning,
    /// The server replied 401 Unauthorized: the token was rejected.
    Unauthorized,
    /// A non-connect transport failure (e.g. the connection was reset
    /// mid-request/mid-response) that also failed on the single
    /// automatic retry.
    Transport(String),
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
            RpcCallError::Rpc { message, .. } => write!(f, "{message}"),
            RpcCallError::Decode(msg) => write!(f, "invalid response from daemon: {msg}"),
        }
    }
}

impl std::error::Error for RpcCallError {}

/// One JSON-RPC client bound to a single daemon address and bearer
/// token. Built fresh by `main.rs` for each process invocation (no
/// cross-invocation state anywhere in this type).
pub struct RpcClient {
    addr: String,
    token: String,
    http: reqwest::blocking::Client,
}

impl RpcClient {
    pub fn new(addr: impl Into<String>, token: impl Into<String>) -> Self {
        let http = reqwest::blocking::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .expect("building a plain-HTTP reqwest client never fails in practice");
        RpcClient {
            addr: addr.into(),
            token: token.into(),
            http,
        }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn url(&self) -> String {
        format!("http://{}/rpc", self.addr)
    }

    fn send_once(&self, body: &Value) -> Result<reqwest::blocking::Response, reqwest::Error> {
        // F5 regression check: every single request this crate ever
        // sends carries the bearer token — there is no code path in
        // this module that posts without it.
        self.http
            .post(self.url())
            .bearer_auth(&self.token)
            .json(body)
            .send()
    }

    /// Sends `body`, retrying exactly once on a non-connect transport
    /// failure (a dropped connection mid-request/mid-response), per
    /// the spec's edge case. A connection-refused failure (the daemon
    /// is simply not running) is reported immediately without a
    /// retry, since retrying it would only waste time on a condition
    /// a second attempt cannot fix.
    fn send_with_retry(&self, body: &Value) -> Result<reqwest::blocking::Response, RpcCallError> {
        match self.send_once(body) {
            Ok(response) => Ok(response),
            Err(e) if e.is_connect() => Err(RpcCallError::DaemonNotRunning),
            Err(_first_attempt_err) => match self.send_once(body) {
                Ok(response) => Ok(response),
                Err(e2) if e2.is_connect() => Err(RpcCallError::DaemonNotRunning),
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

        let response = self.send_with_retry(&body)?;

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
