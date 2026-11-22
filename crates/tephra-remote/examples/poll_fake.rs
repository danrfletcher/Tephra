//! A fake GitHub you steer from a script, for `scripts/verify/remote-pr-6.sh`.
//!
//!   cargo run -p tephra-remote --features test-support --example poll_fake -- URL_FILE REQUEST_LOG
//!
//! It serves the repo `danrfletcher/atlas` (branch `main`: `README.md`,
//! `docs/Guide.md`, `src/lib.rs`) on a loopback port the system picks, behind
//! a gate that can cut the connection, writes the base URL to URL_FILE and
//! then reads one command per line on stdin, answering each with one line on
//! stdout:
//!
//!   put PATH TEXT...   a new commit that sets PATH to TEXT and a newline
//!   remove PATH        a new commit without PATH
//!   move LOCATION      the repo answers 301 with that Location from now on
//!   unmove             the repo is back where it was
//!   down | up          the connection is refused / back, same port
//!   cut N              the next N file bodies are cut short (the connection
//!                      drops part way through)
//!   latency MS         every answer takes MS milliseconds more
//!   clear              forget the requests so far
//!   dump               write the requests so far to REQUEST_LOG, one
//!                      `METHOD PATH [if-none-match]` line each (never a header
//!                      value, so never a token)
//!   quit               stop
//!
//! It binds loopback only, and exits when stdin closes.

use std::io::BufRead;

use tephra_remote::testing::{FakeGithub, LinkGate};

const REPO: &str = "danrfletcher/atlas";

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(url_file), Some(log_file)) = (args.next(), args.next()) else {
        eprintln!("usage: poll_fake URL_FILE REQUEST_LOG");
        std::process::exit(2);
    };

    let github = FakeGithub::start().await;
    github.add_repo(REPO, "main");
    github.put_file(
        REPO,
        "main",
        "README.md",
        b"# Atlas\n\nStart with [[docs/Guide.md]] and see [[Guide]].\n\n## Setup\n",
    );
    github.put_file(
        REPO,
        "main",
        "docs/Guide.md",
        b"# Guide\n\nBack to [[README]].\n",
    );
    github.put_file(
        REPO,
        "main",
        "src/lib.rs",
        b"// the library\npub fn atlas() {}\n",
    );
    // A 6 MB file, to fetch in pieces and to cut off.
    let big: Vec<u8> = (0..6 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();
    github.put_file(REPO, "main", "data/big.bin", &big);
    let gate = LinkGate::in_front_of(github.base_url()).await;
    std::fs::write(&url_file, gate.base_url()).expect("write the URL file");

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut moved = false;
    while let Some(line) = rx.recv().await {
        let (command, rest) = line.split_once(' ').unwrap_or((line.as_str(), ""));
        let answer = match command {
            "put" => match rest.split_once(' ') {
                Some((path, text)) => {
                    let sha = github.put_file(REPO, "main", path, format!("{text}\n").as_bytes());
                    format!("ok put {path} -> {sha}")
                }
                None => "error: put PATH TEXT".to_string(),
            },
            "remove" => {
                let sha = github.remove_file(REPO, "main", rest);
                format!("ok remove {rest} -> {sha}")
            }
            "move" => {
                github.move_repo(REPO, rest);
                moved = true;
                format!("ok move {rest}")
            }
            "unmove" => {
                if moved {
                    github.clear_move(REPO);
                    moved = false;
                }
                "ok unmove".to_string()
            }
            "down" => {
                gate.down();
                "ok down".to_string()
            }
            "up" => {
                gate.up().await;
                "ok up".to_string()
            }
            "cut" => match rest.trim().parse::<u32>() {
                Ok(n) => {
                    github.truncate_next_bodies(n);
                    format!("ok cut {n}")
                }
                Err(_) => "error: cut N".to_string(),
            },
            "latency" => match rest.trim().parse::<u64>() {
                Ok(ms) => {
                    github.set_latency(std::time::Duration::from_millis(ms));
                    format!("ok latency {ms}")
                }
                Err(_) => "error: latency MS".to_string(),
            },
            "clear" => {
                github.request_log().clear();
                "ok clear".to_string()
            }
            "dump" => {
                let mut text = String::new();
                for r in github.request_log().all() {
                    let mut line = format!("{} {}", r.method, r.path);
                    if r.headers.iter().any(|(k, _)| k == "if-none-match") {
                        line.push_str(" if-none-match");
                    }
                    text.push_str(&line);
                    text.push('\n');
                }
                std::fs::write(&log_file, text).expect("write the request log");
                "ok dump".to_string()
            }
            "quit" => {
                println!("ok quit");
                return;
            }
            other => format!("error: unknown command `{other}`"),
        };
        println!("{answer}");
    }
}
