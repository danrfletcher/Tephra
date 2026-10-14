//! Stable content hashing.
//!
//! The hash is a pure function of `content` alone (never of `path`),
//! so byte-identical content from two different paths hashes
//! identically and the same path re-parsed after an unrelated
//! metadata-only change (e.g. mtime) still hashes the same — exactly
//! the property PR-4's watcher needs to skip re-parsing unchanged
//! files.

use sha2::{Digest, Sha256};

pub(crate) fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}
