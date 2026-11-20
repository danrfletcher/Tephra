//! The API base URL of a real adapter, from the `github_api_url` and
//! `drive_api_url` config keys.
//!
//! - `None` gives the provider default.
//! - Trailing slashes are removed, so `https://api.github.com/` and
//!   `https://api.github.com` are the same base.
//! - Any base other than the provider default must have a loopback host
//!   (`127.0.0.1`, `::1`, `localhost`); anything else is refused here, at
//!   construction, so a token can never be sent to an arbitrary host.
//!   (The config key is wider on purpose: it also accepts any `https://`
//!   URL, because PR-1 owns that rule. The adapter is stricter.)

use std::fmt;

use crate::api_url::{is_loopback_url, validate_api_url, ApiUrlError};

pub const GITHUB_DEFAULT_BASE: &str = "https://api.github.com";
pub const DRIVE_DEFAULT_BASE: &str = "https://www.googleapis.com/drive/v3";

/// Why a base URL was refused. Carries no part of the URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseUrlError {
    /// Not a well-formed `http(s)://host[:port][/path]` URL.
    Malformed(ApiUrlError),
    /// A query string or fragment: request paths are appended to the base.
    QueryOrFragment,
    /// Well formed, not the default, and the host is not loopback.
    NotLoopback,
}

impl fmt::Display for BaseUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BaseUrlError::Malformed(e) => write!(f, "API base URL {e}"),
            BaseUrlError::QueryOrFragment => {
                f.write_str("API base URL must not contain a query string or a fragment")
            }
            BaseUrlError::NotLoopback => f.write_str(
                "API base URL must be the provider default or point at this machine \
                 (127.0.0.1, ::1 or localhost); a token is never sent to another host",
            ),
        }
    }
}

impl std::error::Error for BaseUrlError {}

/// The base URL to use: `configured` normalised, or `default`.
pub fn resolve_base_url(configured: Option<&str>, default: &str) -> Result<String, BaseUrlError> {
    let Some(raw) = configured else {
        return Ok(default.to_string());
    };
    validate_api_url(raw).map_err(BaseUrlError::Malformed)?;
    if raw.contains(['?', '#']) {
        return Err(BaseUrlError::QueryOrFragment);
    }
    let normalised = raw.trim_end_matches('/').to_string();
    if normalised.eq_ignore_ascii_case(default) {
        return Ok(default.to_string());
    }
    if is_loopback_url(&normalised) {
        Ok(normalised)
    } else {
        Err(BaseUrlError::NotLoopback)
    }
}
