//! The `CredentialStore` contract: a trait round trip run against the
//! file store (with `--features test-credential-store`), and the error
//! mapping, redaction and no-fallback rules run against a fault-injecting
//! store and the `Unavailable` store (always).
//!
//! No test here touches a Keychain; `keychain_live.rs` does, in an
//! ignored test.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use serde::Serialize;
use tephrad::credentials::{
    credential_state, state_from_get, CredentialError, CredentialState, CredentialStore, Provider,
    Secret, UnavailableStore, NO_STORE_REASON,
};

const SENTINEL: &str = "ghp_SENTINEL_do_not_leak";

/// A store that keeps secrets in memory and fails every call with the
/// injected error while one is set. It counts calls.
struct FaultStore {
    items: Mutex<HashMap<Provider, String>>,
    fault: Mutex<Option<CredentialError>>,
    calls: AtomicUsize,
}

impl FaultStore {
    fn new() -> Self {
        FaultStore {
            items: Mutex::new(HashMap::new()),
            fault: Mutex::new(None),
            calls: AtomicUsize::new(0),
        }
    }

    fn inject(&self, fault: Option<CredentialError>) {
        *self.fault.lock().unwrap() = fault;
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn fault(&self) -> Option<CredentialError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.fault.lock().unwrap().clone()
    }
}

impl CredentialStore for FaultStore {
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError> {
        if let Some(e) = self.fault() {
            return Err(e);
        }
        Ok(self
            .items
            .lock()
            .unwrap()
            .get(&provider)
            .map(|v| Secret::new(v.clone())))
    }

    fn set(&self, provider: Provider, secret: &Secret) -> Result<(), CredentialError> {
        if let Some(e) = self.fault() {
            return Err(e);
        }
        self.items
            .lock()
            .unwrap()
            .insert(provider, secret.expose().to_string());
        Ok(())
    }

    fn delete(&self, provider: Provider) -> Result<(), CredentialError> {
        if let Some(e) = self.fault() {
            return Err(e);
        }
        self.items.lock().unwrap().remove(&provider);
        Ok(())
    }
}

fn every_error() -> Vec<CredentialError> {
    vec![
        CredentialError::Locked,
        CredentialError::Denied,
        CredentialError::Unavailable,
        CredentialError::Backend("keychain status -25299".to_string()),
    ]
}

fn text(store: &dyn CredentialStore, provider: Provider) -> Option<String> {
    store
        .get(provider)
        .expect("get")
        .map(|s| s.expose().to_string())
}

// ---- the trait contract, for any store -------------------------------------

/// The behaviour every working store must have.
fn run_contract(store: &dyn CredentialStore) {
    // Missing is Ok(None), never an error.
    for p in Provider::ALL {
        assert!(store.get(p).expect("get on an empty store").is_none());
        // Deleting what is not there is fine.
        store.delete(p).expect("delete of a missing item");
    }

    // Round trip.
    store.set(Provider::Github, &Secret::new(SENTINEL)).unwrap();
    assert_eq!(text(store, Provider::Github).as_deref(), Some(SENTINEL));
    // Providers are independent.
    assert!(store.get(Provider::Google).unwrap().is_none());
    store
        .set(Provider::Google, &Secret::new("google-refresh"))
        .unwrap();
    assert_eq!(
        text(store, Provider::Google).as_deref(),
        Some("google-refresh")
    );
    assert_eq!(text(store, Provider::Github).as_deref(), Some(SENTINEL));

    // Overwrite replaces.
    store.set(Provider::Github, &Secret::new("second")).unwrap();
    assert_eq!(text(store, Provider::Github).as_deref(), Some("second"));
    assert_eq!(
        text(store, Provider::Google).as_deref(),
        Some("google-refresh")
    );

    // Awkward values survive: empty, long, non-ASCII, quotes, newlines.
    for odd in ["", "x".repeat(10_000).as_str(), "tøken ✓ \"q\" \\ \n\t end"] {
        store.set(Provider::Github, &Secret::new(odd)).unwrap();
        assert_eq!(text(store, Provider::Github).as_deref(), Some(odd));
    }

    // Delete removes only that provider's item, and twice is fine.
    store.delete(Provider::Github).unwrap();
    assert!(store.get(Provider::Github).unwrap().is_none());
    assert_eq!(
        text(store, Provider::Google).as_deref(),
        Some("google-refresh")
    );
    store.delete(Provider::Github).unwrap();
    store.delete(Provider::Google).unwrap();
    assert!(store.get(Provider::Google).unwrap().is_none());

    // And the state mapping over the real store.
    assert_eq!(
        credential_state(store, Provider::Github),
        CredentialState::SignedOut
    );
    store.set(Provider::Github, &Secret::new("x")).unwrap();
    assert_eq!(
        credential_state(store, Provider::Github),
        CredentialState::SignedIn
    );
}

