//! The `auth` commands: `auth github`, `auth google`, `auth status` and
//! `auth logout`.
//!
//! They sign in to the two providers and keep the result in the
//! credential store (`tephra_remote::credentials`: the macOS Keychain,
//! one item per provider). They talk to GitHub, Google and the store,
//! never to the daemon, and add no RPC method. No source exists yet, so
//! "sources go `auth_required`" is delivered as the store state the
//! sources read later: `auth_required` here means no item, or a store
//! that could not be read.
//!
//! Secrets: a token is read from the terminal or a pipe, never from
//! argv; it goes to the store and to the provider's `Authorization`
//! header (GitHub) or token request (Google) and nowhere else. No
//! message here repeats a token, a code, a verifier, a client secret or
//! any text a server sent. There is no plaintext fallback: when the
//! store fails, the command fails.
//!
//! Everything a test needs to replace is a field of [`AuthEnv`]: the
//! store, the token input, the browser opener, the consent timeout and
//! the config values. [`run_from_environment`] builds the real one.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use tephra_remote::credentials::{
    credential_state, open_store, CredentialError, CredentialState, CredentialStore, Provider,
};

use crate::cli::Command;

pub mod config;
pub mod github;
pub mod google;
pub mod pkce;
pub mod prompt;

/// How long `auth google` waits for the consent: 5 minutes.
pub const CONSENT_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// Opens a URL in the user's browser. Failure is reported, not fatal.
pub type Opener<'a> = &'a (dyn Fn(&str) -> std::io::Result<()> + Sync);

/// Everything an `auth` command depends on.
pub struct AuthEnv<'a> {
    pub store: &'a dyn CredentialStore,
    pub input: &'a mut dyn prompt::TokenInput,
    pub opener: Opener<'a>,
    /// The `github_api_url` config value.
    pub github_base_url: Option<String>,
    /// The `google_client_json` config value.
    pub google_client_json: Option<PathBuf>,
    /// The config file path, named in the Google setup message.
    pub config_path: PathBuf,
    /// How long `auth google` waits for the redirect.
    pub consent_timeout: Duration,
}

/// Whether `command` is one of the `auth` commands.
pub fn is_auth_command(command: &Command) -> bool {
    matches!(
        command,
        Command::AuthGithub { .. }
            | Command::AuthGoogle
            | Command::AuthStatus
            | Command::AuthLogout { .. }
    )
}

/// The message for any failure of the store. The reason is the store
/// error's own text, which holds fixed words and a platform status code.
pub(crate) fn keychain_unavailable(e: &CredentialError) -> String {
    format!("Keychain unavailable ({e}). Nothing was stored, and no other place is used instead.")
}

/// Text from outside, made safe to print: control characters (a
/// terminal escape sequence) become `?`, and it is cut at 64 characters.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .take(64)
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// Runs one `auth` command. Returns the exit code: 0 done, 1 failed.
/// (A usage error never gets here: it is exit 2 from `parse_args`.)
pub fn run(
    command: &Command,
    json: bool,
    env: &mut AuthEnv<'_>,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match command {
        Command::AuthGithub { stdin } => github::run(env, *stdin, out, err),
        Command::AuthGoogle => google::run(env, out, err),
        Command::AuthStatus => status(env.store, json, out),
        Command::AuthLogout { provider } => logout(env.store, *provider, out, err),
        _ => {
            let _ = writeln!(err, "tephractl: not an auth command");
            1
        }
    }
}

/// The state word of a provider, in the text and in the JSON alike:
/// `signed in` or `auth_required`.
fn state_word(state: &CredentialState) -> &'static str {
    match state {
        CredentialState::SignedIn => "signed in",
        CredentialState::SignedOut | CredentialState::AuthRequired(_) => "auth_required",
    }
}

/// `auth status`: one line per provider, from the store only.
fn status(store: &dyn CredentialStore, json: bool, out: &mut dyn Write) -> i32 {
    let states: Vec<(Provider, CredentialState)> = Provider::ALL
        .iter()
        .map(|p| (*p, credential_state(store, *p)))
        .collect();
    render_status(&states, json, out);
    0
}

