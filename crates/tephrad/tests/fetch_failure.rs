//! PR-7 failure behaviour:
//!   E7   a drop, a short or empty body, 401/403/404/5xx or an offline
//!        source leaves no final and no temp file, the item `remote`, a reason
//!        in `unit.list`, and no token anywhere; a killed daemon's temp files
//!        are swept on restart and nothing stays `downloading`
//!   E10  a symlink inside the cache root is never followed or written through
//!   E16  ENOSPC on a write or an fsync: a reason naming the disk, no partial
//!        or temp file, state `remote`, other cached files untouched
#![allow(dead_code)]

mod fixtures;
mod remote_support;

use std::io::Write;
use std::os::unix::fs::symlink;
use std::sync::{Arc, Mutex, OnceLock};

use remote_support::{files_under, Daemon, FetchRig, TOKEN};
use serde_json::{json, Value};

#[derive(Clone)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;
    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

fn logs() -> Arc<Mutex<Vec<u8>>> {
    static LOGS: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();
    LOGS.get_or_init(|| {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let _ = tracing::subscriber::set_global_default(
            tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_ansi(false)
                .with_writer(Sink(buf.clone()))
                .finish(),
        );
        buf
    })
    .clone()
}

fn error_of(reply: &Value) -> &Value {
    assert!(reply.get("error").is_some(), "expected an error: {reply}");
    &reply["error"]
}

fn reason_of(reply: &Value) -> String {
    error_of(reply)["data"]["reason"]
        .as_str()
        .unwrap_or_else(|| panic!("no reason: {reply}"))
        .to_string()
}

fn no_token_anywhere(rig: &FetchRig, answers: &[&Value]) {
    for a in answers {
        assert!(!a.to_string().contains(TOKEN), "token in an answer: {a}");
    }
    let listing = rig.d.entries("g");
    assert!(!json!(listing).to_string().contains(TOKEN));
    for rel in files_under(&rig.d.data_dir) {
        let bytes = std::fs::read(rig.d.data_dir.join(&rel)).unwrap_or_default();
        assert!(
            !bytes.windows(TOKEN.len()).any(|w| w == TOKEN.as_bytes()),
            "the token is in {rel:?}"
        );
    }
    let logged = String::from_utf8_lossy(&logs().lock().unwrap()).into_owned();
    assert!(!logged.contains(TOKEN), "the token is in the logs");
}

/// The state every failure leaves: nothing on disk, `remote`, and a reason
/// that `unit.list` shows.
fn assert_left_clean(rig: &FetchRig, path: &str, reply: &Value) -> String {
    let message = error_of(reply)["message"].as_str().unwrap().to_string();
    assert!(!rig.cached_path(path).exists(), "no final file");
    assert!(
        rig.cache_files().is_empty(),
        "no temp file: {:?}",
        rig.cache_files()
    );
    let row = rig.row(path);
    assert_eq!(row.state, "remote");
    let stored = format!("fetch failed: {message}");
    assert_eq!(
        row.reason.as_deref(),
        Some(stored.as_str()),
        "the reason is kept"
    );
    let listed = rig
        .d
        .entries("g")
        .into_iter()
        .find(|e| e["rel_path"] == json!(path))
        .unwrap();
    assert_eq!(listed["state"], json!("remote"));
    assert_eq!(listed["reason"], json!(stored), "unit.list shows why");
    assert!(!message.is_empty());
    message
}

#[test]
fn a_connection_drop_leaves_nothing_and_says_so() {
    let _ = logs();
    let rig = FetchRig::start(|_| {});
    rig.fake.gh.truncate_next_bodies(1);
    let reply = rig.fetch("README.md");
    assert_eq!(reason_of(&reply), "connection_dropped");
    assert_left_clean(&rig, "README.md", &reply);
    no_token_anywhere(&rig, &[&reply]);
    // A retry works and clears the reason.
    let ok = rig.fetch("README.md");
    assert!(ok.get("error").is_none(), "{ok}");
    assert_eq!(rig.row("README.md").reason, None);
}

