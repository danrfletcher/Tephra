//! Config file loading, default generation, and the `Config` schema.

use std::fmt;
use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use tephrad::api_url::validate_api_url;
use tephrad::cache_limit::{parse_cache_limit, DEFAULT_CACHE_LIMIT_TEXT};

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
    /// The most disk the remote-source cache may use, as text: a number
    /// and a unit (`10 GB`; see [`tephrad::cache_limit`]). Kept as the
    /// user's text so the file round-trips; [`Config::cache_limit_bytes`]
    /// gives the byte count, and [`validate_config`] refuses a value that
    /// does not parse. Defaults to `10 GB` when absent.
    #[serde(default = "default_cache_limit")]
    pub cache_limit: String,
    /// Path of the Google OAuth client JSON (the app's client id and
    /// secret, not a user token). Optional; unset by default. Nothing
    /// reads the file yet.
    #[serde(default)]
    pub google_client_json: Option<PathBuf>,
    /// Test-only: base URL of the GitHub API, in place of the real one.
    /// `https://`, or `http://` for a loopback host only.
    #[serde(default)]
    pub github_api_url: Option<String>,
    /// Test-only: base URL of the Google Drive API. Same rule as
    /// `github_api_url`.
    #[serde(default)]
    pub drive_api_url: Option<String>,
    /// Test-only: path of the file credential store, which must lie
    /// inside `data_dir`. Only a build with the `test-credential-store`
    /// feature accepts it; any other build refuses to start with it set.
    /// The field holds a path, never a credential.
    #[serde(default)]
    pub test_credential_file: Option<PathBuf>,
}

fn default_cache_limit() -> String {
    DEFAULT_CACHE_LIMIT_TEXT.to_string()
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
            cache_limit: default_cache_limit(),
            google_client_json: None,
            github_api_url: None,
            drive_api_url: None,
            test_credential_file: None,
        }
    }

    /// `cache_limit` in bytes. Always `Ok` for a config that passed
    /// [`load_or_create`].
    pub fn cache_limit_bytes(&self) -> Result<u64, tephrad::cache_limit::SizeParseError> {
        parse_cache_limit(&self.cache_limit)
    }
}

/// A config file that failed to parse, reduced to what is safe to print.
///
/// `toml::de::Error`'s own `Display` and `Debug` quote the offending
/// source line, and serde's type errors (`invalid type: string
/// "ghp_...", expected a boolean`) quote the value, so a secret typed
/// into the file would be copied to stderr and so to the launchd log.
/// This keeps the error's message with any quoted value cut out, the
/// name of the key on the offending line, and the line and column.
pub struct ParseError {
    message: String,
    key: Option<String>,
    line_col: Option<(usize, usize)>,
}

/// Drops the value serde quotes in `invalid type: <value>, expected
/// <what>` and `invalid value: <value>, expected <what>`, keeping the
/// expectation. Any other message (unknown or duplicate field, a
/// syntax error) names no value and is kept as is.
fn without_quoted_value(message: &str) -> String {
    for kind in ["invalid type", "invalid value"] {
        if message.starts_with(&format!("{kind}: ")) {
            return match message.rsplit_once(", expected ") {
                Some((_, what)) => format!("{kind}, expected {what}"),
                None => kind.to_string(),
            };
        }
    }
    message.to_string()
}

impl ParseError {
    /// `raw` is the text that failed to parse; it is read only to find
    /// the line, column and key of the error's span, never stored.
    fn new(error: &toml::de::Error, raw: &str) -> Self {
        let mut key = None;
        let mut line_col = None;
        if let Some(span) = error.span() {
            let start = span.start.min(raw.len());
            let before = &raw.as_bytes()[..start];
            let line = before.iter().filter(|b| **b == b'\n').count() + 1;
            let col = before.iter().rev().take_while(|b| **b != b'\n').count() + 1;
            line_col = Some((line, col));
            // The key is the bare name left of the first `=` on the
            // line; anything else (a table header, an array element)
            // is not named.
            let line_text = raw.lines().nth(line - 1).unwrap_or_default();
            if let Some((name, _)) = line_text.split_once('=') {
                let name = name.trim();
                let bare = !name.is_empty()
                    && name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                if bare {
                    key = Some(name.to_string());
                }
            }
        }
        ParseError {
            message: without_quoted_value(error.message()),
            key,
            line_col,
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)?;
        if let Some(key) = &self.key {
            if !self.message.contains(key.as_str()) {
                write!(f, " for key `{key}`")?;
            }
        }
        if let Some((line, col)) = self.line_col {
            write!(f, " (line {line}, column {col})")?;
        }
        Ok(())
    }
}

