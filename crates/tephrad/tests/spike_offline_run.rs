//! Runs the offline spike script for real (PR-2): `spikes/run-offline.sh`
//! (S1 git half and S3) on the bare repo `spikes/s1/make-fixture.sh`
//! generates (50 files, one pure rename, one rename with a small edit), then
//! checks the exit status, the numbers file and the transcripts. No mocks.
//!
//! Live REST, S4 and S5 never run here. The credential checks below run the
//! live scripts with `TEPHRA_SPIKE_NO_KEYCHAIN=1`, which makes every Keychain
//! item look missing: each script has to stop before any network call, print
//! `not run: no credential` and exit 0.
//!
//! The run needs `git` (2.25 or newer for the sparse-checkout half) and, for
//! S3's watcher, `cargo`. With `git` missing the tests print a message and
//! return; with an old git the S3 assertions check the `not run` line instead.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn have(cmd: &str, arg: &str) -> bool {
    Command::new(cmd)
        .arg(arg)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

struct OfflineRun {
    output: Output,
    numbers: HashMap<String, String>,
    transcripts: PathBuf,
    _dir: tempfile::TempDir,
}

/// The one shared run of `spikes/run-offline.sh`.
fn offline_run() -> Option<&'static OfflineRun> {
    static RUN: OnceLock<Option<OfflineRun>> = OnceLock::new();
    RUN.get_or_init(|| {
        if !have("git", "--version") {
            eprintln!("skipping: git is not installed");
            return None;
        }
        let dir = tempfile::tempdir().expect("temp dir");
        let out_dir = dir.path().join("out");
        let transcripts = dir.path().join("transcripts");
        let output = Command::new("bash")
            .arg(root().join("spikes/run-offline.sh"))
            .env("TEPHRA_SPIKE_OUT", &out_dir)
            .env("TEPHRA_SPIKE_TRANSCRIPTS", &transcripts)
            .env("TMPDIR", dir.path())
            .output()
            .expect("run spikes/run-offline.sh");
        let numbers = fs::read_to_string(out_dir.join("numbers.txt"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Some(OfflineRun {
            output,
            numbers,
            transcripts,
            _dir: dir,
        })
    })
    .as_ref()
}

fn num<'a>(run: &'a OfflineRun, key: &str) -> &'a str {
    run.numbers.get(key).unwrap_or_else(|| {
        panic!(
            "numbers file has no {key}; it has {:?}\nstdout:\n{}\nstderr:\n{}",
            run.numbers.keys().collect::<Vec<_>>(),
            String::from_utf8_lossy(&run.output.stdout),
            String::from_utf8_lossy(&run.output.stderr)
        )
    })
}

fn num_u(run: &OfflineRun, key: &str) -> u64 {
    num(run, key)
        .parse()
        .unwrap_or_else(|_| panic!("{key} is not a number: {:?}", num(run, key)))
}

fn s3_ran(run: &OfflineRun) -> bool {
    !run.numbers.contains_key("s3.status")
}

#[test]
fn offline_run_exits_zero_and_writes_the_numbers_file() {
    let Some(run) = offline_run() else { return };
    assert!(
        run.output.status.success(),
        "run-offline.sh failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.output.stdout),
        String::from_utf8_lossy(&run.output.stderr)
    );
    assert!(!run.numbers.is_empty(), "numbers file is empty");
    assert!(run.numbers.contains_key("offline.git_version"));
}

#[test]
fn s1_fixture_has_fifty_files_and_both_renames_are_found_by_diff_m() {
    let Some(run) = offline_run() else { return };
    assert_eq!(num_u(run, "s1.fixture.files"), 50);
    assert_eq!(num_u(run, "s1.fixture.ls_tree_entries"), 50);
    assert_eq!(num_u(run, "s1.fixture.rename_count"), 2);
    assert_eq!(
        num_u(run, "s1.fixture.rename_pure_found"),
        1,
        "pure rename (R100)"
    );
    assert_eq!(
        num_u(run, "s1.fixture.rename_edited_found"),
        1,
        "rename with a small edit"
    );
}

