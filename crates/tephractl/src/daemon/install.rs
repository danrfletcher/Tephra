//! The install and uninstall orderings for the launchd agent (G14,
//! G20), written against [`CommandRunner`] so the exact sequence of
//! `security` / `openssl` / `codesign` / `launchctl` calls is tested
//! with a recording mock on every platform. Files (the installed copy,
//! the plist, the logs folder) are real: tests run them in a temp HOME.
//!
//! Install order:
//!
//! 1. check the source binary exists (nothing else has happened yet:
//!    a missing binary makes no command call and writes nothing);
//! 2. `security find-certificate -c "Tephra Local"`; if absent, create
//!    the identity (openssl, `security import` x2, `add-trusted-cert`);
//! 3. copy `tephrad` to a temp file beside the installed path and
//!    `codesign` it there, then fsync and rename it into place, so the
//!    installed path never holds an unsigned or half-written binary;
//! 4. write the plist (temp file + fsync + rename) and create
//!    `~/Library/LaunchAgents` and `~/Library/Logs/tephra`;
//! 5. `launchctl bootout` if the agent is loaded;
//! 6. stop a hand-started tephrad (SIGTERM, wait up to 5 s for the
//!    port), when the label is the default one;
//! 7. `launchctl bootstrap` (on failure: bootout, retry once);
//! 8. wait up to 10 s for an `access.json` written by the new pid.

use std::fmt;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::plist::{self, PlistSpec};
use super::status::{self, LaunchdState, StatusReport};
use super::{launchctl, signing, DEFAULT_LABEL, LABEL_ENV};

/// The port a hand-started `tephrad` binds by default.
pub const DEFAULT_PORT: u16 = 4812;

/// Poll interval and counts for the two waits.
const POLL: Duration = Duration::from_millis(250);
const PORT_FREE_POLLS: u32 = 20; // 5 s
const ACCESS_POLLS: u32 = 40; // 10 s

const ID: &str = "/usr/bin/id";
const PGREP: &str = "/usr/bin/pgrep";
const LSOF: &str = "/usr/sbin/lsof";
const PS: &str = "/bin/ps";
const KILL: &str = "/bin/kill";

/// What a command printed and how it ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    /// `None` when killed by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// The first non-empty stderr line (or stdout if stderr is
    /// empty), for one-line error messages.
    pub fn first_line(&self) -> String {
        [&self.stderr, &self.stdout]
            .iter()
            .flat_map(|s| s.lines())
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("no output")
            .to_string()
    }
}

/// Runs external commands and sleeps. The real implementation spawns
/// processes; tests record argv and script the answers.
pub trait CommandRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CmdOutput>;
    fn sleep(&mut self, duration: Duration);
}

/// A failure the user must see; printed as `tephractl: <message>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallError(pub String);

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for InstallError {}

impl From<io::Error> for InstallError {
    fn from(e: io::Error) -> Self {
        InstallError(e.to_string())
    }
}

type Result<T> = std::result::Result<T, InstallError>;

fn err<T>(message: impl Into<String>) -> Result<T> {
    Err(InstallError(message.into()))
}

/// Where everything lives, derived from `HOME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub home: PathBuf,
    pub label: String,
    /// `~/Library/Application Support/tephra/bin`
    pub bin_dir: PathBuf,
    /// `~/Library/Application Support/tephra/bin/tephrad`
    pub installed_binary: PathBuf,
    /// `~/Library/LaunchAgents/<label>.plist`
    pub plist_path: PathBuf,
    /// `~/Library/Logs/tephra`
    pub logs_dir: PathBuf,
    /// tephrad's data dir, where `access.json` is written.
    pub data_dir: PathBuf,
}

impl Layout {
    pub fn new(home: &Path, label: &str, xdg_data_home: Option<&str>) -> Self {
        let bin_dir = home.join("Library/Application Support/tephra/bin");
        let data_dir = match xdg_data_home.filter(|v| !v.is_empty()) {
            Some(xdg) => PathBuf::from(xdg).join("tephra"),
            None => home.join(".local/share/tephra"),
        };
        Layout {
            home: home.to_path_buf(),
            label: label.to_string(),
            installed_binary: bin_dir.join("tephrad"),
            bin_dir,
            plist_path: home
                .join("Library/LaunchAgents")
                .join(format!("{label}.plist")),
            logs_dir: home.join("Library/Logs/tephra"),
            data_dir,
        }
    }
}

/// The installing shell's environment, read once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonEnv {
    pub home: PathBuf,
    pub label: String,
    pub xdg_config_home: Option<String>,
    pub xdg_data_home: Option<String>,
    /// `std::env::current_exe()`: install defaults to the `tephrad`
    /// next to it.
    pub current_exe: Option<PathBuf>,
}

impl DaemonEnv {
    pub fn from_process() -> std::result::Result<Self, InstallError> {
        let exe = std::env::current_exe().ok();
        Self::from_getter(|k| std::env::var(k).ok(), exe)
    }

