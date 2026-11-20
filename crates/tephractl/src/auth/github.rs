//! `auth github`: read a token, check it with `GET /user`, and store it
//! only when GitHub says 200.
//!
//! The check uses the PR-3 [`GithubClient`] (so the base-URL rule, the
//! 401/403/rate-limit mapping and the no-redirect rule are the adapter's)
//! with a [`CandidateStore`]: a one-secret in-memory store that holds the
//! token under test. Nothing reaches the real store until the answer is
//! 200.

use std::io::Write;
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use tephra_remote::credentials::{CredentialError, CredentialStore, Provider, Secret};
use tephra_remote::github::{GithubAdapter, GithubClient};
use tephra_remote::{RemoteError, SystemClock};

use super::prompt::InputError;
use super::{keychain_unavailable, printable, AuthEnv};

const PROMPT: &str = "GitHub token (input is hidden): ";

/// The longest token accepted. GitHub's are under 100 characters.
const MAX_TOKEN_LEN: usize = 1024;

/// Why a token was refused before anything was sent. Fixed text, never
/// the token.
pub fn check_token_shape(token: &str) -> Result<(), &'static str> {
    if token.is_empty() {
        return Err("the token is empty");
    }
    if token.len() > MAX_TOKEN_LEN {
        return Err("the token is too long");
    }
    if token.chars().any(char::is_whitespace) {
        return Err("the token contains whitespace (check what was pasted)");
    }
    if !token.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err("the token contains characters a GitHub token never has");
    }
    Ok(())
}

/// A store holding only the candidate token. Read-only: `set` and
/// `delete` fail, so the adapter cannot write anywhere with it.
struct CandidateStore(String);

impl CredentialStore for CandidateStore {
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError> {
        Ok((provider == Provider::Github).then(|| Secret::new(self.0.as_str())))
    }

    fn set(&self, _: Provider, _: &Secret) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }

    fn delete(&self, _: Provider) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
}

impl Drop for CandidateStore {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        bytes.iter_mut().for_each(|b| *b = 0);
        std::hint::black_box(&bytes);
    }
}

/// The sentence for a failed check. Names the reason; carries nothing
/// the server sent.
fn reason(error: &RemoteError) -> String {
    match error {
        RemoteError::AuthRejected => "GitHub rejected the token (401). Nothing was stored.".into(),
        RemoteError::PermissionDenied => {
            "GitHub refused the token (403: forbidden). Nothing was stored.".into()
        }
        RemoteError::RateLimited { retry_at } => {
            let secs = retry_at
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!(
                "GitHub's rate limit is reached (403/429); try again after {:02}:{:02} UTC. \
                 Nothing was stored.",
                (secs % 86_400) / 3_600,
                (secs % 3_600) / 60
            )
        }
        RemoteError::Unreachable(cause) => format!(
            "could not reach GitHub ({}). Nothing was stored.",
            cause.as_str()
        ),
        RemoteError::Server { status } => {
            format!("GitHub answered with a server error ({status}). Nothing was stored.")
        }
        RemoteError::Moved { .. } => {
            "GitHub answered with a redirect, which is not followed. Nothing was stored.".into()
        }
        RemoteError::NotFound
        | RemoteError::Unexpected { .. }
        | RemoteError::BadResponse(_)
        | RemoteError::InvalidInput(_) => {
            "GitHub gave an answer this check does not understand. Nothing was stored.".into()
        }
    }
}

/// `auth github [-]`. Returns the exit code.
pub fn run(
    env: &mut AuthEnv<'_>,
    from_stdin: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    // The store first: no point asking for a token it could not keep.
    if let Err(e) = env.store.get(Provider::Github) {
        let _ = writeln!(err, "tephractl: {}", keychain_unavailable(&e));
        return 1;
    }

    let read = if from_stdin {
        env.input.read_line()
    } else if env.input.is_terminal() {
        env.input.read_hidden(PROMPT)
    } else {
        let _ = writeln!(
            err,
            "tephractl: stdin is not a terminal, so the token cannot be typed. \
             Pipe it in with `tephractl auth github -`."
        );
        return 1;
    };
    let token = match read {
        Ok(t) => t,
        Err(InputError::Interrupted) => {
            let _ = writeln!(err, "tephractl: cancelled; nothing was stored.");
            return 1;
        }
        Err(e) => {
            let _ = writeln!(err, "tephractl: {e}; nothing was stored.");
            return 1;
        }
    };
    if let Err(why) = check_token_shape(&token) {
        let _ = writeln!(err, "tephractl: {why}; nothing was sent or stored.");
        return 1;
    }

    let candidate = Secret::new(token.as_str());
    let adapter = match GithubClient::new(
        env.github_base_url.as_deref(),
        Arc::new(CandidateStore(token)),
        Arc::new(SystemClock),
    ) {
        Ok(a) => a,
        Err(e) => {
            let _ = writeln!(err, "tephractl: github_api_url is not usable: {e}");
            return 1;
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(_) => {
            let _ = writeln!(err, "tephractl: cannot start the network runtime");
            return 1;
        }
    };
    let user = match runtime.block_on(adapter.whoami()) {
        Ok(user) => user,
        Err(e) => {
            let _ = writeln!(err, "tephractl: {}", reason(&e));
            return 1;
        }
    };

    if let Err(e) = env.store.set(Provider::Github, &candidate) {
        let _ = writeln!(err, "tephractl: {}", keychain_unavailable(&e));
        return 1;
    }
    let _ = writeln!(out, "signed in as {}", printable(&user.login));
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_accepts_real_token_formats() {
        for ok in [
            "ghp_0123456789abcdefghijklmnopqrstuvwxyzAB",
            "github_pat_11AAAA_bbbb",
        ] {
            assert_eq!(check_token_shape(ok), Ok(()), "{ok}");
        }
    }

    #[test]
    fn shape_refuses_empty_whitespace_control_and_non_ascii() {
        for bad in [
            "", " ", "a b", " ab", "ab ", "a\tb", "a\nb", "a\r", "a\u{0}b", "tökén", "a\u{7f}",
        ] {
            assert!(check_token_shape(bad).is_err(), "{bad:?}");
        }
        assert!(check_token_shape(&"a".repeat(MAX_TOKEN_LEN + 1)).is_err());
    }

    #[test]
    fn shape_messages_never_repeat_the_token() {
        for bad in ["a b ghp_X", "ghp_X\u{0}"] {
            let why = check_token_shape(bad).unwrap_err();
            assert!(!why.contains("ghp_X"), "{why}");
        }
    }
}
