//! Live macOS checks for `tephractl daemon` (G14, G20). All `#[ignore]`:
//! run on the Mac with
//!
//!     cargo build -p tephrad
//!     cargo test -p tephractl --test launchd_live -- --ignored --test-threads=1
//!
//! Safety, which every test here keeps:
//! - the label is `com.danrfletcher.tephrad.test.<pid>`, never the
//!   real agent's, and a non-default label also switches off the
//!   search for (and SIGTERM of) a hand-started tephrad;
//! - HOME is a temp dir for the plist, the installed copy, the logs and
//!   the keychain search list, and the signing identity lives in a
//!   temporary keychain passed with the hidden `--keychain`; Dan's
//!   login keychain is never opened;
//! - `add-trusted-cert` writes a user trust setting that `security`
//!   keeps per user, not per HOME. The guard removes the test
//!   certificate's trust setting, restores the real keychain search
//!   list if it changed, deletes the temp keychain and boots out the
//!   test agent, on success, failure and panic alike.
#![cfg(target_os = "macos")]

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use tephractl::daemon::install::{self, CmdOutput, CommandRunner, InstallOptions, Layout};

fn label() -> String {
    format!("com.danrfletcher.tephrad.test.{}", std::process::id())
}

fn run(program: &str, args: &[&str], home: &Path) -> Output {
    Command::new(program)
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap_or_else(|e| panic!("{program}: {e}"))
}

