//! The HTTP + WebSocket JSON-RPC transport (G6): one axum
//! [`Router`] serving `POST /rpc` (plain HTTP request/response) and
//! `GET /rpc` (WebSocket upgrade, same request/response semantics
//! plus server-pushed `graph:changed` notifications) on a single
//! listener.
//!
//! [`AppState`] carries everything a request handler needs: the
//! token file path (read fresh on every call — E11), the two backend
//! trait objects dispatch calls through, an optional [`GraphRefresh`]
//! (R1: wires a successful `source.*` mutation back into the live
//! in-memory graph), and a broadcast channel for `graph:changed`
//! pushes. Nothing here performs auth itself beyond calling
//! [`crate::rpc::auth::authenticate`]/[`crate::rpc::auth::verify_bearer`];
//! nothing here contains any graph or registry logic beyond calling
//! [`crate::rpc::dispatch::dispatch`] and (when configured)
//! [`GraphRefresh::refresh`].

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::ws::{close_code, CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde_json::Value;
use tokio::sync::{broadcast, watch, Notify};

use crate::rpc::auth::{self, Authenticated};
use crate::rpc::backend::{GraphBackend, GraphRefresh, RefactorBackend, SourceBackend};
use crate::rpc::dispatch::{self, Backends};
use crate::rpc::protocol;

/// E9's oversized-body cap: a request body larger than this is
/// rejected (HTTP 413) by axum's extractor before this server ever
/// attempts to deserialize it as JSON. Also applied (R3) as the
/// WebSocket transport's own maximum message/frame size, so a WS
/// client cannot bypass the same cap just by switching transports.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// How many buffered `graph:changed` events a slow WebSocket client
/// can fall behind by before it starts missing them (a lagged
/// receiver skips forward rather than blocking the broadcaster or
/// every other client — the standard `tokio::sync::broadcast`
/// trade-off, acceptable here since each notification is a
/// best-effort "something changed, re-query if interested" hint, not
/// a reliable event log).
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// `source.*` methods whose success means the on-disk cache changed
/// underneath the in-memory graph (R1): after any of these dispatch
/// successfully, [`handle_single_request`] refreshes
/// [`AppState::graph_refresh`] (when configured) before replying, so
/// the very next `unit.resolve_link`/`graph.get_*` call on this same
/// running daemon already sees the update — no restart required.
const MUTATING_SOURCE_METHODS: &[&str] = &[
    "source.register",
    "source.unregister",
    "source.rescan",
    // GP8: a successful `link.apply_refactor` writes directly to the
    // affected files' content (not through `source.rescan`'s RPC
    // path) and synchronously re-scans every touched source itself
    // (see `crate::refactor::RefactorEngine::write_all`) before
    // returning — this entry is what makes that newly up-to-date
    // cache get rebuilt into the live in-memory graph too, on this
    // same running daemon, before the RPC response goes out (R1).
    "link.apply_refactor",
];

/// Tracks every currently-running [`handle_socket`] task so shutdown
/// (R10) can wait, bounded, for all of them to actually finish —
/// and so drop the last `Arc<AppState>` clone each one holds — before
/// the caller hands its `Arc<SourceRegistry>` back out to be dropped
/// on a plain synchronous thread (see `main.rs`'s doc comment, point
/// 3). axum's own graceful shutdown only waits on in-flight HTTP
/// requests; it has no idea an upgraded WebSocket connection exists
/// at all, so without this, a socket task holding `Arc<AppState>`
/// (and, through it, the registry) can easily outlive `block_on`.
#[derive(Clone)]
pub struct SocketTasks {
    count: Arc<AtomicUsize>,
    idle: Arc<Notify>,
}

impl SocketTasks {
    pub fn new() -> Self {
        SocketTasks {
            count: Arc::new(AtomicUsize::new(0)),
            idle: Arc::new(Notify::new()),
        }
    }

    /// Registers one running task, returning a guard that
    /// decrements the count (and wakes any waiter once it reaches
    /// zero) when dropped — including on panic, since this is a
    /// plain `Drop` impl, not a manual decrement at the end of
    /// [`handle_socket`].
    fn enter(&self) -> SocketTaskGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        SocketTaskGuard {
            tasks: self.clone(),
        }
    }

    /// Waits until every currently-tracked task has finished, or
    /// `timeout` elapses, whichever comes first. A bounded wait, not
    /// an unconditional one: a misbehaving client that never responds
    /// to a `Close` frame must not hang shutdown forever.
    pub async fn wait_until_idle(&self, timeout: std::time::Duration) {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            let _ = tokio::time::timeout(remaining, self.idle.notified()).await;
        }
    }
}

