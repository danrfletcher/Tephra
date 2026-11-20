//! `auth google`: Google sign-in for Drive, by the installed-app flow
//! with a loopback redirect and PKCE (RFC 7636, S256).
//!
//! 1. Read `google_client_json` (the user's own Desktop-app client). Any
//!    problem here is reported, with the setup steps, before a listener
//!    exists.
//! 2. Bind `127.0.0.1:0` and print the consent URL (`access_type=offline`,
//!    `prompt=consent`, scope `drive`, a fresh `state`, the S256
//!    challenge of a fresh verifier); also open it in a browser.
//! 3. Wait for `GET /callback` (at most the consent timeout). A callback
//!    with a wrong or missing `state` is answered 400 and ends nothing;
//!    so is anything else that is not that request.
//! 4. Close the listener, then exchange the code (with the verifier) at
//!    the client's `token_uri`.
//! 5. Store the token set, a JSON object, in the Google credential item.
//!
//! Only this command opens a socket, only on `127.0.0.1`, only for the
//! length of one run: the listener is closed by [`Listener`]'s drop on
//! success, error and timeout. Nothing a server sends is repeated in a
//! message (an `error_description` may echo the code or the verifier).

use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use serde::Deserialize;
use tephra_remote::api_url::{is_loopback_url, validate_api_url};
use tephra_remote::credentials::{Provider, Secret};

use super::pkce;
use super::{keychain_unavailable, AuthEnv};

/// The one scope requested.
pub const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive";

/// The path Google redirects to on the loopback listener.
pub const CALLBACK_PATH: &str = "/callback";

/// Largest request head read from the loopback port.
const MAX_REQUEST_HEAD: usize = 8 * 1024;
/// How long one connection may take to send its request head.
const CONNECTION_READ_TIMEOUT: Duration = Duration::from_secs(2);
/// Connections handled at once; more are dropped unanswered.
const MAX_CONNECTIONS: usize = 32;
/// Largest token endpoint answer read.
const MAX_TOKEN_RESPONSE: u64 = 64 * 1024;

const SETUP_HELP: &str = "\
Google sign-in needs your own Google Cloud OAuth client:
  1. In the Google Cloud Console create an OAuth client ID of type \"Desktop app\"
     (enable the Google Drive API for the project) and download its JSON file.
  2. Set google_client_json = \"/path/to/that/client.json\" in {config}.
  3. Publish the OAuth consent screen \"In production\". A project left in
     \"Testing\" mode gets refresh tokens that Google expires after 7 days,
     and the sign-in would lapse every week.";

fn setup_error(reason: &str, config_path: &Path) -> String {
    format!(
        "tephractl: {reason}\n{}\n",
        SETUP_HELP.replace("{config}", &config_path.display().to_string())
    )
}

/// The Desktop-app client, from the client JSON.
pub struct ClientConfig {
    pub client_id: String,
    client_secret: Secret,
    pub auth_uri: String,
    pub token_uri: String,
}

