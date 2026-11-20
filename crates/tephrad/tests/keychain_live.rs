//! Live macOS Keychain checks, all `#[ignore]`. Run on the Mac with
//!
//!     cargo test -p tephrad --test keychain_live -- --ignored --test-threads=1
//!
//! Safety, which every test here keeps (the same rules as
//! `tephractl`'s `launchd_live.rs`):
//! - everything happens in a temporary keychain file in a tempdir, with
//!   a service name carrying this process's pid. The login keychain is
//!   never opened, read or written;
//! - the temporary keychain is created with a throwaway password, and
//!   user interaction is switched off for the process while a test runs,
//!   so a locked keychain gives an error and never a dialog;
//! - the guard restores the real keychain search list if creating the
//!   temporary keychain changed it, and deletes the temporary keychain,
//!   on success, failure and panic alike.
#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::process::Command;

use security_framework::os::macos::keychain::SecKeychain;
use tephrad::credentials::{
    credential_state, CredentialError, CredentialState, CredentialStore, KeychainStore, Provider,
    Secret,
};

const SECURITY: &str = "/usr/bin/security";

fn security(args: &[&str]) -> std::process::Output {
    Command::new(SECURITY)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("security {args:?}: {e}"))
}

/// `security` with a temporary HOME, so that creating the temporary
/// keychain writes its search-list entry under that HOME and not under
/// the real user's preferences.
fn security_in(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(SECURITY)
        .args(args)
        .env("HOME", home)
        .output()
        .unwrap_or_else(|e| panic!("security {args:?}: {e}"))
}

fn real_search_list() -> String {
    String::from_utf8_lossy(&security(&["list-keychains", "-d", "user"]).stdout).into_owned()
}

/// A throwaway keychain and its teardown.
struct TempKeychain {
    dir: tempfile::TempDir,
    home: PathBuf,
    path: PathBuf,
    password: String,
    list_before: String,
}

impl TempKeychain {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("Library/Preferences")).unwrap();
        let path = dir.path().join("tephra-credential-test.keychain-db");
        let password = format!("tephra-test-{}", std::process::id());
        let list_before = real_search_list();
        // Build the guard before creating anything, so a failure part-way
        // still cleans up.
        let kc = TempKeychain {
            dir,
            home,
            path,
            password,
            list_before,
        };
        let created = security_in(
            &kc.home,
            &[
                "create-keychain",
                "-p",
                &kc.password,
                kc.path.to_str().unwrap(),
            ],
        );
        assert!(created.status.success(), "create-keychain: {created:?}");
        let unlocked = kc.unlock();
        assert!(unlocked.status.success(), "unlock-keychain: {unlocked:?}");
        // Never auto-lock mid-test.
        let settings = security_in(
            &kc.home,
            &["set-keychain-settings", kc.path.to_str().unwrap()],
        );
        assert!(settings.status.success(), "{settings:?}");
        kc
    }

    fn unlock(&self) -> std::process::Output {
        security_in(
            &self.home,
            &[
                "unlock-keychain",
                "-p",
                &self.password,
                self.path.to_str().unwrap(),
            ],
        )
    }

    fn lock(&self) {
        let out = security_in(&self.home, &["lock-keychain", self.path.to_str().unwrap()]);
        assert!(out.status.success(), "lock-keychain: {out:?}");
    }

    fn open(&self) -> SecKeychain {
        SecKeychain::open(&self.path).expect("open the temporary keychain")
    }

    fn store(&self) -> KeychainStore {
        KeychainStore::in_keychain(&service(), self.open())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempKeychain {
    fn drop(&mut self) {
        let now = real_search_list();
        if now != self.list_before {
            let mut args = vec![
                "list-keychains".to_string(),
                "-d".into(),
                "user".into(),
                "-s".into(),
            ];
            args.extend(
                self.list_before
                    .lines()
                    .map(|l| l.trim().trim_matches('"').to_string())
                    .filter(|l| !l.is_empty()),
            );
            let _ = Command::new(SECURITY).args(&args).output();
        }
        let _ = security_in(
            &self.home,
            &["delete-keychain", self.path.to_str().unwrap()],
        );
        let _ = &self.dir; // the tempdir removes the file when dropped
    }
}

fn service() -> String {
    format!("com.danrfletcher.tephra.test.{}", std::process::id())
}

fn text(store: &dyn CredentialStore, p: Provider) -> Option<String> {
    store.get(p).expect("get").map(|s| s.expose().to_string())
}

#[test]
#[ignore = "touches the macOS Keychain (a temporary one); run with --ignored --test-threads=1"]
fn round_trip_overwrite_and_delete_in_a_temporary_keychain() {
    let kc = TempKeychain::new();
    let store = kc.store();

    assert!(text(&store, Provider::Github).is_none());
    assert_eq!(
        credential_state(&store, Provider::Github),
        CredentialState::SignedOut
    );
    store
        .delete(Provider::Github)
        .expect("delete of a missing item");

    store
        .set(Provider::Github, &Secret::new("ghp_SENTINEL_do_not_leak"))
        .unwrap();
    assert_eq!(
        text(&store, Provider::Github).as_deref(),
        Some("ghp_SENTINEL_do_not_leak")
    );
    assert_eq!(
        credential_state(&store, Provider::Github),
        CredentialState::SignedIn
    );
    assert!(text(&store, Provider::Google).is_none(), "per provider");

    store
        .set(Provider::Github, &Secret::new("second ✓"))
        .unwrap();
    assert_eq!(text(&store, Provider::Github).as_deref(), Some("second ✓"));
    store.set(Provider::Google, &Secret::new("g")).unwrap();

    store.delete(Provider::Github).unwrap();
    assert!(text(&store, Provider::Github).is_none());
    assert_eq!(text(&store, Provider::Google).as_deref(), Some("g"));
    store.delete(Provider::Google).unwrap();
    store.delete(Provider::Google).expect("delete twice");

    // The item lived in the temporary keychain only.
    let login = security(&["find-generic-password", "-s", &service(), "-a", "github"]);
    assert!(
        !login.status.success(),
        "no item with the test service in the default search list"
    );
    assert!(kc.path().exists());
}

#[test]
#[ignore = "touches the macOS Keychain (a temporary one); run with --ignored --test-threads=1"]
fn a_locked_keychain_gives_an_error_state_and_a_failed_set_keeps_the_earlier_item() {
    let kc = TempKeychain::new();
    let store = kc.store();
    store
        .set(Provider::Github, &Secret::new("earlier"))
        .unwrap();

    kc.lock();
    // No dialog: with interaction off a locked keychain reports an error.
    let _no_ui = SecKeychain::disable_user_interaction().expect("disable user interaction");

    let got = store.get(Provider::Github);
    let set = store.set(Provider::Github, &Secret::new("replacement"));
    let del = store.delete(Provider::Github);
    for (what, result) in [("get", got.map(|_| ())), ("set", set), ("delete", del)] {
        match result {
            Err(CredentialError::Locked | CredentialError::Denied) => {}
            other => {
                panic!("{what} on a locked keychain: expected Locked or Denied, got {other:?}")
            }
        }
    }
    match credential_state(&store, Provider::Github) {
        CredentialState::AuthRequired(reason) => assert!(!reason.is_empty()),
        other => panic!("expected AuthRequired, got {other:?}"),
    }
    drop(_no_ui);

    let unlocked = kc.unlock();
    assert!(unlocked.status.success(), "{unlocked:?}");
    assert_eq!(
        text(&store, Provider::Github).as_deref(),
        Some("earlier"),
        "the failed set left the earlier item untouched"
    );
}
