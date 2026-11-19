//! The rule for the `github_api_url` and `drive_api_url` config keys: an
//! `https://` URL, or an `http://` URL only when its host is a loopback
//! one, so a token never goes over cleartext to another machine.
//!
//! A small hand parser rather than a URL crate: the daemon needs only
//! the scheme and the host, and a parser that is stricter than a URL
//! library errs on the safe side. It refuses:
//!
//! - whitespace, control characters and backslashes anywhere;
//! - user info (`user@host`, `http://127.0.0.1@evil.example/`): the
//!   host a reader sees first is not always the host that is used;
//! - an empty host, a host with characters outside letters, digits,
//!   `.`, `-` and `_`, and a malformed or out-of-range port;
//! - any scheme but `http` and `https`.
//!
//! Loopback hosts are `localhost` (exactly), an IPv4 address in
//! 127.0.0.0/8 written as four decimal parts, and `[::1]`. A host such
//! as `127.0.0.1.example.com` or the decimal `2130706433` is not one.

use std::fmt;
use std::net::{Ipv4Addr, Ipv6Addr};

/// Why a URL was refused. [`fmt::Display`] names no config key: the
/// caller does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiUrlError {
    Empty,
    /// Whitespace, a control character or a backslash.
    BadCharacter,
    /// No `scheme://`, or a scheme other than http and https.
    BadScheme,
    /// `user@` or `user:password@` before the host.
    UserInfo,
    BadHost,
    BadPort,
    /// `http://` with a host that is not loopback. Carries no text: the
    /// message goes to stderr and the log, and the host is not echoed.
    CleartextToRemoteHost,
}

impl fmt::Display for ApiUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiUrlError::Empty => f.write_str("is empty; expected an https:// URL"),
            ApiUrlError::BadCharacter => {
                f.write_str("contains whitespace, a control character or a backslash")
            }
            ApiUrlError::BadScheme => f.write_str(
                "must start with https://, or with http:// for a loopback host (localhost, 127.0.0.1, [::1])",
            ),
            ApiUrlError::UserInfo => f.write_str("must not contain a user name or password"),
            ApiUrlError::BadHost => f.write_str("has no valid host"),
            ApiUrlError::BadPort => f.write_str("has an invalid port"),
            ApiUrlError::CleartextToRemoteHost => f.write_str(
                "uses http:// with a host that is not loopback; a token must not go over cleartext to another host, so use https://",
            ),
        }
    }
}

impl std::error::Error for ApiUrlError {}

/// Whether `host` (without brackets) is a loopback host.
fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Ok(v4) = host.parse::<Ipv4Addr>() {
        return v4.is_loopback();
    }
    if let Ok(v6) = host.parse::<Ipv6Addr>() {
        return v6.is_loopback();
    }
    false
}

