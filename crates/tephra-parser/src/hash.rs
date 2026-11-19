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
    content_hash_bytes(content.as_bytes())
}

/// Lower-case hex SHA-256 of `bytes`: the same digest [`content_hash`]
/// gives for a UTF-8 string with those bytes. For a file that is not
/// (or not yet known to be) text, such as a binary or a non-UTF-8 file,
/// where there is no `&str` to hash. `tephrad` stores this in
/// `units.hash`, so the stored hash of a UTF-8 file always equals
/// `ParsedUnit::content_hash`.
pub fn content_hash_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    const HELLO: &str = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";

    #[test]
    fn the_str_and_bytes_hashes_are_one_function() {
        // `tephrad` stores `content_hash_bytes` of a file's bytes in
        // `units.hash`; for a UTF-8 file that must be exactly what the
        // parser reports as `content_hash`, or the two drift.
        for text in ["", "hello\n", "café — naïve 日本語\n", "# T\n\n[[x]] ^a1\n"] {
            assert_eq!(
                content_hash(text),
                content_hash_bytes(text.as_bytes()),
                "{text:?}"
            );
        }
        assert_eq!(content_hash(""), EMPTY);
        assert_eq!(content_hash("hello\n"), HELLO);
    }

    #[test]
    fn bytes_that_are_not_utf8_hash_without_any_conversion() {
        let raw = b"caf\xe9 \xff\xfe not utf8 \xc3\x28\n";
        assert_eq!(
            content_hash_bytes(raw),
            "a131fcc2db92cecc24df160d5df699d65b27222aa90a80f7632e36477f6388ed"
        );
        // A lossy conversion would hash differently.
        assert_ne!(
            content_hash(&String::from_utf8_lossy(raw)),
            content_hash_bytes(raw)
        );
    }
}
