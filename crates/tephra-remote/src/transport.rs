//! The one request path both real adapters share: token from the
//! credential store on every call, the E2 latch, the E1 gate, no redirect
//! following, and the mapping of every outcome to a [`RemoteError`].
//!
//! Order of a call:
//! 1. Read the token from the store. A missing token, a locked or denied
//!    store, or any store error is `AuthRejected` with no request and no
//!    other store tried (E17).
//! 2. If the token equals the one the provider last rejected, answer
//!    `AuthRejected` with no request (E2 latch). A different token clears
//!    the way: no restart needed.
//! 3. If a rate limit is still running, answer `RateLimited` with no
//!    request (E1).
//! 4. Send the request, once. Redirects are never followed.
//! 5. Map the answer. A 401 latches the token; a 403 or 429 is classified
//!    by [`crate::rate_limit`]; any 2xx (or a 304 to a conditional
//!    request) clears the limit and the latch.
//!
//! The token only ever lives in a `Secret` and in a header marked
//! sensitive; no log line, error or `Debug` output here carries it.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use percent_encoding::{utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, IF_NONE_MATCH, LOCATION, USER_AGENT,
};

use crate::base_url::{resolve_base_url, BaseUrlError};
use crate::clock::Clock;
use crate::credentials::{CredentialStore, Provider, Secret};
use crate::error::{RemoteError, UnreachableCause};
use crate::rate_limit::{self, Api, RateGate, Verdict};

/// Per-adapter tuning. Not secret: safe to `Debug`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdapterOptions {
    /// Total time for one request, headers and body.
    pub request_timeout: Duration,
    /// Time to open the connection.
    pub connect_timeout: Duration,
}

impl Default for AdapterOptions {
    fn default() -> Self {
        AdapterOptions {
            request_timeout: Duration::from_secs(30),
            connect_timeout: Duration::from_secs(10),
        }
    }
}

/// Everything except letters, digits and `-._~` is percent-encoded in a
/// path segment or a query value.
const ENCODE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Percent-encodes one path segment or query component.
pub(crate) fn encode_component(text: &str) -> String {
    utf8_percent_encode(text, ENCODE).to_string()
}

/// Encodes a `/`-separated path segment by segment. Empty, `.` and `..`
/// segments and control characters are refused: nothing leaves the
/// intended route.
pub(crate) fn encode_path(path: &str) -> Result<String, RemoteError> {
    if path.is_empty() || path.chars().any(|c| c.is_control()) {
        return Err(RemoteError::InvalidInput("path"));
    }
    let mut out = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(RemoteError::InvalidInput("path"));
        }
        out.push(encode_component(segment));
    }
    Ok(out.join("/"))
}

/// One GET.
#[derive(Debug, Default)]
pub(crate) struct Request {
    /// Already encoded, starting with `/`, appended to the base URL.
    pub path: String,
    /// Raw pairs; encoded when the URL is built.
    pub query: Vec<(&'static str, String)>,
    pub accept: Option<&'static str>,
    pub if_none_match: Option<String>,
}

/// A 2xx or (conditional) 304 answer.
#[derive(Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

/// How long a streamed body may take in all. The client's own
/// `request_timeout` is for answers that are read whole; a file is read as
/// it arrives, and a stalled connection is cut by the read timeout (the
/// same `request_timeout`, between two chunks), not by this.
const STREAM_TOTAL_TIMEOUT: Duration = Duration::from_secs(60 * 60);

/// The body of a 2xx answer, read piece by piece: what a file download
/// needs, so no file is held whole in memory. Built by [`Core::stream`] or,
/// for bytes that are already in hand, by [`BodyStream::from_bytes`].
///
/// It never carries a token or a header: a failure is a [`RemoteError`].
pub struct BodyStream {
    inner: Inner,
    length: Option<u64>,
}

enum Inner {
    Http(reqwest::Response),
    Memory(Option<Vec<u8>>),
}

impl fmt::Debug for BodyStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BodyStream")
            .field("length", &self.length)
            .finish_non_exhaustive()
    }
}

impl BodyStream {
    /// A body that is already whole (one piece).
    pub fn from_bytes(bytes: Vec<u8>) -> BodyStream {
        BodyStream {
            length: Some(bytes.len() as u64),
            inner: Inner::Memory(Some(bytes)),
        }
    }

    /// The length the server announced, if it did.
    pub fn content_length(&self) -> Option<u64> {
        self.length
    }

