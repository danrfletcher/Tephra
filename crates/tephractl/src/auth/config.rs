//! The three `config.toml` keys the `auth` commands read:
//! `google_client_json`, `github_api_url` and `test_credential_file`.
//!
//! `tephractl` does not load `tephrad`'s whole config (that would pull
//! the daemon in): it reads these keys from the same file and ignores the
//! rest. `tephrad` validates the file when it starts; here a wrong type
//! is an error naming the key, and nothing of the value is repeated.
//!
//! A missing file is the all-defaults config, as it is for the daemon.

use std::path::{Path, PathBuf};

/// The keys `auth` uses.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub google_client_json: Option<PathBuf>,
    pub github_api_url: Option<String>,
    pub test_credential_file: Option<PathBuf>,
}

/// Why the config could not be read. Fixed text: a TOML parse error can
/// quote the line it stopped at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError(pub String);

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ConfigError {}

/// Reads the keys from the file at `path`.
pub fn load(path: &Path) -> Result<AuthConfig, ConfigError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AuthConfig::default()),
        Err(e) => {
            return Err(ConfigError(format!(
                "cannot read the config file {}: {}",
                path.display(),
                e.kind()
            )))
        }
    };
    parse(&text).map_err(|e| ConfigError(format!("{} ({})", e.0, path.display())))
}

/// Reads the keys from config text.
pub fn parse(text: &str) -> Result<AuthConfig, ConfigError> {
    let table: toml::Table = text
        .parse()
        .map_err(|_| ConfigError("the config file is not valid TOML".to_string()))?;
    let string = |key: &str| -> Result<Option<String>, ConfigError> {
        match table.get(key) {
            None => Ok(None),
            Some(toml::Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(ConfigError(format!("config key {key} must be a string"))),
        }
    };
    Ok(AuthConfig {
        google_client_json: string("google_client_json")?.map(PathBuf::from),
        github_api_url: string("github_api_url")?,
        test_credential_file: string("test_credential_file")?.map(PathBuf::from),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_the_default_config() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load(&dir.path().join("config.toml")).unwrap(),
            AuthConfig::default()
        );
    }

    #[test]
    fn reads_the_three_keys_and_ignores_the_rest() {
        let cfg = parse(
            "port = 4812\n\
             google_client_json = \"/c.json\"\n\
             github_api_url = \"http://127.0.0.1:9\"\n\
             test_credential_file = \"/d/cred\"\n",
        )
        .unwrap();
        assert_eq!(cfg.google_client_json, Some(PathBuf::from("/c.json")));
        assert_eq!(cfg.github_api_url.as_deref(), Some("http://127.0.0.1:9"));
        assert_eq!(cfg.test_credential_file, Some(PathBuf::from("/d/cred")));
    }

    #[test]
    fn a_wrong_type_names_the_key_and_not_the_value() {
        let e = parse("google_client_json = 5\n").unwrap_err();
        assert!(e.0.contains("google_client_json"), "{e}");
    }

    #[test]
    fn invalid_toml_does_not_quote_the_file() {
        let e = parse("github_api_url = ghp_SECRETISH\n").unwrap_err();
        assert!(!e.0.contains("ghp_"), "{e}");
    }
}
