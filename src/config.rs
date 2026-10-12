//! Config file loading, default generation, and the `Config` schema.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The on-disk config schema. Unknown fields are rejected: there is
/// deliberately no Unix-socket-related field, and adding one to
/// `config.toml` fails to parse rather than being silently ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen_addr: String,
    pub data_dir: PathBuf,
    pub log_level: String,
}

impl Config {
    /// Builds the default config, pointing `data_dir` at the resolved
    /// XDG data directory passed in by the caller.
    pub fn defaults(data_dir: PathBuf) -> Self {
        Config {
            listen_addr: "127.0.0.1:4812".to_string(),
            data_dir,
            log_level: "info".to_string(),
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Parse(toml::de::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "{e}"),
            ConfigError::Parse(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(e: io::Error) -> Self {
        ConfigError::Io(e)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(e: toml::de::Error) -> Self {
        ConfigError::Parse(e)
    }
}

/// Renders the default config as TOML with inline comments documenting
/// every field, so a user opening the generated file understands it
/// without external docs.
pub fn render_default_toml(cfg: &Config) -> String {
    format!(
        "# tephrad config\n\
         # listen_addr: the local address tephrad's HTTP/WebSocket JSON-RPC\n\
         # server binds to. Only localhost addresses are supported.\n\
         listen_addr = \"{listen_addr}\"\n\
         \n\
         # data_dir: where tephrad stores its auth token and SQLite cache.\n\
         # Created automatically (recursively) if it does not exist.\n\
         data_dir = \"{data_dir}\"\n\
         \n\
         # log_level: verbosity for tephrad's structured logs.\n\
         # One of: trace, debug, info, warn, error.\n\
         log_level = \"{log_level}\"\n",
        listen_addr = cfg.listen_addr,
        data_dir = cfg.data_dir.display(),
        log_level = cfg.log_level,
    )
}

/// Outcome of loading the config: either it already existed and was
/// parsed, or it was absent and a default was generated and written.
#[derive(Debug)]
pub struct Loaded {
    pub config: Config,
    pub defaults_written: bool,
}

/// Loads the config file at `path`. If the file does not exist, a default
/// config (with `data_dir` set to `default_data_dir`) is generated, written
/// to `path` (creating parent directories as needed), and returned.
///
/// If the file exists but fails to parse, returns `ConfigError::Parse`
/// without modifying the file. This function never panics on malformed
/// input.
pub fn load_or_create(path: &Path, default_data_dir: PathBuf) -> Result<Loaded, ConfigError> {
    if path.exists() {
        let raw = fs::read_to_string(path)?;
        let config: Config = toml::from_str(&raw)?;
        return Ok(Loaded {
            config,
            defaults_written: false,
        });
    }

    let config = Config::defaults(default_data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let rendered = render_default_toml(&config);
    fs::write(path, rendered)?;
    Ok(Loaded {
        config,
        defaults_written: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn default_config_round_trips_through_toml() {
        let cfg = Config::defaults(PathBuf::from("/tmp/tephra-data"));
        let serialized = toml::to_string(&cfg).expect("serialize");
        let deserialized: Config = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(cfg, deserialized);
    }

    #[test]
    fn missing_file_creates_default() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        assert!(!config_path.exists());

        let data_dir = dir.path().join("data");
        let loaded = load_or_create(&config_path, data_dir.clone()).expect("load");

        assert!(loaded.defaults_written);
        assert!(config_path.exists());
        assert_eq!(loaded.config.listen_addr, "127.0.0.1:4812");
        assert_eq!(loaded.config.data_dir, data_dir);
        assert_eq!(loaded.config.log_level, "info");

        // Written file round-trips back to the same config.
        let raw = fs::read_to_string(&config_path).unwrap();
        let reparsed: Config = toml::from_str(&raw).unwrap();
        assert_eq!(reparsed, loaded.config);
    }

    #[test]
    fn malformed_toml_returns_parse_error_not_panic() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, "this is not valid = = toml {{{").unwrap();

        let result = load_or_create(&config_path, dir.path().join("data"));
        match result {
            Err(ConfigError::Parse(_)) => {}
            other => panic!("expected ConfigError::Parse, got {other:?}"),
        }

        // The corrupt file must be left untouched.
        let raw = fs::read_to_string(&config_path).unwrap();
        assert_eq!(raw, "this is not valid = = toml {{{");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            "listen_addr = \"127.0.0.1:4812\"\n\
             data_dir = \"/tmp/tephra-data\"\n\
             log_level = \"info\"\n\
             socket_path = \"/tmp/tephra.sock\"\n",
        )
        .unwrap();

        let result = load_or_create(&config_path, dir.path().join("data"));
        assert!(matches!(result, Err(ConfigError::Parse(_))));
    }
}