impl Default for SocketTasks {
    fn default() -> Self {
        Self::new()
    }
}

struct SocketTaskGuard {
    tasks: SocketTasks,
}

impl Drop for SocketTaskGuard {
    fn drop(&mut self) {
        if self.tasks.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.tasks.idle.notify_waiters();
        }
    }
}

/// Shared state behind every request handler in this module. Always
/// held as `Arc<AppState>` — never cloned field-by-field — so every
/// connection/request sees the same token path and backend instances.
pub struct AppState {
    pub token_path: PathBuf,
    pub graph: Arc<dyn GraphBackend>,
    pub sources: Arc<dyn SourceBackend>,
    pub refactor: Arc<dyn RefactorBackend>,
    events_tx: broadcast::Sender<Value>,
    /// `Some` only in production (`main.rs`'s real `SharedGraph`-backed
    /// wiring); `None` in every test that uses a canned/mocked
    /// [`GraphBackend`] with nothing to rebuild from (R1).
    pub graph_refresh: Option<GraphRefresh>,
    /// `Some` only in production: the same shutdown signal
    /// `axum::serve`'s graceful shutdown and the `graph:changed`
    /// forwarder task select on (R8). `handle_socket` (R10) selects
    /// on this too, so an open WebSocket connection sends a `Close`
    /// frame and returns promptly on shutdown instead of parking
    /// forever and keeping this `AppState` (and the registry it
    /// carries) alive past `block_on`'s return. `None` in every test
    /// fixture that has no shutdown signal to observe.
    pub shutdown_rx: Option<watch::Receiver<bool>>,
    /// Every running [`handle_socket`] task registers itself here
    /// (R10), so shutdown can wait, bounded, for all of them to
    /// actually finish before the registry is handed back out.
    pub socket_tasks: SocketTasks,
}

impl AppState {
    pub fn new(
        token_path: PathBuf,
        graph: Arc<dyn GraphBackend>,
        sources: Arc<dyn SourceBackend>,
        refactor: Arc<dyn RefactorBackend>,
        graph_refresh: Option<GraphRefresh>,
    ) -> Self {
        Self::with_shutdown(token_path, graph, sources, refactor, graph_refresh, None)
    }

