//! `tephractl` binary entry point: parses argv, loads the auth
//! token, builds an [`tephractl::transport::RpcClient`], executes the
//! command, and exits with the resulting code. All real logic lives
//! in the library (`src/lib.rs` and friends) so it's testable without
//! spawning this binary as a subprocess.

use std::io::{self, BufReader, Write};
use std::process::ExitCode;

use tephractl::cli::{self, Action};
use tephractl::commands;
use tephractl::token;
use tephractl::transport::{RpcClient, DEFAULT_ADDR};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let parsed = match cli::parse_args(args) {
        Ok(Action::Help) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Action::Run(parsed)) => parsed,
        Err(e) => {
            eprintln!("tephractl: {e}");
            eprint!("{}", cli::USAGE);
            return ExitCode::FAILURE;
        }
    };

    // Token loading happens before any network call: a missing
    // token file is reported with a single, clear message and a
    // non-zero exit code, never a confusing connection attempt first.
    let token_path = tephractl::paths::default_token_path();
    let bearer_token = match token::load_token(&token_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("tephractl: {e}");
            return ExitCode::FAILURE;
        }
    };

    let addr = parsed
        .addr
        .clone()
        .unwrap_or_else(|| DEFAULT_ADDR.to_string());
    let client = RpcClient::new(addr, bearer_token);

    let mut stdin = BufReader::new(io::stdin());
    let outcome = commands::execute(&parsed.command, parsed.json, &client, &mut stdin);

    if !outcome.stdout.is_empty() {
        print!("{}", outcome.stdout);
    }
    if !outcome.stderr.is_empty() {
        eprint!("{}", outcome.stderr);
    }
    let _ = io::stdout().flush();
    let _ = io::stderr().flush();

    if outcome.exit_code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
