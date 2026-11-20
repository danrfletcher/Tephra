//! Rate-limit rules (E1) as pure functions plus the small state an
//! adapter keeps. The adapter never sleeps: it records a `retry_at`, and
//! while that is in the future it answers `RateLimited` itself without
//! sending a request.
//!
//! Classification, for a 403 or 429 answer (any other status is not a
//! rate limit):
//!
//! GitHub
//! 1. `Retry-After` with a number of seconds (or an HTTP date): retry then.
//! 2. `x-ratelimit-remaining: 0` with a numeric `x-ratelimit-reset` (unix
//!    seconds): retry at the reset time.
//! 3. `x-ratelimit-remaining: 0` without a usable reset, or a body naming
//!    a "secondary rate limit" or "abuse detection": a rate limit with no
//!    usable header, so the fallback applies.
//! 4. Any other 429: the same.
//! 5. Any other 403 (for example "Resource not accessible by
//!    integration"): `PermissionDenied`.
//!
//! Drive
//! - 429: a rate limit; `Retry-After` when present.
//! - 403 whose JSON error has the reason `rateLimitExceeded`,
//!   `userRateLimitExceeded` or `sharingRateLimitExceeded`: the same.
//! - any other 403 (`insufficientPermissions`, ...): `PermissionDenied`.
//!
//! Delays: a usable header gives a delay, never below [`MIN_DELAY`] (a
//! reset in the past or `Retry-After: 0` is one second, not an instant
//! retry) and never above [`MAX_HONOURED`] (so an absurd header cannot
//! overflow a time or park a source for years). A header that is absent,
//! negative, non-numeric or not UTF-8 is not usable: the fallback applies,
//! which doubles from 1 s up to 60 s and starts over after any success.

use std::time::{Duration, SystemTime};

use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::Deserialize;

/// The shortest wait after a rate limit.
pub const MIN_DELAY: Duration = Duration::from_secs(1);
/// The longest wait the header-less fallback reaches.
pub const FALLBACK_CAP: Duration = Duration::from_secs(60);
/// The longest wait a header can ask for (7 days). 86400 s is honoured.
pub const MAX_HONOURED: Duration = Duration::from_secs(7 * 24 * 3600);

/// Which provider's rules to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Api {
    Github,
    Drive,
}

/// What a 403 or 429 answer means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// A rate limit. `Some(delay)` is what the headers asked for
    /// (unclamped); `None` means no usable header.
    RateLimited { delay: Option<Duration> },
    /// A plain permission failure.
    PermissionDenied,
    /// Not a rate limit and not a 403: the caller maps the status itself.
    NotRateLimit,
}

/// A usable `Retry-After`: whole seconds, or an HTTP date (as a delay from
/// `now`, zero when already past). Anything else is `None`.
fn retry_after(headers: &HeaderMap, now: SystemTime) -> Option<Duration> {
    let text = headers.get(RETRY_AFTER)?.to_str().ok()?.trim();
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return text.parse::<u64>().ok().map(Duration::from_secs);
    }
    // An HTTP date. Text with a sign or letters other than a date is not
    // usable; `parse_http_date` rejects it.
    let at = httpdate::parse_http_date(text).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok().map(str::trim)
}

fn github_body_names_a_limit(body: &[u8]) -> bool {
    let text = String::from_utf8_lossy(body).to_ascii_lowercase();
    text.contains("secondary rate limit")
        || text.contains("abuse detection")
        || text.contains("rate limit exceeded")
}

fn classify_github(status: u16, headers: &HeaderMap, body: &[u8], now: SystemTime) -> Verdict {
    if let Some(delay) = retry_after(headers, now) {
        return Verdict::RateLimited { delay: Some(delay) };
    }
    if header_text(headers, "x-ratelimit-remaining") == Some("0") {
        let reset = header_text(headers, "x-ratelimit-reset")
            .filter(|t| !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|t| t.parse::<u64>().ok());
        let delay = reset.map(|secs| {
            // A reset beyond what SystemTime can hold is clamped by
            // MAX_HONOURED later; saturate rather than overflow here.
            let at = SystemTime::UNIX_EPOCH
                .checked_add(Duration::from_secs(secs))
                .unwrap_or(now + MAX_HONOURED);
            at.duration_since(now).unwrap_or(Duration::ZERO)
        });
        return Verdict::RateLimited { delay };
    }
    if github_body_names_a_limit(body) || status == 429 {
        return Verdict::RateLimited { delay: None };
    }
    Verdict::PermissionDenied
}

