//! Starts the fake GitHub and the fake Drive, prints their base URLs and
//! serves until the time is up. For checking the fakes with `curl`.
//!
//!   cargo run -p tephra-remote --features test-support --example fakes -- MODE [SECONDS] [CLIENT_JSON]
//!
//! MODE is `normal` (any token is accepted), `rate-limit` (both fakes
//! answer every request with a limit: GitHub 403 with Retry-After, Drive
//! 429 with Retry-After) or `revoked` (every request gets 401). SECONDS
//! (default 60) is how long to serve. CLIENT_JSON, if given, is a path to
//! write a Desktop-app Google client JSON to: it points `auth_uri` and
//! `token_uri` at the third fake (the Google sign-in pair that
//! `tephractl auth google` uses), so set `google_client_json` to that
//! path. All three listen on `127.0.0.1` on ports
//! the system picks, so nothing here can clash with a running app.

use std::time::Duration;

use tephra_remote::testing::{DriveLimit, FakeDrive, FakeGithub, FakeGoogleOAuth, GithubLimit};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "normal".to_string());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let client_json_path = args.next();

    let github = FakeGithub::start().await;
    github.add_repo("octo/hello", "main");
    github.put_file("octo/hello", "main", "README.md", b"# hello\n");

    let drive = FakeDrive::start().await;
    drive.add_folder("fld", "Notes", None);
    drive.add_file("f1", "notes.md", "text/markdown", Some("fld"), b"# notes\n");

    let oauth = FakeGoogleOAuth::start().await;
    if let Some(path) = &client_json_path {
        if let Err(e) = std::fs::write(path, oauth.client_json()) {
            eprintln!("cannot write {path}: {e}");
            std::process::exit(1);
        }
    }

    match mode.as_str() {
        "normal" => {}
        "rate-limit" => {
            let hour = Duration::from_secs(3600);
            github.rate_limit_for(hour, GithubLimit::RetryAfter403);
            drive.rate_limit_for(hour, DriveLimit::Status429 { retry_after: true });
        }
        "revoked" => {
            github.revoke_token();
            drive.revoke_token();
        }
        other => {
            eprintln!("unknown mode `{other}`: use normal, rate-limit or revoked");
            std::process::exit(2);
        }
    }

    println!("mode: {mode}");
    println!("github base url: {}", github.base_url());
    println!("drive base url: {}", drive.base_url());
    println!("google authorize url: {}", oauth.authorize_url());
    println!("google token url: {}", oauth.token_url());
    if let Some(path) = &client_json_path {
        println!("google client json written to: {path}");
    }
    println!("serving for {seconds} s");
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    println!(
        "requests seen: github {}, drive {}",
        github.request_log().count(),
        drive.request_log().count()
    );
}