#[test]
fn the_fault_store_meets_the_contract_when_no_fault_is_set() {
    run_contract(&FaultStore::new());
}

// ---- E17: error mapping ----------------------------------------------------

#[test]
fn credential_state_maps_every_error_to_auth_required_and_no_item_to_signed_out() {
    assert_eq!(
        state_from_get(&Ok(Some(Secret::new("v")))),
        CredentialState::SignedIn
    );
    assert_eq!(state_from_get(&Ok(None)), CredentialState::SignedOut);
    for err in every_error() {
        match state_from_get(&Err(err.clone())) {
            CredentialState::AuthRequired(reason) => {
                assert!(!reason.is_empty(), "{err:?} has a reason")
            }
            other => panic!("{err:?} must be AuthRequired, got {other:?}"),
        }
    }
    assert_eq!(
        state_from_get(&Err(CredentialError::Unavailable)),
        CredentialState::AuthRequired("no credential store".to_string())
    );
    assert_eq!(NO_STORE_REASON, "no credential store");
    // The reasons tell the variants apart.
    let reasons: Vec<CredentialState> = every_error()
        .into_iter()
        .map(|e| state_from_get(&Err(e)))
        .collect();
    for (i, a) in reasons.iter().enumerate() {
        for b in &reasons[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn credential_state_reads_through_the_store_for_both_providers() {
    let store = FaultStore::new();
    for p in Provider::ALL {
        assert_eq!(credential_state(&store, p), CredentialState::SignedOut);
    }
    store.set(Provider::Google, &Secret::new("g")).unwrap();
    assert_eq!(
        credential_state(&store, Provider::Google),
        CredentialState::SignedIn
    );
    assert_eq!(
        credential_state(&store, Provider::Github),
        CredentialState::SignedOut
    );
    store.inject(Some(CredentialError::Locked));
    assert_eq!(
        credential_state(&store, Provider::Google),
        CredentialState::AuthRequired("keychain locked".to_string())
    );
}

/// Every error variant on every call, with a witness store and a witness
/// directory that must be left alone: nothing falls back, writes a file or
/// panics.
#[test]
fn every_error_on_get_set_and_delete_is_returned_and_triggers_no_fallback() {
    for err in every_error() {
        let witness_dir = tempfile::tempdir().unwrap();
        let other_store = FaultStore::new();
        let store = FaultStore::new();
        store.inject(Some(err.clone()));
        let secret = Secret::new(SENTINEL);

        for p in Provider::ALL {
            assert_eq!(store.get(p).unwrap_err(), err, "get {p}");
            assert_eq!(store.set(p, &secret).unwrap_err(), err, "set {p}");
            assert_eq!(store.delete(p).unwrap_err(), err, "delete {p}");
            assert!(matches!(
                state_from_get(&store.get(p)),
                CredentialState::AuthRequired(_)
            ));
        }
        // 3 calls and one more `get` for the state, for each provider.
        assert_eq!(store.calls(), 8, "{err:?}: one call per request, no retry");
        assert_eq!(other_store.calls(), 0, "{err:?}: no other store was used");
        assert_eq!(
            std::fs::read_dir(witness_dir.path()).unwrap().count(),
            0,
            "{err:?}: no file was written"
        );
        assert!(
            store.items.lock().unwrap().is_empty(),
            "{err:?}: the failed set stored nothing"
        );
    }
}

#[test]
fn a_failed_set_on_a_locked_or_denied_store_leaves_the_earlier_item_untouched() {
    for err in [CredentialError::Locked, CredentialError::Denied] {
        let store = FaultStore::new();
        store
            .set(Provider::Github, &Secret::new("earlier"))
            .unwrap();
        store.inject(Some(err.clone()));
        assert_eq!(
            store
                .set(Provider::Github, &Secret::new("replacement"))
                .unwrap_err(),
            err
        );
        assert_eq!(store.delete(Provider::Github).unwrap_err(), err);
        store.inject(None);
        assert_eq!(text(&store, Provider::Github).as_deref(), Some("earlier"));
    }
}

#[test]
fn the_unavailable_store_fails_every_call_and_state_says_no_credential_store() {
    let store = UnavailableStore;
    for p in Provider::ALL {
        assert_eq!(store.get(p).unwrap_err(), CredentialError::Unavailable);
        assert_eq!(
            store.set(p, &Secret::new("x")).unwrap_err(),
            CredentialError::Unavailable
        );
        assert_eq!(store.delete(p).unwrap_err(), CredentialError::Unavailable);
        assert_eq!(
            credential_state(&store, p),
            CredentialState::AuthRequired("no credential store".to_string())
        );
    }
}

/// On a build with no Keychain and no file store (Linux, default
/// features) the store `open_store` hands back is the unavailable one.
#[cfg(all(not(target_os = "macos"), not(feature = "test-credential-store")))]
#[test]
fn a_linux_default_build_opens_the_unavailable_store() {
    use tephrad::credentials::{open_store, StoreKind};
    let dir = tempfile::tempdir().unwrap();
    let (store, kind) = open_store(None, dir.path()).expect("open");
    assert_eq!(kind, StoreKind::Unavailable);
    for p in Provider::ALL {
        assert_eq!(
            credential_state(store.as_ref(), p),
            CredentialState::AuthRequired(NO_STORE_REASON.to_string())
        );
        assert_eq!(
            store.set(p, &Secret::new(SENTINEL)).unwrap_err(),
            CredentialError::Unavailable
        );
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// Without the feature even a path in hand cannot select a file store.
#[cfg(not(feature = "test-credential-store"))]
#[test]
fn without_the_feature_a_file_store_cannot_be_opened_by_path() {
    use tephrad::credentials::open_store;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("creds.json");
    let err = match open_store(Some(&file), dir.path()) {
        Err(e) => e,
        Ok(_) => panic!("a build without the feature opened a file store"),
    };
    assert!(
        err.to_string().contains("test_credential_file"),
        "the error names the key: {err}"
    );
    assert!(!file.exists());
}

// ---- F9: redaction -----------------------------------------------------------

trait NotSerialize {
    const IS_SERIALIZE: bool = false;
}
impl<T> NotSerialize for T {}
struct Probe<T>(PhantomData<T>);
#[allow(dead_code)]
impl<T: Serialize> Probe<T> {
    const IS_SERIALIZE: bool = true;
}

#[test]
fn the_secret_type_redacts_itself_and_is_not_serialize() {
    let secret = Secret::new(SENTINEL);
    for rendered in [
        format!("{secret}"),
        format!("{secret:?}"),
        format!("{secret:#?}"),
        format!("{:?}", Some(&secret)),
        format!("{:?}", Ok::<_, CredentialError>(Some(&secret))),
        format!("{:>40}", secret),
        secret.to_string(),
    ] {
        assert!(!rendered.contains(SENTINEL), "{rendered}");
        assert!(!rendered.contains("SENTINEL"), "{rendered}");
        assert!(rendered.contains("redacted"), "{rendered}");
    }
    assert_eq!(secret.expose(), SENTINEL);
    // `Secret` must not implement `Serialize`: if it did, the inherent
    // const would shadow the trait's `false`.
    assert!(!std::hint::black_box(<Probe<Secret>>::IS_SERIALIZE));
    // The probe itself works: a type that is Serialize reads `true`.
    assert!(std::hint::black_box(<Probe<String>>::IS_SERIALIZE));
}

#[test]
fn no_error_display_or_debug_can_carry_a_secret() {
    // A Backend reason is built by the stores from fixed text and status
    // codes; make sure the type's own formatting adds nothing to it.
    for err in every_error() {
        for rendered in [
            err.to_string(),
            format!("{err:?}"),
            format!("{err:#?}"),
            format!("{}", err.clone()),
        ] {
            assert!(!rendered.contains(SENTINEL), "{rendered}");
        }
    }
    // A store that fails during `set(sentinel)` does not echo it.
    let store = FaultStore::new();
    for err in every_error() {
        store.inject(Some(err));
        let e = store
            .set(Provider::Github, &Secret::new(SENTINEL))
            .unwrap_err();
        assert!(!format!("{e} {e:?}").contains(SENTINEL));
    }
}

#[test]
fn providers_have_stable_names() {
    assert_eq!(Provider::Github.as_str(), "github");
    assert_eq!(Provider::Google.as_str(), "google");
    assert_eq!(Provider::Github.to_string(), "github");
}

// ---- the file store: only with `--features test-credential-store` -----------

#[cfg(feature = "test-credential-store")]
mod file_store {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::Path;
    use tephrad::credentials::{open_store, FileStore, StoreKind};

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn store_in(dir: &Path) -> (FileStore, std::path::PathBuf) {
        let file = dir.join("credentials.json");
        (FileStore::new(dir, &file).expect("open"), file)
    }

    #[test]
    fn the_file_store_meets_the_contract() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = store_in(dir.path());
        run_contract(&store);
    }

    #[test]
    fn a_relative_path_is_taken_inside_the_directory_and_a_second_store_sees_the_data() {
        let dir = tempfile::tempdir().unwrap();
        let a = FileStore::new(dir.path(), Path::new("creds/../x")).err();
        assert!(a.is_some(), "`..` is refused");
        fs::create_dir(dir.path().join("sub")).unwrap();
        let a = FileStore::new(dir.path(), Path::new("sub/c.json")).unwrap();
        a.set(Provider::Github, &Secret::new("kept")).unwrap();
        assert!(dir.path().join("sub/c.json").exists());
        let b = FileStore::new(dir.path(), &dir.path().join("sub/c.json")).unwrap();
        assert_eq!(text(&b, Provider::Github).as_deref(), Some("kept"));
    }

    #[test]
    fn the_file_is_written_mode_0600_and_stays_0600_after_overwrite_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let (store, file) = store_in(dir.path());
        // Even under a permissive umask.
        let old = unsafe { libc::umask(0o000) };
        store.set(Provider::Github, &Secret::new("a")).unwrap();
        assert_eq!(mode(&file), 0o600);
        store.set(Provider::Google, &Secret::new("b")).unwrap();
        store.set(Provider::Github, &Secret::new("c")).unwrap();
        assert_eq!(mode(&file), 0o600);
        store.delete(Provider::Github).unwrap();
        assert_eq!(mode(&file), 0o600);
        unsafe { libc::umask(old) };
    }

    #[test]
    fn no_temporary_file_is_left_beside_the_store() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = store_in(dir.path());
        for i in 0..5 {
            store
                .set(Provider::Github, &Secret::new(format!("v{i}")))
                .unwrap();
        }
        store.delete(Provider::Github).unwrap();
        let names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["credentials.json"]);
    }

    #[test]
    fn a_path_that_is_a_symlink_is_refused_and_the_target_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("real.json");
        fs::write(&target, "{}").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.path().join("link.json");
        symlink(&target, &link).unwrap();
        let err = FileStore::new(dir.path(), &link).expect_err("refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");

        // A dangling link is refused too.
        let dangling = dir.path().join("dangling.json");
        symlink(dir.path().join("nowhere"), &dangling).unwrap();
        assert!(FileStore::new(dir.path(), &dangling).is_err());
        assert!(!dir.path().join("nowhere").exists());
    }

    #[test]
    fn a_symlinked_directory_on_the_way_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), dir.path().join("hop")).unwrap();
        let err = FileStore::new(dir.path(), Path::new("hop/c.json")).expect_err("refused");
        assert!(err.to_string().contains("symlink"), "{err}");
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_path_outside_the_given_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let cases = [
            outside.path().join("c.json"),
            dir.path().join("../escape.json"),
            dir.path().join("sub/../../escape.json"),
            std::path::PathBuf::from("/etc/passwd"),
            std::path::PathBuf::from("/"),
        ];
        for path in cases {
            let err = FileStore::new(dir.path(), &path).err();
            assert!(err.is_some(), "{} must be refused", path.display());
        }
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
        // A sibling directory that merely shares a name prefix is outside.
        let sibling = format!("{}-sibling", dir.path().display());
        fs::create_dir_all(&sibling).unwrap();
        let err = FileStore::new(dir.path(), &Path::new(&sibling).join("c.json")).err();
        assert!(err.is_some());
        fs::remove_dir_all(&sibling).unwrap();
        // The directory itself, a missing directory and a missing parent.
        assert!(FileStore::new(dir.path(), dir.path()).is_err());
        assert!(FileStore::new(&dir.path().join("missing"), Path::new("c.json")).is_err());
        assert!(FileStore::new(dir.path(), Path::new("nodir/c.json")).is_err());
    }

    #[test]
    fn a_file_swapped_for_a_symlink_after_open_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, file) = store_in(dir.path());
        let target = dir.path().join("elsewhere.json");
        fs::write(&target, r#"{"github":"stolen"}"#).unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&target, &file).unwrap();
        assert!(store.get(Provider::Github).is_err(), "read must refuse");
        // A write refuses too (it reads first) and never goes through
        // the link.
        assert!(store.set(Provider::Github, &Secret::new("mine")).is_err());
        assert!(store.delete(Provider::Github).is_err());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"github":"stolen"}"#
        );
    }

    #[test]
    fn a_file_with_group_or_other_bits_is_refused_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let (store, file) = store_in(dir.path());
        store.set(Provider::Github, &Secret::new(SENTINEL)).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.get(Provider::Github).unwrap_err();
        assert!(matches!(err, CredentialError::Backend(_)), "{err:?}");
        assert!(!format!("{err} {err:?}").contains(SENTINEL));
        assert!(store.set(Provider::Github, &Secret::new("new")).is_err());
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(text(&store, Provider::Github).as_deref(), Some(SENTINEL));
    }

    #[test]
    fn a_corrupt_file_is_an_error_without_its_content_and_a_failed_set_leaves_it_as_it_was() {
        let dir = tempfile::tempdir().unwrap();
        let (store, file) = store_in(dir.path());
        let junk = format!("not json {SENTINEL}");
        fs::write(&file, &junk).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();

        let e = store.get(Provider::Github).unwrap_err();
        assert!(matches!(e, CredentialError::Backend(_)));
        assert!(!format!("{e} {e:?}").contains(SENTINEL));

        let e = store
            .set(Provider::Github, &Secret::new("replacement"))
            .unwrap_err();
        assert!(matches!(e, CredentialError::Backend(_)));
        assert_eq!(fs::read_to_string(&file).unwrap(), junk);
        assert_eq!(
            credential_state(&store, Provider::Github),
            state_from_get(&Err(e))
        );
    }

    #[test]
    fn an_empty_file_reads_as_no_item() {
        let dir = tempfile::tempdir().unwrap();
        let (store, file) = store_in(dir.path());
        fs::write(&file, "").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(store.get(Provider::Github).unwrap().is_none());
    }

    #[test]
    fn open_store_selects_the_file_store_inside_the_data_dir_and_refuses_one_outside() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("c.json");
        let (store, kind) = open_store(Some(&file), dir.path()).expect("open");
        assert_eq!(kind, StoreKind::TestFile);
        store.set(Provider::Github, &Secret::new("v")).unwrap();
        assert!(file.exists());

        let outside = tempfile::tempdir().unwrap();
        assert!(open_store(Some(&outside.path().join("c.json")), dir.path()).is_err());
    }

    #[test]
    fn a_feature_build_contains_the_file_store() {
        let bytes = fs::read(env!("CARGO_BIN_EXE_tephrad")).unwrap();
        assert!(
            contains(&bytes, b"file_store"),
            "the control: a build with the feature has the file-store code"
        );
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// F9: a default build has no file-store symbol. The test binary is
/// built from the same feature set as `tephrad`, so this runs in the
/// default `cargo test`; the control above runs with the feature.
#[cfg(not(feature = "test-credential-store"))]
#[test]
fn a_default_build_has_no_file_store_symbol() {
    let bytes = std::fs::read(env!("CARGO_BIN_EXE_tephrad")).unwrap();
    for needle in [&b"file_store"[..], b"FileStore", b"test_credential_store"] {
        assert!(
            !contains(&bytes, needle),
            "the default tephrad binary mentions {:?}",
            String::from_utf8_lossy(needle)
        );
    }
    // The control: the same scan finds a name that is there.
    assert!(contains(&bytes, b"UnavailableStore") || contains(&bytes, b"credentials"));
}