    /// Same as [`AppState::new`], but also wires in the shutdown
    /// signal (R10) that `handle_socket` selects on. Production
    /// (`main.rs`) is the only caller that passes `Some`; every test
    /// fixture keeps using plain [`AppState::new`].
    pub fn with_shutdown(
        token_path: PathBuf,
        graph: Arc<dyn GraphBackend>,
        sources: Arc<dyn SourceBackend>,
        refactor: Arc<dyn RefactorBackend>,
        graph_refresh: Option<GraphRefresh>,
        shutdown_rx: Option<watch::Receiver<bool>>,
    ) -> Self {
        let (events_tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        AppState {
            token_path,
            graph,
            sources,
            refactor,
            events_tx,
            graph_refresh,
            shutdown_rx,
            socket_tasks: SocketTasks::new(),
        }
    }

    /// Pushes a `graph:changed` notification to every currently
    /// connected, authenticated WebSocket client. A payload with no
    /// subscribers (nobody connected right now) is simply dropped —
    /// there is nothing to buffer for a client that isn't listening.
    pub fn broadcast_graph_changed(&self, payload: Value) {
        let _ = self.events_tx.send(payload);
    }
}

/// Builds the router this server exposes: `POST /rpc` and
/// `GET /rpc` (WebSocket upgrade) on the same path, both gated by
/// [`auth::authenticate`]. The body-size cap (E9) is applied as a
/// layer, so it runs before this crate's own handler code ever sees
/// the bytes.
pub fn build_router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/rpc", get(ws_or_reject).post(http_rpc_handler))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

/// Wraps `value` as a `200 OK` JSON response. Every JSON-RPC-shaped
/// result/error pair goes out this way over HTTP; only the
/// unauthenticated case (401) and the oversized-body case (413,
/// handled entirely by axum's own extractor before this code runs)
/// use a different status.
fn json_200(value: Value) -> axum::response::Response {
    (StatusCode::OK, axum::Json(value)).into_response()
}

async fn http_rpc_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let proof = match auth::authenticate(&headers, &state.token_path) {
        Ok(proof) => proof,
        Err(_) => {
            // E6/E7: a clear, unambiguous JSON-RPC error body, and a
            // distinct HTTP status — never a 200 with an ambiguous
            // body.
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(protocol::unauthorized_response()),
            )
                .into_response();
        }
    };

    let raw: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        // E14: an empty body (or any unparseable one) is a parse
        // error, not a hang or a panic.
        Err(_) => return json_200(protocol::parse_error_response()),
    };

    if raw.is_array() {
        return json_200(protocol::batch_rejected_response());
    }

    json_200(handle_single_request(proof, raw, state).await)
}

/// Validates envelope + dispatches one already-parsed, non-batch
/// request. Shared between the HTTP and WebSocket handlers so both
/// transports produce byte-identical results for the same input (the
/// transport-parity requirement).
///
/// The actual [`dispatch::dispatch`] call (and, when it was a
/// successful mutating `source.*` call, the follow-up
/// [`GraphRefresh::refresh`]) runs inside
/// `tokio::task::spawn_blocking` (R6): `source.register`/`rescan` can
/// run a full synchronous filesystem walk plus SQLite writes, and a
/// graph rebuild opens its own SQLite connection and re-reads every
/// row — none of that may ever occupy one of this server's async
/// worker threads directly, or a few concurrent calls would starve
/// every other connection on this listener, including `graph:changed`
/// pushes.
async fn handle_single_request(proof: Authenticated, raw: Value, state: Arc<AppState>) -> Value {
    let req = match protocol::validate_envelope(&raw) {
        Ok(req) => req,
        Err(e) => return protocol::error_response(e.id, e.code, e.message, None),
    };

    let graph = state.graph.clone();
    let sources = state.sources.clone();
    let refactor = state.refactor.clone();
    let refresh = state.graph_refresh.clone();
    let method = req.method.clone();
    let params = req.params.clone();

    let outcome = tokio::task::spawn_blocking(move || {
        let backends = Backends {
            graph: graph.as_ref(),
            sources: sources.as_ref(),
            refactor: refactor.as_ref(),
        };
        let result = dispatch::dispatch(&proof, &method, &params, &backends);
        if result.is_ok() && MUTATING_SOURCE_METHODS.contains(&method.as_str()) {
            if let Some(refresh) = &refresh {
                if let Err(e) = refresh.refresh() {
                    tracing::warn!(
                        method = %method,
                        error = %e,
                        "failed to refresh the in-memory graph after a source mutation"
                    );
                }
            }
        }
        result
    })
    .await;

    match outcome {
        Ok(Ok(result)) => protocol::success_response(req.id, result),
        Ok(Err((code, message, data))) => protocol::error_response(req.id, code, message, data),
        Err(_join_error) => protocol::error_response(
            req.id,
            protocol::INTERNAL_ERROR,
            "internal error dispatching request",
            None,
        ),
    }
}

/// `GET /rpc`: either a genuine WebSocket upgrade request (handled by
/// [`ws_rpc_handler`]) or a plain `GET` with no `Upgrade` header,
/// which this server has no other use for. axum's [`WebSocketUpgrade`]
/// extractor itself rejects a non-upgrade `GET` with `400 Bad
/// Request` before this function's body runs, so there is no
/// ambiguous "GET /rpc behaves like POST /rpc" path.
async fn ws_or_reject(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    ws_rpc_handler(ws, State(state), headers).await
}