#[test]
fn an_empty_body_is_a_short_body_and_is_refused() {
    let rig = FetchRig::start(|_| {});
    rig.fake.gh.empty_next_bodies(1);
    let reply = rig.fetch("README.md");
    assert!(
        ["corrupt_body", "unexpected_answer"].contains(&reason_of(&reply).as_str()),
        "{reply}"
    );
    assert_left_clean(&rig, "README.md", &reply);
}

/// The body is as long as the listing says, but its bytes are not the blob
/// the listing names (the git blob sha does not match): refused the same way.
#[test]
fn a_body_of_the_right_size_with_the_wrong_blob_sha_is_refused_and_nothing_is_kept() {
    let _ = logs();
    let rig = FetchRig::start(|_| {});
    let listed = rig.row("README.md");
    assert!(!rig.cached_path("README.md").exists(), "nothing cached yet");
    rig.fake.gh.corrupt_next_bodies(1);
    let reads = rig.reads();
    let reply = rig.fetch("README.md");
    assert_eq!(rig.reads(), reads + 1, "the body was asked for, and came");
    assert_eq!(reason_of(&reply), "corrupt_body", "{reply}");
    let message = assert_left_clean(&rig, "README.md", &reply);
    assert!(
        message.contains("not the file the listing names"),
        "the sha check said no, not the size check: {message}"
    );
    assert_eq!(rig.row("README.md").sha, listed.sha, "the listing is kept");
    no_token_anywhere(&rig, &[&reply]);
    // The next fetch starts afresh and gets the right bytes.
    let ok = rig.fetch("README.md");
    assert!(ok.get("error").is_none(), "{ok}");
    assert_eq!(rig.row("README.md").state, "cached");
}

#[test]
fn a_body_that_is_not_the_size_the_listing_gave_is_refused_and_nothing_is_kept() {
    let rig = FetchRig::start(|_| {});
    for (size, what) in [(10_000, "longer"), (3, "shorter")] {
        let cache = tephra_cache::Cache::open(&rig.d.cache_path).unwrap();
        cache
            .connection()
            .execute(
                "UPDATE remote_items SET size = ?1 WHERE source = 'g' AND path = 'README.md'",
                [size],
            )
            .unwrap();
        drop(cache);
        let reply = rig.fetch("README.md");
        assert_eq!(reason_of(&reply), "corrupt_body", "listing {what}: {reply}");
        assert_left_clean(&rig, "README.md", &reply);
    }
}

#[test]
fn http_errors_give_a_reason_and_leave_the_item_remote() {
    let _ = logs();
    for (name, status, reasons) in [
        ("403", 403u16, &["permission_denied", "rate_limited"][..]),
        ("404", 404, &["remote_missing"]),
        ("500", 500, &["server_error"]),
        ("503", 503, &["server_error"]),
    ] {
        let rig = FetchRig::start(|_| {});
        rig.fake.gh.echo_authorization_in_errors(true);
        rig.fake.gh.fail_next(20, status);
        let reply = rig.fetch("README.md");
        assert!(
            reasons.contains(&reason_of(&reply).as_str()),
            "{name}: {reply}"
        );
        assert_left_clean(&rig, "README.md", &reply);
        no_token_anywhere(&rig, &[&reply]);
    }
}

#[test]
fn a_401_says_to_sign_in_again_and_never_echoes_the_token() {
    let _ = logs();
    let rig = FetchRig::start(|_| {});
    rig.fake.gh.echo_authorization_in_errors(true);
    rig.fake.gh.revoke_token();
    let reply = rig.fetch("README.md");
    assert_eq!(reason_of(&reply), "auth_required");
    let message = assert_left_clean(&rig, "README.md", &reply);
    assert!(message.contains("tephractl auth github"), "{message}");
    no_token_anywhere(&rig, &[&reply]);
}

