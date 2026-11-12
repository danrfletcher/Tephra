//! `tephractl daemon status` (G14): turns `launchctl print` output and
//! the on-disk facts (plist, installed copy, `access.json`, codesign
//! authority) into one [`StatusReport`], and renders it as the three
//! status lines or as `--json`.
//!
//! Parsing and rendering are pure, so they are unit-tested against
//! captured `launchctl print` output (`tests/fixtures/launchctl/`) on
//! every platform. [`gather`] is the only part that runs commands, and
//! it goes through the [`CommandRunner`] trait.

use std::fs;
use std::path::PathBuf;

use serde_json::{json, Value};

use super::install::{CommandRunner, Layout};
use super::{launchctl, signing};

/// What launchd says about the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchdState {
    /// `launchctl print` found no such service.
    NotLoaded,
    Running {
        pid: u32,
    },
    NotRunning {
        /// Exit code of the last run, or `-N` when it was killed by
        /// signal N (the convention `launchctl list` uses). `None`
        /// when launchd reports no run yet.
        last_exit: Option<i32>,
    },
}

/// Parses `launchctl print gui/<uid>/<label>`. `ok` is whether the
/// command exited 0: a non-zero exit is "not loaded" whatever it
/// printed.
///
/// Only top-level (one tab) fields of the service block are read:
/// nested blocks such as `event triggers` also contain `state = ...`
/// lines that must not be mistaken for the service's own.
pub fn parse_print(ok: bool, output: &str) -> LaunchdState {
    if !ok {
        return LaunchdState::NotLoaded;
    }
    let mut state: Option<&str> = None;
    let mut pid: Option<u32> = None;
    let mut last_exit: Option<i32> = None;
    let mut last_signal: Option<i32> = None;

    for line in output.lines() {
        let Some(field) = line.strip_prefix('\t') else {
            continue;
        };
        if field.starts_with('\t') {
            continue;
        }
        let Some((key, value)) = field.split_once(" = ") else {
            continue;
        };
        let value = value.trim();
        match key {
            "state" => state = Some(value),
            "pid" => pid = value.parse().ok(),
            // `0`, `78: EX_CONFIG`, `75: EX_TEMPFAIL` or `(never exited)`:
            // the number is what comes before any `:`.
            "last exit code" => last_exit = parse_exit_code(value),
            "last terminating signal" => {
                // "Killed: 9"
                last_signal = value
                    .rsplit(|c: char| !c.is_ascii_digit())
                    .find(|s| !s.is_empty())
                    .and_then(|n| n.parse().ok());
            }
            _ => {}
        }
    }

    match (state, pid) {
        (Some("running"), Some(pid)) => LaunchdState::Running { pid },
        (None, _) => LaunchdState::NotLoaded,
        _ => {
            // A signal death wins over a stale exit code: launchd
            // reports whichever ended the last run.
            let last_exit = match (last_signal, last_exit) {
                (Some(sig), _) => Some(-sig),
                (None, code) => code,
            };
            LaunchdState::NotRunning { last_exit }
        }
    }
}

/// The integer in a `last exit code` value. launchd appends the
/// sysexits name to non-zero codes (`78: EX_CONFIG`) and prints
/// `(never exited)` before the first run, which is `None`.
fn parse_exit_code(value: &str) -> Option<i32> {
    let number = value.split_once(':').map_or(value, |(n, _)| n);
    number.trim().parse().ok()
}

/// What `access.json` said, if it can be trusted for the running
/// daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessInfo {
    pub documents_readable: bool,
    pub error_kind: Option<String>,
    pub pid: u32,
}

/// Reads `access.json` from `data_dir`. Missing or malformed is
/// `None`: the probe is advisory and a half-understood file must not
/// become a claim.
pub fn read_access(data_dir: &std::path::Path) -> Option<AccessInfo> {
    let raw = fs::read_to_string(data_dir.join("access.json")).ok()?;
    let value: Value = serde_json::from_str(&raw).ok()?;
    Some(AccessInfo {
        documents_readable: value.get("documents_readable")?.as_bool()?,
        error_kind: value
            .get("error_kind")
            .and_then(Value::as_str)
            .map(str::to_string),
        pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
    })
}