/// An OAuth endpoint must be Google's (https, a google.com or
/// googleapis.com host) or on this machine (the test servers).
fn endpoint_ok(url: &str) -> bool {
    if validate_api_url(url).is_err() {
        return false;
    }
    if is_loopback_url(url) {
        return true;
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host.ends_with(".google.com") || host.ends_with(".googleapis.com")
}

/// Largest client JSON read.
const MAX_CLIENT_JSON: u64 = 64 * 1024;

/// Reads the client JSON at `path`. The error is a sentence that names
/// the problem and never quotes the file.
pub fn load_client(path: Option<&Path>) -> Result<ClientConfig, String> {
    let Some(path) = path else {
        return Err("google_client_json is not set in the config file".to_string());
    };
    let file = std::fs::File::open(path).map_err(|e| {
        format!(
            "cannot read the Google client file {} ({})",
            path.display(),
            e.kind()
        )
    })?;
    let mut text = String::new();
    file.take(MAX_CLIENT_JSON + 1)
        .read_to_string(&mut text)
        .map_err(|_| format!("{} is not a readable text file", path.display()))?;
    let malformed = || {
        format!(
            "{} is not a Google Desktop-app client JSON (expected an \"installed\" object with \
             client_id, client_secret, auth_uri and token_uri)",
            path.display()
        )
    };
    if text.len() as u64 > MAX_CLIENT_JSON {
        return Err(malformed());
    }
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| malformed())?;
    if value.get("web").is_some() && value.get("installed").is_none() {
        return Err(format!(
            "{} is a Web application client; create a \"Desktop app\" client instead",
            path.display()
        ));
    }
    let installed = value.get("installed").ok_or_else(malformed)?;
    let field = |name: &str| -> Result<String, String> {
        match installed.get(name).and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => Ok(s.to_string()),
            _ => Err(malformed()),
        }
    };
    let client_id = field("client_id")?;
    let client_secret = Secret::new(field("client_secret")?);
    let auth_uri = field("auth_uri")?;
    let token_uri = field("token_uri")?;
    if !endpoint_ok(&auth_uri) || !endpoint_ok(&token_uri) {
        return Err(format!(
            "{} points auth_uri or token_uri somewhere other than Google; \
             the client secret is only sent to Google",
            path.display()
        ));
    }
    Ok(ClientConfig {
        client_id,
        client_secret,
        auth_uri,
        token_uri,
    })
}

/// Everything but letters, digits and `-._~` is percent-encoded.
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

fn encode(text: &str) -> String {
    utf8_percent_encode(text, QUERY_VALUE).to_string()
}

/// The consent URL for one run.
pub fn consent_url(
    client: &ClientConfig,
    redirect_uri: &str,
    state: &str,
    challenge: &str,
) -> String {
    let sep = if client.auth_uri.contains('?') {
        '&'
    } else {
        '?'
    };
    format!(
        "{}{sep}client_id={}&redirect_uri={}&response_type=code&scope={}\
         &access_type=offline&prompt=consent&state={}\
         &code_challenge={}&code_challenge_method=S256",
        client.auth_uri,
        encode(&client.client_id),
        encode(redirect_uri),
        encode(DRIVE_SCOPE),
        encode(state),
        encode(challenge),
    )
}

/// What a valid callback carried.
enum Event {
    Code(Secret),
    /// The `error` value, reduced to lowercase letters and `_`.
    Denied(String),
}

/// State shared by the connection threads.
struct Shared {
    state: String,
    host: String,
    /// Set by the first valid callback; later ones are refused.
    done: AtomicBool,
    live: AtomicUsize,
    tx: Sender<Event>,
}

/// The loopback listener. Dropping it stops the acceptor and closes the
/// port (the drop returns after the socket is closed).
struct Listener {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    fn start(state: String, tx: Sender<Event>) -> std::io::Result<Listener> {
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            state,
            host: format!("127.0.0.1:{port}"),
            done: AtomicBool::new(false),
            live: AtomicUsize::new(0),
            tx,
        });
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let thread = thread::spawn(move || {
            while !stop_flag.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if shared.live.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                            shared.live.fetch_sub(1, Ordering::SeqCst);
                            continue;
                        }
                        let shared = shared.clone();
                        thread::spawn(move || {
                            handle(stream, &shared);
                            shared.live.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Err(_) => thread::sleep(Duration::from_millis(10)),
                }
            }
            // `listener` drops here: the port is closed.
        });
        Ok(Listener {
            port,
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn respond(stream: &mut TcpStream, status: &str, body: &str) {
    let head = format!(
        "HTTP/1.1 {status}\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         Cache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\n\
         X-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}

/// The request head (up to the blank line), or `None` when the client
/// sent nothing usable in time.
fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while buf.len() < MAX_REQUEST_HEAD {
        match stream.read(&mut chunk) {
            Ok(0) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let end = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
            return String::from_utf8(buf[..end].to_vec()).ok();
        }
    }
    None
}

/// The query as `(key, value)` pairs, percent-decoded.
fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            let decode = |s: &str| percent_decode_str(s).decode_utf8_lossy().into_owned();
            (decode(k), decode(v))
        })
        .collect()
}

