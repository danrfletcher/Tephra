//! Auth token generation and storage.
//!
//! The token is a 256-bit value from an OS CSPRNG, hex-encoded, stored at
//! `<data_dir>/token` with mode `0600`. It is never regenerated once it
//! exists; permissions are re-checked and repaired on every startup.

use std::fmt;
use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use rand::rngs::OsRng;
use rand::RngCore;

const TOKEN_FILE_NAME: &str = "token";
const TOKEN_MODE: u32 = 0o600;
const DATA_DIR_MODE: u32 = 0o700;
const TOKEN_HEX_LEN: usize = 64;
/// Bits that make a token file "looser than 0600" per F5/C10: any
/// group or world permission (read, write, or execute).
const TOKEN_LOOSE_MASK: u32 = 0o077;
/// Bits that make a directory world/group-writable, the only thing
/// F5/C12 requires `ensure_data_dir` to strip from a pre-existing,
/// user-configured `data_dir`.
const DIR_WRITE_MASK: u32 = 0o022;

#[derive(Debug)]
pub enum TokenError {
    Io(io::Error),
    /// The existing token file's contents are not a 64-character hex
    /// string (e.g. empty, truncated, or corrupted). Rather than reuse
    /// an invalid value — which could let an empty bearer token through
    /// a later auth check — this is treated as a hard startup failure.
    InvalidTokenContents,
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenError::Io(e) => write!(f, "{e}"),
            TokenError::InvalidTokenContents => write!(
                f,
                "token file contents are not a valid {TOKEN_HEX_LEN}-character hex token"
            ),
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
/// read, validated, and reused unchanged — never regenerated. A token
/// file whose contents are not a valid 64-hex-char value is treated as a
/// hard error rather than silently accepted.
///
/// The token value is never logged by this function; callers must also
/// avoid logging it.
pub fn ensure_token(data_dir: &Path) -> Result<EnsureOutcome, TokenError> {
    ensure_data_dir(data_dir)?;

    let token_path = data_dir.join(TOKEN_FILE_NAME);

    if token_path.exists() {
        let permissions_repaired = repair_permissions_if_loose(&token_path)?;
        let raw = fs::read_to_string(&token_path)?;
        let token = validate_token(&raw)?;
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

/// Ensures `data_dir` exists and is non-world-writable, whether or not
/// it already existed before this call. Idempotent and safe to call
/// from multiple startup stages (e.g. both the directory-creation stage
/// in `main.rs` and this module's own token-ensure stage).
///
/// `data_dir` is user-configurable, so the two cases are handled
/// differently:
/// - If it does not exist, it (and any missing parents) is created with
///   mode `0700` directly via `DirBuilder`, rather than `create_dir_all`
///   followed by a separate `chmod`. That avoids a window, under a
///   permissive umask, where the directory briefly exists world- or
///   group-writable before the mode is tightened.
/// - If it already exists, only group/other write bits are stripped
///   (`mode & !0o022`); the directory's other permission bits (e.g. an
///   owner-only `0750`, or extra read/execute bits) are left as the
///   user configured them. A warning is logged when the mode actually
///   changes. Forcing an existing, pre-populated directory to `0700`
///   unconditionally would silently clobber permissions the user set up
///   for reasons unrelated to tephrad.
pub fn ensure_data_dir(data_dir: &Path) -> io::Result<()> {
    if data_dir.exists() {
        let metadata = fs::metadata(data_dir)?;
        let mode = metadata.permissions().mode() & 0o777;
        let tightened = mode & !DIR_WRITE_MASK;
        if tightened != mode {
            fs::set_permissions(data_dir, fs::Permissions::from_mode(tightened))?;
            tracing::warn!(
                path = %data_dir.display(),
                old_mode = format!("{mode:03o}"),
                new_mode = format!("{tightened:03o}"),
                "data directory was group/world-writable; tightened"
            );
        }
        return Ok(());
    }

    fs::DirBuilder::new()
        .recursive(true)
        .mode(DATA_DIR_MODE)
        .create(data_dir)
}

/// Generates a 256-bit token from an OS CSPRNG, hex-encoded to 64 chars.
fn generate_token() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Validates that `raw` (after trimming surrounding whitespace) is
/// exactly [`TOKEN_HEX_LEN`] hex characters, returning the trimmed value.
fn validate_token(raw: &str) -> Result<String, TokenError> {
    let trimmed = raw.trim();
    if trimmed.len() == TOKEN_HEX_LEN && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(trimmed.to_string())
    } else {
        Err(TokenError::InvalidTokenContents)
    }
}

/// Writes `token` to `path` atomically: the value is written to a
/// sibling temp file created with mode `0600` (so the token is never
/// briefly world-readable), fsynced to disk, then renamed into place.
/// `rename` on POSIX filesystems is an atomic replace, so a crash
/// between the write and the rename leaves either the old state (no
/// file) or the new one — never a truncated or empty token file.
fn write_token_file(path: &Path, token: &str) -> io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(TOKEN_FILE_NAME);

    let mut suffix_bytes = [0u8; 8];
    OsRng.fill_bytes(&mut suffix_bytes);
    let suffix: String = suffix_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let tmp_path = parent.join(format!(".{file_name}.tmp-{suffix}"));

    let write_result = (|| -> io::Result<()> {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true).mode(TOKEN_MODE);
        let mut file = options.open(&tmp_path)?;
        use std::io::Write;
        file.write_all(token.as_bytes())?;
        file.sync_all()?;
        Ok(())
    })();

    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }

    Ok(())
}