/// Everything `daemon status` and the tail of `daemon install` report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusReport {
    pub installed: bool,
    pub state: LaunchdState,
    pub plist: Option<PathBuf>,
    pub binary: Option<PathBuf>,
    pub signed_by: Option<String>,
    /// `Some` only when `access.json` was written by the pid that is
    /// running now; a leftover from an earlier run says nothing about
    /// this one.
    pub access: Option<AccessInfo>,
}

impl StatusReport {
    pub fn running(&self) -> bool {
        matches!(self.state, LaunchdState::Running { .. })
    }

    pub fn pid(&self) -> Option<u32> {
        match self.state {
            LaunchdState::Running { pid } => Some(pid),
            _ => None,
        }
    }

    pub fn last_exit(&self) -> Option<i32> {
        match self.state {
            LaunchdState::NotRunning { last_exit } => last_exit,
            _ => None,
        }
    }

    pub fn documents_readable(&self) -> Option<bool> {
        self.access.as_ref().map(|a| a.documents_readable)
    }
}

/// The one status line: exactly one of `installed, running (pid N)`,
/// `installed, not running (last exit N)` or `not installed`. When
/// launchd has no last exit to report (never loaded, or never ran) the
/// parenthesis is left off rather than invent a number.
pub fn render_status_line(report: &StatusReport) -> String {
    if !report.installed {
        return "not installed".to_string();
    }
    match &report.state {
        LaunchdState::Running { pid } => format!("installed, running (pid {pid})"),
        LaunchdState::NotRunning { last_exit: Some(n) } => {
            format!("installed, not running (last exit {n})")
        }
        LaunchdState::NotRunning { last_exit: None } | LaunchdState::NotLoaded => {
            "installed, not running".to_string()
        }
    }
}

/// The Full Disk Access text shown by both install and status, or
/// `None` when there is nothing to say yet (not running, or no
/// `access.json` from this pid). When `~/Documents` is denied it is
/// two lines: `Full Disk Access: permission_denied` (the same word
/// `tephractl source list` uses) then the fix line.
pub fn render_access_line(report: &StatusReport) -> Option<String> {
    let access = report.access.as_ref()?;
    Some(if access.documents_readable {
        "Full Disk Access: ok".to_string()
    } else {
        match access.error_kind.as_deref() {
            Some(crate::daemon::ERROR_KIND_PERMISSION_DENIED) => format!(
                "Full Disk Access: {}\n{}",
                crate::daemon::ERROR_KIND_PERMISSION_DENIED,
                crate::daemon::FIX_LINE
            ),
            Some(other) => format!("Full Disk Access: unknown (~/Documents: {other})"),
            None => "Full Disk Access: unknown".to_string(),
        }
    })
}

/// The whole text output of `daemon status`.
pub fn render_text(report: &StatusReport) -> String {
    let mut out = render_status_line(report);
    out.push('\n');
    if report.installed {
        if report.running() {
            if let Some(line) = render_access_line(report) {
                out.push_str(&line);
                out.push('\n');
            }
        } else if report.state == LaunchdState::NotLoaded || report.last_exit().is_some() {
            out.push_str("run tephractl daemon install to repair it\n");
        }
    }
    out
}

/// `--json`: `{installed, running, pid, last_exit, plist, binary,
/// signed_by, documents_readable}`. Unknown values are `null`.
pub fn render_json(report: &StatusReport) -> String {
    let value = json!({
        "installed": report.installed,
        "running": report.running(),
        "pid": report.pid(),
        "last_exit": report.last_exit(),
        "plist": report.plist.as_ref().map(|p| p.display().to_string()),
        "binary": report.binary.as_ref().map(|p| p.display().to_string()),
        "signed_by": report.signed_by,
        "documents_readable": report.documents_readable(),
    });
    format!(
        "{}\n",
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
    )
}