const DRIVE_RATE_REASONS: [&str; 3] = [
    "rateLimitExceeded",
    "userRateLimitExceeded",
    "sharingRateLimitExceeded",
];

#[derive(Deserialize)]
struct DriveErrorEnvelope {
    error: Option<DriveErrorBody>,
}

#[derive(Deserialize)]
struct DriveErrorBody {
    #[serde(default)]
    errors: Vec<DriveErrorItem>,
}

#[derive(Deserialize)]
struct DriveErrorItem {
    reason: Option<String>,
}

fn drive_reason_is_rate_limit(body: &[u8]) -> bool {
    let Ok(env) = serde_json::from_slice::<DriveErrorEnvelope>(body) else {
        return false;
    };
    env.error.is_some_and(|e| {
        e.errors.iter().any(|item| {
            item.reason
                .as_deref()
                .is_some_and(|r| DRIVE_RATE_REASONS.contains(&r))
        })
    })
}

fn classify_drive(status: u16, headers: &HeaderMap, body: &[u8], now: SystemTime) -> Verdict {
    if status == 429 || drive_reason_is_rate_limit(body) {
        return Verdict::RateLimited {
            delay: retry_after(headers, now),
        };
    }
    Verdict::PermissionDenied
}

/// Classifies a 403 or 429 answer. Other statuses are
/// [`Verdict::NotRateLimit`].
pub fn classify(
    api: Api,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
    now: SystemTime,
) -> Verdict {
    if status != 403 && status != 429 {
        return Verdict::NotRateLimit;
    }
    match api {
        Api::Github => classify_github(status, headers, body, now),
        Api::Drive => classify_drive(status, headers, body, now),
    }
}

/// The header-less fallback: 1 s, 2 s, 4 s ... capped at 60 s, starting
/// over after a success.
#[derive(Debug, Default, Clone)]
pub struct Backoff {
    fallbacks: u32,
}

impl Backoff {
    /// The wait after one more rate-limited answer. A header-given delay
    /// is clamped to `[MIN_DELAY, MAX_HONOURED]` and leaves the doubling
    /// where it was; `None` takes the next fallback step.
    pub fn next_delay(&mut self, header_delay: Option<Duration>) -> Duration {
        match header_delay {
            Some(d) => d.clamp(MIN_DELAY, MAX_HONOURED),
            None => {
                let shift = self.fallbacks.min(6);
                self.fallbacks = self.fallbacks.saturating_add(1);
                Duration::from_secs(1u64 << shift).min(FALLBACK_CAP)
            }
        }
    }

    /// Any success starts the doubling over.
    pub fn reset(&mut self) {
        self.fallbacks = 0;
    }
}

/// What an adapter remembers between calls: until when it must not send.
#[derive(Debug, Default, Clone)]
pub struct RateGate {
    retry_at: Option<SystemTime>,
    backoff: Backoff,
}

impl RateGate {
    /// `Err(retry_at)` while `now` is before the recorded retry time; the
    /// first call at or after it is `Ok`.
    pub fn check(&self, now: SystemTime) -> Result<(), SystemTime> {
        match self.retry_at {
            Some(at) if now < at => Err(at),
            _ => Ok(()),
        }
    }

    /// Records a rate-limited answer and returns the new `retry_at`.
    pub fn record_limited(
        &mut self,
        now: SystemTime,
        header_delay: Option<Duration>,
    ) -> SystemTime {
        let delay = self.backoff.next_delay(header_delay);
        let at = now.checked_add(delay).unwrap_or(now + MAX_HONOURED);
        self.retry_at = Some(at);
        at
    }

    /// Any successful answer clears the limit and the doubling.
    pub fn record_success(&mut self) {
        self.retry_at = None;
        self.backoff.reset();
    }
}