/// Returns `true` if the file's permissions were found looser than
/// `0600` — any group or world bit set (`mode & 0o077 != 0`) — and have
/// been tightened. A mode that differs from `0600` only in owner bits
/// (e.g. `0400`, already at least as restrictive) is left untouched:
/// C10 requires repairing only modes "looser than 0600 (group/world
/// readable)", not any deviation from the exact value.
fn repair_permissions_if_loose(path: &Path) -> io::Result<bool> {
    let metadata = fs::metadata(path)?;
    let mode = metadata.permissions().mode() & 0o777;
    if mode & TOKEN_LOOSE_MASK != 0 {
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

    #[test]
    fn empty_token_file_is_rejected_not_reused() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        ensure_data_dir(&data_dir).unwrap();
        let token_path = data_dir.join("token");
        fs::write(&token_path, "").unwrap();
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();

        let result = ensure_token(&data_dir);
        match result {
            Err(TokenError::InvalidTokenContents) => {}
            Ok(_) => panic!("expected TokenError::InvalidTokenContents, got Ok"),
            Err(other) => panic!("expected TokenError::InvalidTokenContents, got {other}"),
        }
    }

    #[test]
    fn truncated_token_file_is_rejected_not_reused() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        ensure_data_dir(&data_dir).unwrap();
        let token_path = data_dir.join("token");
        // Only 10 hex chars, not the required 64.
        fs::write(&token_path, "deadbeef00").unwrap();
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();

        let result = ensure_token(&data_dir);
        assert!(matches!(result, Err(TokenError::InvalidTokenContents)));
    }

    #[test]
    fn restrictive_0400_token_file_is_left_alone() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");

        let first = ensure_token(&data_dir).expect("first call");
        let token_path = data_dir.join("token");

        // 0400 (owner read-only) is already at least as restrictive as
        // 0600 (owner read-write) in the dimension C10 cares about:
        // group/world access. It must not be "repaired".
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o400)).unwrap();

        let second = ensure_token(&data_dir).expect("second call");

        assert!(!second.generated);
        assert!(
            !second.permissions_repaired,
            "0400 must not be reported or treated as a repair"
        );
        assert_eq!(first.token, second.token);

        let mode = fs::metadata(&token_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o400, "0400 permissions must be left untouched");
    }

    #[test]
    fn ensure_data_dir_leaves_existing_0755_dir_unchanged() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o755)).unwrap();

        ensure_data_dir(&data_dir).expect("ensure_data_dir");

        // 0755 has no group/other write bits, so it's already
        // non-world-writable; a pre-existing, user-configured data_dir
        // must not be unconditionally forced to 0700.
        let mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn ensure_data_dir_strips_write_bits_from_existing_world_writable_dir() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).unwrap();
        fs::set_permissions(&data_dir, fs::Permissions::from_mode(0o777)).unwrap();

        ensure_data_dir(&data_dir).expect("ensure_data_dir");

        let mode = fs::metadata(&data_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "group/other write bits must be stripped, other bits left alone"
        );
    }

    #[test]
    fn write_token_file_leaves_no_temp_file_behind() {
        let dir = tempdir().unwrap();
        let data_dir = dir.path().join("data");
        ensure_token(&data_dir).expect("ensure_token");

        let leftovers: Vec<_> = fs::read_dir(&data_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp file(s) left behind: {leftovers:?}"
        );
    }
}