    pub fn from_getter(
        get: impl Fn(&str) -> Option<String>,
        current_exe: Option<PathBuf>,
    ) -> std::result::Result<Self, InstallError> {
        let home = match get("HOME").filter(|h| !h.is_empty()) {
            Some(h) => PathBuf::from(h),
            None => return err("HOME is not set"),
        };
        if !home.is_absolute() {
            return err(format!(
                "HOME must be an absolute path, got {}",
                home.display()
            ));
        }
        Ok(DaemonEnv {
            home,
            label: get(LABEL_ENV)
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| DEFAULT_LABEL.to_string()),
            xdg_config_home: get("XDG_CONFIG_HOME").filter(|v| !v.is_empty()),
            xdg_data_home: get("XDG_DATA_HOME").filter(|v| !v.is_empty()),
            current_exe,
        })
    }

    pub fn layout(&self) -> Layout {
        Layout::new(&self.home, &self.label, self.xdg_data_home.as_deref())
    }

    pub fn install_options(&self, from: Option<&str>, keychain: Option<&str>) -> InstallOptions {
        InstallOptions {
            from: from.map(PathBuf::from),
            keychain: keychain.map(PathBuf::from),
            current_exe: self.current_exe.clone(),
            xdg_config_home: self.xdg_config_home.clone(),
            xdg_data_home: self.xdg_data_home.clone(),
            stop_hand_started: self.label == DEFAULT_LABEL,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOptions {
    /// `--from <path>`.
    pub from: Option<PathBuf>,
    /// Hidden `--keychain <path>` (the live test's temporary keychain).
    pub keychain: Option<PathBuf>,
    pub current_exe: Option<PathBuf>,
    pub xdg_config_home: Option<String>,
    pub xdg_data_home: Option<String>,
    /// Look for and stop a hand-started tephrad. Off for a non-default
    /// label, so a test run never stops the user's own daemon.
    pub stop_hand_started: bool,
}

impl InstallOptions {
    fn source_binary(&self) -> Result<PathBuf> {
        if let Some(from) = &self.from {
            return Ok(from.clone());
        }
        match self.current_exe.as_deref().and_then(Path::parent) {
            Some(dir) => Ok(dir.join("tephrad")),
            None => {
                err("can't tell where tephractl is running from; pass --from <path to tephrad>")
            }
        }
    }
}

fn current_uid(runner: &mut dyn CommandRunner) -> Result<u32> {
    let out = runner.run(ID, &["-u".to_string()])?;
    match out.stdout.trim().parse() {
        Ok(uid) if out.success() => Ok(uid),
        _ => err("couldn't determine the current user id (id -u failed)"),
    }
}

// ---- install --------------------------------------------------------

/// `tephractl daemon install`. Writes progress and the final status to
/// `out`; an `Err` is the failure message.
pub fn install(
    runner: &mut dyn CommandRunner,
    layout: &Layout,
    opts: &InstallOptions,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    // 1. Nothing is touched, and no command runs, if the binary to
    // install isn't there.
    let source = opts.source_binary()?;
    if !source.is_file() {
        return err(format!("tephrad binary not found at {}", source.display()));
    }
    let keychain = opts.keychain.as_deref();
    let uid = current_uid(runner)?;

    // 2. The signing identity.
    let created = ensure_identity(runner, keychain, out)?;
    if !json {
        let _ = writeln!(
            out,
            "signing identity: {} ({})",
            signing::IDENTITY_NAME,
            if created { "created" } else { "found" }
        );
    }

    // 3. Copy, sign, rename into place.
    install_binary(runner, &source, layout, keychain)?;
    if !json {
        let _ = writeln!(
            out,
            "installed copy: {} (signed by {})",
            layout.installed_binary.display(),
            signing::IDENTITY_NAME
        );
    }

    // 4. The plist and the logs folder.
    write_plist(layout, opts)?;
    if !json {
        let _ = writeln!(out, "launchd agent: {}", layout.plist_path.display());
    }

    // 5. Boot out whatever is loaded, so the new plist and copy are
    // what launchd runs.
    if is_loaded(runner, uid, &layout.label)? {
        let o = runner.run(
            launchctl::PROGRAM,
            &launchctl::bootout_args(uid, &layout.label),
        )?;
        if !o.success() {
            return err(format!(
                "launchctl bootout failed: {}; the new plist is written but the old agent is still loaded",
                o.first_line()
            ));
        }
    }

    // 6. A hand-started tephrad would hold the port.
    if opts.stop_hand_started {
        stop_hand_started(runner, uid, out)?;
    }

    // 7. Load it; one retry after bootout.
    bootstrap_with_retry(runner, uid, layout)?;

    // 8. Wait for the daemon's own readability probe.
    wait_for_access(runner, uid, layout)?;

    let report = status::gather(runner, layout, uid)?;
    if json {
        let _ = write!(out, "{}", status::render_json(&report));
    } else {
        let _ = write!(out, "{}", status::render_text(&report));
        if report.running() && report.access.is_none() {
            let _ = writeln!(
                out,
                "Full Disk Access: unknown (tephrad did not report within 10 s; run tephractl daemon status)"
            );
        }
    }
    if !report.running() {
        return err(format!(
            "tephrad was loaded but is not running; see the logs in {}",
            layout.logs_dir.display()
        ));
    }
    Ok(())
}

/// Finds or creates the "Tephra Local" identity. Returns whether it
/// was created.
fn ensure_identity(
    runner: &mut dyn CommandRunner,
    keychain: Option<&Path>,
    out: &mut dyn Write,
) -> Result<bool> {
    let found = runner.run(signing::SECURITY, &signing::find_certificate_args(keychain))?;
    if found.success() {
        return Ok(false);
    }

    let _ = writeln!(
        out,
        "creating the \"{}\" code-signing identity in your keychain: macOS may ask for your password or approval",
        signing::IDENTITY_NAME
    );
    let dir = PrivateDir::create()?;
    let config = dir.path().join("openssl.cnf");
    let key = dir.path().join("key.pem");
    let cert = dir.path().join("cert.pem");
    fs::write(&config, signing::openssl_config())?;

    let gen = runner.run(
        signing::OPENSSL,
        &signing::openssl_req_args(&config, &key, &cert),
    )?;
    if !gen.success() {
        return err(format!(
            "couldn't generate the signing certificate: {}. Nothing was changed.",
            gen.first_line()
        ));
    }

    let steps: [(&str, String, Vec<String>); 3] = [
        (
            "import the private key",
            signing::SECURITY.to_string(),
            signing::import_args(&key, keychain),
        ),
        (
            "import the certificate",
            signing::SECURITY.to_string(),
            signing::import_args(&cert, keychain),
        ),
        (
            "trust the certificate for code signing",
            signing::SECURITY.to_string(),
            signing::trust_args(&cert, keychain),
        ),
    ];
    for (what, program, args) in steps {
        let o = runner.run(&program, &args)?;
        if !o.success() {
            // Leave no half-made identity behind: a later install
            // would find the certificate and skip creating it.
            let mut cleanup = vec![
                "delete-identity".to_string(),
                "-c".to_string(),
                signing::IDENTITY_NAME.to_string(),
            ];
            if let Some(k) = keychain {
                cleanup.push(k.display().to_string());
            }
            let _ = runner.run(signing::SECURITY, &cleanup);
            return err(format!(
                "keychain step failed ({what}): {}. It may need your password or approval; run tephractl daemon install again to retry. Nothing else was changed.",
                o.first_line()
            ));
        }
    }
    Ok(true)
}

/// A mode-0700 directory for the private key; removed on drop, so the
/// key never outlives the install, whichever way it ends.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create() -> io::Result<Self> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("tephra-identity-{}-{nanos}", std::process::id()));
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(PrivateDir(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Copies `source` beside the installed path, signs that copy, and
/// renames it into place.
fn install_binary(
    runner: &mut dyn CommandRunner,
    source: &Path,
    layout: &Layout,
    keychain: Option<&Path>,
) -> Result<()> {
    fs::create_dir_all(&layout.bin_dir)?;
    let tmp = layout
        .bin_dir
        .join(format!(".tephrad.tmp-{}", std::process::id()));

    let result = (|| -> Result<()> {
        fs::copy(source, &tmp)?;
        set_executable(&tmp)?;
        fs::File::open(&tmp)?.sync_all()?;

        if let Some(k) = keychain {
            // codesign only finds identities in a keychain on the
            // search list. Put the given one first, keeping the rest.
            let current = runner.run(signing::SECURITY, &signing::list_keychains_args())?;
            let mut list = vec![k.display().to_string()];
            list.extend(
                signing::parse_keychain_list(&current.stdout)
                    .into_iter()
                    .filter(|existing| Path::new(existing) != k),
            );
            let set = runner.run(signing::SECURITY, &signing::set_keychain_list_args(&list))?;
            if !set.success() {
                return err(format!(
                    "couldn't add the keychain to the search list: {}",
                    set.first_line()
                ));
            }
        }

        let signed = runner.run(signing::CODESIGN, &signing::sign_args(&tmp, keychain))?;
        if !signed.success() {
            return err(format!(
                "codesign failed: {}. If macOS asked to let codesign use the \"{}\" key, choose Always Allow and run tephractl daemon install again.",
                signed.first_line(),
                signing::IDENTITY_NAME
            ));
        }
        fs::File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &layout.installed_binary)?;
        sync_dir(&layout.bin_dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(unix)]
fn set_executable(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Writes `contents` to `path` via a temp file in the same folder:
/// fsync, then rename, then fsync the folder.
fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir)?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(contents)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        sync_dir(dir);
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_plist(layout: &Layout, opts: &InstallOptions) -> Result<()> {
    let home = match layout.home.to_str() {
        Some(h) => h.to_string(),
        None => return err("HOME is not valid UTF-8, which the launchd plist needs"),
    };
    fs::create_dir_all(&layout.logs_dir)?;
    let spec = PlistSpec {
        label: layout.label.clone(),
        program: layout.installed_binary.clone(),
        stdout_path: layout.logs_dir.join("tephrad.out.log"),
        stderr_path: layout.logs_dir.join("tephrad.err.log"),
        home,
        xdg_config_home: opts.xdg_config_home.clone(),
        xdg_data_home: opts.xdg_data_home.clone(),
    };
    let xml = plist::render_plist(&spec).map_err(|e| InstallError(e.to_string()))?;
    write_atomic(&layout.plist_path, xml.as_bytes())?;
    Ok(())
}

fn is_loaded(runner: &mut dyn CommandRunner, uid: u32, label: &str) -> Result<bool> {
    let o = runner.run(launchctl::PROGRAM, &launchctl::print_args(uid, label))?;
    Ok(status::parse_print(o.success(), &o.stdout) != LaunchdState::NotLoaded)
}

fn parse_pids(output: &str) -> Vec<u32> {
    output
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// The pids listening on the default port.
fn port_listeners(runner: &mut dyn CommandRunner) -> Result<Vec<u32>> {
    let o = runner.run(
        LSOF,
        &[
            "-nP".to_string(),
            format!("-iTCP:{DEFAULT_PORT}"),
            "-sTCP:LISTEN".to_string(),
            "-t".to_string(),
        ],
    )?;
    Ok(parse_pids(&o.stdout))
}

/// Stops a `tephrad` that launchd doesn't manage (this runs after the
/// agent was booted out, so any `tephrad` left is hand-started).
///
/// A process holding the port that is *not* named `tephrad` is never
/// signalled: install fails and names it instead.
fn stop_hand_started(runner: &mut dyn CommandRunner, uid: u32, out: &mut dyn Write) -> Result<()> {
    let by_name = runner.run(
        PGREP,
        &[
            "-u".to_string(),
            uid.to_string(),
            "-x".to_string(),
            "tephrad".to_string(),
        ],
    )?;
    let mut targets = parse_pids(&by_name.stdout);

    for pid in port_listeners(runner)? {
        if targets.contains(&pid) {
            continue;
        }
        let comm = runner.run(
            PS,
            &[
                "-p".to_string(),
                pid.to_string(),
                "-o".to_string(),
                "comm=".to_string(),
            ],
        )?;
        let name = comm.stdout.trim();
        let base = name.rsplit('/').next().unwrap_or(name);
        if base == "tephrad" {
            targets.push(pid);
        } else {
            return err(format!(
                "127.0.0.1:{DEFAULT_PORT} is already in use by pid {pid} ({}), which is not tephrad; not touching it. The agent was written but not loaded.",
                if name.is_empty() { "unknown process" } else { name }
            ));
        }
    }
    if targets.is_empty() {
        return Ok(());
    }

    for pid in &targets {
        let _ = writeln!(out, "stopping hand-started tephrad (pid {pid})");
        let o = runner.run(KILL, &["-TERM".to_string(), pid.to_string()])?;
        if !o.success() {
            return err(format!(
                "couldn't stop hand-started tephrad (pid {pid}): {}. The agent was written but not loaded.",
                o.first_line()
            ));
        }
    }

    for _ in 0..PORT_FREE_POLLS {
        if port_listeners(runner)?.is_empty() {
            return Ok(());
        }
        runner.sleep(POLL);
    }
    err(format!(
        "127.0.0.1:{DEFAULT_PORT} is still in use 5 s after stopping tephrad; the agent was written but not loaded"
    ))
}

fn bootstrap_with_retry(runner: &mut dyn CommandRunner, uid: u32, layout: &Layout) -> Result<()> {
    let args = launchctl::bootstrap_args(uid, &layout.plist_path);
    let first = runner.run(launchctl::PROGRAM, &args)?;
    if first.success() {
        return Ok(());
    }
    // e.g. error 5 (the agent is already loaded, or a bootout hasn't
    // finished): boot out and try once more.
    let _ = runner.run(
        launchctl::PROGRAM,
        &launchctl::bootout_args(uid, &layout.label),
    );
    runner.sleep(Duration::from_secs(1));
    let second = runner.run(launchctl::PROGRAM, &args)?;
    if second.success() {
        return Ok(());
    }
    let stderr = second.stderr.trim();
    err(format!(
        "launchctl bootstrap failed: {}\nthe plist is left at {}; tephrad is not running (tephractl daemon status)",
        if stderr.is_empty() { second.first_line() } else { stderr.to_string() },
        layout.plist_path.display()
    ))
}

/// Waits up to 10 s for an `access.json` written by the pid launchd
/// is now running. Not finding one is not an error: the caller says
/// the Full Disk Access state is unknown.
fn wait_for_access(runner: &mut dyn CommandRunner, uid: u32, layout: &Layout) -> Result<()> {
    for _ in 0..ACCESS_POLLS {
        let o = runner.run(
            launchctl::PROGRAM,
            &launchctl::print_args(uid, &layout.label),
        )?;
        if let LaunchdState::Running { pid } = status::parse_print(o.success(), &o.stdout) {
            if status::read_access(&layout.data_dir).is_some_and(|a| a.pid == pid) {
                return Ok(());
            }
        }
        runner.sleep(POLL);
    }
    Ok(())
}

// ---- uninstall ------------------------------------------------------

/// `tephractl daemon uninstall`: bootout, then remove the plist and the
/// installed copy. Keeps the identity and the logs, and says so.
pub fn uninstall(
    runner: &mut dyn CommandRunner,
    layout: &Layout,
    out: &mut dyn Write,
) -> Result<()> {
    let uid = current_uid(runner)?;
    let loaded = is_loaded(runner, uid, &layout.label)?;
    let has_plist = layout.plist_path.exists();
    let has_binary = layout.installed_binary.exists();
    if !loaded && !has_plist && !has_binary {
        let _ = writeln!(out, "not installed");
        return Ok(());
    }

    if loaded {
        let o = runner.run(
            launchctl::PROGRAM,
            &launchctl::bootout_args(uid, &layout.label),
        )?;
        if !o.success() {
            // Stop here: removing the plist under a loaded agent would
            // leave launchd running something with no way to unload it.
            return err(format!(
                "launchctl bootout failed: {}; nothing was removed",
                o.first_line()
            ));
        }
    }
    if has_plist {
        fs::remove_file(&layout.plist_path)?;
        let _ = writeln!(out, "removed {}", layout.plist_path.display());
    }
    if has_binary {
        fs::remove_file(&layout.installed_binary)?;
        let _ = writeln!(out, "removed {}", layout.installed_binary.display());
    }
    let _ = writeln!(
        out,
        "kept: the \"{}\" signing identity in your keychain (so a later install keeps the Full Disk Access grant) and the logs in {}",
        signing::IDENTITY_NAME,
        layout.logs_dir.display()
    );
    Ok(())
}

// ---- status ---------------------------------------------------------

/// `tephractl daemon status`.
pub fn print_status(
    runner: &mut dyn CommandRunner,
    layout: &Layout,
    json: bool,
    out: &mut dyn Write,
) -> Result<()> {
    let uid = current_uid(runner)?;
    let report: StatusReport = status::gather(runner, layout, uid)?;
    let text = if json {
        status::render_json(&report)
    } else {
        status::render_text(&report)
    };
    let _ = write!(out, "{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// Records every call and answers from a script. Files are real:
    /// it checks the installed copy and plist at the moments the real
    /// commands would see them, and plays the daemon by writing
    /// `access.json` when `launchctl bootstrap` succeeds.
    struct Mock {
        calls: Vec<String>,
        layout: Layout,
        identity_present: bool,
        loaded: bool,
        /// Which keychain step (by call label) fails.
        failing: Option<&'static str>,
        bootstrap_results: VecDeque<i32>,
        running_pid: u32,
        write_access: bool,
        access_body: String,
        /// pids `pgrep` reports.
        pgrep: Vec<u32>,
        /// What `lsof` reports on each call, in order; the last entry
        /// repeats.
        lsof: VecDeque<Vec<u32>>,
        ps_comm: String,
        key_paths: Vec<PathBuf>,
    }

    impl Mock {
        fn new(layout: &Layout) -> Self {
            Mock {
                calls: Vec::new(),
                layout: layout.clone(),
                identity_present: false,
                loaded: false,
                failing: None,
                bootstrap_results: VecDeque::new(),
                running_pid: 4242,
                write_access: true,
                access_body: String::new(),
                pgrep: Vec::new(),
                lsof: VecDeque::from([Vec::new()]),
                ps_comm: "tephrad".to_string(),
                key_paths: Vec::new(),
            }
        }

        fn ok(stdout: &str) -> CmdOutput {
            CmdOutput {
                code: Some(0),
                stdout: stdout.to_string(),
                stderr: String::new(),
            }
        }

        fn fail(code: i32, stderr: &str) -> CmdOutput {
            CmdOutput {
                code: Some(code),
                stdout: String::new(),
                stderr: stderr.to_string(),
            }
        }

        /// A short label for a call, e.g. `security import key`.
        fn label(program: &str, args: &[String]) -> String {
            let prog = program.rsplit('/').next().unwrap_or(program);
            match prog {
                "security" => match args[0].as_str() {
                    "import" if args[1].ends_with("key.pem") => "security import key".to_string(),
                    "import" => "security import cert".to_string(),
                    other => format!("security {other}"),
                },
                "codesign" if args[0] == "-dvv" => "codesign display".to_string(),
                "codesign" => "codesign sign".to_string(),
                "openssl" => "openssl req".to_string(),
                "launchctl" => format!("launchctl {}", args[0]),
                other => other.to_string(),
            }
        }

        fn labels(&self) -> Vec<&str> {
            self.calls.iter().map(String::as_str).collect()
        }

        fn position(&self, label: &str) -> usize {
            self.calls
                .iter()
                .position(|c| c == label)
                .unwrap_or_else(|| panic!("no `{label}` call in {:?}", self.calls))
        }

        fn count(&self, label: &str) -> usize {
            self.calls.iter().filter(|c| *c == label).count()
        }

        fn print_output(&self) -> CmdOutput {
            if !self.loaded {
                return Mock::fail(113, "Could not find service");
            }
            Mock::ok(&format!(
                "x = {{\n\tstate = running\n\tpid = {}\n}}\n",
                self.running_pid
            ))
        }
    }

    impl CommandRunner for Mock {
        fn run(&mut self, program: &str, args: &[String]) -> io::Result<CmdOutput> {
            let label = Mock::label(program, args);
            self.calls.push(label.clone());
            let step_failed = self.failing == Some(label.as_str());
            Ok(match label.as_str() {
                "id" => Mock::ok("501\n"),
                "security find-certificate" => {
                    if self.identity_present {
                        Mock::ok("keychain: login\n")
                    } else {
                        Mock::fail(44, "could not be found")
                    }
                }
                "openssl req" => {
                    let arg_after = |flag: &str| {
                        let i = args.iter().position(|a| a == flag).unwrap();
                        PathBuf::from(&args[i + 1])
                    };
                    let key = arg_after("-keyout");
                    let cert = arg_after("-out");
                    assert!(fs::read_to_string(arg_after("-config"))
                        .unwrap()
                        .contains("codeSigning"));
                    fs::write(&key, "PRIVATE KEY").unwrap();
                    fs::write(&cert, "CERT").unwrap();
                    self.key_paths.push(key);
                    Mock::ok("")
                }
                "security import key" | "security import cert" | "security add-trusted-cert" => {
                    if step_failed {
                        Mock::fail(1, "User interaction is not allowed.")
                    } else {
                        if label == "security import cert" {
                            self.identity_present = true;
                        }
                        Mock::ok("")
                    }
                }
                "security delete-identity" => {
                    self.identity_present = false;
                    Mock::ok("")
                }
                "security list-keychains" => {
                    if args.contains(&"-s".to_string()) {
                        Mock::ok("")
                    } else {
                        Mock::ok("    \"/Users/dan/Library/Keychains/login.keychain-db\"\n")
                    }
                }
                "codesign sign" => {
                    // The copy is signed beside its final path; the
                    // installed path must not be touched until the
                    // signature is on.
                    let target = PathBuf::from(args.last().unwrap());
                    assert_ne!(target, self.layout.installed_binary);
                    assert_eq!(target.parent().unwrap(), self.layout.bin_dir);
                    assert!(target.is_file(), "the copy must exist before it is signed");
                    assert_eq!(
                        &args[..5],
                        [
                            "--force",
                            "--sign",
                            "Tephra Local",
                            "--identifier",
                            "com.danrfletcher.tephrad"
                        ]
                        .map(String::from)
                    );
                    if self.failing == Some("codesign sign") {
                        Mock::fail(1, "no identity found")
                    } else {
                        Mock::ok("")
                    }
                }
                "codesign display" => CmdOutput {
                    code: Some(0),
                    stdout: String::new(),
                    stderr: "Identifier=com.danrfletcher.tephrad\nAuthority=Tephra Local\n"
                        .to_string(),
                },
                "launchctl print" => self.print_output(),
                "launchctl bootout" => {
                    self.loaded = false;
                    Mock::ok("")
                }
                "launchctl bootstrap" => {
                    assert!(
                        self.layout.plist_path.is_file(),
                        "the plist is written before bootstrap"
                    );
                    assert!(
                        self.layout.installed_binary.is_file(),
                        "the copy is in place before bootstrap"
                    );
                    assert_eq!(args[1], "gui/501");
                    match self.bootstrap_results.pop_front().unwrap_or(0) {
                        0 => {
                            self.loaded = true;
                            if self.write_access {
                                fs::create_dir_all(&self.layout.data_dir).unwrap();
                                let body = if self.access_body.is_empty() {
                                    format!(
                                        r#"{{"documents_readable":true,"error_kind":null,"checked_at":1,"pid":{}}}"#,
                                        self.running_pid
                                    )
                                } else {
                                    self.access_body.clone()
                                };
                                fs::write(self.layout.data_dir.join("access.json"), body).unwrap();
                            }
                            Mock::ok("")
                        }
                        code => Mock::fail(code, "Bootstrap failed: 5: Input/output error"),
                    }
                }
                "pgrep" => {
                    if self.pgrep.is_empty() {
                        Mock::fail(1, "")
                    } else {
                        Mock::ok(
                            &self
                                .pgrep
                                .iter()
                                .map(|p| format!("{p}\n"))
                                .collect::<String>(),
                        )
                    }
                }
                "lsof" => {
                    let pids = if self.lsof.len() > 1 {
                        self.lsof.pop_front().unwrap()
                    } else {
                        self.lsof.front().cloned().unwrap_or_default()
                    };
                    if pids.is_empty() {
                        Mock::fail(1, "")
                    } else {
                        Mock::ok(&pids.iter().map(|p| format!("{p}\n")).collect::<String>())
                    }
                }
                "ps" => Mock::ok(&format!("{}\n", self.ps_comm)),
                "kill" => Mock::ok(""),
                other => panic!("unexpected call: {other} {args:?}"),
            })
        }

        fn sleep(&mut self, _duration: Duration) {}
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        layout: Layout,
        opts: InstallOptions,
        source: PathBuf,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("Zoë Müller");
        fs::create_dir_all(&home).unwrap();
        let src_dir = dir.path().join("target/release");
        fs::create_dir_all(&src_dir).unwrap();
        let source = src_dir.join("tephrad");
        fs::write(&source, "tephrad build 1").unwrap();
        let layout = Layout::new(&home, DEFAULT_LABEL, None);
        let opts = InstallOptions {
            from: None,
            keychain: None,
            current_exe: Some(src_dir.join("tephractl")),
            xdg_config_home: None,
            xdg_data_home: None,
            stop_hand_started: true,
        };
        Fixture {
            _dir: dir,
            layout,
            opts,
            source,
        }
    }

    fn run_install(f: &Fixture, mock: &mut Mock) -> (Result<()>, String) {
        let mut out = Vec::new();
        let r = install(mock, &f.layout, &f.opts, false, &mut out);
        (r, String::from_utf8(out).unwrap())
    }

    #[test]
    fn first_install_runs_every_step_in_the_documented_order() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        let (result, out) = run_install(&f, &mut mock);
        result.unwrap();

        let order = [
            "id",
            "security find-certificate",
            "openssl req",
            "security import key",
            "security import cert",
            "security add-trusted-cert",
            "codesign sign",
            "launchctl print",
            "pgrep",
            "lsof",
            "launchctl bootstrap",
        ];
        let positions: Vec<usize> = order.iter().map(|l| mock.position(l)).collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "calls out of order: {:?}",
            mock.labels()
        );
        // The wait for access.json is a poll of launchctl print after
        // the bootstrap.
        let bootstrap = mock.position("launchctl bootstrap");
        assert!(mock.calls[bootstrap + 1..]
            .iter()
            .any(|c| c == "launchctl print"));
        assert_eq!(mock.count("launchctl bootout"), 0, "nothing was loaded");

        assert!(
            out.contains("signing identity: Tephra Local (created)"),
            "{out}"
        );
        assert!(out.contains("installed, running (pid 4242)"), "{out}");
        assert!(out.contains("Full Disk Access: ok"), "{out}");
    }

    #[test]
    fn first_install_leaves_exactly_the_documented_files() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        run_install(&f, &mut mock).0.unwrap();

        assert_eq!(
            fs::read_to_string(&f.layout.installed_binary).unwrap(),
            "tephrad build 1"
        );
        let leftovers: Vec<String> = fs::read_dir(&f.layout.bin_dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            leftovers,
            ["tephrad"],
            "no temp copy is left beside the install"
        );
        assert!(f.layout.logs_dir.is_dir(), "the logs folder is created");

        let plist = fs::read_to_string(&f.layout.plist_path).unwrap();
        assert!(plist.contains(&format!(
            "<string>{}</string>",
            f.layout.installed_binary.display()
        )));
        assert!(plist.contains("Zoë Müller"));
        assert!(
            !plist.contains("target/release"),
            "the plist never points into target/"
        );

        // The private key only ever lived in a private temp dir.
        assert_eq!(mock.key_paths.len(), 1);
        assert!(!mock.key_paths[0].exists());
        assert!(!mock.key_paths[0].parent().unwrap().exists());
    }

    #[test]
    fn reinstall_reuses_the_identity_boots_out_then_bootstraps_again() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        run_install(&f, &mut mock).0.unwrap();

        // New build, same identity, agent loaded.
        fs::write(&f.source, "tephrad build 2").unwrap();
        mock.calls.clear();
        let (result, out) = run_install(&f, &mut mock);
        result.unwrap();

        assert_eq!(mock.count("openssl req"), 0, "never a second identity");
        assert_eq!(mock.count("security import key"), 0);
        assert!(
            out.contains("signing identity: Tephra Local (found)"),
            "{out}"
        );
        assert!(mock.position("codesign sign") < mock.position("launchctl bootout"));
        assert!(mock.position("launchctl bootout") < mock.position("launchctl bootstrap"));
        assert_eq!(mock.count("launchctl bootstrap"), 1);
        assert_eq!(
            fs::read_to_string(&f.layout.installed_binary).unwrap(),
            "tephrad build 2"
        );
    }

    #[test]
    fn rerunning_install_rewrites_the_plist_the_same_way() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        run_install(&f, &mut mock).0.unwrap();
        let first = fs::read_to_string(&f.layout.plist_path).unwrap();
        run_install(&f, &mut mock).0.unwrap();
        assert_eq!(fs::read_to_string(&f.layout.plist_path).unwrap(), first);
    }

    #[test]
    fn a_missing_source_binary_makes_no_calls_and_changes_nothing() {
        let f = fixture();
        fs::remove_file(&f.source).unwrap();
        let mut mock = Mock::new(&f.layout);
        let (result, out) = run_install(&f, &mut mock);

        let message = result.unwrap_err().0;
        assert_eq!(
            message,
            format!("tephrad binary not found at {}", f.source.display())
        );
        assert!(
            mock.calls.is_empty(),
            "no command may run: {:?}",
            mock.calls
        );
        assert!(out.is_empty());
        assert_eq!(
            fs::read_dir(&f.layout.home).unwrap().count(),
            0,
            "nothing written under HOME"
        );
    }

    #[test]
    fn from_overrides_the_binary_next_to_tephractl() {
        let mut f = fixture();
        let other = f.source.parent().unwrap().join("elsewhere");
        fs::write(&other, "other build").unwrap();
        f.opts.from = Some(other);
        let mut mock = Mock::new(&f.layout);
        run_install(&f, &mut mock).0.unwrap();
        assert_eq!(
            fs::read_to_string(&f.layout.installed_binary).unwrap(),
            "other build"
        );
    }

    #[test]
    fn a_declined_keychain_step_stops_cleanly_and_leaves_no_half_identity() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.failing = Some("security add-trusted-cert");
        let (result, out) = run_install(&f, &mut mock);

        let message = result.unwrap_err().0;
        assert!(
            message.contains("keychain step failed (trust the certificate"),
            "{message}"
        );
        assert!(message.contains("password or approval"), "{message}");
        assert!(
            out.contains("macOS may ask for your password or approval"),
            "{out}"
        );
        assert_eq!(
            mock.count("security delete-identity"),
            1,
            "the half-made identity is removed"
        );
        for later in ["codesign sign", "launchctl bootstrap"] {
            assert_eq!(mock.count(later), 0, "{later} must not run");
        }
        assert!(!f.layout.installed_binary.exists());
        assert!(!f.layout.plist_path.exists());
        assert!(
            !mock.key_paths[0].parent().unwrap().exists(),
            "the key dir is removed on failure too"
        );
    }

    #[test]
    fn a_failed_codesign_leaves_no_copy_and_no_plist() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.failing = Some("codesign sign");
        let message = run_install(&f, &mut mock).0.unwrap_err().0;
        assert!(
            message.contains("codesign failed: no identity found"),
            "{message}"
        );
        assert!(!f.layout.installed_binary.exists());
        assert!(!f.layout.plist_path.exists());
        assert_eq!(
            fs::read_dir(&f.layout.bin_dir).unwrap().count(),
            0,
            "temp copy removed"
        );
    }

    #[test]
    fn a_hand_started_tephrad_is_stopped_before_bootstrap() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.pgrep = vec![777];
        // Port busy on the first look (and the hand-started pid is the
        // listener), free after SIGTERM.
        mock.lsof = VecDeque::from([vec![777], vec![777], Vec::new()]);
        let (result, out) = run_install(&f, &mut mock);
        result.unwrap();

        assert!(
            out.contains("stopping hand-started tephrad (pid 777)"),
            "{out}"
        );
        assert!(mock.position("kill") < mock.position("launchctl bootstrap"));
        assert_eq!(mock.count("kill"), 1);
    }

    #[test]
    fn a_port_that_stays_busy_fails_with_the_reason_and_leaves_the_plist_unloaded() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.pgrep = vec![777];
        mock.lsof = VecDeque::from([vec![777]]);
        let message = run_install(&f, &mut mock).0.unwrap_err().0;

        assert!(
            message.contains("127.0.0.1:4812 is still in use"),
            "{message}"
        );
        assert!(message.contains("not loaded"), "{message}");
        assert_eq!(mock.count("launchctl bootstrap"), 0);
        assert!(f.layout.plist_path.is_file(), "the plist stays on disk");
        assert!(!mock.loaded);
    }

    #[test]
    fn a_non_tephrad_process_on_the_port_is_never_signalled() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.lsof = VecDeque::from([vec![900]]);
        mock.ps_comm = "/Applications/Slack.app/Contents/MacOS/Slack".to_string();
        let message = run_install(&f, &mut mock).0.unwrap_err().0;

        assert!(message.contains("pid 900"), "{message}");
        assert!(message.contains("not tephrad"), "{message}");
        assert_eq!(
            mock.count("kill"),
            0,
            "someone else's process is left alone"
        );
        assert_eq!(mock.count("launchctl bootstrap"), 0);
    }

    #[test]
    fn a_non_default_label_never_looks_for_a_hand_started_tephrad() {
        let mut f = fixture();
        f.layout = Layout::new(&f.layout.home, "com.danrfletcher.tephrad.test", None);
        f.opts.stop_hand_started = false;
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.pgrep = vec![777];
        run_install(&f, &mut mock).0.unwrap();
        for never in ["pgrep", "lsof", "kill", "ps"] {
            assert_eq!(
                mock.count(never),
                0,
                "{never} must not run for a test label"
            );
        }
        assert!(f
            .layout
            .plist_path
            .ends_with("com.danrfletcher.tephrad.test.plist"));
    }

    #[test]
    fn a_failed_bootstrap_is_retried_once_after_bootout() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.bootstrap_results = VecDeque::from([5, 0]);
        run_install(&f, &mut mock).0.unwrap();

        let calls = mock.labels();
        let first = mock.position("launchctl bootstrap");
        assert_eq!(calls[first + 1], "launchctl bootout");
        assert_eq!(calls[first + 2], "launchctl bootstrap");
        assert_eq!(mock.count("launchctl bootstrap"), 2);
    }

    #[test]
    fn a_bootstrap_that_fails_twice_prints_launchctls_stderr_and_keeps_the_plist() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.bootstrap_results = VecDeque::from([5, 5]);
        let message = run_install(&f, &mut mock).0.unwrap_err().0;

        assert!(
            message.contains("Bootstrap failed: 5: Input/output error"),
            "{message}"
        );
        assert_eq!(mock.count("launchctl bootstrap"), 2, "exactly one retry");
        assert!(f.layout.plist_path.is_file());

        let mut out = Vec::new();
        print_status(&mut mock, &f.layout, false, &mut out).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            "installed, not running\nrun tephractl daemon install to repair it\n"
        );
    }

    #[test]
    fn install_waits_for_an_access_json_from_the_new_pid_and_ignores_a_stale_one() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.access_body =
            r#"{"documents_readable":true,"error_kind":null,"checked_at":1,"pid":1}"#.to_string();
        let (result, out) = run_install(&f, &mut mock);
        result.unwrap();

        assert!(out.contains("installed, running (pid 4242)"), "{out}");
        assert!(
            !out.contains("Full Disk Access: ok"),
            "a leftover from another pid says nothing: {out}"
        );
        assert!(out.contains("Full Disk Access: unknown"), "{out}");
        assert!(
            mock.count("launchctl print") > 10,
            "it polled for the new pid: {}",
            mock.count("launchctl print")
        );
    }

    #[test]
    fn install_prints_the_fix_line_when_the_daemon_cannot_read_documents() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        mock.access_body = r#"{"documents_readable":false,"error_kind":"permission_denied","checked_at":1,"pid":4242}"#.to_string();
        let (result, out) = run_install(&f, &mut mock);
        result.unwrap();
        assert!(out.contains(crate::daemon::FIX_LINE), "{out}");
        assert!(!out.contains("Full Disk Access: ok"));
    }

    #[test]
    fn the_keychain_option_goes_first_on_the_search_list_and_to_codesign() {
        let mut f = fixture();
        f.opts.keychain = Some(PathBuf::from("/tmp/t.keychain-db"));
        let mut mock = Mock::new(&f.layout);
        mock.identity_present = true;
        run_install(&f, &mut mock).0.unwrap();
        assert!(mock.position("security list-keychains") < mock.position("codesign sign"));
        assert_eq!(
            mock.count("security list-keychains"),
            2,
            "read the list, then set it"
        );
    }

    #[test]
    fn uninstall_boots_out_removes_plist_and_copy_and_says_what_it_kept() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        run_install(&f, &mut mock).0.unwrap();
        mock.calls.clear();

        let mut out = Vec::new();
        uninstall(&mut mock, &f.layout, &mut out).unwrap();
        let out = String::from_utf8(out).unwrap();

        assert_eq!(
            mock.labels(),
            ["id", "launchctl print", "launchctl bootout"]
        );
        assert!(!f.layout.plist_path.exists());
        assert!(!f.layout.installed_binary.exists());
        assert!(f.layout.logs_dir.is_dir(), "logs are kept");
        assert!(
            out.contains("kept: the \"Tephra Local\" signing identity"),
            "{out}"
        );
        assert!(
            out.contains(&f.layout.logs_dir.display().to_string()),
            "{out}"
        );
        assert!(
            mock.identity_present,
            "the identity is never deleted by uninstall"
        );
        assert_eq!(mock.count("security delete-identity"), 0);
    }

    #[test]
    fn uninstall_when_nothing_is_installed_says_so_and_succeeds() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        let mut out = Vec::new();
        uninstall(&mut mock, &f.layout, &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "not installed\n");
        assert_eq!(mock.count("launchctl bootout"), 0);
    }

    #[test]
    fn uninstall_removes_nothing_if_the_agent_will_not_boot_out() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        run_install(&f, &mut mock).0.unwrap();
        // The agent is loaded, but bootout is refused.
        struct Refuse<'a>(&'a mut Mock);
        impl CommandRunner for Refuse<'_> {
            fn run(&mut self, program: &str, args: &[String]) -> io::Result<CmdOutput> {
                if args.first().map(String::as_str) == Some("bootout") {
                    return Ok(Mock::fail(1, "Boot-out failed: 1: Operation not permitted"));
                }
                self.0.run(program, args)
            }
            fn sleep(&mut self, d: Duration) {
                self.0.sleep(d)
            }
        }
        let mut out = Vec::new();
        let message = uninstall(&mut Refuse(&mut mock), &f.layout, &mut out)
            .unwrap_err()
            .0;
        assert!(message.contains("nothing was removed"), "{message}");
        assert!(f.layout.plist_path.is_file());
        assert!(f.layout.installed_binary.is_file());
    }

    #[test]
    fn status_when_nothing_is_installed_is_not_installed() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        let mut out = Vec::new();
        print_status(&mut mock, &f.layout, true, &mut out).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["installed"], false);
        assert_eq!(v["running"], false);
        assert_eq!(
            mock.count("codesign display"),
            0,
            "no copy, nothing to inspect"
        );
    }

    #[test]
    fn status_of_a_running_install_reports_signer_pid_and_readability() {
        let f = fixture();
        let mut mock = Mock::new(&f.layout);
        run_install(&f, &mut mock).0.unwrap();
        let mut out = Vec::new();
        print_status(&mut mock, &f.layout, true, &mut out).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["installed"], true);
        assert_eq!(v["running"], true);
        assert_eq!(v["pid"], 4242);
        assert_eq!(v["signed_by"], "Tephra Local");
        assert_eq!(v["documents_readable"], true);
        assert_eq!(v["binary"], f.layout.installed_binary.display().to_string());
    }

    #[test]
    fn env_copies_xdg_vars_only_when_set_and_honours_the_label_override() {
        let get = |k: &str| match k {
            "HOME" => Some("/Users/dan".to_string()),
            "XDG_DATA_HOME" => Some("/Users/dan/.xdg".to_string()),
            "XDG_CONFIG_HOME" => Some(String::new()),
            "TEPHRA_LAUNCHD_LABEL" => Some("com.danrfletcher.tephrad.test".to_string()),
            _ => None,
        };
        let env = DaemonEnv::from_getter(get, None).unwrap();
        assert_eq!(env.xdg_config_home, None, "empty counts as unset");
        assert_eq!(
            env.layout().data_dir,
            PathBuf::from("/Users/dan/.xdg/tephra")
        );
        let opts = env.install_options(None, None);
        assert!(
            !opts.stop_hand_started,
            "a test label never stops a real daemon"
        );
        assert_eq!(opts.xdg_data_home.as_deref(), Some("/Users/dan/.xdg"));

        let plain =
            DaemonEnv::from_getter(|k| (k == "HOME").then(|| "/Users/dan".to_string()), None)
                .unwrap();
        assert_eq!(plain.label, "com.danrfletcher.tephrad");
        assert!(plain.install_options(None, None).stop_hand_started);
        assert_eq!(
            plain.layout().data_dir,
            PathBuf::from("/Users/dan/.local/share/tephra")
        );
    }

    #[test]
    fn env_refuses_a_missing_or_relative_home() {
        assert_eq!(
            DaemonEnv::from_getter(|_| None, None).unwrap_err().0,
            "HOME is not set"
        );
        assert!(
            DaemonEnv::from_getter(|k| (k == "HOME").then(|| "rel".to_string()), None).is_err()
        );
    }

    #[test]
    fn layout_paths_match_the_spec() {
        let l = Layout::new(Path::new("/Users/dan"), DEFAULT_LABEL, None);
        assert_eq!(
            l.installed_binary,
            PathBuf::from("/Users/dan/Library/Application Support/tephra/bin/tephrad")
        );
        assert_eq!(
            l.plist_path,
            PathBuf::from("/Users/dan/Library/LaunchAgents/com.danrfletcher.tephrad.plist")
        );
        assert_eq!(l.logs_dir, PathBuf::from("/Users/dan/Library/Logs/tephra"));
    }
}
