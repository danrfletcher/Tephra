//! XDG-style path resolution for tephra's config and data directories.
//!
//! Resolution order, for both config and data directories:
//! 1. The relevant `XDG_*_HOME` environment variable, if set.
//! 2. `$HOME/.config/tephra` (config) or `$HOME/.local/share/tephra` (data).
//!
//! The lookups are implemented against an injectable environment-reader
//! closure so the resolution logic itself can be unit tested deterministically,
//! without mutating real process-global environment variables (which would
//! race across parallel test threads). The public functions below read the
//! real process environment.

use std::env;
use std::path::PathBuf;

/// Returns the directory tephra's config file lives in, honoring
/// `XDG_CONFIG_HOME` and falling back to `~/.config/tephra`.
pub fn config_dir() -> PathBuf {
    resolve_dir("XDG_CONFIG_HOME", ".config", |k| env::var(k).ok())
}

/// Returns the directory tephra's data (token, cache) lives in, honoring
/// `XDG_DATA_HOME` and falling back to `~/.local/share/tephra`.
pub fn data_dir() -> PathBuf {
    resolve_dir("XDG_DATA_HOME", ".local/share", |k| env::var(k).ok())
}

/// Core resolution logic, parameterized over an environment-variable lookup
/// so it can be exercised in unit tests without touching real env state.
fn resolve_dir<F>(xdg_var: &str, home_fallback_suffix: &str, get: F) -> PathBuf
where
    F: Fn(&str) -> Option<String>,
{
    if let Some(v) = get(xdg_var) {
        if !v.is_empty() {
            return PathBuf::from(v).join("tephra");
        }
    }
    let home = get("HOME").unwrap_or_else(|| ".".to_string());
    PathBuf::from(home)
        .join(home_fallback_suffix)
        .join("tephra")
}

/// Default path to the config file inside [`config_dir`].
pub fn default_config_path() -> PathBuf {
    config_dir().join("config.toml")
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
    fn config_dir_honors_xdg_config_home_override() {
        let get = env_map(&[
            ("XDG_CONFIG_HOME", "/custom/config"),
            ("HOME", "/home/someone"),
        ]);
        let dir = resolve_dir("XDG_CONFIG_HOME", ".config", get);
        assert_eq!(dir, PathBuf::from("/custom/config/tephra"));
    }

    #[test]
    fn data_dir_honors_xdg_data_home_override() {
        let get = env_map(&[("XDG_DATA_HOME", "/custom/data"), ("HOME", "/home/someone")]);
        let dir = resolve_dir("XDG_DATA_HOME", ".local/share", get);
        assert_eq!(dir, PathBuf::from("/custom/data/tephra"));
    }

    #[test]
    fn config_dir_falls_back_to_home_config_when_xdg_unset() {
        let get = env_map(&[("HOME", "/home/someone")]);
        let dir = resolve_dir("XDG_CONFIG_HOME", ".config", get);
        assert_eq!(dir, PathBuf::from("/home/someone/.config/tephra"));
    }

    #[test]
    fn data_dir_falls_back_to_home_local_share_when_xdg_unset() {
        let get = env_map(&[("HOME", "/home/someone")]);
        let dir = resolve_dir("XDG_DATA_HOME", ".local/share", get);
        assert_eq!(dir, PathBuf::from("/home/someone/.local/share/tephra"));
    }

    #[test]
    fn empty_xdg_var_is_treated_as_unset() {
        let get = env_map(&[("XDG_CONFIG_HOME", ""), ("HOME", "/home/someone")]);
        let dir = resolve_dir("XDG_CONFIG_HOME", ".config", get);
        assert_eq!(dir, PathBuf::from("/home/someone/.config/tephra"));
    }
}