/// Checks `url` against the rule in the module docs.
pub fn validate_api_url(url: &str) -> Result<(), ApiUrlError> {
    if url.is_empty() {
        return Err(ApiUrlError::Empty);
    }
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return Err(ApiUrlError::BadCharacter);
    }
    let (scheme, rest) = url.split_once("://").ok_or(ApiUrlError::BadScheme)?;
    let https = scheme.eq_ignore_ascii_case("https");
    if !https && !scheme.eq_ignore_ascii_case("http") {
        return Err(ApiUrlError::BadScheme);
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.contains('@') {
        return Err(ApiUrlError::UserInfo);
    }

    let (host, port) = if let Some(after_bracket) = authority.strip_prefix('[') {
        let (inside, tail) = after_bracket.split_once(']').ok_or(ApiUrlError::BadHost)?;
        if inside.parse::<Ipv6Addr>().is_err() {
            return Err(ApiUrlError::BadHost);
        }
        let port = match tail {
            "" => None,
            t => Some(t.strip_prefix(':').ok_or(ApiUrlError::BadHost)?),
        };
        (inside, port)
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        }
    };
    if let Some(port) = port {
        if port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port.parse::<u16>().is_err()
        {
            return Err(ApiUrlError::BadPort);
        }
    }
    let bracketed = authority.starts_with('[');
    if host.is_empty()
        || (!bracketed
            && !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_')))
    {
        return Err(ApiUrlError::BadHost);
    }

    if https || is_loopback_host(host) {
        Ok(())
    } else {
        Err(ApiUrlError::CleartextToRemoteHost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn https_urls_and_loopback_http_urls_are_accepted() {
        for url in [
            "https://api.github.com",
            "https://api.github.com/",
            "HTTPS://api.github.com/v3",
            "https://www.googleapis.com/drive/v3?x=1#f",
            "https://example.com:8443/base",
            "https://[2001:db8::1]:443/",
            "http://127.0.0.1:8080",
            "http://127.0.0.1",
            "http://127.1.2.3:9/",
            "http://localhost:9000/api",
            "http://LOCALHOST",
            "http://[::1]:8080/x",
            "https://127.0.0.1:1",
            "https://localhost",
        ] {
            assert_eq!(validate_api_url(url), Ok(()), "{url}");
        }
    }

    #[test]
    fn cleartext_to_another_host_is_refused() {
        for url in [
            "http://api.github.com",
            "http://example.com:80/",
            "http://0.0.0.0:8080",
            "http://10.0.0.5",
            "http://192.168.1.2:3000",
            "http://127.0.0.1.example.com",
            "http://localhost.example.com",
            "http://2130706433",
            "http://[::]:80",
            "http://[fe80::1]",
            "http://[::ffff:127.0.0.1]",
        ] {
            assert_eq!(
                validate_api_url(url),
                Err(ApiUrlError::CleartextToRemoteHost),
                "{url}"
            );
        }
    }

    #[test]
    fn other_malformed_urls_are_refused() {
        let cases: &[(&str, ApiUrlError)] = &[
            ("", ApiUrlError::Empty),
            ("api.github.com", ApiUrlError::BadScheme),
            ("//api.github.com", ApiUrlError::BadScheme),
            ("ftp://example.com", ApiUrlError::BadScheme),
            ("file:///etc/passwd", ApiUrlError::BadScheme),
            ("ws://127.0.0.1", ApiUrlError::BadScheme),
            ("https:/example.com", ApiUrlError::BadScheme),
            ("https://", ApiUrlError::BadHost),
            ("https:///path", ApiUrlError::BadHost),
            ("https://:443", ApiUrlError::BadHost),
            ("https://exa mple.com", ApiUrlError::BadCharacter),
            (" https://example.com", ApiUrlError::BadCharacter),
            ("https://example.com\n", ApiUrlError::BadCharacter),
            ("https://example.com\\@evil.com", ApiUrlError::BadCharacter),
            ("http://127.0.0.1@evil.example/", ApiUrlError::UserInfo),
            ("https://user:pw@example.com", ApiUrlError::UserInfo),
            ("https://exa%6dple.com", ApiUrlError::BadHost),
            ("https://exa*mple.com", ApiUrlError::BadHost),
            ("https://example.com:", ApiUrlError::BadPort),
            ("https://example.com:http", ApiUrlError::BadPort),
            ("https://example.com:65536", ApiUrlError::BadPort),
            ("https://example.com:-1", ApiUrlError::BadPort),
            ("https://[::1", ApiUrlError::BadHost),
            ("https://[nope]", ApiUrlError::BadHost),
            ("https://[::1]x", ApiUrlError::BadHost),
            ("https://::1", ApiUrlError::BadHost),
        ];
        for (url, want) in cases {
            assert_eq!(validate_api_url(url).as_ref(), Err(want), "{url:?}");
        }
    }

    #[test]
    fn the_message_names_the_rule_not_a_key() {
        let msg = validate_api_url("http://example.com")
            .unwrap_err()
            .to_string();
        assert!(msg.contains("https://"), "{msg}");
    }

    /// No refusal message carries any part of the URL: a token typed
    /// into a host, a port, user info or a query must not reach stderr.
    #[test]
    fn no_message_echoes_the_url() {
        for url in [
            "http://ghp_SENTINEL_do_not_leak.evil.com/",
            "https://x:ghp_SENTINEL_do_not_leak@example.com",
            "https://example.com:ghp_SENTINEL_do_not_leak",
            "ftp://ghp_SENTINEL_do_not_leak",
            "https://ghp_SENTINEL_do_not_leak***",
            "https://ghp_SENTINEL_do_not_leak*.com",
            "ghp_SENTINEL_do_not_leak",
        ] {
            let msg = validate_api_url(url).unwrap_err().to_string();
            assert!(!msg.contains("ghp_SENTINEL"), "{url}: {msg}");
        }
    }
}