/// The one value of `key`: `Ok(None)` when absent, `Err` when repeated.
fn single<'a>(pairs: &'a [(String, String)], key: &str) -> Result<Option<&'a str>, ()> {
    let mut found = pairs.iter().filter(|(k, _)| k == key);
    let first = found.next().map(|(_, v)| v.as_str());
    if found.next().is_some() {
        return Err(());
    }
    Ok(first)
}

fn handle(mut stream: TcpStream, shared: &Shared) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(CONNECTION_READ_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CONNECTION_READ_TIMEOUT));
    let Some(head) = read_head(&mut stream) else {
        return;
    };
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap_or("").split(' ');
    let (method, target) = (
        request_line.next().unwrap_or(""),
        request_line.next().unwrap_or(""),
    );
    if method != "GET" {
        return respond(&mut stream, "400 Bad Request", "bad request");
    }
    let host_ok = lines.any(|l| {
        l.split_once(':')
            .is_some_and(|(n, v)| n.eq_ignore_ascii_case("host") && v.trim() == shared.host)
    });
    if !host_ok {
        return respond(&mut stream, "400 Bad Request", "bad request");
    }
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != CALLBACK_PATH {
        return respond(&mut stream, "404 Not Found", "not found");
    }
    let pairs = parse_query(query);
    let (Ok(state), Ok(code), Ok(error)) = (
        single(&pairs, "state"),
        single(&pairs, "code"),
        single(&pairs, "error"),
    ) else {
        return respond(&mut stream, "400 Bad Request", "bad request");
    };
    // The state is checked first, for every outcome: a request that does
    // not carry this run's state cannot cancel or complete it.
    if !state.is_some_and(|s| pkce::constant_time_eq(s, &shared.state)) {
        return respond(&mut stream, "400 Bad Request", "bad request");
    }
    let event = match (error, code) {
        (Some(e), _) => Event::Denied(
            e.chars()
                .filter(|c| c.is_ascii_lowercase() || *c == '_')
                .take(40)
                .collect(),
        ),
        (None, Some(c)) if !c.is_empty() => Event::Code(Secret::new(c)),
        _ => return respond(&mut stream, "400 Bad Request", "bad request"),
    };
    if shared
        .done
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return respond(&mut stream, "404 Not Found", "not found");
    }
    let body = match event {
        Event::Code(_) => {
            "Tephra received the sign-in. You can close this tab and return to the terminal."
        }
        Event::Denied(_) => "Sign-in was not completed. You can close this tab.",
    };
    let _ = shared.tx.send(event);
    respond(&mut stream, "200 OK", body);
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    scope: Option<String>,
    token_type: Option<String>,
    error: Option<String>,
}

