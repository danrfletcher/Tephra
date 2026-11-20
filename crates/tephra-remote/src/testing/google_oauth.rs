//! The fake Google sign-in pair `tephractl auth google` talks to: the
//! consent endpoint (`GET /authorize`, which "approves" at once by
//! redirecting to the caller's loopback address) and the token endpoint
//! (`POST /token`).
//!
//! It checks what Google checks that matters to the client: the client
//! id and secret, a loopback `redirect_uri`, the S256 code challenge, a
//! single-use code, and at the token endpoint that the code verifier
//! hashes to the challenge. It needs no bearer token.
//!
//! Scripting: `deny_next_consent`, `fail_next_token`,
//! `omit_refresh_token_next`. Every request's parameters are recorded
//! for the test to read ([`AuthorizeRequest`], [`TokenRequest`]).
//!
//! The error answers deliberately echo the code and the verifier in
//! their `error_description`, the way a careless server could, so a test
//! can prove the client never forwards a server's error text.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::{Form, Query, State};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{json_response, serve, with_header, AuthMode, Common, Flavor, Served};
use crate::clock::{Clock, SystemClock};

/// The one scope the fake grants.
const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

/// One request to `/authorize`, as the fake saw its query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizeRequest {
    pub params: BTreeMap<String, String>,
}

impl AuthorizeRequest {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }
}

/// One request to `/token`, as the fake saw its form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenRequest {
    pub params: BTreeMap<String, String>,
}

impl TokenRequest {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }
}

struct IssuedCode {
    challenge: String,
    redirect_uri: String,
}

struct TokenFailure {
    status: u16,
    error: String,
}

struct State_ {
    client_id: String,
    client_secret: String,
    access_token: String,
    refresh_token: String,
    codes: HashMap<String, IssuedCode>,
    issued: u64,
    deny_next: bool,
    fail_next: VecDeque<TokenFailure>,
    omit_refresh_next: bool,
    authorize_requests: Vec<AuthorizeRequest>,
    token_requests: Vec<TokenRequest>,
}

type Shared = (Arc<Mutex<State_>>,);

/// The fake Google OAuth server. Dropping it closes the port.
pub struct FakeGoogleOAuth {
    base_url: String,
    state: Arc<Mutex<State_>>,
    common: Arc<Common<()>>,
    _served: Served,
}

impl FakeGoogleOAuth {
    /// Starts on `127.0.0.1:0`.
    pub async fn start() -> FakeGoogleOAuth {
        let state = Arc::new(Mutex::new(State_ {
            client_id: "fake-client-id.apps.googleusercontent.com".to_string(),
            client_secret: "fake-client-secret".to_string(),
            access_token: "ya29.fake-access-token".to_string(),
            refresh_token: "1//fake-refresh-token".to_string(),
            codes: HashMap::new(),
            issued: 0,
            deny_next: false,
            fail_next: VecDeque::new(),
            omit_refresh_next: false,
            authorize_requests: Vec::new(),
            token_requests: Vec::new(),
        }));
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let common = Arc::new(Common::new(
            clock,
            Flavor {
                unauthorized: || plain(401, "unauthorized"),
                error: |status| plain(status, "error"),
                limited: |_, _, _| plain(429, "limited"),
            },
        ));
        *common.auth() = AuthMode::Open;
        let router = Router::new()
            .route("/authorize", get(authorize))
            .route("/token", post(token))
            .with_state((state.clone(),));
        let served = serve(router, common.clone()).await;
        FakeGoogleOAuth {
            base_url: format!("http://{}", served.addr),
            state,
            common,
            _served: served,
        }
    }

    fn state(&self) -> MutexGuard<'_, State_> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `http://127.0.0.1:<port>`.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The consent endpoint, what a client JSON's `auth_uri` holds.
    pub fn authorize_url(&self) -> String {
        format!("{}/authorize", self.base_url)
    }

    /// The token endpoint, what a client JSON's `token_uri` holds.
    pub fn token_url(&self) -> String {
        format!("{}/token", self.base_url)
    }

    pub fn client_id(&self) -> String {
        self.state().client_id.clone()
    }

    pub fn client_secret(&self) -> String {
        self.state().client_secret.clone()
    }

    /// Replaces the client id and secret the fake checks and puts in
    /// [`client_json`](Self::client_json).
    pub fn set_client_credentials(&self, client_id: &str, client_secret: &str) {
        let mut state = self.state();
        state.client_id = client_id.to_string();
        state.client_secret = client_secret.to_string();
    }

    /// The text of a Desktop-app client JSON that points at this fake.
    pub fn client_json(&self) -> String {
        let state = self.state();
        json!({
            "installed": {
                "client_id": state.client_id,
                "project_id": "fake-project",
                "auth_uri": self.authorize_url(),
                "token_uri": self.token_url(),
                "client_secret": state.client_secret,
                "redirect_uris": ["http://localhost"],
            }
        })
        .to_string()
    }

    /// The tokens the next successful exchanges hand out.
    pub fn set_tokens(&self, access_token: &str, refresh_token: &str) {
        let mut state = self.state();
        state.access_token = access_token.to_string();
        state.refresh_token = refresh_token.to_string();
    }

    /// The next consent request is answered as if the user pressed
    /// Cancel: a redirect with `error=access_denied` and the state.
    pub fn deny_next_consent(&self) {
        self.state().deny_next = true;
    }

