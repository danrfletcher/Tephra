//! Auth token generation and storage.
//!
//! The token is a 256-bit value from an OS CSPRNG, hex-encoded, stored at
//! `<data_dir>/token` with mode `0600`. It is never regenerated once it
//! exists; permissions are re-checked and repaired on every startup.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rand::rngs::OsRng;
use rand::RngCore;

const TOKEN_FILE_NAME: &str = "token";
const TOKEN_MODE: u32 = 0o600;
const DATA_DIR_MODE: u32 = 0o700;

#[derive(Debug)]
pub enum TokenError {
    Io(io::Error),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for TokenError {}

impl From<io::Error> for TokenError {
    fn from(e: io::Error) -> Self {
        TokenError::Io(e)
    }
}

/// Outcome of [`ensure_token`]: whether a fresh token was generated, and
/// whether loose permissions were repaired on an existing file.
pub struct EnsureOutcome {
    // Consumed by unit tests today, and by the RPC authentication layer
    // added in a later PR; not read in PR-1's main.rs since the token is
    // never logged and not yet checked against incoming requests.
    #[allow(dead_code)]
    pub token: String,
    pub generated: bool,
    pub permissions_repaired: bool,
}

/// Ensures `data_dir` exists (non-world-writable) and that a token file
/// exists inside it with mode `0600`, generating one from an OS CSPRNG if
/// absent. On an existing token file, permissions are checked and
/// tightened back to `0600` if found looser; the token value itself is
/// read and reused unchanged — never regenerated.
///
/// The token value is never logged by this function; callers must also
/// avoid logging it.
pub fn ensure_token(data_dir: &Path) -> Result<EnsureOutcome, TokenError> {
    ensure_data_dir(data_dir)?;

    let token_path = data_dir.join(TOKEN_FILE_NAME);

    if token_path.exists() {
        let permissions_repaired = repair_permissions_if_loose(&token_path)?;
        let token = fs::read_to_string(&token_path)?.trim().to_string();
        return Ok(EnsureOutcome {
            token,
            generated: false,
            permissions_repaired,
        });
    }

    let token = generate_token();
    write_token_file(&token_path, &token)?;
    Ok(EnsureOutcome {
        token,
        generated: true,
        permissions_repaired: false,
    })
}

fn ensure_data_dir(data_dir: &Path) -> io::Result<()> {
    let existed = data_dir.exists();
    fs::create_dir_all(data_dir)?;
    if !existed {
        fs::set_permissions(data_dir, fs::Permissions::from_mode(DATA_DIR_MODE))?;
    }
    Ok(())
}

/// Generates a 256-bit token from an OS CSPRNG, hex-encoded to 64 chars.
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_token_file(path: &Path, token: &str) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(TOKEN_MODE);
    let mut file = options.open(path)?;
    use std::io::Write;
    file.write_all(token.as_bytes())?;
    Ok(())
}

/// Returns `true` if the file's permissions were found looser than 0600
/// and have been tightened.
fn repair_permissions_if_loose(path: &Path) -> io::Result<bool> {
    let metadata = fs::metadata(path)?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode != TOKEN_MODE {
        fs::set_permissions(path, fs::Permissions::from_mode(TOKEN_MODE))?;
        return Ok(true);
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn ensure_token_generates_64_hex_char_token() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        let outcome = ensure_token(&data_dir).expect("ensure_token");

        assert!(outcome.generated);
        assert_eq!(outcome.token.len(), 64);
        assert!(outcome.token.chars().all(|c| c.is_ascii_hexdigit()));

        let token_path = data_dir.join("token");
        let mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn ensure_token_is_idempotent_on_second_call() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");

        let first = ensure_token(&data_dir).expect("first call");
        let second = ensure_token(&data_dir).expect("second call");

        assert!(first.generated);
        assert!(!second.generated);
        assert_eq!(first.token, second.token);
    }

    #[test]
    fn ensure_token_repairs_loosened_permissions() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");

        let first = ensure_token(&data_dir).expect("first call");
        let token_path = data_dir.join("token");

        // Loosen the permissions between calls, simulating an external
        // change (e.g. a misconfigured umask on another process).
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o644)).unwrap();

        let second = ensure_token(&data_dir).expect("second call");

        assert!(!second.generated);
        assert!(second.permissions_repaired);
        assert_eq!(first.token, second.token);

        let mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn data_dir_is_created_non_world_writable() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        ensure_token(&data_dir).expect("ensure_token");

        let mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
}
