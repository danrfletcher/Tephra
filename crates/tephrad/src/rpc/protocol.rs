//! JSON-RPC 2.0 envelope types, the standard error codes (E8), and
//! the small set of server-constructed response shapes every
//! transport (`server.rs`'s HTTP and WebSocket handlers) builds
//! through this module rather than hand-assembling `serde_json::Value`
//! trees inline.
//!
//! Batch requests (a top-level JSON array) are explicitly rejected,
//! not accepted (a documented judgement call where the spec offered
//! either option and asked for one, tested): see
//! [`batch_rejected_response`] and `tests/unit/rpc_envelope.rs`.

use serde::Serialize;
use serde_json::{json, Value};

/// Invalid JSON was received by the server (E14: an empty body, or
/// any other input `serde_json` can't parse at all).
pub const PARSE_ERROR: i64 = -32700;
/// The JSON sent is not a valid envelope: missing/wrong `"jsonrpc"`,
/// missing `"method"`, or (this server's own choice, since batches are
/// rejected rather than accepted) a top-level array.
pub const INVALID_REQUEST: i64 = -32600;
/// `"method"` does not name one of this server's registered verbs.
pub const METHOD_NOT_FOUND: i64 = -32601;
/// `"params"` is not a JSON object, or a method's required
/// fields/types inside it are missing or wrong.
pub const INVALID_PARAMS: i64 = -32602;
/// Reserved implementation-defined range (JSON-RPC 2.0 §5.1 allows
/// `-32000` to `-32099` for server-defined errors); used for this
/// server's own domain errors below.
pub const UNAUTHORIZED: i64 = -32001;
pub const RESOLVE_ERROR: i64 = -32010;
pub const SOURCE_ERROR: i64 = -32011;
/// Standard JSON-RPC 2.0 "Internal error" (§5.1): used only when a
/// dispatch call panicked inside its `tokio::task::spawn_blocking`
/// task (R6) — a bug, not a client-facing validation/auth/domain
/// failure, so it gets the generic reserved code rather than
/// [`SOURCE_ERROR`]/[`RESOLVE_ERROR`].
pub const INTERNAL_ERROR: i64 = -32603;

/// One JSON-RPC error object.
#[derive(Debug, Clone, Serialize)]
pub struct RpcErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A fully validated request: `id` is always present (`Value::Null`
/// if the client omitted it — see [`validate_envelope`]'s doc comment
/// for why this server treats a missing `id` as `null` rather than as
/// a fire-and-forget notification), `method` is always a non-empty
/// string, and `params` is always a JSON object (defaulted to `{}`
/// when the client omitted it entirely).
#[derive(Debug, Clone)]
pub struct ValidRequest {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// An envelope-level validation failure, carrying whatever `id` could
/// be recovered from the raw input (so a client can still correlate
/// the error with its own call whenever the input was parseable
/// enough to contain one).
#[derive(Debug, Clone)]
pub struct EnvelopeError {
    pub id: Value,
    pub code: i64,
    pub message: String,
}

/// Validates a parsed JSON value as a JSON-RPC 2.0 request envelope
/// (E8). Does not look at `method` beyond "is it a non-empty string"
/// — whether the *name* is a known method is [`crate::rpc::dispatch`]'s
/// job (`-32601`), not this function's (`-32600`).
///
/// A missing `"id"` is accepted and treated as `Value::Null` rather
/// than rejected as `-32600` or treated as a fire-and-forget
/// notification per the bare JSON-RPC spec: every method this server
/// exposes is a request/response call, so there is always something
/// useful to reply with, and a client that forgot `"id"` gets a clear
/// response rather than silence. This is a judgement call the spec
/// left open; see `tests/unit/rpc_envelope.rs`'s
/// `missing_id_is_treated_as_null_not_rejected` for the pinned
/// behavior.
pub fn validate_envelope(raw: &Value) -> Result<ValidRequest, EnvelopeError> {
    let id = raw.get("id").cloned().unwrap_or(Value::Null);

    if !raw.is_object() {
        return Err(EnvelopeError {
            id,
            code: INVALID_REQUEST,
            message: "request must be a JSON object".to_string(),
        });
    }

    let jsonrpc_ok = matches!(raw.get("jsonrpc"), Some(Value::String(v)) if v == "2.0");
    if !jsonrpc_ok {
        return Err(EnvelopeError {
            id,
            code: INVALID_REQUEST,
            message: "missing or invalid \"jsonrpc\": must be the string \"2.0\"".to_string(),
        });
    }

    let method = match raw.get("method") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        _ => {
            return Err(EnvelopeError {
                id,
                code: INVALID_REQUEST,
                message: "missing or invalid \"method\": must be a non-empty string".to_string(),
            })
        }
    };

    let params = match raw.get("params") {
        None | Some(Value::Null) => Value::Object(Default::default()),
        Some(Value::Object(map)) => Value::Object(map.clone()),
        Some(_) => {
            return Err(EnvelopeError {
                id,
                code: INVALID_PARAMS,
                message: "\"params\" must be a JSON object".to_string(),
            })
        }
    };

    Ok(ValidRequest { id, method, params })
}

/// `{"jsonrpc":"2.0","id":<id>,"result":<result>}`.
pub fn success_response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// `{"jsonrpc":"2.0","id":<id>,"error":{"code":...,"message":...}}`.
pub fn error_response(id: Value, code: i64, message: impl Into<String>, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message.into() });
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({ "jsonrpc": "2.0", "id": id, "error": error })
}

/// A JSON-RPC Notification (no `"id"` field at all — distinguishing
/// it, per the spec, from any request/response pair): used only for
/// the server-pushed `graph:changed` event, never for a method's
/// reply.
pub fn notification(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "method": method, "params": params })
}

/// Response for input that couldn't be parsed as JSON at all (E14),
/// including a genuinely empty body.
pub fn parse_error_response() -> Value {
    error_response(Value::Null, PARSE_ERROR, "invalid JSON", None)
}

/// Response for a top-level JSON array (batch request) — rejected
/// outright rather than processed, see this module's doc comment.
pub fn batch_rejected_response() -> Value {
    error_response(
        Value::Null,
        INVALID_REQUEST,
        "batch requests are not supported",
        None,
    )
}

/// Response for a request that failed [`crate::rpc::auth::authenticate`]
/// (E6/E7). `id` is always `Value::Null` here: authentication happens
/// before the body is ever parsed (see `server.rs`), so there is no
/// client-supplied `id` to echo back yet.
pub fn unauthorized_response() -> Value {
    error_response(Value::Null, UNAUTHORIZED, "unauthorized", None)
}
