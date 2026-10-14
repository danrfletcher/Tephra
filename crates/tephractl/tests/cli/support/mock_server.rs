//! A minimal, fully scripted JSON-RPC-over-HTTP mock server for
//! `tephractl`'s integration tests (`tests/cli/*`). Deliberately not
//! a reuse of `tephrad`'s own `tests/rpc_fixtures` harness: that
//! module lives inside `tephrad`'s test-only sources (not its public
//! library surface) and its two methods under test here
//! (`link.preview_refactor` / `link.apply_refactor`) aren't even
//! implemented by the real dispatcher yet (PR-8's job) — this crate
//! needs full, direct control over every response (including
//! deliberately malformed/dropped ones for the transport-failure
//! tests), independent of what PR-6's dispatcher currently supports.
//!
//! Each [`MockServer::start`] takes an ordered list of [`Behavior`]s:
//! the Nth accepted connection is served the Nth behavior (clamped to
//! the last one if more connections arrive than configured — tests
//! that care about an exact call count assert on
//! [`MockServer::received`]'s length instead of relying on that
//! clamp). No TLS, no keep-alive, no concurrency: one connection is
//! fully handled before the next is accepted, which is all a CLI
//! integration test needs.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::Value;

/// One captured request: method/path aren't tracked (every call in
/// this crate posts to the same `/rpc` path) but the headers (for
/// asserting the bearer token actually sent) and decoded JSON body
/// are.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub headers: HashMap<String, String>,
    pub body: Value,
}

#[derive(Clone)]
pub enum Behavior {
    /// Read the request fully, then respond with `status` and the
    /// given JSON `body`.
    Respond { status: u16, body: Value },
    /// Read the request fully, then close the connection without
    /// writing any response at all — simulates the daemon dropping
    /// the connection mid-request/mid-response.
    Drop,
}

pub struct MockServer {
    pub addr: String,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
    _handle: thread::JoinHandle<()>,
}

impl MockServer {
    pub fn start(behaviors: Vec<Behavior>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local_addr").to_string();
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_thread = Arc::clone(&received);

        let handle = thread::spawn(move || {
            let total = behaviors.len();
            for (served, stream) in listener.incoming().enumerate() {
                if served >= total {
                    break;
                }
                let stream = match stream {
                    Ok(s) => s,
                    Err(_) => break,
                };
                let behavior = behaviors[served].clone();
                handle_connection(stream, behavior, &received_for_thread);
            }
        });

        MockServer {
            addr,
            received,
            _handle: handle,
        }
    }

    /// All requests received so far, in arrival order.
    pub fn received(&self) -> Vec<RecordedRequest> {
        self.received.lock().unwrap().clone()
    }

    pub fn call_count(&self) -> usize {
        self.received.lock().unwrap().len()
    }
}

fn handle_connection(
    mut stream: TcpStream,
    behavior: Behavior,
    received: &Arc<Mutex<Vec<RecordedRequest>>>,
) {
    let recorded = match read_request(&mut stream) {
        Some(r) => r,
        None => return,
    };
    received.lock().unwrap().push(recorded);

    match behavior {
        Behavior::Respond { status, body } => {
            write_response(&mut stream, status, &body);
        }
        Behavior::Drop => {
            // Dropping `stream` here (end of scope, no response
            // written) closes the socket out from under the client
            // mid-request, which is exactly the "connection dropped"
            // failure mode `transport.rs`'s retry logic must handle.
        }
    }
}

/// Reads a full HTTP/1.1 request (request line + headers + body,
/// using `Content-Length`) off `stream` and decodes the body as
/// JSON. Returns `None` if the connection closed before a full
/// request arrived (nothing to record).
fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut reader = BufReader::new(stream);
    let mut headers = HashMap::new();

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).ok()? == 0 {
        return None;
    }

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 || line == "\r\n" || line.is_empty() {
            break;
        }
        let line = line.trim_end();
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let mut body_bytes = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body_bytes).ok()?;
    }

    let body: Value = if body_bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body_bytes).unwrap_or(Value::Null)
    };

    Some(RecordedRequest { headers, body })
}

fn write_response(stream: &mut TcpStream, status: u16, body: &Value) {
    let body_text = serde_json::to_string(body).expect("serialize mock response body");
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        _ => "Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_text}",
        body_text.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// Convenience for the common case: a single successful JSON-RPC
/// response (`{"jsonrpc":"2.0","id":1,"result":<result>}`).
pub fn ok_envelope(result: Value) -> Value {
    serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": result})
}

/// Convenience for a single JSON-RPC error response.
pub fn err_envelope(code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = serde_json::json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    serde_json::json!({"jsonrpc": "2.0", "id": 1, "error": error})
}

/// Loads one of the canned JSON-RPC payloads under
/// `tests/fixtures/<name>.json`.
pub fn load_fixture(name: &str) -> Value {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let raw =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("reading fixture {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parsing fixture {path}: {e}"))
}
