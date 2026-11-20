//! Shared pieces for the `auth` tests (`remote_auth`, `google_loopback`,
//! `auth_secrets`): in-memory and failing credential stores, a scripted
//! token input, the fake servers on their own runtime, a harness that
//! runs a command in process, and a tiny HTTP client the test opener
//! uses to follow the consent redirect.
//!
//! Nothing here touches the real Keychain, the network beyond loopback
//! ports the fakes bind, or the user's config and data directories.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tephra_remote::credentials::{CredentialError, CredentialStore, Provider, Secret};
use tephra_remote::testing::{FakeGithub, FakeGoogleOAuth};
use tephractl::auth::prompt::{InputError, TokenInput};
use tephractl::auth::{self, AuthEnv};
use tephractl::cli::{self, Action, Command};

/// An in-memory credential store that counts its writes.
#[derive(Default)]
pub struct MemStore {
    items: Mutex<HashMap<&'static str, String>>,
    pub sets: Mutex<u32>,
    pub deletes: Mutex<u32>,
}

impl MemStore {
    pub fn get_text(&self, provider: Provider) -> Option<String> {
        self.items.lock().unwrap().get(provider.as_str()).cloned()
    }
    pub fn len(&self) -> usize {
        self.items.lock().unwrap().len()
    }
    pub fn put(&self, provider: Provider, text: &str) {
        self.items
            .lock()
            .unwrap()
            .insert(provider.as_str(), text.to_string());
    }
    pub fn set_count(&self) -> u32 {
        *self.sets.lock().unwrap()
    }
}

impl CredentialStore for MemStore {
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError> {
        Ok(self
            .items
            .lock()
            .unwrap()
            .get(provider.as_str())
            .map(|t| Secret::new(t.as_str())))
    }
    fn set(&self, provider: Provider, secret: &Secret) -> Result<(), CredentialError> {
        *self.sets.lock().unwrap() += 1;
        self.items
            .lock()
            .unwrap()
            .insert(provider.as_str(), secret.expose().to_string());
        Ok(())
    }
    fn delete(&self, provider: Provider) -> Result<(), CredentialError> {
        *self.deletes.lock().unwrap() += 1;
        self.items.lock().unwrap().remove(provider.as_str());
        Ok(())
    }
}

/// Which calls of a [`FailingStore`] fail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailOn {
    /// Every call.
    All,
    /// Only `set` (reads work, so the command gets as far as storing).
    SetOnly,
}

/// A store that implements the PR-1 trait and fails with `error`.
pub struct FailingStore {
    pub error: CredentialError,
    pub on: FailOn,
    pub inner: MemStore,
}

impl FailingStore {
    pub fn new(error: CredentialError, on: FailOn) -> FailingStore {
        FailingStore {
            error,
            on,
            inner: MemStore::default(),
        }
    }
}

impl CredentialStore for FailingStore {
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError> {
        match self.on {
            FailOn::All => Err(self.error.clone()),
            FailOn::SetOnly => self.inner.get(provider),
        }
    }
    fn set(&self, _: Provider, _: &Secret) -> Result<(), CredentialError> {
        Err(self.error.clone())
    }
    fn delete(&self, _: Provider) -> Result<(), CredentialError> {
        Err(self.error.clone())
    }
}

/// A token input with a fixed answer.
pub struct ScriptedInput {
    pub terminal: bool,
    pub hidden: Option<String>,
    pub line: Option<String>,
    pub asked_hidden: bool,
    pub asked_line: bool,
}

impl ScriptedInput {
    /// A terminal that will type `token` at the hidden prompt.
    pub fn typed(token: &str) -> ScriptedInput {
        ScriptedInput {
            terminal: true,
            hidden: Some(token.to_string()),
            line: None,
            asked_hidden: false,
            asked_line: false,
        }
    }
    /// A pipe holding `text` (the line, with its ending as given).
    pub fn piped(text: &str) -> ScriptedInput {
        ScriptedInput {
            terminal: false,
            hidden: None,
            line: Some(text.to_string()),
            asked_hidden: false,
            asked_line: false,
        }
    }
    /// A pipe with nothing in it and no terminal.
    pub fn none() -> ScriptedInput {
        ScriptedInput {
            terminal: false,
            hidden: None,
            line: None,
            asked_hidden: false,
            asked_line: false,
        }
    }
}

impl TokenInput for ScriptedInput {
    fn is_terminal(&self) -> bool {
        self.terminal
    }
    fn read_hidden(&mut self, _prompt: &str) -> Result<String, InputError> {
        self.asked_hidden = true;
        self.hidden.clone().ok_or(InputError::Eof)
    }
    fn read_line(&mut self) -> Result<String, InputError> {
        self.asked_line = true;
        // Same rule as the real reader: strip one line ending.
        match &self.line {
            None => Err(InputError::Eof),
            Some(text) => {
                let first = text.split('\n').next().unwrap_or("");
                Ok(first.strip_suffix('\r').unwrap_or(first).to_string())
            }
        }
    }
}