/// Exchanges the code. The error is a sentence with nothing the server
/// said in it, except an `error` code reduced to `[a-z_]`.
fn exchange(
    client: &ClientConfig,
    code: &Secret,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Secret, String> {
    let mut builder = reqwest::blocking::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30));
    if is_loopback_url(&client.token_uri) {
        builder = builder.no_proxy();
    }
    let http = builder
        .build()
        .map_err(|_| "cannot set up the HTTP client".to_string())?;
    let form: String = [
        ("grant_type", "authorization_code"),
        ("code", code.expose()),
        ("client_id", client.client_id.as_str()),
        ("client_secret", client.client_secret.expose()),
        ("redirect_uri", redirect_uri),
        ("code_verifier", verifier),
    ]
    .iter()
    .map(|(k, v)| format!("{k}={}", encode(v)))
    .collect::<Vec<_>>()
    .join("&");
    let response = http
        .post(&client.token_uri)
        .header("content-type", "application/x-www-form-urlencoded")
        .body(form)
        .send()
        .map_err(|e| {
            format!(
                "could not reach Google's token endpoint ({})",
                if e.is_timeout() {
                    "timed out"
                } else {
                    "connection failed"
                }
            )
        })?;
    let status = response.status();
    let mut body = Vec::new();
    let _ = response.take(MAX_TOKEN_RESPONSE).read_to_end(&mut body);
    let parsed: Option<TokenResponse> = serde_json::from_slice(&body).ok();
    if !status.is_success() {
        let code_word = parsed.and_then(|p| p.error).filter(|e| {
            !e.is_empty() && e.len() <= 40 && e.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
        });
        return Err(match (status.as_u16(), code_word) {
            (400 | 401, Some(word)) => {
                format!("Google refused the authorization code ({word})")
            }
            (s, _) => format!("Google's token endpoint answered HTTP {s}"),
        });
    }
    let parsed = parsed.ok_or("Google's token answer was not understood")?;
    let access = parsed
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or("Google's answer had no access token")?;
    let refresh = parsed.refresh_token.filter(|t| !t.is_empty()).ok_or(
        "Google returned no refresh token (remove Tephra under your Google Account's \
         third-party access, then run auth google again)",
    )?;
    if parsed
        .scope
        .as_deref()
        .is_some_and(|granted| !granted.split(' ').any(|s| s == DRIVE_SCOPE))
    {
        return Err(
            "Google did not grant Drive access (tick the Drive permission on the \
                    consent screen)"
                .to_string(),
        );
    }
    let expires_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        + parsed.expires_in.unwrap_or(3600);
    let set = serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_at": expires_at,
        "scope": DRIVE_SCOPE,
        "token_type": parsed.token_type.unwrap_or_else(|| "Bearer".to_string()),
    });
    Ok(Secret::new(set.to_string()))
}

fn timeout_text(timeout: Duration) -> String {
    let secs = timeout.as_secs();
    if secs >= 60 && secs.is_multiple_of(60) {
        let minutes = secs / 60;
        format!("{minutes} minute{}", if minutes == 1 { "" } else { "s" })
    } else {
        format!("{secs} second{}", if secs == 1 { "" } else { "s" })
    }
}