impl fmt::Debug for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParseError")
            .field("message", &self.message)
            .field("key", &self.key)
            .field("line_col", &self.line_col)
            .finish()
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    /// The file is not valid for [`Config`]; see [`ParseError`] for why
    /// the offending source line is not carried.
    Parse(ParseError),
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
    /// `cache_limit` is not a number and a unit; the text names the key.
    InvalidCacheLimit(String),
    /// `github_api_url` or `drive_api_url` breaks the https / loopback-http
    /// rule; the text names the key.
    InvalidApiUrl(String),
    /// `test_credential_file` is set on a build without the
    /// `test-credential-store` feature; the text names the key. Never
    /// built (so never constructed) when the feature is on.
    #[cfg_attr(feature = "test-credential-store", allow(dead_code))]
    TestCredentialFileNotBuilt(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "{e}"),
            ConfigError::Parse(e) => write!(f, "{e}"),
            ConfigError::InvalidListenAddr(reason) => write!(f, "{reason}"),
            ConfigError::InvalidLogLevel(reason) => write!(f, "{reason}"),
            ConfigError::InvalidCacheLimit(reason) => write!(f, "{reason}"),
            ConfigError::InvalidApiUrl(reason) => write!(f, "{reason}"),
            ConfigError::TestCredentialFileNotBuilt(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(e: io::Error) -> Self {
        ConfigError::Io(e)
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
            "log_level is not one of: {levels}",
            levels = VALID_LOG_LEVELS.join(", ")
        )))
    }
}

/// Validates every field of `config` that load-time checks can catch
/// before the rest of startup proceeds: `listen_addr` must be a
/// loopback socket address, `log_level` one of the documented values,
/// `cache_limit` a number and a unit, `github_api_url` and
/// `drive_api_url` an `https://` URL or a loopback `http://` one, and
/// `test_credential_file` unset unless this build has the
/// `test-credential-store` feature. Every message names its key.
/// `qualify_ambiguous_links` needs no check here: a boolean has no
/// invalid value, and a value of any other TOML type (`"yes"`, `1`)
/// never reaches this function, because it fails to deserialize as
/// [`ConfigError::Parse`], naming the key. The same holds for the
/// path keys given a non-string.
fn validate_config(config: &Config) -> Result<(), ConfigError> {
    validate_listen_addr(&config.listen_addr)?;
    validate_log_level(&config.log_level)?;
    validate_cache_limit(&config.cache_limit)?;
    validate_api_url_key("github_api_url", config.github_api_url.as_deref())?;
    validate_api_url_key("drive_api_url", config.drive_api_url.as_deref())?;
    validate_test_credential_file(config.test_credential_file.as_deref())?;
    Ok(())
}

fn validate_cache_limit(text: &str) -> Result<(), ConfigError> {
    match parse_cache_limit(text) {
        Ok(_) => Ok(()),
        Err(e) => Err(ConfigError::InvalidCacheLimit(format!("cache_limit {e}"))),
    }
}

fn validate_api_url_key(key: &str, url: Option<&str>) -> Result<(), ConfigError> {
    // The value is deliberately not echoed: a refused URL is often one
    // with a token in its user info or query (`https://x:<token>@host`,
    // `?key=<token>`), and the error goes to stderr and the daemon log.
    match url.map(validate_api_url) {
        None | Some(Ok(())) => Ok(()),
        Some(Err(e)) => Err(ConfigError::InvalidApiUrl(format!("{key} {e}"))),
    }
}