#[test]
fn an_offline_source_gives_a_reason_and_stays_remote() {
    let _ = logs();
    let clock = Arc::new(tephra_remote::FakeClock::at_epoch_2026());
    let fake = remote_support::Fake::start_gated(clock.clone());
    fake.atlas();
    let d = Daemon::start_full(
        &fake,
        remote_support::MemStore::signed_in(),
        None,
        clock,
        Default::default(),
    );
    d.add_github("g", remote_support::REPO);
    d.wait_active("g");
    fake.network_down();
    let reply = d.rpc("unit.fetch", json!({"unit": "@g:README.md"}));
    assert_eq!(reason_of(&reply), "offline", "{reply}");
    assert!(files_under(&d.data_dir.join("remote").join("g")).is_empty());
    let row = Cache::open_ro(&d)
        .remote_fetch_row("g", "README.md")
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "remote");
    assert!(row.reason.is_some());
    assert!(!reply.to_string().contains(TOKEN));
    // Back online: the same call works.
    fake.network_up();
    let reply = d.rpc("unit.fetch", json!({"unit": "@g:README.md"}));
    assert!(reply.get("error").is_none(), "{reply}");
    d.close();
}

struct Cache;
impl Cache {
    fn open_ro(d: &Daemon) -> tephra_cache::Cache {
        tephra_cache::Cache::open_read_only(&d.cache_path).unwrap()
    }
}

// ---- E16: the disk is full --------------------------------------------------

#[test]
fn a_full_disk_on_a_write_names_the_disk_and_leaves_nothing_and_touches_no_other_file() {
    let big = vec![7u8; 600 * 1024];
    let rig = FetchRig::start(|fake| {
        fake.gh
            .put_file(remote_support::REPO, "main", "data/big.bin", &big);
    });
    let other = rig.fetch("README.md");
    assert!(other.get("error").is_none());
    let kept = std::fs::read(rig.cached_path("README.md")).unwrap();

    // The second write of the big file fails, so part of it was written.
    rig.probe
        .fail_write_number(rig.probe.writes.load(std::sync::atomic::Ordering::SeqCst) + 2);
    let reply = rig.fetch("data/big.bin");
    assert_eq!(reason_of(&reply), "disk_full", "{reply}");
    assert!(error_of(&reply)["message"]
        .as_str()
        .unwrap()
        .contains("disk is full"));
    assert!(!rig.cached_path("data/big.bin").exists());
    assert_eq!(rig.row("data/big.bin").state, "remote");
    assert_eq!(
        rig.cache_files(),
        vec![std::path::PathBuf::from("clone/README.md")],
        "no partial or temp file; the other file stays"
    );
    assert_eq!(std::fs::read(rig.cached_path("README.md")).unwrap(), kept);
    assert_eq!(rig.row("README.md").state, "cached");

    rig.probe.heal();
    assert!(rig.fetch("data/big.bin").get("error").is_none());
    assert_eq!(std::fs::read(rig.cached_path("data/big.bin")).unwrap(), big);
}

#[test]
fn a_full_disk_on_the_file_fsync_or_the_directory_fsync_is_the_same() {
    let rig = FetchRig::start(|_| {});
    for (what, set) in [
        (
            "file fsync",
            Box::new(|on| rig.probe.fail_file_sync(on)) as Box<dyn Fn(bool)>,
        ),
        (
            "directory fsync",
            Box::new(|on| rig.probe.fail_dir_sync(on)),
        ),
    ] {
        set(true);
        let reply = rig.fetch("README.md");
        assert_eq!(reason_of(&reply), "disk_full", "{what}: {reply}");
        assert_left_clean(&rig, "README.md", &reply);
        set(false);
    }
    assert!(rig.fetch("README.md").get("error").is_none());
}

// ---- E7: a killed daemon -----------------------------------------------------

