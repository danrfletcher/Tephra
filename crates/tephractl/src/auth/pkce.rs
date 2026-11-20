//! PKCE (RFC 7636) for `auth google`: a fresh code verifier per run, its
//! S256 challenge, and the random `state` that ties a callback to the
//! run that asked for it.
//!
//! Randomness is the operating system's (`OsRng`). Everything here is
//! base64url with no `=` padding.

use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

/// RFC 7636 section 4.1: a verifier is 43 to 128 characters.
pub const MIN_VERIFIER_LEN: usize = 43;
pub const MAX_VERIFIER_LEN: usize = 128;

/// Random bytes behind a verifier: 48 bytes is 64 characters.
const VERIFIER_BYTES: usize = 48;
/// Random bytes behind a `state`: 32 bytes is 43 characters.
const STATE_BYTES: usize = 32;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding.
pub fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..chunk.len() + 1 {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

fn random_text(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buf);
    let text = base64url(&buf);
    buf.iter_mut().for_each(|b| *b = 0);
    text
}

/// A new code verifier: 64 unreserved characters.
pub fn new_verifier() -> String {
    random_text(VERIFIER_BYTES)
}

/// The S256 code challenge of `verifier`:
/// `BASE64URL(SHA256(ASCII(verifier)))`.
pub fn challenge_for(verifier: &str) -> String {
    base64url(&Sha256::digest(verifier.as_bytes()))
}

/// A new random `state` value.
pub fn new_state() -> String {
    random_text(STATE_BYTES)
}

/// Whether `a` and `b` are equal, taking the same time whatever the
/// position of the first difference (the `state` check on a callback).
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        diff |= usize::from(x ^ y);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// RFC 7636 appendix B.
    #[test]
    fn rfc7636_appendix_b_vector() {
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn rfc7636_appendix_b_octets_encode_to_the_appendix_verifier() {
        let octets = [
            116u8, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212,
            37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        ];
        assert_eq!(
            base64url(&octets),
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        );
    }

    #[test]
    fn base64url_has_no_padding_and_uses_the_url_alphabet() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn verifiers_are_within_the_rfc_length_bounds_and_unreserved() {
        for _ in 0..200 {
            let v = new_verifier();
            assert!(
                (MIN_VERIFIER_LEN..=MAX_VERIFIER_LEN).contains(&v.len()),
                "{}",
                v.len()
            );
            assert!(v
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        }
    }

    #[test]
    fn nothing_here_has_padding() {
        for _ in 0..50 {
            assert!(!new_verifier().contains('='));
            assert!(!new_state().contains('='));
            assert!(!challenge_for(&new_verifier()).contains('='));
        }
    }

    #[test]
    fn challenge_is_43_characters() {
        assert_eq!(challenge_for(&new_verifier()).len(), 43);
    }

    #[test]
    fn verifiers_and_states_differ_on_every_call() {
        let verifiers: HashSet<String> = (0..500).map(|_| new_verifier()).collect();
        let states: HashSet<String> = (0..500).map(|_| new_state()).collect();
        assert_eq!(verifiers.len(), 500);
        assert_eq!(states.len(), 500);
        assert!(verifiers.is_disjoint(&states));
    }

    #[test]
    fn constant_time_eq_agrees_with_eq() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(constant_time_eq("", ""));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("ab", "abc"));
        assert!(!constant_time_eq("", "a"));
    }
}
