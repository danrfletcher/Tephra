//! Bearer-token authentication (P2/F5): every RPC method call must
//! carry `Authorization: Bearer <token>` matching the current
//! contents of the token file byte-for-byte (after trimming the
//! trailing whitespace the file is written with), re-read from disk
//! on every single call so a rotated token takes effect on the very
//! next request (E11) — there is no in-memory cache of the token
//! value anywhere in this module.
//!
//! [`Authenticated`] is a zero-sized proof type with no public
//! constructor outside [`authenticate`]: [`crate::rpc::dispatch::dispatch`]
//! requires one by value (actually, by reference — see that
//! function), so there is exactly one code path in this crate that can
//! ever reach a JSON-RPC method handler, and it has always already
//! run this check. This is a compile-time guarantee, not a
//! per-method opt-in (F5-adjacent) — the doctest below demonstrates
//! that constructing one from outside this module fails to compile.
//!
//! ```compile_fail
//! // Private tuple field: this does not compile outside `rpc::auth`.
//! let _ = tephrad::rpc::auth::Authenticated(());
//! ```

use std::fmt;
use std::path::Path;

use axum::http::HeaderMap;
use subtle::ConstantTimeEq;

const BEARER_PREFIX: &str = "Bearer ";

/// Fixed comparison width, in bytes of token content (beyond the
/// 8-byte big-endian length prefix — see [`fixed_width_buf`]), that
/// [`constant_time_token_eq`] always compares regardless of either
/// input's real length. Real tokens are exactly 64 hex characters
/// (`token::TOKEN_HEX_LEN`), so this comfortably covers every
/// legitimate comparison while keeping the fixed buffer small; an
/// `Authorization` header longer than this (or longer than the HTTP
/// server's own header-size limit, which is far smaller) simply has
/// its excess bytes ignored by the fixed-width encoding, but the
/// embedded length prefix still ensures it can never collide with a
/// shorter candidate that happens to share the first `CMP_WIDTH`
/// bytes.
const CMP_WIDTH: usize = 128;

/// Zero-sized proof that [`authenticate`] accepted the current
/// request's credentials. See the module doc comment.
///
/// `Debug` is derived purely so `Result<Authenticated, _>::unwrap_err()`
/// is usable in tests; it reveals nothing (the type carries no
/// fields) and is never used to leak any credential. `Clone`/`Copy`
/// are likewise free (the type is zero-sized) and let a proof move
/// into a `tokio::task::spawn_blocking` closure by value (R6) without
/// weakening the "only `authenticate`/`verify_bearer` can construct
/// one" guarantee — both constructors stay private to this module.
#[derive(Debug, Clone, Copy)]
pub struct Authenticated(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// No `Authorization` header at all, or one that isn't in
    /// `Bearer <token>` form.
    Missing,
    /// A bearer credential was supplied but it does not match the
    /// token file's current contents.
    Mismatch,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Missing => write!(f, "missing or malformed Authorization header"),
            AuthError::Mismatch => write!(f, "invalid bearer token"),
        }
    }
}

impl std::error::Error for AuthError {}

/// Extracts the bearer credential from `headers`, or `None` if
/// absent, not valid UTF-8, or not in `Authorization: Bearer <token>`
/// form.
fn extract_bearer(headers: &HeaderMap) -> Option<&str> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    raw.strip_prefix(BEARER_PREFIX)
}

/// Builds a fixed-width comparison buffer for `s`: an 8-byte
/// big-endian length prefix (so two different-length inputs can never
/// collide just because one happens to be a byte-for-byte prefix of
/// the other), followed by up to [`CMP_WIDTH`] bytes of `s` itself,
/// zero-padded. Every call processes exactly the same number of
/// bytes regardless of how long or short `s` actually is — the
/// property E7 requires: comparison cost must not leak the real
/// token's length or any prefix of it through timing.
fn fixed_width_buf(s: &str) -> [u8; CMP_WIDTH + 8] {
    let mut buf = [0u8; CMP_WIDTH + 8];
    buf[..8].copy_from_slice(&(s.len() as u64).to_be_bytes());
    let bytes = s.as_bytes();
    let n = bytes.len().min(CMP_WIDTH);
    buf[8..8 + n].copy_from_slice(&bytes[..n]);
    buf
}