    /// The next piece, or `None` at the end. A body that is cut short is
    /// `Unreachable(BodyCut)` (or `Timeout` when the connection went
    /// quiet).
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, RemoteError> {
        match &mut self.inner {
            Inner::Memory(bytes) => Ok(bytes.take().filter(|b| !b.is_empty())),
            Inner::Http(response) => match response.chunk().await {
                Ok(piece) => Ok(piece.map(|b| b.to_vec())),
                Err(e) => Err(RemoteError::Unreachable(match classify_transport(&e) {
                    UnreachableCause::Timeout => UnreachableCause::Timeout,
                    _ => UnreachableCause::BodyCut,
                })),
            },
        }
    }
}

/// What one exchange gave back.
enum Answer {
    Whole(Reply),
    Stream(BodyStream),
}

#[derive(Default)]
struct State {
    gate: RateGate,
    /// The token the provider rejected with a 401, while it is still the
    /// one in the store.
    latched: Option<Secret>,
}

pub(crate) struct Core {
    api: Api,
    provider: Provider,
    base: String,
    client: reqwest::Client,
    store: Arc<dyn CredentialStore>,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core")
            .field("api", &self.api)
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl Core {
    /// The constructor of the public adapters: the base URL is resolved
    /// and checked (default, or loopback only) before anything is built.
    pub(crate) fn new(
        api: Api,
        provider: Provider,
        configured_base: Option<&str>,
        default_base: &str,
        store: Arc<dyn CredentialStore>,
        clock: Arc<dyn Clock>,
        options: AdapterOptions,
    ) -> Result<Core, BaseUrlError> {
        let base = resolve_base_url(configured_base, default_base)?;
        Ok(Core::build(api, provider, base, store, clock, options))
    }

    fn build(
        api: Api,
        provider: Provider,
        base: String,
        store: Arc<dyn CredentialStore>,
        clock: Arc<dyn Clock>,
        options: AdapterOptions,
    ) -> Core {
        let mut builder = reqwest::Client::builder()
            // E13: a redirect is reported, never followed, so the
            // Authorization header cannot go to another location.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(options.request_timeout)
            .read_timeout(options.request_timeout)
            .connect_timeout(options.connect_timeout)
            .user_agent(concat!("tephra-remote/", env!("CARGO_PKG_VERSION")));
        if crate::api_url::is_loopback_url(&base) {
            // A loopback base is a test fake or a local stand-in: never
            // route it through a system proxy.
            builder = builder.no_proxy();
        }
        let client = builder
            .build()
            .expect("an HTTP client with these options always builds");
        Core {
            api,
            provider,
            base,
            client,
            store,
            clock,
            state: Mutex::new(State::default()),
        }
    }

    pub(crate) fn base(&self) -> &str {
        &self.base
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reads the token from the store; any failure is `AuthRejected` and
    /// no request is made.
    fn token(&self) -> Result<Secret, RemoteError> {
        match self.store.get(self.provider) {
            Ok(Some(token)) => Ok(token),
            Ok(None) => {
                tracing::debug!(provider = %self.provider, "no token in the credential store");
                Err(RemoteError::AuthRejected)
            }
            Err(e) => {
                tracing::debug!(provider = %self.provider, reason = %e, "credential store refused the read");
                Err(RemoteError::AuthRejected)
            }
        }
    }

    fn url(&self, request: &Request) -> String {
        let mut url = format!("{}{}", self.base, request.path);
        for (i, (key, value)) in request.query.iter().enumerate() {
            url.push(if i == 0 { '?' } else { '&' });
            url.push_str(key);
            url.push('=');
            url.push_str(&encode_component(value));
        }
        url
    }

    pub(crate) async fn get(&self, request: Request) -> Result<Reply, RemoteError> {
        match self.exchange(request, false).await? {
            Answer::Whole(reply) => Ok(reply),
            Answer::Stream(_) => unreachable!("a whole read never answers with a stream"),
        }
    }

    /// Like [`Core::get`], but a 2xx answer's body is handed over unread:
    /// the caller reads it piece by piece. Every other outcome, and every
    /// step before the body, is the same as for `get`.
    pub(crate) async fn stream(&self, request: Request) -> Result<BodyStream, RemoteError> {
        match self.exchange(request, true).await? {
            Answer::Stream(body) => Ok(body),
            Answer::Whole(reply) => Ok(BodyStream::from_bytes(reply.body)),
        }
    }

    async fn exchange(&self, request: Request, streaming: bool) -> Result<Answer, RemoteError> {
        let token = self.token()?;
        let now = self.clock.now();
        {
            let state = self.lock();
            if state
                .latched
                .as_ref()
                .is_some_and(|rejected| rejected.expose() == token.expose())
            {
                return Err(RemoteError::AuthRejected);
            }
            if let Err(retry_at) = state.gate.check(now) {
                return Err(RemoteError::RateLimited { retry_at });
            }
        }

        let mut bearer = HeaderValue::from_str(&format!("Bearer {}", token.expose()))
            .map_err(|_| RemoteError::AuthRejected)?;
        bearer.set_sensitive(true);
        let conditional = request.if_none_match.is_some();
        let mut builder = self
            .client
            .get(self.url(&request))
            .header(AUTHORIZATION, bearer);
        if let Some(accept) = request.accept {
            builder = builder.header(ACCEPT, accept);
        }
        if let Some(etag) = &request.if_none_match {
            let value =
                HeaderValue::from_str(etag).map_err(|_| RemoteError::InvalidInput("etag"))?;
            builder = builder.header(IF_NONE_MATCH, value);
        }
        if streaming {
            builder = builder.timeout(STREAM_TOTAL_TIMEOUT);
        }
        if self.api == Api::Github {
            builder = builder.header("x-github-api-version", "2022-11-28").header(
                USER_AGENT,
                concat!("tephra-remote/", env!("CARGO_PKG_VERSION")),
            );
        }

        let response = builder.send().await.map_err(|e| {
            let cause = classify_transport(&e);
            tracing::debug!(cause = cause.as_str(), "request did not complete");
            RemoteError::Unreachable(cause)
        })?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();

        match status {
            200..=299 if streaming => {
                self.succeeded();
                Ok(Answer::Stream(BodyStream {
                    length: response.content_length(),
                    inner: Inner::Http(response),
                }))
            }
            200..=299 => {
                let body = response.bytes().await.map_err(|e| {
                    let cause = match classify_transport(&e) {
                        UnreachableCause::Timeout => UnreachableCause::Timeout,
                        _ => UnreachableCause::BodyCut,
                    };
                    RemoteError::Unreachable(cause)
                })?;
                self.succeeded();
                Ok(Answer::Whole(Reply {
                    status,
                    headers,
                    body: body.to_vec(),
                }))
            }
            304 if conditional => {
                self.succeeded();
                Ok(Answer::Whole(Reply {
                    status,
                    headers,
                    body: Vec::new(),
                }))
            }
            300..=399 => {
                let location = headers
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                Err(RemoteError::Moved { location })
            }
            401 => {
                self.lock().latched = Some(Secret::new(token.expose()));
                tracing::debug!(provider = %self.provider, "token rejected; latched until the stored token changes");
                Err(RemoteError::AuthRejected)
            }
            403 | 429 => {
                let body = read_capped(response).await;
                match rate_limit::classify(self.api, status, &headers, &body, now) {
                    Verdict::RateLimited { delay } => {
                        let retry_at = self.lock().gate.record_limited(now, delay);
                        tracing::debug!(provider = %self.provider, "rate limited");
                        Err(RemoteError::RateLimited { retry_at })
                    }
                    Verdict::PermissionDenied => Err(RemoteError::PermissionDenied),
                    Verdict::NotRateLimit => Err(RemoteError::Unexpected { status }),
                }
            }
            404 | 410 => Err(RemoteError::NotFound),
            500..=599 => Err(RemoteError::Server { status }),
            _ => Err(RemoteError::Unexpected { status }),
        }
    }

    fn succeeded(&self) {
        let mut state = self.lock();
        state.gate.record_success();
        state.latched = None;
    }
}

/// The start of an error body (at most 64 KiB), enough to classify it.
async fn read_capped(mut response: reqwest::Response) -> Vec<u8> {
    const CAP: usize = 64 * 1024;
    let mut out = Vec::new();
    while out.len() < CAP {
        match response.chunk().await {
            Ok(Some(chunk)) => out.extend_from_slice(&chunk),
            _ => break,
        }
    }
    out.truncate(CAP);
    out
}

/// Maps a transport error to its cause by kind and, for a failed connect,
/// by the text of its source chain (the resolver's error has no type to
/// match on).
pub(crate) fn classify_transport(error: &reqwest::Error) -> UnreachableCause {
    if error.is_timeout() {
        return UnreachableCause::Timeout;
    }
    if error.is_connect() {
        let mut chain = String::new();
        let mut source: Option<&dyn std::error::Error> = Some(error);
        while let Some(e) = source {
            chain.push_str(&e.to_string().to_ascii_lowercase());
            chain.push(' ');
            source = e.source();
        }
        let dns = [
            "dns error",
            "failed to lookup",
            "name or service not known",
            "nodename nor servname",
            "no address associated",
        ];
        if dns.iter().any(|needle| chain.contains(needle)) {
            return UnreachableCause::Dns;
        }
        return UnreachableCause::Connect;
    }
    if error.is_body() || error.is_decode() {
        return UnreachableCause::BodyCut;
    }
    UnreachableCause::Other
}