#[test]
fn s1_listing_and_fetch_put_no_file_content_in_dot_git_but_the_first_index_read_does() {
    let Some(run) = offline_run() else { return };
    // A blobless clone, a fetch and ls-tree bring commits and trees only.
    for step in ["after_blobless_clone", "after_fetch", "after_ls_tree"] {
        assert_eq!(
            num_u(run, &format!("s1.fixture.{step}.git_blob_objects")),
            0,
            "blobs after {step}"
        );
    }
    // diff -M needs both sides of the rename with an edit: git fetches them.
    assert!(num_u(run, "s1.fixture.after_diff_m.git_blob_objects") >= 1);
    // The first-index read fetches every text file's content into .git.
    let landed = num_u(run, "s1.fixture.blobs_landed_by_index_read");
    assert!(landed > 0, "first-index read landed no blobs in .git");
    assert!(
        num_u(run, "s1.fixture.after_index_read.git_blob_objects")
            >= num_u(run, "s1.fixture.text_files")
    );
    assert!(
        num_u(run, "s1.fixture.after_index_read.git_bytes")
            > num_u(run, "s1.fixture.after_ls_tree.git_bytes"),
        ".git must be bigger after the first-index read"
    );
    assert_eq!(num_u(run, "s1.fixture.before_clone.git_kb"), 0);
}

#[test]
fn s3_sparse_set_read_outside_and_protect_checks() {
    let Some(run) = offline_run() else { return };
    if !s3_ran(run) {
        let status = num(run, "s3.status");
        assert!(
            status.starts_with("not run:"),
            "S3 skipped without a reason: {status}"
        );
        return;
    }
    // After `sparse-checkout set docs` the working tree holds the 4 root files
    // and the 20 docs files.
    assert_eq!(num_u(run, "s3.A.working_tree_files"), 24);
    // Adding src brings 20 more.
    assert_eq!(num_u(run, "s3.C.working_tree_files"), 44);
    // The fetch changes only .git.
    assert_eq!(num_u(run, "s3.B.working_tree_files"), 24);
    // Reading a file outside the set goes through git: one blob lands in
    // .git, and the file never appears in the working tree.
    assert_eq!(num_u(run, "s3.read_outside.exit_code"), 0);
    assert_eq!(num_u(run, "s3.read_outside.promisor_blobs_added"), 1);
    assert!(num_u(run, "s3.read_outside.promisor_packs_added") >= 1);
    assert_eq!(num(run, "s3.outside_file_on_disk_before_read"), "no");
    assert_eq!(num(run, "s3.outside_file_on_disk_after_read"), "no");
    // Narrowing the set: the clean file leaves the working tree and the status
    // stays clean for it; the edited file is kept by git; the file with an
    // unpushed commit is removed from disk (its commit stays in history).
    assert_eq!(
        num(run, "s3.E.after_narrowing.docs_doc_04_clean_file"),
        "absent"
    );
    assert_eq!(
        num(run, "s3.E.after_narrowing.docs_doc_02_dirty_file"),
        "present"
    );
    assert_eq!(
        num(run, "s3.E.after_narrowing.docs_doc_03_unpushed_commit_file"),
        "absent"
    );
    assert_eq!(num_u(run, "s3.E.local_commits_ahead_of_origin"), 1);
    assert_eq!(
        num_u(run, "s3.E.after_narrowing.local_commits_ahead_of_origin"),
        1
    );
    // A file taken out of the set is not a change; one removed by hand is.
    assert_eq!(
        num_u(run, "s3.E.after_hand_removal.status_changes"),
        num_u(run, "s3.E.after_narrowing.status_changes") + 1
    );
    assert!(num(run, "s3.E.after_hand_removal.status_line").contains("mod-05"));
    // The watcher produced a log; counts are recorded, not asserted (event
    // delivery timing is the OS's).
    for phase in ["A", "B", "C", "D", "E1", "E2", "E3"] {
        assert!(
            run.numbers
                .contains_key(&format!("s3.{phase}.events_total")),
            "no event count for phase {phase}"
        );
    }
}

#[test]
fn transcripts_are_saved_for_both_spikes() {
    let Some(run) = offline_run() else { return };
    for (file, marker) in [("S1.txt", "== S1 git half"), ("S3.txt", "S3")] {
        let text = fs::read_to_string(run.transcripts.join(file))
            .unwrap_or_else(|e| panic!("no transcript {file}: {e}"));
        assert!(text.contains(marker), "{file} lacks {marker:?}:\n{text}");
    }
    // The home directory is redacted from transcripts.
    let home = std::env::var("HOME").unwrap_or_default();
    if home.len() > 2 {
        for file in ["S1.txt", "S3.txt"] {
            let text = fs::read_to_string(run.transcripts.join(file)).unwrap();
            assert!(
                !text.contains(&home),
                "{file} must not contain the home directory"
            );
        }
    }
}

