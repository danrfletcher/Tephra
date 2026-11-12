//! Full Disk Access (macOS TCC) probe and the one fix line shown for it.
//!
//! A `tephrad` started by launchd does not inherit Terminal's privacy
//! permission, so it may be unable to list `~/Documents`, `~/Desktop`
//! or `~/Downloads`. On start, `tephrad` runs one `read_dir` on
//! `$HOME/Documents` and records the outcome in `<data_dir>/access.json`
//! (G20) so `tephractl daemon install|status` can tell the user, without
//! a new RPC method: the method set stays the closed list. The file is
//! written with temp file + fsync + rename, so a reader never sees a
//! half-written one.
//!
//! The same fix line is used by `source add` when a path cannot be read
//! because of TCC ([`crate::registry::PathProbe::NotReadablePrivacy`]).
//!
//! This module only reads one directory listing and writes one file in
//! the daemon's own data dir; it has no UI and no client dependency.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// File name of the probe result inside the data directory.
pub const ACCESS_FILE_NAME: &str = "access.json";

/// `error_kind` values written to `access.json`.
pub const ERROR_KIND_PERMISSION_DENIED: &str = "permission_denied";
pub const ERROR_KIND_NOT_FOUND: &str = "not_found";
pub const ERROR_KIND_OTHER: &str = "other";

/// The installed copy the user must give Full Disk Access to (G20).
pub const INSTALLED_BINARY_HINT: &str = "~/Library/Application Support/tephra/bin/tephrad";

/// The result of one readability probe: the exact shape of
/// `access.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessReport {
    pub documents_readable: bool,
    /// `None` when readable; otherwise one of the `ERROR_KIND_*`
    /// values.
    pub error_kind: Option<String>,
    /// Unix seconds when the probe ran.
    pub checked_at: i64,
    /// The pid of the `tephrad` that ran the probe, so a reader can
    /// tell a fresh result from one a previous run left behind.
    pub pid: u32,
}

/// The one-line fix for a path `tephrad` cannot read because of macOS
/// privacy settings. `what` names what could not be read, e.g.
/// `~/Documents`.
pub fn fix_line(what: &str) -> String {
    format!(
        "tephrad can't read {what}: open System Settings > Privacy & Security > Full Disk Access, add {INSTALLED_BINARY_HINT}, then run tephractl daemon install again"
    )
}

/// Maps a failed `read_dir` to an `error_kind` value.
pub fn error_kind_of(err: &io::Error) -> &'static str {
    match err.kind() {
        io::ErrorKind::PermissionDenied => ERROR_KIND_PERMISSION_DENIED,
        io::ErrorKind::NotFound => ERROR_KIND_NOT_FOUND,
        _ => ERROR_KIND_OTHER,
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Runs `read_dir` on `<home>/Documents` and reports what happened.
/// `home` is `$HOME`; `None` (unset) is reported as `not_found`.
pub fn probe_documents(home: Option<&Path>, pid: u32) -> AccessReport {
    let outcome = match home {
        None => Err(ERROR_KIND_NOT_FOUND),
        Some(home) => match fs::read_dir(home.join("Documents")) {
            Ok(_) => Ok(()),
            Err(e) => Err(error_kind_of(&e)),
        },
    };
    AccessReport {
        documents_readable: outcome.is_ok(),
        error_kind: outcome.err().map(str::to_string),
        checked_at: now_unix(),
        pid,
    }
}

/// Where `access.json` lives for a data directory.
pub fn access_path(data_dir: &Path) -> PathBuf {
    data_dir.join(ACCESS_FILE_NAME)
}

/// Writes `report` to `<data_dir>/access.json` with temp file + fsync +
/// rename. The data dir must already exist.
pub fn write_access_json(data_dir: &Path, report: &AccessReport) -> io::Result<()> {
    let final_path = access_path(data_dir);
    let tmp_path = data_dir.join(format!(".{ACCESS_FILE_NAME}.tmp-{}", report.pid));
    let body = serde_json::to_vec(report).map_err(io::Error::other)?;

    let result = (|| {
        let mut file = fs::File::create(&tmp_path)?;
        file.write_all(&body)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, &final_path)?;
        // Persist the rename itself too.
        if let Ok(dir) = fs::File::open(data_dir) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    result
}

/// The startup step: probe `$HOME/Documents`, write `access.json`, and
/// return the report. A failure to write is returned to the caller,
/// which logs it and carries on: the probe is advisory, never a reason
/// to refuse to start.
pub fn probe_and_record(data_dir: &Path, home: Option<&Path>) -> io::Result<AccessReport> {
    let report = probe_documents(home, std::process::id());
    write_access_json(data_dir, &report)?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readable_documents_reports_ok_with_no_error_kind() {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("Documents")).unwrap();
        let report = probe_documents(Some(home.path()), 42);
        assert!(report.documents_readable);
        assert_eq!(report.error_kind, None);
        assert_eq!(report.pid, 42);
        assert!(report.checked_at > 0);
    }

    #[test]
    fn missing_documents_reports_not_found() {
        let home = tempfile::tempdir().unwrap();
        let report = probe_documents(Some(home.path()), 1);
        assert!(!report.documents_readable);
        assert_eq!(report.error_kind.as_deref(), Some(ERROR_KIND_NOT_FOUND));
    }

    #[test]
    fn unset_home_reports_not_found() {
        let report = probe_documents(None, 1);
        assert!(!report.documents_readable);
        assert_eq!(report.error_kind.as_deref(), Some(ERROR_KIND_NOT_FOUND));
    }

    #[test]
    fn write_replaces_atomically_and_leaves_no_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let first = AccessReport {
            documents_readable: false,
            error_kind: Some(ERROR_KIND_PERMISSION_DENIED.to_string()),
            checked_at: 1,
            pid: 10,
        };
        write_access_json(dir.path(), &first).unwrap();
        let second = AccessReport {
            documents_readable: true,
            error_kind: None,
            checked_at: 2,
            pid: 11,
        };
        write_access_json(dir.path(), &second).unwrap();

        let raw = fs::read_to_string(access_path(dir.path())).unwrap();
        let parsed: AccessReport = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed, second);
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec![ACCESS_FILE_NAME.to_string()]);
    }

    #[test]
    fn access_json_has_exactly_the_documented_keys() {
        let report = AccessReport {
            documents_readable: true,
            error_kind: None,
            checked_at: 5,
            pid: 7,
        };
        let value = serde_json::to_value(&report).unwrap();
        let mut keys: Vec<&str> = value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["checked_at", "documents_readable", "error_kind", "pid"]
        );
    }

    #[test]
    fn fix_line_names_the_path_and_the_installed_copy() {
        let line = fix_line("~/Documents");
        assert_eq!(
            line,
            "tephrad can't read ~/Documents: open System Settings > Privacy & Security > Full Disk Access, add ~/Library/Application Support/tephra/bin/tephrad, then run tephractl daemon install again"
        );
    }
}
