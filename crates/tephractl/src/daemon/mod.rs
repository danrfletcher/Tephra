//! `tephractl daemon install|uninstall|status` (G14, G20): run
//! `tephrad` as a per-user launchd agent on macOS.
//!
//! Module map:
//! - [`plist`]: renders the LaunchAgent property list (pure).
//! - [`launchctl`]: `launchctl` argv builders (pure).
//! - [`signing`]: the "Tephra Local" identity's `security`/`openssl`/
//!   `codesign` argv builders and the `codesign` report parser (pure).
//! - [`status`]: parses `launchctl print`, renders the status lines
//!   and `--json` (pure), and gathers the facts.
//! - [`install`]: the install and uninstall orderings, written against
//!   the [`install::CommandRunner`] trait so they are tested with a
//!   recording runner on every platform.
//!
//! Only [`SystemRunner`] (which really spawns processes) and [`run`]'s
//! macOS arm are gated on `target_os = "macos"`; everywhere else every
//! `daemon` command prints [`NOT_MACOS_MESSAGE`] and exits 1. These
//! commands never read the token file and never talk to the daemon
//! over RPC: `main.rs` routes them here before either happens.

pub mod install;
pub mod launchctl;
pub mod plist;
pub mod signing;
pub mod status;

use std::io::Write;

use crate::cli::Command;

/// Printed (to stderr) by every `daemon` command off macOS.
pub const NOT_MACOS_MESSAGE: &str =
    "tephractl daemon: launchd is macOS only; start tephrad by hand";

/// The agent's label, unless `TEPHRA_LAUNCHD_LABEL` overrides it.
pub const DEFAULT_LABEL: &str = "com.danrfletcher.tephrad";

/// Env var that overrides the label. Test-only: the live test uses it
/// so it can never touch the real agent. A non-default label also
/// turns off the hand-started-tephrad search, which would otherwise
/// find (and stop) the user's own daemon.
pub const LABEL_ENV: &str = "TEPHRA_LAUNCHD_LABEL";

/// `access.json`'s `error_kind` for a refused read (matches tephrad's
/// `access::ERROR_KIND_PERMISSION_DENIED`; the two crates share no
/// code, and `tests/daemon_contract.rs` checks they agree).
pub const ERROR_KIND_PERMISSION_DENIED: &str = "permission_denied";

/// The one Full Disk Access fix line (matches tephrad's
/// `access::fix_line("~/Documents")`, checked by the same test).
pub const FIX_LINE: &str = "tephrad can't read ~/Documents: open System Settings > Privacy & Security > Full Disk Access, add ~/Library/Application Support/tephra/bin/tephrad, then run tephractl daemon install again";

/// Whether `command` is one of the `daemon` verbs, which `main.rs`
/// must route around the token and RPC client.
pub fn is_daemon_command(command: &Command) -> bool {
    matches!(
        command,
        Command::DaemonInstall { .. } | Command::DaemonUninstall | Command::DaemonStatus
    )
}

/// Runs a `daemon` command against the real system. Writes the result
/// to `out`/`err` and returns the exit code.
#[cfg(target_os = "macos")]
pub fn run(command: &Command, json: bool, out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let env = match install::DaemonEnv::from_process() {
        Ok(env) => env,
        Err(e) => {
            let _ = writeln!(err, "tephractl: {e}");
            return 1;
        }
    };
    let mut runner = SystemRunner;
    execute(command, json, &env, &mut runner, out, err)
}

/// Off macOS: the launchd message and exit 1, whatever the verb.
#[cfg(not(target_os = "macos"))]
pub fn run(_command: &Command, _json: bool, _out: &mut dyn Write, err: &mut dyn Write) -> i32 {
    let _ = writeln!(err, "{NOT_MACOS_MESSAGE}");
    1
}

/// The platform-independent core of [`run`]: dispatches to install,
/// uninstall or status with the given runner. Returns the exit code.
pub fn execute(
    command: &Command,
    json: bool,
    env: &install::DaemonEnv,
    runner: &mut dyn install::CommandRunner,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let layout = env.layout();
    let result = match command {
        Command::DaemonInstall { from, keychain } => {
            let opts = env.install_options(from.as_deref(), keychain.as_deref());
            install::install(runner, &layout, &opts, json, out)
        }
        Command::DaemonUninstall => install::uninstall(runner, &layout, out),
        Command::DaemonStatus => install::print_status(runner, &layout, json, out),
        _ => unreachable!("execute() is only called for daemon commands"),
    };
    match result {
        Ok(()) => 0,
        Err(e) => {
            let _ = writeln!(err, "tephractl: {e}");
            1
        }
    }
}

/// Runs real processes.
#[cfg(target_os = "macos")]
pub struct SystemRunner;

#[cfg(target_os = "macos")]
impl install::CommandRunner for SystemRunner {
    fn run(&mut self, program: &str, args: &[String]) -> std::io::Result<install::CmdOutput> {
        let output = std::process::Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(install::CmdOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn sleep(&mut self, duration: std::time::Duration) {
        std::thread::sleep(duration);
    }
}