    /// The next token request fails with `status` and the OAuth error
    /// code `error` (`invalid_grant`, `invalid_client`, ...) in a JSON body that
    /// also echoes the code and verifier; a 5xx has no JSON body.
    pub fn fail_next_token(&self, status: u16, error: &str) {
        self.state().fail_next.push_back(TokenFailure {
            status,
            error: error.to_string(),
        });
    }

    /// The next successful exchange leaves out the refresh token.
    pub fn omit_refresh_token_next(&self) {
        self.state().omit_refresh_next = true;
    }

    /// Every `/authorize` request so far.
    pub fn authorize_requests(&self) -> Vec<AuthorizeRequest> {
        self.state().authorize_requests.clone()
    }

    /// Every `/token` request so far.
    pub fn token_requests(&self) -> Vec<TokenRequest> {
        self.state().token_requests.clone()
    }

    /// Every HTTP request the fake saw (method, path, query, headers).
    pub fn request_log(&self) -> super::RequestLog {
        self.common.log.clone()
    }
}

fn plain(status: u16, message: &str) -> Response {
    json_response(status, &json!({ "error": message }))
}

fn lock(shared: &Shared) -> MutexGuard<'_, State_> {
    shared.0.lock().unwrap_or_else(|e| e.into_inner())
}

fn encode(text: &str) -> String {
    utf8_percent_encode(text, NON_ALPHANUMERIC).to_string()
}

fn redirect(location: &str) -> Response {
    with_header(
        {
            let mut r = Response::new(axum::body::Body::empty());
            *r.status_mut() = axum::http::StatusCode::FOUND;
            r
        },
        "location",
        location,
    )
}

fn is_loopback_redirect(uri: &str) -> bool {
    uri.strip_prefix("http://127.0.0.1:")
        .and_then(|rest| rest.split('/').next())
        .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
}

async fn authorize(
    State(shared): State<Shared>,
    Query(query): Query<BTreeMap<String, String>>,
) -> Response {
    let mut state = lock(&shared);
    state.authorize_requests.push(AuthorizeRequest {
        params: query.clone(),
    });
    let get = |k: &str| query.get(k).map(String::as_str).unwrap_or("");
    if get("client_id") != state.client_id {
        return plain(400, "invalid_client");
    }
    let redirect_uri = get("redirect_uri");
    if !is_loopback_redirect(redirect_uri) {
        return plain(400, "redirect_uri_mismatch");
    }
    let sep = if redirect_uri.contains('?') { '&' } else { '?' };
    let client_state = get("state");
    if state.deny_next {
        state.deny_next = false;
        return redirect(&format!(
            "{redirect_uri}{sep}error=access_denied&state={}",
            encode(client_state)
        ));
    }
    if get("response_type") != "code"
        || get("code_challenge_method") != "S256"
        || get("code_challenge").is_empty()
    {
        return plain(400, "invalid_request");
    }
    state.issued += 1;
    let code = format!("fake-code-{}-{}", state.issued, state.codes.len());
    state.codes.insert(
        code.clone(),
        IssuedCode {
            challenge: get("code_challenge").to_string(),
            redirect_uri: redirect_uri.to_string(),
        },
    );
    redirect(&format!(
        "{redirect_uri}{sep}code={}&state={}",
        encode(&code),
        encode(client_state)
    ))
}

fn b64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        let chars = chunk.len() + 1;
        for i in 0..chars {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// A rejection that echoes the secrets it was sent, like a careless
/// server: the client must not pass this text on.
fn grant_error(params: &BTreeMap<String, String>, error: &str) -> Response {
    json_response(
        400,
        &json!({
            "error": error,
            "error_description": format!(
                "Bad Request: code={} code_verifier={}",
                params.get("code").map(String::as_str).unwrap_or(""),
                params.get("code_verifier").map(String::as_str).unwrap_or("")
            ),
        }),
    )
}

async fn token(
    State(shared): State<Shared>,
    Form(form): Form<BTreeMap<String, String>>,
) -> Response {
    let mut state = lock(&shared);
    state.token_requests.push(TokenRequest {
        params: form.clone(),
    });
    if let Some(failure) = state.fail_next.pop_front() {
        // The scripted failure consumes the code, like a real one that
        // was spent.
        if let Some(code) = form.get("code") {
            state.codes.remove(code);
        }
        return if (400..500).contains(&failure.status) {
            let mut response = grant_error(&form, &failure.error);
            *response.status_mut() =
                axum::http::StatusCode::from_u16(failure.status).expect("a valid status");
            response
        } else {
            plain(failure.status, "backend_error")
        };
    }
    let get = |k: &str| form.get(k).map(String::as_str).unwrap_or("");
    if get("grant_type") != "authorization_code" {
        return plain(400, "unsupported_grant_type");
    }
    if get("client_id") != state.client_id || get("client_secret") != state.client_secret {
        return plain(401, "invalid_client");
    }
    let verifier = get("code_verifier");
    let verifier_ok = (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
    let Some(issued) = state.codes.remove(get("code")) else {
        return grant_error(&form, "invalid_grant");
    };
    if !verifier_ok
        || issued.redirect_uri != get("redirect_uri")
        || b64url(&Sha256::digest(verifier.as_bytes())) != issued.challenge
    {
        return grant_error(&form, "invalid_grant");
    }
    let mut body = json!({
        "access_token": state.access_token,
        "expires_in": 3599,
        "scope": DRIVE_SCOPE,
        "token_type": "Bearer",
    });
    if state.omit_refresh_next {
        state.omit_refresh_next = false;
    } else {
        body["refresh_token"] = json!(state.refresh_token);
    }
    json_response(200, &body)
}
