//! Config file loading, default generation, and the `Config` schema.

use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
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
    /// The Atlas fallback (G20, FR16): when `true`, a link fix that
    /// rewrites a plain wikilink whose bare name also exists in another
    /// registered source writes the fuller same-source path
    /// (`[[docs/Plan]]`) instead of the bare name, so Obsidian cannot
    /// pick the wrong file when several sources are linked into one
    /// vault. Read from this file only, once, at start. `serde(default)`
    /// so a file written before the key existed still loads, as `false`.
    #[serde(default)]
    pub qualify_ambiguous_links: bool,
}

impl Config {
    /// Builds the default config, pointing `data_dir` at the resolved
    /// XDG data directory passed in by the caller.
    pub fn defaults(data_dir: PathBuf) -> Self {
        Config {
            listen_addr: "127.0.0.1:4812".to_string(),
            data_dir,
            log_level: "info".to_string(),
            qualify_ambiguous_links: false,
        }
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Parse(toml::de::Error),
    /// `listen_addr` failed to parse as a socket address, or parsed but
    /// is not a loopback address. tephrad's RPC server will perform disk
    /// mutations (a later PR), so only loopback addresses are accepted.
    InvalidListenAddr(String),
    /// `log_level` is not one of the five values tephrad documents and
    /// accepts. Left unvalidated, `tracing_subscriber::EnvFilter`
    /// happily parses an arbitrary word (e.g. a typo like "infoo") as a
    /// target directive instead of a level, silently enabling no
    /// output at all rather than falling back to `info`.
    InvalidLogLevel(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "{e}"),
            ConfigError::Parse(e) => write!(f, "{e}"),
            ConfigError::InvalidListenAddr(reason) => write!(f, "{reason}"),
            ConfigError::InvalidLogLevel(reason) => write!(f, "{reason}"),
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

/// The five `log_level` values tephrad documents and accepts, matching
/// the generated config's comment and the `tracing` level names.
const VALID_LOG_LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

/// Validates that `level` is one of [`VALID_LOG_LEVELS`]. Rejecting
/// anything else here — rather than handing an arbitrary string to
/// `tracing_subscriber::EnvFilter`, which parses unrecognized words as
/// target directives and so silently filters out all output — ensures
/// an invalid level is a loud startup failure instead of a daemon that
/// runs with no logs at all.
fn validate_log_level(level: &str) -> Result<(), ConfigError> {
    if VALID_LOG_LEVELS.contains(&level) {
        Ok(())
    } else {
        Err(ConfigError::InvalidLogLevel(format!(
            "log_level \"{level}\" is not one of: {levels}",
            levels = VALID_LOG_LEVELS.join(", ")
        )))
    }
}

/// Validates every field of `config` that load-time checks can catch
/// before the rest of startup proceeds: `listen_addr` must be a
/// loopback socket address, and `log_level` must be one of the
/// documented values. `qualify_ambiguous_links` needs no check here: a
/// boolean has no invalid value, and a value of any other TOML type
/// (`"yes"`, `1`) never reaches this function, because it fails to
/// deserialize as [`ConfigError::Parse`], naming the key.
fn validate_config(config: &Config) -> Result<(), ConfigError> {
    validate_listen_addr(&config.listen_addr)?;
    validate_log_level(&config.log_level)?;
    Ok(())
}

/// Validates that `addr` parses as a socket address and that its IP is a
/// loopback address (127.0.0.1 or ::1). tephrad's RPC server performs
/// disk mutations, so binding to a non-loopback address would expose
/// that to the local network; this is rejected at the config stage with
/// a clear error rather than silently binding wider than intended.
fn validate_listen_addr(addr: &str) -> Result<(), ConfigError> {
    match addr.parse::<SocketAddr>() {
        Ok(sock) if sock.ip().is_loopback() => Ok(()),
        Ok(sock) => Err(ConfigError::InvalidListenAddr(format!(
            "listen_addr \"{addr}\" is not a loopback address (resolved to {ip}); only loopback addresses (127.0.0.1, ::1) are supported",
            ip = sock.ip()
        ))),
        Err(_) => Err(ConfigError::InvalidListenAddr(format!(
            "listen_addr \"{addr}\" is not a valid socket address (expected host:port)"
        ))),
    }
}

/// Renders the default config as TOML with inline comments documenting
/// every field, so a user opening the generated file understands it
/// without external docs.
pub fn render_default_toml(cfg: &Config) -> String {
    // data_dir is interpolated through `toml::Value`'s own serializer so
    // that a path containing a backslash, double quote, or other
    // TOML-significant character is escaped (or rendered as a literal
    // string) correctly, rather than producing a config file that fails
    // to parse on the next start.
    let data_dir_literal = toml::Value::String(cfg.data_dir.display().to_string()).to_string();
    format!(
        "# tephrad config\n\
         # listen_addr: the local address tephrad's HTTP/WebSocket JSON-RPC\n\
         # server binds to. Only loopback addresses are supported; tephrad\n\
         # refuses to start with any other address.\n\
         listen_addr = \"{listen_addr}\"\n\
         \n\
         # data_dir: where tephrad stores its auth token and SQLite cache.\n\
         # Created automatically (recursively) if it does not exist.\n\
         data_dir = {data_dir}\n\
         \n\
         # log_level: verbosity for tephrad's structured logs.\n\
         # One of: trace, debug, info, warn, error.\n\
         log_level = \"{log_level}\"\n\
         \n\
         # qualify_ambiguous_links: the Atlas fallback. Leave false unless\n\
         # Atlas asks for it. When true, a link fix that rewrites a plain\n\
         # [[Name]] whose name also exists in another registered source\n\
         # writes the fuller same-source path ([[docs/Name]]), so Obsidian\n\
         # cannot open the same-named file in the other source. Read at\n\
         # start; changing it affects only fixes made after the next start.\n\
         qualify_ambiguous_links = {qualify_ambiguous_links}\n",
        listen_addr = cfg.listen_addr,
        data_dir = data_dir_literal,
        log_level = cfg.log_level,
        qualify_ambiguous_links = cfg.qualify_ambiguous_links,
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
        validate_config(&config)?;
        return Ok(Loaded {
            config,
            defaults_written: false,
        });
    }

    let config = Config::defaults(default_data_dir);
    validate_config(&config)?;
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

    #[test]
    fn non_loopback_listen_addr_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            "listen_addr = \"0.0.0.0:4812\"\n\
             data_dir = \"/tmp/tephra-data\"\n\
             log_level = \"info\"\n",
        )
        .unwrap();

        let result = load_or_create(&config_path, dir.path().join("data"));
        match result {
            Err(ConfigError::InvalidListenAddr(reason)) => {
                assert!(reason.contains("0.0.0.0:4812"));
            }
            other => panic!("expected ConfigError::InvalidListenAddr, got {other:?}"),
        }

        // The user's file must not have been overwritten.
        let raw = fs::read_to_string(&config_path).unwrap();
        assert!(raw.contains("0.0.0.0:4812"));
    }