#[test]
fn offline_run_with_old_git_says_not_run_for_s3() {
    // The gate itself, tested with a fake `git` first on PATH that reports
    // 2.20: S3 must say `not run` and the script must still exit 0.
    if !have("git", "--version") {
        eprintln!("skipping: git is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = dir.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let real_git = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v git"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let shim = format!(
        "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo 'git version 2.20.1'; exit 0; fi\nexec '{real_git}' \"$@\"\n"
    );
    let shim_path = bin.join("git");
    fs::write(&shim_path, shim).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let out_dir = dir.path().join("out");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("bash")
        .arg(root().join("spikes/run-offline.sh"))
        .env("TEPHRA_SPIKE_OUT", &out_dir)
        .env("PATH", path)
        .env("TMPDIR", dir.path())
        .output()
        .expect("run spikes/run-offline.sh");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let numbers = fs::read_to_string(out_dir.join("numbers.txt")).unwrap();
    assert!(
        numbers.contains("s3.status=not run: git older than 2.25"),
        "numbers:\n{numbers}"
    );
    assert!(
        !numbers.contains("s3.A."),
        "S3 must not have run:\n{numbers}"
    );
    // S1's git half does not need 2.25 and still ran.
    assert!(numbers.contains("s1.fixture.files=50"));
}

/// Runs a live script with every Keychain item pretending to be missing.
fn run_without_credential(program: &str, args: &[&str]) -> (Output, String) {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = Command::new(program)
        .args(args)
        .env("TEPHRA_SPIKE_NO_KEYCHAIN", "1")
        .env("TEPHRA_SPIKE_OUT", dir.path())
        .env("TMPDIR", dir.path())
        .current_dir(root())
        .output()
        .unwrap_or_else(|e| panic!("run {program}: {e}"));
    let numbers = fs::read_to_string(dir.path().join("numbers-live.txt")).unwrap_or_default();
    (output, numbers)
}

#[test]
fn missing_credentials_exit_zero_with_not_run_and_touch_no_network() {
    // S1 live without --unauthenticated stops before git or REST.
    let (out, numbers) = run_without_credential("bash", &["spikes/s1/live.sh"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("not run: no credential"));
    assert!(
        numbers.contains("s1.public.rest.status=not run: no credential"),
        "{numbers}"
    );

    if have("python3", "--version") {
        for (script, key) in [
            ("spikes/s4/run.py", "s4.status"),
            ("spikes/s5/run.py", "s5.status"),
        ] {
            for mode in ["run", "list"] {
                if script.contains("s4") && mode == "list" {
                    continue;
                }
                let (out, numbers) = run_without_credential("python3", &[script, mode]);
                assert!(
                    out.status.success(),
                    "{script} {mode}: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
                assert!(String::from_utf8_lossy(&out.stdout).contains("not run: no credential"));
                assert!(
                    numbers.contains(&format!("{key}=not run: no credential")),
                    "{numbers}"
                );
            }
        }
        let (out, numbers) = run_without_credential("python3", &["spikes/s4/run.py", "recheck"]);
        assert!(out.status.success());
        assert!(
            numbers.contains("s4.recheck.status=not run: no credential"),
            "{numbers}"
        );
        let (out, numbers) = run_without_credential(
            "python3",
            &[
                "spikes/s1/rest.py",
                "--repo",
                "a/b",
                "--base",
                "x",
                "--head",
                "y",
                "--label",
                "s1.t",
            ],
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            numbers.contains("s1.t.rest.status=not run: no credential"),
            "{numbers}"
        );
    }
}

#[test]
fn python_helper_selftests_pass() {
    if !have("python3", "--version") {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    for (script, flag) in [
        ("spikes/s4/run.py", "selftest"),
        ("spikes/s5/run.py", "selftest"),
        // the rate-limit path: a 403 with a reset time, no partial numbers
        ("spikes/s1/rest.py", "--selftest"),
    ] {
        let out = Command::new("python3")
            .args([script, flag])
            .current_dir(root())
            .output()
            .expect("run selftest");
        assert!(
            out.status.success() && String::from_utf8_lossy(&out.stdout).contains("selftest ok"),
            "{script}: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

// ---------------------------------------------------------------------------
// No network with a credential present, rename comparison and rclone checksum
// (review round 1: R1, R2, R3).
// ---------------------------------------------------------------------------

/// A TCP port nothing listens on, for a proxy that refuses every connection.
fn dead_proxy() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind a scratch port");
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    format!("http://127.0.0.1:{port}")
}

/// A directory holding a `security` shim that answers
/// `security find-generic-password -s <item> -w` with a made-up credential
/// (base64 of a JSON object), so the live scripts believe a credential
/// exists. Nothing real is read or written.
fn fake_keychain(dir: &Path) -> PathBuf {
    let bin = dir.join("fakebin");
    fs::create_dir_all(&bin).unwrap();
    let shim = "#!/bin/sh\nprintf '%s' '{\"client_id\":\"fake\",\"client_secret\":\"fake\",\"refresh_token\":\"fake\",\"issued_at\":1}' | base64 | tr -d '\\n'\n";
    let path = bin.join("security");
    fs::write(&path, shim).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    bin
}

/// A command whose every HTTP(S) request is refused, with the fake credential.
fn no_network_command(program: &str, args: &[&str], dir: &Path) -> Command {
    let proxy = dead_proxy();
    let path = format!(
        "{}:{}",
        fake_keychain(dir).display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(root())
        .env("PATH", path)
        .env("TEPHRA_SPIKE_OUT", dir.join("out"))
        .env("TMPDIR", dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("TEPHRA_SPIKE_NO_KEYCHAIN")
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env(var, &proxy);
    }
    cmd
}

fn numbers_of(dir: &Path) -> String {
    fs::read_to_string(dir.join("out/numbers-live.txt")).unwrap_or_default()
}

#[test]
fn google_spikes_with_a_credential_and_no_network_say_not_run_and_exit_zero() {
    if !have("python3", "--version") {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    for (script, mode, key) in [
        ("spikes/s4/run.py", "run", "s4.status"),
        ("spikes/s4/run.py", "recheck", "s4.recheck.status"),
        ("spikes/s5/run.py", "run", "s5.status"),
        ("spikes/s5/run.py", "list", "s5.status"),
        ("spikes/s5/run.py", "resume", "s5.status"),
    ] {
        let dir = tempfile::tempdir().expect("temp dir");
        let out = no_network_command("python3", &[script, mode], dir.path())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{script} {mode} exited {:?}:\n{stdout}\n{stderr}",
            out.status.code()
        );
        assert!(
            !stderr.contains("Traceback"),
            "{script} {mode} printed a traceback:\n{stderr}"
        );
        assert!(
            stdout.contains("not run: no network"),
            "{script} {mode}:\n{stdout}"
        );
        let numbers = numbers_of(dir.path());
        assert!(
            numbers.contains(&format!("{key}=not run: no network")),
            "{script} {mode}: {numbers}"
        );
    }
}

/// T1: a Keychain answer wrapped over several lines (what GNU `base64` prints
/// for anything over 76 columns) must come back as one header-safe token, not
/// a value with embedded newlines.
#[test]
fn github_keychain_token_has_no_whitespace_when_the_item_value_is_wrapped() {
    if !have("python3", "--version") {
        eprintln!("skipping: python3 is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    let bin = dir.path().join("fakebin");
    fs::create_dir_all(&bin).unwrap();
    let shim = bin.join("security");
    fs::write(
        &shim,
        "#!/bin/sh\nprintf 'abcdefgh\\nijklmnop\\nqrstuvwx\\n'\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("python3")
        .args([
            "-c",
            "import sys; sys.path.insert(0, 'spikes/s1'); import rest; print(repr(rest.keychain_token()))",
        ])
        .current_dir(root())
        .env("PATH", path)
        .env_remove("TEPHRA_SPIKE_NO_KEYCHAIN")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "'abcdefghijklmnopqrstuvwx'"
    );
}

#[test]
fn run_live_with_a_credential_and_no_network_runs_every_spike_and_exits_zero() {
    if !have("python3", "--version") || !have("git", "--version") {
        eprintln!("skipping: python3 or git is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("temp dir");
    // Lists an earlier run left behind. S1's live half must delete them, not
    // compare against them.
    let out_dir = dir.path().join("out");
    fs::create_dir_all(&out_dir).unwrap();
    fs::write(
        out_dir.join("s1.public-git-renames.txt"),
        "renamed old.md new.md\n",
    )
    .unwrap();
    fs::write(
        out_dir.join("s1.public-rest-renames.txt"),
        "renamed a.md b.md\n",
    )
    .unwrap();
    let out = no_network_command("bash", &["spikes/run-live.sh"], dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "run-live.sh exited {:?}:\n{stdout}\n{stderr}",
        out.status.code()
    );
    assert!(!stderr.contains("Traceback"), "{stderr}");
    let numbers = numbers_of(dir.path());
    // S4 and S5 both ran to their own status line: S4 did not abort the run.
    for want in [
        "s4.status=not run: no network",
        "s5.status=not run: no network",
        "s1.public.rest.status=not run: no network",
    ] {
        assert!(numbers.contains(want), "no {want} in:\n{numbers}");
    }
    // R1: neither half finished, so there is no rename comparison number.
    assert!(
        numbers.contains("s1.public.rename_disagreements=not run:"),
        "{numbers}"
    );
    assert!(
        !out_dir.join("s1.public-git-renames.txt").exists()
            && !out_dir.join("s1.public-rest-renames.txt").exists(),
        "stale rename lists were not cleared"
    );
}

/// Source the shared helpers and call `spike_compare_renames s1.t` in a scratch
/// output directory holding the given lists. Returns the numbers file.
fn compare_renames(git_list: Option<&str>, rest_list: Option<&str>) -> String {
    let dir = tempfile::tempdir().expect("temp dir");
    let out_dir = dir.path().join("out");
    fs::create_dir_all(&out_dir).unwrap();
    if let Some(text) = git_list {
        fs::write(out_dir.join("s1.t-git-renames.txt"), text).unwrap();
    }
    if let Some(text) = rest_list {
        fs::write(out_dir.join("s1.t-rest-renames.txt"), text).unwrap();
    }
    let out = Command::new("bash")
        .arg("-c")
        .arg(". spikes/lib/common.sh && spike_compare_renames s1.t")
        .current_dir(root())
        .env("TEPHRA_SPIKE_OUT", &out_dir)
        .env_remove("NUMBERS")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(out_dir.join("numbers.txt")).unwrap_or_default()
}

#[test]
fn rename_comparison_runs_only_when_both_halves_wrote_a_list() {
    let git = "renamed a.md b.md\nrenamed c.md d.md\n";
    // The reported case: git found 2 renames, REST did not finish. No number.
    let numbers = compare_renames(Some(git), None);
    assert!(
        numbers.contains("s1.t.rename_disagreements=not run: the REST half did not finish"),
        "{numbers}"
    );
    let numbers = compare_renames(None, Some(git));
    assert!(
        numbers.contains("s1.t.rename_disagreements=not run: the git half did not finish"),
        "{numbers}"
    );
    let numbers = compare_renames(None, None);
    assert!(
        numbers.contains("rename_disagreements=not run: the git and REST half did not finish"),
        "{numbers}"
    );
    // Both finished and agree (in any order): 0.
    let numbers = compare_renames(Some(git), Some("renamed c.md d.md\nrenamed a.md b.md\n"));
    assert!(numbers.contains("s1.t.rename_disagreements=0"), "{numbers}");
    // Both finished and differ: the number of differing lines.
    let numbers = compare_renames(Some(git), Some("renamed a.md b.md\n"));
    assert!(numbers.contains("s1.t.rename_disagreements=1"), "{numbers}");
    // Both finished with no renames at all is a real answer: 0, not `not run`.
    let numbers = compare_renames(Some(""), Some(""));
    assert!(numbers.contains("s1.t.rename_disagreements=0"), "{numbers}");
}

#[test]
fn fetch_rclone_refuses_an_archive_that_does_not_match_the_pinned_checksum() {
    let dir = tempfile::tempdir().expect("temp dir");
    // Whatever the target, the pinned version's path under the base URL.
    let release = dir.path().join("mirror/v1.76.0");
    fs::create_dir_all(&release).unwrap();
    for target in ["osx-arm64", "osx-amd64", "linux-amd64", "linux-arm64"] {
        fs::write(
            release.join(format!("rclone-v1.76.0-{target}.zip")),
            "not the real archive",
        )
        .unwrap();
    }
    let bin = dir.path().join("bin");
    let out = Command::new("bash")
        .arg(root().join("spikes/s2/fetch-rclone.sh"))
        .env(
            "TEPHRA_SPIKE_RCLONE_BASE",
            format!("file://{}", dir.path().join("mirror").display()),
        )
        .env("TEPHRA_SPIKE_BIN_DIR", &bin)
        .env("TMPDIR", dir.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.starts_with("not run: no rclone release") {
        eprintln!("skipping: no rclone release for this platform");
        return;
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}{stderr}");
    assert!(stderr.contains("checksum mismatch"), "{stderr}");
    assert!(
        !bin.join("rclone").exists(),
        "a binary was installed from an archive that failed the check"
    );
}