#[cfg(feature = "test-credential-store")]
fn validate_test_credential_file(_path: Option<&Path>) -> Result<(), ConfigError> {
    Ok(())
}

#[cfg(not(feature = "test-credential-store"))]
fn validate_test_credential_file(path: Option<&Path>) -> Result<(), ConfigError> {
    match path {
        None => Ok(()),
        Some(_) => Err(ConfigError::TestCredentialFileNotBuilt(
            "test_credential_file is set, but this build of tephrad has no test credential store; \
             remove the key (it needs a build with the test-credential-store feature)"
                .to_string(),
        )),
    }
}

/// Validates that `addr` parses as a socket address and that its IP is a
/// loopback address (127.0.0.1 or ::1). tephrad's RPC server performs
/// disk mutations, so binding to a non-loopback address would expose
/// that to the local network; this is rejected at the config stage with
/// a clear error rather than silently binding wider than intended.
fn validate_listen_addr(addr: &str) -> Result<(), ConfigError> {
    match addr.parse::<SocketAddr>() {
        Ok(sock) if sock.ip().is_loopback() => Ok(()),
        Ok(_) => Err(ConfigError::InvalidListenAddr(
            "listen_addr is not a loopback address; only loopback addresses (127.0.0.1, ::1) are supported".to_string(),
        )),
        Err(_) => Err(ConfigError::InvalidListenAddr(
            "listen_addr is not a valid socket address (expected host:port)".to_string(),
        )),
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
         qualify_ambiguous_links = {qualify_ambiguous_links}\n\
         \n\
         # cache_limit: the most disk the remote-source cache may use, as a\n\
         # number and a unit: MB and GB are 1000-based, MiB and GiB are\n\
         # 1024-based (for example \"500 MB\", \"1.5 GiB\"). Must be above zero.\n\
         # Default 10 GB (10,000,000,000 bytes).\n\
         cache_limit = {cache_limit}\n\
         \n\
         # google_client_json: path to the Google OAuth client JSON used to\n\
         # sign in to Google Drive. Optional; leave it commented out until\n\
         # you set up a Drive source.\n\
         {google_client_json}\n\
         \n\
         # TEST ONLY - github_api_url / drive_api_url: replace the GitHub and\n\
         # Google Drive API base URLs, for tests against a local fake\n\
         # server. https:// URLs, or http:// only for localhost, 127.x.x.x\n\
         # or [::1]. Leave both commented out in normal use.\n\
         {github_api_url}\n\
         {drive_api_url}\n\
         \n\
         # TEST ONLY - test_credential_file: keep credentials in a plain\n\
         # file inside data_dir instead of the macOS Keychain. Only a build\n\
         # with the test-credential-store feature accepts this key; every\n\
         # other build refuses to start while it is set. Never set it in\n\
         # normal use.\n\
         {test_credential_file}\n",
        listen_addr = cfg.listen_addr,
        data_dir = data_dir_literal,
        log_level = cfg.log_level,
        qualify_ambiguous_links = cfg.qualify_ambiguous_links,
        cache_limit = toml::Value::String(cfg.cache_limit.clone()),
        google_client_json = optional_path_line(
            "google_client_json",
            cfg.google_client_json.as_deref(),
            "/path/to/client_secret.json"
        ),
        github_api_url = optional_string_line(
            "github_api_url",
            cfg.github_api_url.as_deref(),
            "http://127.0.0.1:8080"
        ),
        drive_api_url = optional_string_line(
            "drive_api_url",
            cfg.drive_api_url.as_deref(),
            "http://127.0.0.1:8081"
        ),
        test_credential_file = optional_path_line(
            "test_credential_file",
            cfg.test_credential_file.as_deref(),
            "/path/inside/data_dir/credentials.json"
        ),
    )
}