    #[test]
    fn unparseable_listen_addr_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            "listen_addr = \"not-an-address\"\n\
             data_dir = \"/tmp/tephra-data\"\n\
             log_level = \"info\"\n",
        )
        .unwrap();

        let result = load_or_create(&config_path, dir.path().join("data"));
        assert!(matches!(result, Err(ConfigError::InvalidListenAddr(_))));
    }

    #[test]
    fn invalid_log_level_is_rejected() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        fs::write(
            &config_path,
            "listen_addr = \"127.0.0.1:4812\"\n\
             data_dir = \"/tmp/tephra-data\"\n\
             log_level = \"loud\"\n",
        )
        .unwrap();

        let result = load_or_create(&config_path, dir.path().join("data"));
        match result {
            Err(ConfigError::InvalidLogLevel(reason)) => {
                assert!(reason.contains("loud"));
            }
            other => panic!("expected ConfigError::InvalidLogLevel, got {other:?}"),
        }

        // The user's file must not have been overwritten.
        let raw = fs::read_to_string(&config_path).unwrap();
        assert!(raw.contains("loud"));
    }

    #[test]
    fn render_default_toml_escapes_data_dir_specials_and_round_trips() {
        // A data_dir containing a double quote and a backslash, as could
        // happen with an unusual XDG_DATA_HOME, must not corrupt the
        // generated TOML.
        let cfg = Config::defaults(PathBuf::from("/tmp/weird\"dir\\path"));
        let rendered = render_default_toml(&cfg);
        let parsed: Config = toml::from_str(&rendered).expect("generated config must parse");
        assert_eq!(parsed.data_dir, cfg.data_dir);
    }

    // --- qualify_ambiguous_links (G20) ---

    /// The three original keys, as a file written before G20 had them.
    const THREE_KEYS: &str = "listen_addr = \"127.0.0.1:4812\"\n\
                              data_dir = \"/tmp/tephra-data\"\n\
                              log_level = \"info\"\n";

    /// Loads `contents` as a config file; returns the result and the file
    /// text as it is afterwards.
    fn load_text(contents: &str) -> (Result<Loaded, ConfigError>, String) {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        fs::write(&config_path, contents).unwrap();
        let result = load_or_create(&config_path, dir.path().join("data"));
        (result, fs::read_to_string(&config_path).unwrap())
    }

    #[test]
    fn qualify_key_absent_parses_to_false() {
        let (result, after) = load_text(THREE_KEYS);
        assert!(
            !result
                .expect("a file without the key loads")
                .config
                .qualify_ambiguous_links
        );
        assert_eq!(after, THREE_KEYS, "the file is not rewritten");
    }

    #[test]
    fn qualify_key_true_and_false_parse() {
        for (text, want) in [("true", true), ("false", false)] {
            let (result, _) = load_text(&format!("{THREE_KEYS}qualify_ambiguous_links = {text}\n"));
            let loaded = result.expect("load");
            assert_eq!(loaded.config.qualify_ambiguous_links, want, "{text}");
            assert!(!loaded.defaults_written);
        }
    }

    #[test]
    fn qualify_key_of_another_type_fails_with_a_parse_error_naming_the_key() {
        for value in ["\"yes\"", "1", "0", "\"true\"", "[true]"] {
            let text = format!("{THREE_KEYS}qualify_ambiguous_links = {value}\n");
            let (result, after) = load_text(&text);
            match result {
                Err(ConfigError::Parse(e)) => assert!(
                    e.to_string().contains("qualify_ambiguous_links"),
                    "{value}: the error must name the key, got: {e}"
                ),
                other => panic!("{value}: expected ConfigError::Parse, got {other:?}"),
            }
            assert_eq!(after, text, "{value}: the user's file is left untouched");
        }
    }

    #[test]
    fn qualify_key_is_accepted_beside_the_three_existing_keys_and_a_misspelling_is_not() {
        let (ok, _) = load_text(&format!("{THREE_KEYS}qualify_ambiguous_links = true\n"));
        let cfg = ok.expect("four keys load").config;
        assert_eq!(cfg.listen_addr, "127.0.0.1:4812");
        assert_eq!(cfg.log_level, "info");
        assert!(cfg.qualify_ambiguous_links);

        let text = format!("{THREE_KEYS}qualify_ambiguous_link = true\n");
        let (bad, after) = load_text(&text);
        match bad {
            Err(ConfigError::Parse(e)) => {
                assert!(e.to_string().contains("qualify_ambiguous_link"), "{e}")
            }
            other => panic!("expected ConfigError::Parse, got {other:?}"),
        }
        assert_eq!(after, text);
    }

    #[test]
    fn qualify_default_is_false_and_round_trips() {
        let cfg = Config::defaults(PathBuf::from("/tmp/tephra-data"));
        assert!(!cfg.qualify_ambiguous_links);
        let on = Config {
            qualify_ambiguous_links: true,
            ..cfg.clone()
        };
        for c in [cfg, on] {
            let back: Config = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
            assert_eq!(back, c);
        }
    }

    #[test]
    fn generated_config_has_the_key_off_with_its_comment_and_parses_back_equal() {
        let dir = tempdir().expect("tempdir");
        let config_path = dir.path().join("config.toml");
        let loaded = load_or_create(&config_path, dir.path().join("data")).expect("load");
        let raw = fs::read_to_string(&config_path).unwrap();
        assert!(raw.contains("qualify_ambiguous_links = false\n"), "{raw}");
        assert!(
            raw.contains("# qualify_ambiguous_links: the Atlas fallback."),
            "{raw}"
        );
        assert_eq!(toml::from_str::<Config>(&raw).unwrap(), loaded.config);

        // Rendered with the key on, the file says so and parses back the same.
        let on = Config {
            qualify_ambiguous_links: true,
            ..loaded.config
        };
        let rendered = render_default_toml(&on);
        assert!(rendered.contains("qualify_ambiguous_links = true\n"));
        assert_eq!(toml::from_str::<Config>(&rendered).unwrap(), on);
    }
}