/// Collects the facts. Runs `launchctl print` and `codesign -dvv`
/// through `runner`; reads the plist, the copy and `access.json` from
/// the filesystem. Never writes anything.
pub fn gather(
    runner: &mut dyn CommandRunner,
    layout: &Layout,
    uid: u32,
) -> std::io::Result<StatusReport> {
    let print = runner.run(
        launchctl::PROGRAM,
        &launchctl::print_args(uid, &layout.label),
    )?;
    let state = parse_print(print.success(), &print.stdout);

    let plist_exists = layout.plist_path.is_file();
    let binary_exists = layout.installed_binary.is_file();

    let signed_by = if binary_exists {
        let out = runner.run(
            signing::CODESIGN,
            &signing::display_args(&layout.installed_binary),
        )?;
        // `codesign -d` writes its report to stderr.
        signing::parse_authority(&out.stderr)
    } else {
        None
    };

    let access = match state {
        LaunchdState::Running { pid } => read_access(&layout.data_dir).filter(|a| a.pid == pid),
        _ => None,
    };

    Ok(StatusReport {
        installed: plist_exists,
        state,
        plist: plist_exists.then(|| layout.plist_path.clone()),
        binary: binary_exists.then(|| layout.installed_binary.clone()),
        signed_by,
        access,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/launchctl")
            .join(name);
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    fn report(state: LaunchdState) -> StatusReport {
        StatusReport {
            installed: true,
            state,
            plist: Some(PathBuf::from(
                "/Users/dan/Library/LaunchAgents/com.danrfletcher.tephrad.plist",
            )),
            binary: Some(PathBuf::from(
                "/Users/dan/Library/Application Support/tephra/bin/tephrad",
            )),
            signed_by: Some("Tephra Local".to_string()),
            access: None,
        }
    }

    #[test]
    fn running_fixture_gives_running_with_pid() {
        assert_eq!(
            parse_print(true, &fixture("print_running.txt")),
            LaunchdState::Running { pid: 94200 }
        );
    }

    #[test]
    fn not_running_fixture_gives_the_last_exit_code() {
        assert_eq!(
            parse_print(true, &fixture("print_not_running_exit.txt")),
            LaunchdState::NotRunning {
                last_exit: Some(78)
            }
        );
    }

    #[test]
    fn nested_state_lines_are_not_mistaken_for_the_services_own() {
        // The exit fixture has a nested `state = active` under
        // `event triggers`; the service's own `state = not running`
        // must win.
        let text = fixture("print_not_running_exit.txt");
        assert!(text.contains("\t\t\tstate = active"));
        assert!(matches!(
            parse_print(true, &text),
            LaunchdState::NotRunning { .. }
        ));
    }

    #[test]
    fn killed_by_signal_is_reported_as_a_negative_signal_number() {
        assert_eq!(
            parse_print(true, &fixture("print_not_running_signal.txt")),
            LaunchdState::NotRunning {
                last_exit: Some(-9)
            }
        );
    }

    #[test]
    fn never_run_has_no_last_exit() {
        assert_eq!(
            parse_print(true, &fixture("print_never_run.txt")),
            LaunchdState::NotRunning { last_exit: None }
        );
    }

    #[test]
    fn not_loaded_fixture_with_a_failing_exit_is_not_loaded() {
        assert_eq!(
            parse_print(false, &fixture("print_not_loaded.txt")),
            LaunchdState::NotLoaded
        );
    }

    #[test]
    fn empty_successful_output_is_not_loaded_not_a_guess() {
        assert_eq!(parse_print(true, ""), LaunchdState::NotLoaded);
    }

    #[test]
    fn the_three_status_lines() {
        assert_eq!(
            render_status_line(&report(LaunchdState::Running { pid: 4242 })),
            "installed, running (pid 4242)"
        );
        assert_eq!(
            render_status_line(&report(LaunchdState::NotRunning { last_exit: Some(1) })),
            "installed, not running (last exit 1)"
        );
        let mut gone = report(LaunchdState::NotLoaded);
        gone.installed = false;
        assert_eq!(render_status_line(&gone), "not installed");
    }

    #[test]
    fn installed_but_never_loaded_says_not_running_without_inventing_an_exit() {
        assert_eq!(
            render_status_line(&report(LaunchdState::NotLoaded)),
            "installed, not running"
        );
    }

    #[test]
    fn text_adds_the_access_line_only_for_a_running_daemon_with_a_matching_probe() {
        let mut r = report(LaunchdState::Running { pid: 7 });
        assert_eq!(render_text(&r), "installed, running (pid 7)\n");

        r.access = Some(AccessInfo {
            documents_readable: true,
            error_kind: None,
            pid: 7,
        });
        assert_eq!(
            render_text(&r),
            "installed, running (pid 7)\nFull Disk Access: ok\n"
        );

        r.access = Some(AccessInfo {
            documents_readable: false,
            error_kind: Some("permission_denied".to_string()),
            pid: 7,
        });
        assert_eq!(
            render_text(&r),
            format!(
                "installed, running (pid 7)\nFull Disk Access: permission_denied\n{}\n",
                crate::daemon::FIX_LINE
            )
        );
    }

    #[test]
    fn denied_access_says_permission_denied_and_keeps_the_exact_fix_line() {
        let mut r = report(LaunchdState::Running { pid: 7 });
        r.access = Some(AccessInfo {
            documents_readable: false,
            error_kind: Some("permission_denied".to_string()),
            pid: 7,
        });
        let line = render_access_line(&r).unwrap();
        let mut lines = line.lines();
        assert_eq!(lines.next(), Some("Full Disk Access: permission_denied"));
        assert_eq!(lines.next(), Some(crate::daemon::FIX_LINE));
        assert_eq!(lines.next(), None);
        assert!(render_text(&r).contains("permission_denied"));
    }

    #[test]
    fn exit_code_forms_launchd_prints() {
        assert_eq!(parse_exit_code("0"), Some(0));
        assert_eq!(parse_exit_code("1"), Some(1));
        assert_eq!(parse_exit_code("78: EX_CONFIG"), Some(78));
        assert_eq!(parse_exit_code("75: EX_TEMPFAIL"), Some(75));
        assert_eq!(parse_exit_code("(never exited)"), None);
        assert_eq!(parse_exit_code(""), None);
    }

    #[test]
    fn exit_code_with_a_sysexits_name_still_suggests_reinstalling() {
        // Real launchd output for a deleted program (EX_CONFIG).
        let state = parse_print(true, &fixture("print_not_running_exit.txt"));
        let text = render_text(&report(state));
        assert!(text.starts_with("installed, not running (last exit 78)\n"));
        assert!(text.contains("run tephractl daemon install to repair it"));
    }

    #[test]
    fn a_crashed_agent_suggests_reinstalling() {
        let text = render_text(&report(LaunchdState::NotRunning {
            last_exit: Some(78),
        }));
        assert_eq!(
            text,
            "installed, not running (last exit 78)\nrun tephractl daemon install to repair it\n"
        );
    }

    #[test]
    fn not_installed_prints_just_that_line() {
        let mut r = report(LaunchdState::NotLoaded);
        r.installed = false;
        r.plist = None;
        r.binary = None;
        assert_eq!(render_text(&r), "not installed\n");
    }

    #[test]
    fn json_has_exactly_the_documented_keys_and_null_for_unknowns() {
        let mut r = report(LaunchdState::Running { pid: 7 });
        r.access = Some(AccessInfo {
            documents_readable: true,
            error_kind: None,
            pid: 7,
        });
        let v: Value = serde_json::from_str(&render_json(&r)).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "binary",
                "documents_readable",
                "installed",
                "last_exit",
                "pid",
                "plist",
                "running",
                "signed_by"
            ]
        );
        assert_eq!(v["installed"], true);
        assert_eq!(v["running"], true);
        assert_eq!(v["pid"], 7);
        assert!(v["last_exit"].is_null());
        assert_eq!(v["signed_by"], "Tephra Local");
        assert_eq!(v["documents_readable"], true);

        let mut off = report(LaunchdState::NotRunning { last_exit: Some(3) });
        off.installed = false;
        off.plist = None;
        off.binary = None;
        off.signed_by = None;
        let v: Value = serde_json::from_str(&render_json(&off)).unwrap();
        assert_eq!(v["installed"], false);
        assert_eq!(v["running"], false);
        assert!(v["pid"].is_null());
        assert_eq!(v["last_exit"], 3);
        assert!(v["plist"].is_null());
        assert!(v["binary"].is_null());
        assert!(v["signed_by"].is_null());
        assert!(v["documents_readable"].is_null());
    }

    #[test]
    fn read_access_ignores_a_missing_or_malformed_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_access(dir.path()), None);
        fs::write(dir.path().join("access.json"), "{not json").unwrap();
        assert_eq!(read_access(dir.path()), None);
        fs::write(
            dir.path().join("access.json"),
            r#"{"documents_readable":true,"error_kind":null,"checked_at":1,"pid":9}"#,
        )
        .unwrap();
        assert_eq!(
            read_access(dir.path()),
            Some(AccessInfo {
                documents_readable: true,
                error_kind: None,
                pid: 9
            })
        );
    }
}