async fn ws_rpc_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let bearer = match auth::bearer_from_headers(&headers) {
        Some(bearer) => bearer,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                axum::Json(protocol::unauthorized_response()),
            )
                .into_response()
        }
    };

    match auth::verify_bearer(&bearer, &state.token_path) {
        Ok(_proof) => {
            // R3: the same 10 MiB cap E9 applies to `POST /rpc`'s
            // body, applied here to this transport's own
            // message/frame size — without this, axum/tungstenite's
            // defaults (64 MiB message, 16 MiB frame) let a WS client
            // make the server buffer and fully `serde_json`-parse a
            // body far larger than the HTTP cap ever allows.
            let ws = ws
                .max_message_size(MAX_BODY_BYTES)
                .max_frame_size(MAX_BODY_BYTES);
            ws.on_upgrade(move |socket| handle_socket(socket, state, bearer))
        }
        Err(_) => (
            StatusCode::UNAUTHORIZED,
            axum::Json(protocol::unauthorized_response()),
        )
            .into_response(),
    }
}

/// Drives one authenticated WebSocket connection for its whole
/// lifetime: client messages are parsed/dispatched/replied to exactly
/// like an HTTP request, and `graph:changed` broadcasts are forwarded
/// as JSON-RPC notifications whenever one fires, interleaved via
/// [`tokio::select!`] so a connection with no pending client request
/// can still receive a push (E "receive without issuing a request
/// first").
///
/// `bearer` is the raw credential value presented at the handshake
/// (see [`ws_rpc_handler`]); it is re-verified against the token
/// file's *current* contents (via [`auth::verify_bearer`]) before
/// every single message this connection ever dispatches (R2 fix) —
/// there is no per-frame `Authorization` header in the WebSocket
/// protocol to re-extract, so the connection retains the value itself
/// instead of the one-time proof. A token rotated out from under an
/// already-open socket is therefore rejected on this socket's very
/// next message, not just on the next new handshake, matching G6/E11.
///
/// R10 fix: this is a thin wrapper around [`run_socket_loop`] that
/// registers one entry in `state.socket_tasks` for as long as *any*
/// `Arc<AppState>` clone tied to this connection could still be
/// alive, and only releases it after that — never concurrently with
/// it. `run_socket_loop` takes `state` *by value*, so by the time its
/// `.await` resolves here, every local binding inside it (including
/// its own `Arc<AppState>` parameter) has already, synchronously,
/// finished dropping as an ordinary consequence of that function
/// returning; this wrapper itself never holds a second clone past
/// that point; only `_task_guard`, which carries no reference to
/// `AppState` at all, is live in this frame, so there is no ordering
/// ambiguity for `shutdown`'s caller (`main.rs`'s
/// `socket_tasks.wait_until_idle`) to race against. The guard still
/// decrements (and wakes any waiter) on every exit path, including a
/// panic inside `run_socket_loop`, since it's a plain `Drop` impl.
async fn handle_socket(socket: WebSocket, state: Arc<AppState>, bearer: String) {
    let _task_guard = state.socket_tasks.enter();
    run_socket_loop(socket, state, bearer).await;
}