fn render_status(states: &[(Provider, CredentialState)], json: bool, out: &mut dyn Write) {
    if json {
        let mut map = serde_json::Map::new();
        for (provider, state) in states {
            map.insert(
                provider.as_str().to_string(),
                serde_json::json!({ "state": state_word(state) }),
            );
        }
        let _ = writeln!(out, "{}", serde_json::Value::Object(map));
        return;
    }
    for (provider, state) in states {
        match state {
            CredentialState::AuthRequired(why) => {
                let _ = writeln!(
                    out,
                    "{}: auth_required ({})",
                    provider.as_str(),
                    printable(why)
                );
            }
            other => {
                let _ = writeln!(out, "{}: {}", provider.as_str(), state_word(other));
            }
        }
    }
}

/// `auth logout <provider>`: deletes that provider's item. Local only.
fn logout(
    store: &dyn CredentialStore,
    provider: Provider,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    match store.get(provider) {
        Ok(None) => {
            let _ = writeln!(out, "not signed in to {provider}");
            return 0;
        }
        Ok(Some(_)) => {}
        Err(e) => {
            let _ = writeln!(err, "tephractl: {}", keychain_unavailable(&e));
            return 1;
        }
    }
    match store.delete(provider) {
        Ok(()) => {
            let _ = writeln!(
                out,
                "signed out of {provider} (local only: the token is not revoked at the provider)"
            );
            0
        }
        Err(e) => {
            let _ = writeln!(err, "tephractl: {}", keychain_unavailable(&e));
            1
        }
    }
}

/// A store that fails every call with one error, for a store that could
/// not even be opened.
struct FailedStore(CredentialError);

impl CredentialStore for FailedStore {
    fn get(
        &self,
        _: Provider,
    ) -> Result<Option<tephra_remote::credentials::Secret>, CredentialError> {
        Err(self.0.clone())
    }
    fn set(
        &self,
        _: Provider,
        _: &tephra_remote::credentials::Secret,
    ) -> Result<(), CredentialError> {
        Err(self.0.clone())
    }
    fn delete(&self, _: Provider) -> Result<(), CredentialError> {
        Err(self.0.clone())
    }
}

/// The browser launch: `open` on macOS, `xdg-open` elsewhere, as a plain
/// child process with the URL as its one argument (no shell). The child
/// is reaped on a thread so it never lingers as a zombie.
pub fn open_in_browser(url: &str) -> std::io::Result<()> {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(program)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

/// Runs an `auth` command the way `main` does: config from
/// `config.toml`, the real store, the real terminal and browser.
pub fn run_from_environment(
    command: &Command,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let config_path = crate::paths::default_config_path();
    let loaded = config::load(&config_path);
    let (cfg, config_error) = match loaded {
        Ok(cfg) => (cfg, None),
        Err(e) => (config::AuthConfig::default(), Some(e)),
    };

    // A config file that cannot be read decides which store is meant, so
    // no command guesses: `status` reports `auth_required` for both
    // providers (it exits 0 either way), every other command fails.
    let store: Box<dyn CredentialStore> = match &config_error {
        Some(e) => {
            let _ = writeln!(err, "tephractl: {e}");
            if !matches!(command, Command::AuthStatus) {
                return 1;
            }
            Box::new(FailedStore(CredentialError::Unavailable))
        }
        None => match open_store(
            cfg.test_credential_file.as_deref(),
            &crate::paths::data_dir(),
        ) {
            Ok((store, _kind)) => store,
            Err(e) => Box::new(FailedStore(e)),
        },
    };

    let mut input = prompt::StdinInput;
    let mut env = AuthEnv {
        store: store.as_ref(),
        input: &mut input,
        opener: &open_in_browser,
        github_base_url: cfg.github_api_url,
        google_client_json: cfg.google_client_json,
        config_path,
        consent_timeout: CONSENT_TIMEOUT,
    };
    run(command, json, &mut env, out, err)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printable_replaces_control_characters_and_cuts_long_text() {
        assert_eq!(printable("dan\u{1b}[31m"), "dan?[31m");
        assert_eq!(printable(&"x".repeat(200)).len(), 64);
        assert_eq!(printable("a\nb"), "a?b");
    }

    #[test]
    fn status_json_uses_the_two_state_words() {
        let mut out = Vec::new();
        render_status(
            &[
                (Provider::Github, CredentialState::SignedIn),
                (
                    Provider::Google,
                    CredentialState::AuthRequired("keychain locked".into()),
                ),
            ],
            true,
            &mut out,
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["github"]["state"], "signed in");
        assert_eq!(v["google"]["state"], "auth_required");
    }
}
