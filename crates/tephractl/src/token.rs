//! Bearer-token loading (P2/F5): `tephractl` reads the local token
//! file fresh on every invocation — there is no caching, no daemon
//! state in this crate, and each process run opens its own fresh
//! connection (one of this PR's edge cases).
//!
//! This module only loads the token's *client-side* contents; whether
//! the daemon actually accepts it is a transport-layer concern
//! (`crate::transport::RpcCallError::Unauthorized`, surfaced as
//! "authentication failed", distinct from this module's "no token
//! file at all" case).

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub enum TokenError {
    /// No file at all at the expected path (E "token file missing").
    Missing(PathBuf),
    /// The file exists but could not be read for some other reason
    /// (permissions, I/O error, ...).
    Io(PathBuf, io::Error),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Missing(path) => write!(
                f,
                "no auth token found at {} (is tephrad running? starting it once writes this file)",
                path.display()
            ),
            TokenError::Io(path, e) => {
                write!(f, "failed to read token file {}: {e}", path.display())
            }
        }
    }
}

impl std::error::Error for TokenError {}

/// Loads the bearer token from `path`, trimmed of surrounding
/// whitespace. A missing file is reported distinctly from any other
/// I/O failure, so callers can print the single, clear "no token
/// file" message the spec requires rather than a generic I/O error.
pub fn load_token(path: &Path) -> Result<String, TokenError> {
    match fs::read_to_string(path) {
        Ok(raw) => Ok(raw.trim().to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(TokenError::Missing(path.to_path_buf()))
        }
        Err(e) => Err(TokenError::Io(path.to_path_buf(), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn loads_and_trims_a_present_token_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "abc123\n").unwrap();

        let token = load_token(&path).expect("token should load");
        assert_eq!(token, "abc123");
    }

    #[test]
    fn missing_token_file_is_a_distinct_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token");

        let err = load_token(&path).expect_err("must error on missing file");
        assert!(matches!(err, TokenError::Missing(p) if p == path));
    }

    #[test]
    fn missing_token_error_message_is_clear() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("token");
        let err = load_token(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("no auth token found"));
        assert!(message.contains(&path.display().to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_token_file_is_an_io_error_not_missing() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("token");
        fs::write(&path, "abc123").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        let result = load_token(&path);
        // Running as root (e.g. inside some CI containers) bypasses
        // file permission checks entirely, so only assert the
        // distinction when this environment actually enforces them.
        if let Err(err) = result {
            assert!(matches!(err, TokenError::Io(..)));
        }

        // Always restore permissions so the tempdir can be cleaned up.
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
}