/// Drives one authenticated WebSocket connection for its whole
/// lifetime: client messages are parsed/dispatched/replied to exactly
/// like an HTTP request, and `graph:changed` broadcasts are forwarded
/// as JSON-RPC notifications whenever one fires, interleaved via
/// [`tokio::select!`] so a connection with no pending client request
/// can still receive a push (E "receive without issuing a request
/// first").
///
/// `bearer` is the raw credential value presented at the handshake
/// (see [`ws_rpc_handler`]); it is re-verified against the token
/// file's *current* contents (via [`auth::verify_bearer`]) before
/// every single message this connection ever dispatches (R2 fix) —
/// there is no per-frame `Authorization` header in the WebSocket
/// protocol to re-extract, so the connection retains the value itself
/// instead of the one-time proof. A token rotated out from under an
/// already-open socket is therefore rejected on this socket's very
/// next message, not just on the next new handshake, matching G6/E11.
///
/// A client abruptly disconnecting (TCP reset, close frame, or any
/// socket error) simply ends this loop and drops the connection
/// handle (E12) — never a panic, and never a state leak into any
/// other connection, since every connection gets its own `rx`
/// subscription and owns its own `socket`.
///
/// R10 fix: when `state` carries a production shutdown signal
/// (`state.shutdown_rx`), this loop also selects on it. Before this
/// fix, it never watched shutdown at all — its `RecvError::Closed`
/// arm even parked on `future::pending` forever — so an open
/// socket's `Arc<AppState>` (and the registry it carries) reliably
/// outlived `block_on`, panicking on drop exactly as R8 described for
/// the forwarder task. On shutdown this loop now sends a `1001 going
/// away` close frame and returns promptly instead, which (per
/// [`handle_socket`]'s own doc comment) is what lets its caller's
/// `Arc<AppState>` parameter actually finish dropping before
/// `state.socket_tasks`'s guard is released.
async fn run_socket_loop(mut socket: WebSocket, state: Arc<AppState>, bearer: String) {
    let mut rx = state.events_tx.subscribe();
    let mut shutdown_rx = state.shutdown_rx.clone();

    loop {
        tokio::select! {
            _ = async {
                match shutdown_rx.as_mut() {
                    Some(rx) => {
                        // `Err` here means the sender was dropped
                        // without ever observing a new value since
                        // this receiver's last check — treated the
                        // same as an actual shutdown signal, since
                        // either way there is nothing more to select
                        // on and holding this connection open any
                        // longer serves no purpose.
                        let _ = rx.changed().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            }, if shutdown_rx.is_some() => {
                let _ = socket
                    .send(Message::Close(Some(CloseFrame {
                        code: close_code::AWAY,
                        reason: "server is shutting down".into(),
                    })))
                    .await;
                break;
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let proof = match auth::verify_bearer(&bearer, &state.token_path) {
                            Ok(proof) => proof,
                            Err(_) => {
                                // R2/E11: the token on disk no longer
                                // matches what this socket presented at
                                // the handshake — reject this (and every
                                // further) message on this connection
                                // rather than silently keep serving it
                                // under a revoked credential.
                                let reply = protocol::unauthorized_response();
                                let text = serde_json::to_string(&reply)
                                    .expect("a constructed JSON-RPC response always serializes");
                                let _ = socket.send(Message::Text(text.into())).await;
                                let _ = socket
                                    .send(Message::Close(Some(CloseFrame {
                                        code: close_code::POLICY,
                                        reason: "bearer token no longer valid".into(),
                                    })))
                                    .await;
                                break;
                            }
                        };

                        let reply = match serde_json::from_str::<Value>(&text) {
                            Ok(raw) if raw.is_array() => protocol::batch_rejected_response(),
                            Ok(raw) => handle_single_request(proof, raw, state.clone()).await,
                            Err(_) => protocol::parse_error_response(),
                        };
                        let text = serde_json::to_string(&reply)
                            .expect("a constructed JSON-RPC response always serializes");
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) => break,
                    Some(Ok(_)) => {
                        // Ping/Pong/Binary: axum answers Ping with Pong
                        // automatically; nothing else to do here.
                    }
                    Some(Err(_)) | None => break,
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(payload) => {
                        let note = protocol::notification("graph:changed", payload);
                        let text = serde_json::to_string(&note)
                            .expect("a constructed notification always serializes");
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        // This client missed some events; keep going
                        // rather than tearing down the connection over
                        // a best-effort notification channel.
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        // The sender side only drops when `AppState`
                        // itself is dropped (server shutdown); nothing
                        // more to push, but keep serving client
                        // requests until the socket itself closes.
                        std::future::pending::<()>().await;
                    }
                }
            }
        }
    }
}
