//! The HTTP + WebSocket JSON-RPC transport (G6): one axum
//! [`Router`] serving `POST /rpc` (plain HTTP request/response) and
//! `GET /rpc` (WebSocket upgrade, same request/response semantics
//! plus server-pushed `graph:changed` notifications) on a single
//! listener.
//!
//! [`AppState`] carries everything a request handler needs: the
//! token file path (read fresh on every call — E11), the two backend
//! trait objects dispatch calls through, and a broadcast channel for
//! `graph:changed` pushes. Nothing here performs auth itself beyond
//! calling [`crate::rpc::auth::authenticate`]; nothing here contains
//! any graph or registry logic beyond calling
//! [`crate::rpc::dispatch::dispatch`].

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::rpc::auth::{authenticate, Authenticated};
use crate::rpc::backend::{GraphBackend, SourceBackend};
use crate::rpc::dispatch::{self, Backends};
use crate::rpc::protocol;

/// E9's oversized-body cap: a request body larger than this is
/// rejected (HTTP 413) by axum's extractor before this server ever
/// attempts to deserialize it as JSON.
pub const MAX_BODY_BYTES: usize = 10 * 1024 * 1024;

/// How many buffered `graph:changed` events a slow WebSocket client
/// can fall behind by before it starts missing them (a lagged
/// receiver skips forward rather than blocking the broadcaster or
/// every other client — the standard `tokio::sync::broadcast`
/// trade-off, acceptable here since each notification is a
/// best-effort "something changed, re-query if interested" hint, not
/// a reliable event log).
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// Shared state behind every request handler in this module. Always
/// held as `Arc<AppState>` — never cloned field-by-field — so every
/// connection/request sees the same token path and backend instances.
pub struct AppState {
    pub token_path: PathBuf,
    pub graph: Arc<dyn GraphBackend>,
    pub sources: Arc<dyn SourceBackend>,
    events_tx: broadcast::Sender<Value>,
}

impl AppState {
    pub fn new(
        token_path: PathBuf,
        graph: Arc<dyn GraphBackend>,
        sources: Arc<dyn SourceBackend>,
    ) -> Self {
        let (events_tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        AppState {
            token_path,
            graph,
            sources,
            events_tx,
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
/// [`authenticate`]. The body-size cap (E9) is applied as a layer, so
/// it runs before this crate's own handler code ever sees the bytes.
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
    let proof = match authenticate(&headers, &state.token_path) {
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

    json_200(handle_single_request(&proof, &raw, &state))
}

/// Validates envelope + dispatches one already-parsed, non-batch
/// request. Shared between the HTTP and WebSocket handlers so both
/// transports produce byte-identical results for the same input (the
/// transport-parity requirement).
fn handle_single_request(proof: &Authenticated, raw: &Value, state: &AppState) -> Value {
    let req = match protocol::validate_envelope(raw) {
        Ok(req) => req,
        Err(e) => return protocol::error_response(e.id, e.code, e.message, None),
    };

    let backends = Backends {
        graph: state.graph.as_ref(),
        sources: state.sources.as_ref(),
    };

    match dispatch::dispatch(proof, &req.method, &req.params, &backends) {
        Ok(result) => protocol::success_response(req.id, result),
        Err((code, message, data)) => protocol::error_response(req.id, code, message, data),
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
    match authenticate(&headers, &state.token_path) {
        Ok(proof) => ws.on_upgrade(move |socket| handle_socket(socket, state, proof)),
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
/// `proof` is obtained once, at the handshake (see [`ws_rpc_handler`]),
/// and reused for every message this connection ever sends — a
/// judgement call where the spec is silent on whether an
/// already-open WebSocket must be re-authenticated per message (there
/// is no per-frame `Authorization` header in the WebSocket protocol
/// to re-check against); see `tests/integration/rpc_concurrency.rs`'s
/// token-rotation test for what this does and does not cover.
///
/// A client abruptly disconnecting (TCP reset, close frame, or any
/// socket error) simply ends this loop and drops the connection
/// handle (E12) — never a panic, and never a state leak into any
/// other connection, since every connection gets its own `rx`
/// subscription and owns its own `socket`.
async fn handle_socket(mut socket: WebSocket, state: Arc<AppState>, proof: Authenticated) {
    let mut rx = state.events_tx.subscribe();

    loop {
        tokio::select! {
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        let reply = match serde_json::from_str::<Value>(&text) {
                            Ok(raw) if raw.is_array() => protocol::batch_rejected_response(),
                            Ok(raw) => handle_single_request(&proof, &raw, &state),
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