/// The fakes, on their own multi-thread runtime.
pub struct Fakes {
    pub rt: tokio::runtime::Runtime,
    pub github: FakeGithub,
    pub google: FakeGoogleOAuth,
}

impl Fakes {
    pub fn start() -> Fakes {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let github = rt.block_on(FakeGithub::start());
        let google = rt.block_on(FakeGoogleOAuth::start());
        Fakes { rt, github, google }
    }
}

/// What a command printed and returned.
#[derive(Debug, Clone)]
pub struct Ran {
    pub code: i32,
    pub out: String,
    pub err: String,
}

/// The pieces of one run of an `auth` command.
pub struct Harness {
    pub store: Arc<dyn CredentialStore>,
    pub input: ScriptedInput,
    pub github_base_url: Option<String>,
    pub client_json: Option<PathBuf>,
    pub config_path: PathBuf,
    pub timeout: Duration,
}

impl Harness {
    pub fn new(store: Arc<dyn CredentialStore>, config_dir: &Path) -> Harness {
        Harness {
            store,
            input: ScriptedInput::none(),
            github_base_url: None,
            client_json: None,
            config_path: config_dir.join("config.toml"),
            timeout: Duration::from_secs(5),
        }
    }

    /// Runs `args` (as typed after `tephractl`) through the real parser
    /// and the real command code, with `opener` as the browser.
    pub fn run(
        &mut self,
        args: &[&str],
        opener: &(dyn Fn(&str) -> std::io::Result<()> + Sync),
    ) -> Ran {
        let parsed = match cli::parse_args(args.iter().copied()).expect("a usage error") {
            Action::Run(p) => p,
            Action::Help => panic!("help"),
        };
        assert!(
            auth::is_auth_command(&parsed.command),
            "{:?}",
            parsed.command
        );
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut env = AuthEnv {
            store: self.store.as_ref(),
            input: &mut self.input,
            opener,
            github_base_url: self.github_base_url.clone(),
            google_client_json: self.client_json.clone(),
            config_path: self.config_path.clone(),
            consent_timeout: self.timeout,
        };
        let code = auth::run(&parsed.command, parsed.json, &mut env, &mut out, &mut err);
        Ran {
            code,
            out: String::from_utf8(out).unwrap(),
            err: String::from_utf8(err).unwrap(),
        }
    }

    /// Runs a command that needs no browser.
    pub fn run_plain(&mut self, args: &[&str]) -> Ran {
        self.run(args, &|_| panic!("no browser expected"))
    }
}

pub fn parse(args: &[&str]) -> Command {
    match cli::parse_args(args.iter().copied()).expect("a usage error") {
        Action::Run(p) => p.command,
        Action::Help => panic!("help"),
    }
}

/// A free loopback port nothing listens on (bound, noted, released).
pub fn closed_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// What a raw HTTP request got back.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub location: Option<String>,
    pub body: String,
}

/// `METHOD url` over a plain TCP socket (no redirects, no proxy). The
/// `Host` header is the URL's authority unless `host` overrides it.
pub fn http(method: &str, url: &str, host: Option<&str>) -> std::io::Result<Reply> {
    let rest = url.strip_prefix("http://").expect("an http url");
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let mut stream = TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
        host.unwrap_or(authority)
    )?;
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("no status line"))?;
    let location = lines.find_map(|l| {
        let (n, v) = l.split_once(':')?;
        n.eq_ignore_ascii_case("location")
            .then(|| v.trim().to_string())
    });
    Ok(Reply {
        status,
        location,
        body: body.to_string(),
    })
}

/// The consent URL in a run's stdout: the first `http` line.
pub fn consent_url_in(out: &str) -> String {
    out.lines()
        .find(|l| l.starts_with("http"))
        .expect("a consent URL line")
        .to_string()
}

/// The loopback port a run said it listens on.
pub fn port_in(out: &str) -> u16 {
    let marker = "127.0.0.1:";
    let line = out.lines().find(|l| l.contains("Waiting up to")).unwrap();
    let rest = &line[line.find(marker).unwrap() + marker.len()..];
    rest.split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

/// The value of query parameter `key` in `url`, percent-decoded.
pub fn query_param(url: &str, key: &str) -> Option<String> {
    let query = url.split_once('?')?.1;
    query.split('&').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k == key).then(|| {
            percent_encoding::percent_decode_str(v)
                .decode_utf8_lossy()
                .into_owned()
        })
    })
}

/// Every file under `root`, as `(path, bytes)`.
pub fn files_under(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(bytes) = std::fs::read(&path) {
                found.push((path, bytes));
            }
        }
    }
    found
}

pub fn contains_bytes(haystack: &[u8], needle: &str) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|w| w == needle.as_bytes())
}