#[test]
fn a_restart_sweeps_the_temp_files_of_a_killed_fetch_and_nothing_stays_downloading() {
    let clock = Arc::new(tephra_remote::FakeClock::at_epoch_2026());
    let fake = remote_support::Fake::start_with_clock(clock.clone());
    fake.atlas();
    let d = Daemon::start_full(
        &fake,
        remote_support::MemStore::signed_in(),
        None,
        clock.clone(),
        Default::default(),
    );
    d.add_github("g", remote_support::REPO);
    d.wait_active("g");
    let cache_path = d.cache_path.clone();
    let dir = d.close_keeping_data();

    // What a killed fetch leaves: a temp file next to where the file goes,
    // and an item that says `downloading`.
    let clone = dir.path().join("remote").join("g").join("clone");
    std::fs::create_dir_all(&clone).unwrap();
    let temp = clone.join(tephrad::remote::safe_path::temp_name(7));
    std::fs::write(&temp, b"half a file").unwrap();
    let nested = clone.join("docs");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join(tephrad::remote::safe_path::temp_name(8)), b"x").unwrap();
    let keep = clone.join("keep.txt");
    std::fs::write(&keep, b"mine").unwrap();
    let cache = tephra_cache::Cache::open(&cache_path).unwrap();
    cache
        .connection()
        .execute(
            "UPDATE remote_items SET state = 'downloading' WHERE source = 'g' AND path = 'README.md'",
            [],
        )
        .unwrap();
    drop(cache);

    let d = Daemon::restart_clocked(&fake, dir.path(), clock);
    let row = Cache::open_ro(&d)
        .remote_fetch_row("g", "README.md")
        .unwrap()
        .unwrap();
    assert_eq!(row.state, "remote", "nothing stays downloading");
    assert!(!temp.exists(), "the temp file is swept");
    assert!(!nested
        .join(tephrad::remote::safe_path::temp_name(8))
        .exists());
    assert_eq!(std::fs::read(&keep).unwrap(), b"mine", "other files stay");
    assert!(d
        .rpc("unit.fetch", json!({"unit": "@g:README.md"}))
        .get("error")
        .is_none());
    d.close();
}

// ---- E10: symlinks -------------------------------------------------------------

#[test]
fn a_symlink_in_the_cache_is_never_followed_or_written_through() {
    let outside = tempfile::tempdir().unwrap();
    let rig = FetchRig::start(|_| {});
    let clone = rig.d.data_dir.join("remote").join("g").join("clone");
    std::fs::create_dir_all(&clone).unwrap();

    // docs -> a directory outside the cache: a fetch of docs/Guide.md must
    // not write there.
    symlink(outside.path(), clone.join("docs")).unwrap();
    let reply = rig.fetch("docs/Guide.md");
    assert!(reply.get("error").is_some(), "{reply}");
    assert!(
        std::fs::read_dir(outside.path()).unwrap().next().is_none(),
        "nothing was written through the link"
    );
    assert_eq!(rig.row("docs/Guide.md").state, "remote");
    std::fs::remove_file(clone.join("docs")).unwrap();

    // README.md -> a file outside: the file is not overwritten.
    let target = outside.path().join("precious.txt");
    std::fs::write(&target, b"precious").unwrap();
    symlink(&target, clone.join("README.md")).unwrap();
    let reply = rig.fetch("README.md");
    assert!(reply.get("error").is_some(), "{reply}");
    assert_eq!(std::fs::read(&target).unwrap(), b"precious");
    assert!(std::fs::symlink_metadata(clone.join("README.md"))
        .unwrap()
        .file_type()
        .is_symlink());

    // The whole `clone` directory a link: nothing is written through it.
    std::fs::remove_file(clone.join("README.md")).unwrap();
    std::fs::remove_dir_all(&clone).unwrap();
    symlink(outside.path(), &clone).unwrap();
    let reply = rig.fetch("docs/Guide.md");
    assert!(reply.get("error").is_some(), "{reply}");
    assert_eq!(
        std::fs::read_dir(outside.path()).unwrap().count(),
        1,
        "only precious.txt is out there"
    );
}