/// Constant-time (with respect to both inputs' length and content)
/// comparison of two token candidates, per E7. Exposed separately
/// from [`authenticate`] so `tests/unit/rpc_auth.rs` can exercise the
/// comparison itself without an HTTP request or a real token file on
/// disk.
pub fn constant_time_token_eq(a: &str, b: &str) -> bool {
    let fa = fixed_width_buf(a);
    let fb = fixed_width_buf(b);
    fa.ct_eq(&fb).into()
}

/// Extracts the bearer credential from `headers`, as an owned
/// `String`, without checking it against the token file at all.
///
/// Exposed (R2 fix) so a WebSocket handshake can retain the raw
/// credential presented at upgrade time and re-run [`verify_bearer`]
/// against it before dispatching every later message on that same
/// connection — there is no per-frame `Authorization` header in the
/// WebSocket protocol to re-extract from, so the connection's own
/// handshake-time value must be the thing re-checked.
pub fn bearer_from_headers(headers: &HeaderMap) -> Option<String> {
    extract_bearer(headers).map(str::to_string)
}

/// Checks `provided` against the token file at `token_path`'s current
/// contents, fresh from disk on every call (no cache — E11). Any I/O
/// error reading the token file (missing, permissions, mid-rotation
/// race, ...) is treated the same as a token mismatch, never as "no
/// auth required": there is no fail-open path here. Likewise, an
/// empty or all-whitespace token file (which should never happen in
/// practice — `token::ensure_token` always writes a valid 64-hex-char
/// value — but is not this module's job to assume) can never be
/// matched by an empty bearer credential.
///
/// The lower-level half of [`authenticate`], factored out so a
/// WebSocket connection can re-run exactly this check per message
/// (R2) against the bearer value it retained at handshake time,
/// without needing a fresh `HeaderMap` (which doesn't exist per-frame
/// on an already-open socket).
pub fn verify_bearer(provided: &str, token_path: &Path) -> Result<Authenticated, AuthError> {
    let expected_raw = std::fs::read_to_string(token_path).unwrap_or_default();
    let expected = expected_raw.trim();

    if expected.is_empty() {
        return Err(AuthError::Mismatch);
    }
    if constant_time_token_eq(provided, expected) {
        Ok(Authenticated(()))
    } else {
        Err(AuthError::Mismatch)
    }
}

/// Reads the token file at `token_path` fresh on every call (no
/// cache — E11) and checks `headers` against it. See [`verify_bearer`]
/// for the comparison's own semantics; this adds only the
/// header-extraction step on top of it.
pub fn authenticate(headers: &HeaderMap, token_path: &Path) -> Result<Authenticated, AuthError> {
    let provided = extract_bearer(headers).ok_or(AuthError::Missing)?;
    verify_bearer(provided, token_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_width_buf_distinguishes_lengths_even_with_shared_prefix() {
        // "ab" and "ab\0" would collide under a naive fixed-size
        // zero-padded compare that didn't also encode the real
        // length; the length prefix must keep them distinct.
        let a = fixed_width_buf("ab");
        let b = fixed_width_buf("ab\0");
        assert_ne!(a, b);
    }

    #[test]
    fn constant_time_token_eq_matches_identical_strings() {
        assert!(constant_time_token_eq("a".repeat(64).as_str(), "a".repeat(64).as_str()));
    }

    #[test]
    fn constant_time_token_eq_rejects_different_strings_of_equal_length() {
        assert!(!constant_time_token_eq(
            &"a".repeat(64),
            &format!("{}{}", "a".repeat(63), "b")
        ));
    }

    #[test]
    fn constant_time_token_eq_rejects_different_lengths() {
        assert!(!constant_time_token_eq("short", "short-but-longer"));
    }

    #[test]
    fn constant_time_token_eq_rejects_empty_against_nonempty() {
        assert!(!constant_time_token_eq("", "a".repeat(64).as_str()));
    }

    #[test]
    fn constant_time_token_eq_treats_empty_against_empty_as_equal() {
        // The comparison itself is symmetric and length-aware only;
        // `authenticate` is the layer that separately refuses an
        // empty *expected* token outright (see its own tests), so
        // this low-level helper is intentionally permissive here.
        assert!(constant_time_token_eq("", ""));
    }
}
