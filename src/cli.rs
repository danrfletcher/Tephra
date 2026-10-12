//! Minimal CLI flag parsing: `--config <path>` and `-h`/`--help`.
//!
//! Deliberately hand-rolled rather than pulling in an argument-parsing
//! crate, since this PR pins `tokio`, `serde`, `toml`, `rand`, and
//! `tracing` as its only runtime dependencies.

use std::fmt;
use std::path::PathBuf;

pub const USAGE: &str = "\
tephrad - federated graph and link engine daemon

USAGE:
    tephrad [OPTIONS]

OPTIONS:
    --config <path>    Path to the config file (default: ~/.config/tephra/config.toml)
    -h, --help          Print this help and exit
";

#[derive(Debug, PartialEq)]
pub enum Action {
    /// Print usage and exit 0 without touching disk or binding the listener.
    Help,
    /// Run the daemon, loading config from the given path.
    Run { config_path: PathBuf },
}

#[derive(Debug, PartialEq)]
pub struct ArgsError(pub String);

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Parses CLI args (excluding argv[0]) into an [`Action`].
///
/// `default_config_path` is injected rather than resolved internally so
/// this function stays a pure, directly testable mapping from args to
/// action — path resolution (which touches `HOME`/XDG env vars) lives in
/// `paths.rs` and is applied by the caller only when no `--config` flag
/// is given.
pub fn parse_args<I, S>(args: I, default_config_path: PathBuf) -> Result<Action, ArgsError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut config_path: Option<PathBuf> = None;
    let mut iter = args.into_iter();

    while let Some(arg) = iter.next() {
        let arg = arg.as_ref();
        match arg {
            "-h" | "--help" => return Ok(Action::Help),
            "--config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| ArgsError("--config requires a path argument".to_string()))?;
                config_path = Some(PathBuf::from(value.as_ref()));
            }
            other => {
                return Err(ArgsError(format!("unrecognized argument: {other}")));
            }
        }
    }

    Ok(Action::Run {
        config_path: config_path.unwrap_or(default_config_path),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_path() -> PathBuf {
        PathBuf::from("/home/user/.config/tephra/config.toml")
    }

    #[test]
    fn no_args_uses_default_config_path() {
        let action = parse_args(Vec::<&str>::new(), default_path()).unwrap();
        assert_eq!(
            action,
            Action::Run {
                config_path: default_path()
            }
        );
    }

    #[test]
    fn help_flag_short() {
        let action = parse_args(["-h"], default_path()).unwrap();
        assert_eq!(action, Action::Help);
    }

    #[test]
    fn help_flag_long() {
        let action = parse_args(["--help"], default_path()).unwrap();
        assert_eq!(action, Action::Help);
    }

    #[test]
    fn config_flag_overrides_default_path() {
        let action = parse_args(["--config", "/tmp/custom.toml"], default_path()).unwrap();
        assert_eq!(
            action,
            Action::Run {
                config_path: PathBuf::from("/tmp/custom.toml")
            }
        );
    }

    #[test]
    fn config_flag_without_value_errors() {
        let result = parse_args(["--config"], default_path());
        assert!(result.is_err());
    }

    #[test]
    fn unknown_flag_errors() {
        let result = parse_args(["--bogus"], default_path());
        assert!(result.is_err());
    }
}