/// The real (not temp-HOME) user keychain search list.
fn real_search_list() -> String {
    let out = Command::new("/usr/bin/security")
        .args(["list-keychains", "-d", "user"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A throwaway keychain, a temp HOME, and the teardown for both.
struct Harness {
    dir: tempfile::TempDir,
    home: PathBuf,
    keychain: PathBuf,
    label: String,
    real_list_before: String,
    uid: String,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("Zoë home");
        // `security list-keychains -s` only persists if the user's
        // Preferences folder exists, and this HOME is empty.
        fs::create_dir_all(home.join("Library/Preferences")).unwrap();
        let keychain = dir.path().join("tephra-test.keychain-db");
        // A throwaway password for a throwaway keychain.
        let password = format!("tephra-test-{}", std::process::id());
        let real_list_before = real_search_list();
        let created = run(
            "/usr/bin/security",
            &[
                "create-keychain",
                "-p",
                &password,
                keychain.to_str().unwrap(),
            ],
            &home,
        );
        assert!(created.status.success(), "create-keychain: {created:?}");
        for args in [
            vec![
                "unlock-keychain",
                "-p",
                &password,
                keychain.to_str().unwrap(),
            ],
            vec!["set-keychain-settings", keychain.to_str().unwrap()],
        ] {
            let o = run("/usr/bin/security", &args, &home);
            assert!(o.status.success(), "{args:?}: {o:?}");
        }
        let uid = String::from_utf8(
            Command::new("/usr/bin/id")
                .arg("-u")
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
        .trim()
        .to_string();
        Harness {
            dir,
            home,
            keychain,
            label: label(),
            real_list_before,
            uid,
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(&self.home, &self.label, None)
    }

    /// A tephrad config that binds an ephemeral port, so a launchd-run
    /// test daemon can never collide with the real one on 4812.
    fn write_tephrad_config(&self) {
        let config = self.home.join(".config/tephra/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            config,
            format!(
                "listen_addr = \"127.0.0.1:0\"\ndata_dir = \"{}\"\nlog_level = \"info\"\n",
                self.layout().data_dir.display()
            ),
        )
        .unwrap();
    }

    fn certificate_count(&self) -> usize {
        let o = run(
            "/usr/bin/security",
            &[
                "find-certificate",
                "-a",
                "-c",
                "Tephra Local",
                self.keychain.to_str().unwrap(),
            ],
            &self.home,
        );
        String::from_utf8_lossy(&o.stdout)
            .matches("\"labl\"")
            .count()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Boot out the test agent, whatever state the test left it in.
        let _ = Command::new("/bin/launchctl")
            .args(["bootout", &format!("gui/{}/{}", self.uid, self.label)])
            .output();

        // Remove the trust setting `add-trusted-cert` left for the
        // test certificate (and only that certificate).
        let pem = run(
            "/usr/bin/security",
            &[
                "find-certificate",
                "-c",
                "Tephra Local",
                "-p",
                self.keychain.to_str().unwrap(),
            ],
            &self.home,
        );
        if pem.status.success() {
            let pem_path = self.dir.path().join("test-cert.pem");
            if fs::write(&pem_path, &pem.stdout).is_ok() {
                let _ = run(
                    "/usr/bin/security",
                    &["remove-trusted-cert", pem_path.to_str().unwrap()],
                    &self.home,
                );
            }
        }

        // The real search list must be what it was.
        let now = real_search_list();
        if now != self.real_list_before {
            let restored: Vec<String> = self
                .real_list_before
                .lines()
                .map(|l| l.trim().trim_matches('"').to_string())
                .filter(|l| !l.is_empty())
                .collect();
            let mut args = vec![
                "list-keychains".to_string(),
                "-d".into(),
                "user".into(),
                "-s".into(),
            ];
            args.extend(restored);
            let _ = Command::new("/usr/bin/security").args(&args).output();
        }

        let _ = run(
            "/usr/bin/security",
            &["delete-keychain", self.keychain.to_str().unwrap()],
            &self.home,
        );
        // `dir` removes the temp HOME.
    }
}

/// Real `security`/`openssl`/`codesign`/`id`, run with HOME pointed at the
/// temp dir, but `launchctl` faked: lets the signing path be exercised
/// for real on a Mac where bootstrapping an agent isn't possible.
struct SigningLiveRunner {
    home: PathBuf,
    loaded: bool,
    access_json: PathBuf,
}

impl CommandRunner for SigningLiveRunner {
    fn run(&mut self, program: &str, args: &[String]) -> io::Result<CmdOutput> {
        if program.ends_with("launchctl") {
            return Ok(match args[0].as_str() {
                "print" if self.loaded => CmdOutput {
                    code: Some(0),
                    stdout: "x = {\n\tstate = running\n\tpid = 4242\n}\n".to_string(),
                    stderr: String::new(),
                },
                "print" => CmdOutput {
                    code: Some(113),
                    stdout: String::new(),
                    stderr: "not found".into(),
                },
                "bootstrap" => {
                    self.loaded = true;
                    fs::create_dir_all(self.access_json.parent().unwrap())?;
                    fs::write(
                        &self.access_json,
                        r#"{"documents_readable":true,"error_kind":null,"checked_at":1,"pid":4242}"#,
                    )?;
                    CmdOutput {
                        code: Some(0),
                        stdout: String::new(),
                        stderr: String::new(),
                    }
                }
                "bootout" => {
                    self.loaded = false;
                    CmdOutput {
                        code: Some(0),
                        stdout: String::new(),
                        stderr: String::new(),
                    }
                }
                other => panic!("unexpected launchctl {other}"),
            });
        }
        let args_vec: Vec<String> = args.to_vec();
        let output = Command::new(program)
            .args(&args_vec)
            .env("HOME", &self.home)
            .stdin(std::process::Stdio::null())
            .output()?;
        Ok(CmdOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    fn sleep(&mut self, _duration: Duration) {}
}

/// G20 for real: `security`, `openssl` and `codesign` against a
/// temporary keychain. Install creates the identity once, signs the
/// copy with `Authority=Tephra Local` and the right identifier, and a
/// second install reuses the identity (still one certificate).
#[test]
#[ignore = "macOS: uses real security/codesign with a temporary keychain"]
fn signing_creates_one_identity_and_signs_the_copy_for_real() {
    let h = Harness::new();
    let layout = h.layout();

    // Any small Mach-O will do as the "tephrad" to install.
    let source = h.dir.path().join("tephrad-src");
    fs::copy("/usr/bin/true", &source).unwrap();
    let opts = InstallOptions {
        from: Some(source),
        keychain: Some(h.keychain.clone()),
        current_exe: None,
        xdg_config_home: None,
        xdg_data_home: None,
        stop_hand_started: false,
    };
    let mut runner = SigningLiveRunner {
        home: h.home.clone(),
        loaded: false,
        access_json: layout.data_dir.join("access.json"),
    };

    let mut out = Vec::new();
    install::install(&mut runner, &layout, &opts, false, &mut out)
        .unwrap_or_else(|e| panic!("install failed: {e}\n{}", String::from_utf8_lossy(&out)));
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("signing identity: Tephra Local (created)"),
        "{out}"
    );
    assert_eq!(h.certificate_count(), 1);

    let shown = run(
        "/usr/bin/codesign",
        &[
            "-dv",
            "--verbose=4",
            layout.installed_binary.to_str().unwrap(),
        ],
        &h.home,
    );
    let report = String::from_utf8_lossy(&shown.stderr);
    assert!(report.contains("Authority=Tephra Local"), "{report}");
    assert!(
        report.contains("Identifier=com.danrfletcher.tephrad"),
        "{report}"
    );
    let verify = run(
        "/usr/bin/codesign",
        &[
            "--verify",
            "--strict",
            layout.installed_binary.to_str().unwrap(),
        ],
        &h.home,
    );
    assert!(verify.status.success(), "{verify:?}");

    // Re-install: same identity, still exactly one certificate.
    let mut out = Vec::new();
    install::install(&mut runner, &layout, &opts, false, &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(
        out.contains("signing identity: Tephra Local (found)"),
        "{out}"
    );
    assert_eq!(h.certificate_count(), 1, "never a second identity");

    let mut out = Vec::new();
    install::uninstall(&mut runner, &layout, &mut out).unwrap();
    assert!(!layout.installed_binary.exists());
    assert_eq!(h.certificate_count(), 1, "uninstall keeps the identity");
}

fn tephractl(h: &Harness, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tephractl"))
        .args(args)
        .env_clear()
        .env("HOME", &h.home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("TEPHRA_LAUNCHD_LABEL", &h.label)
        .output()
        .expect("run tephractl")
}

/// G14 for real: install, status, kill -9 restart, uninstall, with a
/// real `launchctl bootstrap` into `gui/<uid>` under a test label. Needs
/// `cargo build -p tephrad` first, and a shell that may use launchctl
/// (not a sandboxed agent session).
#[test]
#[ignore = "macOS: bootstraps a real launchd agent under a test label"]
fn install_status_kill_restart_uninstall_cycle() {
    let h = Harness::new();
    let tephrad = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/debug/tephrad");
    assert!(
        tephrad.is_file(),
        "build it first: cargo build -p tephrad ({})",
        tephrad.display()
    );
    h.write_tephrad_config();
    let layout = h.layout();

    let installed = tephractl(
        &h,
        &[
            "daemon",
            "install",
            "--from",
            tephrad.to_str().unwrap(),
            "--keychain",
            h.keychain.to_str().unwrap(),
        ],
    );
    let stdout = String::from_utf8_lossy(&installed.stdout);
    assert!(
        installed.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(stdout.contains("installed, running (pid "), "{stdout}");
    assert!(layout.installed_binary.is_file() && layout.plist_path.is_file());

    let lint = Command::new("/usr/bin/plutil")
        .args(["-lint", layout.plist_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(lint.status.success());

    let json = tephractl(&h, &["daemon", "status", "--json"]);
    let v: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(v["installed"], true);
    assert_eq!(v["running"], true);
    assert_eq!(v["signed_by"], "Tephra Local");
    let pid = v["pid"].as_u64().expect("a pid");

    // A crash (kill -9 of *this test's* daemon, by pid) is restarted.
    let killed = Command::new("/bin/kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    let mut new_pid = None;
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(500));
        let v: serde_json::Value =
            serde_json::from_slice(&tephractl(&h, &["daemon", "status", "--json"]).stdout).unwrap();
        if v["running"] == true && v["pid"].as_u64() != Some(pid) {
            new_pid = v["pid"].as_u64();
            break;
        }
    }
    assert!(
        new_pid.is_some(),
        "launchd must restart a killed tephrad within 30 s"
    );

    let removed = tephractl(&h, &["daemon", "uninstall"]);
    assert!(removed.status.success());
    let after = tephractl(&h, &["daemon", "status"]);
    assert_eq!(
        String::from_utf8_lossy(&after.stdout).trim(),
        "not installed"
    );
    assert!(!layout.plist_path.exists() && !layout.installed_binary.exists());
    let listed = Command::new("/bin/launchctl")
        .args(["print", &format!("gui/{}/{}", h.uid, h.label)])
        .output()
        .unwrap();
    assert!(
        !listed.status.success(),
        "the test agent is gone from launchd"
    );
}
