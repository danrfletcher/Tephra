//! What an adapter call can fail with. One enum for both providers, so
//! the layers above map failures once.
//!
//! No variant carries a token, a request header or a response body: the
//! only server-supplied text is `Moved::location`, a redirect target. The
//! derived `Debug` and the `Display` text are therefore safe to log.

use std::fmt;
use std::time::SystemTime;

/// Why a network call failed before an HTTP answer was complete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnreachableCause {
    /// The connection was refused or reset.
    Connect,
    /// The host name did not resolve.
    Dns,
    /// No answer within the adapter's timeout.
    Timeout,
    /// The answer started and the body was cut off.
    BodyCut,
    /// Any other transport failure (TLS, protocol).
    Other,
}

impl UnreachableCause {
    pub fn as_str(self) -> &'static str {
        match self {
            UnreachableCause::Connect => "connection failed",
            UnreachableCause::Dns => "host name did not resolve",
            UnreachableCause::Timeout => "timed out",
            UnreachableCause::BodyCut => "response was cut off",
            UnreachableCause::Other => "transport error",
        }
    }
}

/// An adapter failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteError {
    /// The provider's rate limit (E1). Calls before `retry_at` are refused
    /// locally without a request; the first one at or after it goes out.
    RateLimited { retry_at: SystemTime },
    /// No token, an unreadable credential store, a token the provider
    /// answered 401 to, or a call made with a token already rejected (E2).
    AuthRejected,
    /// A 403 that is a plain permission failure, not a rate limit and not
    /// an auth error.
    PermissionDenied,
    /// 404 (also what GitHub says for a private repo the token cannot see).
    NotFound,
    /// Refused, unresolvable, timed out or cut off (G6): not an auth or
    /// rate-limit problem.
    Unreachable(UnreachableCause),
    /// 5xx: transient.
    Server { status: u16 },
    /// A redirect (E13). Never followed, and no `Authorization` header goes
    /// to `location`.
    Moved { location: String },
    /// Any other status the adapter has no meaning for (400, 410, 422, ...).
    Unexpected { status: u16 },
    /// A 2xx answer whose body was not what the endpoint documents.
    BadResponse(&'static str),
    /// A caller-supplied name or id that cannot be put in a request path
    /// safely. Nothing was sent.
    InvalidInput(&'static str),
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteError::RateLimited { retry_at } => {
                let secs = retry_at
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                write!(f, "rate limited; retry at unix time {secs}")
            }
            RemoteError::AuthRejected => f.write_str("authentication rejected or missing"),
            RemoteError::PermissionDenied => f.write_str("permission denied"),
            RemoteError::NotFound => f.write_str("not found"),
            RemoteError::Unreachable(cause) => write!(f, "unreachable: {}", cause.as_str()),
            RemoteError::Server { status } => write!(f, "server error {status}"),
            RemoteError::Moved { location } => write!(f, "moved to {location}"),
            RemoteError::Unexpected { status } => write!(f, "unexpected status {status}"),
            RemoteError::BadResponse(what) => write!(f, "bad response: {what}"),
            RemoteError::InvalidInput(what) => write!(f, "invalid input: {what}"),
        }
    }
}

impl std::error::Error for RemoteError {}

/// The answer of a conditional request (`If-None-Match`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conditional<T> {
    /// 304: nothing changed since the ETag the caller sent.
    NotModified,
    /// 200: the new value.
    Modified(T),
}