/// `auth google`. Returns the exit code.
pub fn run(env: &mut AuthEnv<'_>, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let client = match load_client(env.google_client_json.as_deref()) {
        Ok(c) => c,
        Err(reason) => {
            let _ = write!(err, "{}", setup_error(&reason, &env.config_path));
            return 1;
        }
    };
    if let Err(e) = env.store.get(Provider::Google) {
        let _ = writeln!(err, "tephractl: {}", keychain_unavailable(&e));
        return 1;
    }

    let verifier = pkce::new_verifier();
    let challenge = pkce::challenge_for(&verifier);
    let state = pkce::new_state();
    let (tx, rx) = mpsc::channel();
    let listener = match Listener::start(state.clone(), tx) {
        Ok(l) => l,
        Err(e) => {
            let _ = writeln!(
                err,
                "tephractl: cannot listen on 127.0.0.1 for the Google redirect ({})",
                e.kind()
            );
            return 1;
        }
    };
    let redirect_uri = format!("http://127.0.0.1:{}{CALLBACK_PATH}", listener.port);
    let url = consent_url(&client, &redirect_uri, &state, &challenge);
    let _ = writeln!(
        out,
        "Open this URL to sign in to Google:\n\n{url}\n\n\
         Waiting up to {} for the redirect to 127.0.0.1:{} ...",
        timeout_text(env.consent_timeout),
        listener.port
    );
    let _ = out.flush();
    if let Err(e) = (env.opener)(&url) {
        let _ = writeln!(
            err,
            "tephractl: could not open a browser ({}); open the URL above yourself.",
            e.kind()
        );
    }

    let event = rx.recv_timeout(env.consent_timeout);
    // Closed before the exchange: the port is shut on success and on
    // every failure below.
    drop(listener);
    let code = match event {
        Ok(Event::Code(code)) => code,
        Ok(Event::Denied(why)) => {
            let _ = if why == "access_denied" {
                writeln!(err, "tephractl: consent denied; nothing was stored.")
            } else {
                writeln!(
                    err,
                    "tephractl: Google ended the sign-in ({}); nothing was stored.",
                    if why.is_empty() {
                        "no reason given"
                    } else {
                        &why
                    }
                )
            };
            return 1;
        }
        Err(RecvTimeoutError::Timeout) => {
            let _ = writeln!(
                err,
                "tephractl: timed out after {} waiting for the Google consent; \
                 nothing was stored.",
                timeout_text(env.consent_timeout)
            );
            return 1;
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = writeln!(
                err,
                "tephractl: the redirect listener stopped; nothing was stored."
            );
            return 1;
        }
    };

    let token_set = match exchange(&client, &code, &verifier, &redirect_uri) {
        Ok(set) => set,
        Err(reason) => {
            let _ = writeln!(err, "tephractl: {reason}. Nothing was stored.");
            return 1;
        }
    };
    if let Err(e) = env.store.set(Provider::Google, &token_set) {
        let _ = writeln!(err, "tephractl: {}", keychain_unavailable(&e));
        return 1;
    }
    let _ = writeln!(
        out,
        "signed in to Google (Drive access, refresh token stored)"
    );
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(auth_uri: &str) -> ClientConfig {
        ClientConfig {
            client_id: "id.apps.googleusercontent.com".into(),
            client_secret: Secret::new("s"),
            auth_uri: auth_uri.into(),
            token_uri: "https://oauth2.googleapis.com/token".into(),
        }
    }

    #[test]
    fn consent_url_carries_every_required_parameter() {
        let url = consent_url(
            &client("https://accounts.google.com/o/oauth2/v2/auth"),
            "http://127.0.0.1:5555/callback",
            "STATE",
            "CHAL",
        );
        for part in [
            "https://accounts.google.com/o/oauth2/v2/auth?client_id=id.apps.googleusercontent.com",
            "redirect_uri=http%3A%2F%2F127.0.0.1%3A5555%2Fcallback",
            "response_type=code",
            "scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive",
            "access_type=offline",
            "prompt=consent",
            "state=STATE",
            "code_challenge=CHAL",
            "code_challenge_method=S256",
        ] {
            assert!(url.contains(part), "{part} missing from {url}");
        }
    }

    #[test]
    fn consent_url_appends_to_an_auth_uri_that_has_a_query() {
        let url = consent_url(
            &client("https://accounts.google.com/auth?x=1"),
            "r",
            "s",
            "c",
        );
        assert!(url.starts_with("https://accounts.google.com/auth?x=1&client_id="));
    }

    #[test]
    fn endpoints_are_google_or_loopback_only() {
        for ok in [
            "https://accounts.google.com/o/oauth2/auth",
            "https://oauth2.googleapis.com/token",
            "http://127.0.0.1:9/token",
            "http://localhost:9/token",
        ] {
            assert!(endpoint_ok(ok), "{ok}");
        }
        for bad in [
            "http://accounts.google.com/o",
            "https://evil.example/token",
            "https://accounts.google.com.evil.example/token",
            "https://evilgoogle.com/token",
            "ftp://accounts.google.com/",
            "https://user@accounts.google.com/",
            "",
        ] {
            assert!(!endpoint_ok(bad), "{bad}");
        }
    }

    #[test]
    fn query_parsing_decodes_and_detects_repeats() {
        let pairs = parse_query("code=4%2F0A-b&state=x&state=y&e=");
        assert_eq!(single(&pairs, "code"), Ok(Some("4/0A-b")));
        assert_eq!(single(&pairs, "state"), Err(()));
        assert_eq!(single(&pairs, "nope"), Ok(None));
        assert_eq!(single(&pairs, "e"), Ok(Some("")));
    }

    #[test]
    fn timeout_text_reads_naturally() {
        assert_eq!(timeout_text(Duration::from_secs(300)), "5 minutes");
        assert_eq!(timeout_text(Duration::from_secs(60)), "1 minute");
        assert_eq!(timeout_text(Duration::from_secs(1)), "1 second");
        assert_eq!(timeout_text(Duration::from_secs(90)), "90 seconds");
    }
}