/// `key = "value"` when `value` is set, else the same line commented out
/// with `example`. Values go through `toml::Value` for escaping.
fn optional_string_line(key: &str, value: Option<&str>, example: &str) -> String {
    match value {
        Some(v) => format!("{key} = {}", toml::Value::String(v.to_string())),
        None => format!("# {key} = {}", toml::Value::String(example.to_string())),
    }
}

fn optional_path_line(key: &str, value: Option<&Path>, example: &str) -> String {
    optional_string_line(
        key,
        value.map(|p| p.display().to_string()).as_deref(),
        example,
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
        let config: Config =
            toml::from_str(&raw).map_err(|e| ConfigError::Parse(ParseError::new(&e, &raw)))?;
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
                assert!(reason.starts_with("listen_addr "), "{reason}");
                assert!(!reason.contains("0.0.0.0:4812"), "{reason}");
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
                assert!(reason.starts_with("log_level "), "{reason}");
                assert!(!reason.contains("loud"), "{reason}");
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
    // --- remote-source keys: cache_limit, google_client_json, the
    // test-only API URLs and test_credential_file ---

    const NEW_KEYS: [&str; 5] = [
        "cache_limit",
        "google_client_json",
        "github_api_url",
        "drive_api_url",
        "test_credential_file",
    ];

    #[test]
    fn a_file_written_before_the_new_keys_loads_with_defaults_and_is_not_rewritten() {
        let (result, after) = load_text(THREE_KEYS);
        let cfg = result.expect("old file loads").config;
        assert_eq!(cfg.cache_limit, "10 GB");
        assert_eq!(cfg.cache_limit_bytes(), Ok(10_000_000_000));
        assert_eq!(
            cfg.cache_limit_bytes(),
            Ok(tephrad::cache_limit::DEFAULT_CACHE_LIMIT_BYTES)
        );
        assert_eq!(cfg.google_client_json, None);
        assert_eq!(cfg.github_api_url, None);
        assert_eq!(cfg.drive_api_url, None);
        assert_eq!(cfg.test_credential_file, None);
        assert!(!cfg.qualify_ambiguous_links);
        assert_eq!(after, THREE_KEYS);
    }

    #[test]
    fn the_four_key_file_of_the_previous_version_also_loads() {
        let text = format!("{THREE_KEYS}qualify_ambiguous_links = true\n");
        let cfg = load_text(&text).0.expect("loads").config;
        assert!(cfg.qualify_ambiguous_links);
        assert_eq!(cfg.cache_limit_bytes(), Ok(10_000_000_000));
    }

    #[test]
    fn cache_limit_values_load_and_convert_to_bytes() {
        for (text, bytes) in [
            ("500 MB", 500_000_000u64),
            ("1.5 GiB", 1_610_612_736),
            ("2GB", 2_000_000_000),
            ("64 MiB", 67_108_864),
        ] {
            let (result, _) = load_text(&format!("{THREE_KEYS}cache_limit = \"{text}\"\n"));
            let cfg = result.unwrap_or_else(|e| panic!("{text}: {e}")).config;
            assert_eq!(cfg.cache_limit, text);
            assert_eq!(cfg.cache_limit_bytes(), Ok(bytes), "{text}");
        }
    }

    #[test]
    fn every_bad_cache_limit_is_a_startup_error_naming_the_key() {
        for bad in [
            "",
            "   ",
            "5000",
            "5 KB",
            "5 gb",
            "-1 GB",
            "0 GB",
            "0",
            "5  GB",
            " 5 GB",
            ".5 GB",
            "1e3 MB",
            "ten GB",
            "99999999999999999999 GB",
            "18446744073710 MB",
        ] {
            let text = format!("{THREE_KEYS}cache_limit = \"{bad}\"\n");
            let (result, after) = load_text(&text);
            match result {
                Err(ConfigError::InvalidCacheLimit(msg)) => {
                    assert!(msg.starts_with("cache_limit "), "{bad:?}: {msg}");
                }
                other => panic!("{bad:?}: expected InvalidCacheLimit, got {other:?}"),
            }
            assert_eq!(after, text, "{bad:?}: the user's file is left untouched");
        }
    }

    /// A secret typed into a size, a log level, a listen address or an
    /// API URL host must not come back in the error text (stderr, log).
    #[test]
    fn no_value_error_echoes_the_value() {
        const S: &str = "ghp_SENTINEL_do_not_leak";
        for line in [
            format!("cache_limit = \"{S}\""),
            format!("cache_limit = \"5 {S}\""),
            format!("log_level = \"{S}\""),
            format!("listen_addr = \"{S}\""),
            format!("listen_addr = \"0.0.0.0:1\" # {S}"),
            format!("github_api_url = \"http://{S}.evil.com/\""),
            format!("drive_api_url = \"http://{S}.evil.com:80/x\""),
        ] {
            let text = format!(
                "listen_addr = \"127.0.0.1:4812\"\ndata_dir = \"/d\"\nlog_level = \"info\"\n{line}\n"
            );
            // A duplicate key is a parse error; those lines replace the
            // earlier one by being appended to a text without it.
            let text = if line.starts_with("listen_addr") {
                format!("data_dir = \"/d\"\nlog_level = \"info\"\n{line}\n")
            } else if line.starts_with("log_level") {
                format!("listen_addr = \"127.0.0.1:4812\"\ndata_dir = \"/d\"\n{line}\n")
            } else {
                text
            };
            let (result, _) = load_text(&text);
            let err = result.expect_err(&line);
            assert!(!err.to_string().contains("SENTINEL"), "{line}: {err}");
        }
    }

    #[test]
    fn a_non_string_cache_limit_is_a_parse_error_naming_the_key() {
        for value in ["10", "10.5", "true", "[1]"] {
            let (result, _) = load_text(&format!("{THREE_KEYS}cache_limit = {value}\n"));
            match result {
                Err(ConfigError::Parse(e)) => assert!(e.to_string().contains("cache_limit"), "{e}"),
                other => panic!("{value}: expected a parse error, got {other:?}"),
            }
        }
    }

    #[test]
    fn google_client_json_is_an_optional_path() {
        let unset = load_text(THREE_KEYS).0.unwrap().config;
        assert_eq!(unset.google_client_json, None);

        let (set, _) = load_text(&format!(
            "{THREE_KEYS}google_client_json = \"/home/me/client.json\"\n"
        ));
        assert_eq!(
            set.unwrap().config.google_client_json,
            Some(PathBuf::from("/home/me/client.json"))
        );

        let (bad, _) = load_text(&format!("{THREE_KEYS}google_client_json = 5\n"));
        assert!(
            matches!(bad, Err(ConfigError::Parse(e)) if e.to_string().contains("google_client_json"))
        );
    }

    #[test]
    fn api_urls_accept_https_and_loopback_http() {
        for url in [
            "https://api.github.com",
            "https://example.com:8443/base",
            "http://127.0.0.1:8080",
            "http://localhost:9000",
            "http://[::1]:8080",
        ] {
            let (result, _) = load_text(&format!(
                "{THREE_KEYS}github_api_url = \"{url}\"\ndrive_api_url = \"{url}\"\n"
            ));
            let cfg = result.unwrap_or_else(|e| panic!("{url}: {e}")).config;
            assert_eq!(cfg.github_api_url.as_deref(), Some(url));
            assert_eq!(cfg.drive_api_url.as_deref(), Some(url));
        }
    }

    #[test]
    fn a_bad_api_url_is_a_startup_error_naming_its_key() {
        let bad_urls = [
            "http://api.github.com",
            "http://10.0.0.5:80",
            "http://127.0.0.1@evil.example/",
            "ftp://127.0.0.1",
            "api.github.com",
            "",
            "https://",
            "https://example.com:99999",
        ];
        for key in ["github_api_url", "drive_api_url"] {
            for bad in bad_urls {
                let text = format!("{THREE_KEYS}{key} = \"{bad}\"\n");
                let (result, after) = load_text(&text);
                match result {
                    Err(ConfigError::InvalidApiUrl(msg)) => {
                        assert!(msg.starts_with(&format!("{key} ")), "{key} {bad:?}: {msg}");
                    }
                    other => panic!("{key} {bad:?}: expected InvalidApiUrl, got {other:?}"),
                }
                assert_eq!(after, text);
            }
        }
    }

    #[cfg(not(feature = "test-credential-store"))]
    #[test]
    fn test_credential_file_is_a_startup_error_naming_the_key_without_the_feature() {
        let text = format!("{THREE_KEYS}test_credential_file = \"/tmp/creds.json\"\n");
        let (result, after) = load_text(&text);
        match result {
            Err(ConfigError::TestCredentialFileNotBuilt(msg)) => {
                assert!(msg.starts_with("test_credential_file "), "{msg}");
                assert!(msg.contains("test-credential-store"), "{msg}");
            }
            other => panic!("expected TestCredentialFileNotBuilt, got {other:?}"),
        }
        assert_eq!(after, text);
    }

    #[cfg(feature = "test-credential-store")]
    #[test]
    fn test_credential_file_loads_with_the_feature() {
        let text = format!("{THREE_KEYS}test_credential_file = \"/tmp/creds.json\"\n");
        let cfg = load_text(&text).0.expect("loads").config;
        assert_eq!(
            cfg.test_credential_file,
            Some(PathBuf::from("/tmp/creds.json"))
        );
    }

    #[test]
    fn a_token_key_is_still_an_unknown_field_and_no_field_can_hold_a_token() {
        for key in [
            "token",
            "github_token",
            "access_token",
            "api_token",
            "password",
        ] {
            let text = format!("{THREE_KEYS}{key} = \"ghp_SENTINEL\"\n");
            match load_text(&text).0 {
                Err(ConfigError::Parse(e)) => {
                    assert!(e.to_string().contains(key), "{key}: {e}");
                    assert!(e.to_string().contains("unknown field"), "{key}: {e}");
                }
                other => panic!("{key}: expected a parse error, got {other:?}"),
            }
        }
        // The error text carries the key but not the value or its line.
        let text = format!("{THREE_KEYS}token = \"ghp_SENTINEL_do_not_leak\"\n");
        match load_text(&text).0 {
            Err(e @ ConfigError::Parse(_)) => {
                for shown in [e.to_string(), format!("{e:?}")] {
                    assert!(!shown.contains("ghp_SENTINEL_do_not_leak"), "{shown}");
                }
                assert!(e.to_string().contains("(line 4, column 1)"), "{e}");
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
        // A wrong-typed value: serde's own message quotes it, ours does not.
        let text = format!("{THREE_KEYS}qualify_ambiguous_links = \"ghp_SENTINEL_do_not_leak\"\n");
        match load_text(&text).0 {
            Err(e @ ConfigError::Parse(_)) => {
                for shown in [e.to_string(), format!("{e:?}")] {
                    assert!(!shown.contains("ghp_SENTINEL_do_not_leak"), "{shown}");
                }
                assert!(e.to_string().contains("qualify_ambiguous_links"), "{e}");
                assert!(e.to_string().contains("expected a boolean"), "{e}");
            }
            other => panic!("expected a parse error, got {other:?}"),
        }
        // Every key of a fully populated Config, by name.
        let full = Config {
            google_client_json: Some(PathBuf::from("/c.json")),
            github_api_url: Some("https://a.example".into()),
            drive_api_url: Some("https://b.example".into()),
            test_credential_file: Some(PathBuf::from("/d/c.json")),
            ..Config::defaults(PathBuf::from("/d"))
        };
        let table: toml::Table = toml::from_str(&toml::to_string(&full).unwrap()).unwrap();
        let mut keys: Vec<&str> = table.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "cache_limit",
                "data_dir",
                "drive_api_url",
                "github_api_url",
                "google_client_json",
                "listen_addr",
                "log_level",
                "qualify_ambiguous_links",
                "test_credential_file",
            ]
        );
    }

    #[test]
    fn the_new_keys_round_trip_through_toml() {
        let full = Config {
            cache_limit: "1.5 GiB".into(),
            google_client_json: Some(PathBuf::from("/home/me/client.json")),
            github_api_url: Some("http://127.0.0.1:8080".into()),
            drive_api_url: Some("https://drive.example/v3".into()),
            test_credential_file: Some(PathBuf::from("/d/c.json")),
            ..Config::defaults(PathBuf::from("/tmp/tephra-data"))
        };
        let back: Config = toml::from_str(&toml::to_string(&full).unwrap()).unwrap();
        assert_eq!(back, full);
        let from_defaults = Config::defaults(PathBuf::from("/tmp/tephra-data"));
        let back: Config = toml::from_str(&toml::to_string(&from_defaults).unwrap()).unwrap();
        assert_eq!(back, from_defaults);
    }

    #[test]
    fn the_generated_config_comments_every_new_key_and_marks_the_test_only_ones() {
        let rendered = render_default_toml(&Config::defaults(PathBuf::from("/tmp/tephra-data")));
        for key in NEW_KEYS {
            assert!(
                rendered.contains(&format!("{key} = ")),
                "{key} appears (live or as a commented example):\n{rendered}"
            );
        }
        assert!(rendered.contains("# cache_limit: "), "{rendered}");
        assert!(rendered.contains("# google_client_json: "), "{rendered}");
        assert!(
            rendered.contains("# TEST ONLY - github_api_url / drive_api_url: "),
            "{rendered}"
        );
        assert!(
            rendered.contains("# TEST ONLY - test_credential_file: "),
            "{rendered}"
        );
        // The default text is live; the optional keys are examples only.
        assert!(
            rendered.contains("\ncache_limit = \"10 GB\"\n"),
            "{rendered}"
        );
        for key in &NEW_KEYS[1..] {
            assert!(
                !rendered
                    .lines()
                    .any(|l| l.starts_with(&format!("{key} = "))),
                "{key} must be commented out by default:\n{rendered}"
            );
        }
        // And it parses to exactly the defaults.
        let parsed: Config = toml::from_str(&rendered).expect("generated config parses");
        assert_eq!(parsed, Config::defaults(PathBuf::from("/tmp/tephra-data")));
    }

    #[test]
    fn a_generated_config_with_every_key_set_escapes_and_parses_back() {
        let full = Config {
            cache_limit: "2 GB".into(),
            google_client_json: Some(PathBuf::from("/odd \"dir\"\\client.json")),
            github_api_url: Some("http://127.0.0.1:8080".into()),
            drive_api_url: Some("http://localhost:8081".into()),
            test_credential_file: Some(PathBuf::from("/d/cred.json")),
            ..Config::defaults(PathBuf::from("/tmp/tephra-data"))
        };
        let rendered = render_default_toml(&full);
        assert_eq!(toml::from_str::<Config>(&rendered).expect("parses"), full);
    }

    #[test]
    fn listen_addr_log_level_and_unknown_keys_behave_as_before_with_the_new_keys_present() {
        let (bad_addr, _) = load_text(
            "listen_addr = \"0.0.0.0:1\"\ndata_dir = \"/d\"\nlog_level = \"info\"\ncache_limit = \"1 GB\"\n",
        );
        assert!(matches!(bad_addr, Err(ConfigError::InvalidListenAddr(_))));
        let (bad_level, _) = load_text(
            "listen_addr = \"127.0.0.1:1\"\ndata_dir = \"/d\"\nlog_level = \"loud\"\ncache_limit = \"1 GB\"\n",
        );
        assert!(matches!(bad_level, Err(ConfigError::InvalidLogLevel(_))));
        let (unknown, _) = load_text(&format!("{THREE_KEYS}cache_limits = \"1 GB\"\n"));
        assert!(matches!(unknown, Err(ConfigError::Parse(_))));
    }
}
