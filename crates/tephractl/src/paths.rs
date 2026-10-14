//! XDG-style path resolution for tephra's token file, mirroring
//! `tephrad`'s own `src/paths.rs` resolution order (that module is
//! binary-only inside the `tephrad` crate, so this is a deliberate,
//! independent copy rather than a shared dependency — PR-7 owns no
//! server-side code and must not reach into `tephrad`'s private
//! modules).
//!
//! Resolution order:
//! 1. `XDG_DATA_HOME`, if set and non-empty: `<XDG_DATA_HOME>/tephra`.
//! 2. `$HOME/.local/share/tephra`.
//!
//! The lookup is implemented against an injectable environment-reader
//! closure so it stays unit-testable without mutating real process-global
//! environment variables (which would race across parallel test threads).
//! [`default_token_path`] reads the real process environment.

use std::env;
use std::path::PathBuf;

/// Returns tephra's data directory, honoring `XDG_DATA_HOME` and
/// falling back to `~/.local/share/tephra` — matching P2's documented
/// token location (`~/.local/share/tephra/token`).
pub fn data_dir() -> PathBuf {
    resolve_data_dir(|k| env::var(k).ok())
}

/// The default token path: `<data_dir>/token`.
pub fn default_token_path() -> PathBuf {
    data_dir().join("token")
}

fn resolve_data_dir<F>(get: F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(v) = get("XDG_DATA_HOME") {
        if !v.is_empty() {
            return PathBuf::from(v).join("tephra");
        }
    }
    let home = get("HOME").unwrap_or_else(|| ".".to_string());
    PathBuf::from(home).join(".local/share").join("tephra")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_map(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn honors_xdg_data_home_override() {
        let get = env_map(&[("XDG_DATA_HOME", "/custom/data"), ("HOME", "/home/someone")]);
        assert_eq!(resolve_data_dir(get), PathBuf::from("/custom/data/tephra"));
    }

    #[test]
    fn falls_back_to_home_local_share_when_xdg_unset() {
        let get = env_map(&[("HOME", "/home/someone")]);
        assert_eq!(
            resolve_data_dir(get),
            PathBuf::from("/home/someone/.local/share/tephra")
        );
    }

    #[test]
    fn empty_xdg_var_is_treated_as_unset() {
        let get = env_map(&[("XDG_DATA_HOME", ""), ("HOME", "/home/someone")]);
        assert_eq!(
            resolve_data_dir(get),
            PathBuf::from("/home/someone/.local/share/tephra")
        );
    }

    #[test]
    fn default_token_path_appends_token_file_name() {
        let get = env_map(&[("XDG_DATA_HOME", "/custom/data")]);
        let dir = resolve_data_dir(get);
        assert_eq!(
            dir.join("token"),
            PathBuf::from("/custom/data/tephra/token")
        );
    }
}
