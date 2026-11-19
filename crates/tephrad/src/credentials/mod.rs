//! Where the daemon keeps provider credentials (a GitHub token, a Google
//! refresh token): only in an OS credential store, never in `config.toml`,
//! `cache.db`, the environment, a log line or an error message.
//!
//! - [`CredentialStore`] is the trait: `get`, `set` and `delete` one
//!   [`Secret`] per [`Provider`].
//! - [`Secret`] redacts itself in `Debug` and `Display`, has no
//!   `Serialize`, and is only read through [`Secret::expose`], so a
//!   secret reaches a log or a file only by a call that names `expose`.
//! - [`CredentialError`] separates a locked, denied, unavailable or
//!   failing store from "no item", which is `Ok(None)` on `get`. No
//!   variant ever carries a secret.
//! - [`credential_state`] / [`state_from_get`] fold all of that into
//!   `SignedIn`, `SignedOut` or `AuthRequired(reason)`.
//!
//! Stores: the macOS Keychain (`keychain.rs`, macOS only), a test-only
//! file store (`file_store.rs`, only with the cargo feature
//! `test-credential-store`, off by default), and [`UnavailableStore`] on
//! every other build, which fails every call as a locked Keychain would.
//! There is no fallback between them: [`open_store`] picks one and a
//! failure of that one is the caller's to handle, not a reason to try
//! another store or write a file.

use std::fmt;
use std::path::Path;

#[cfg(feature = "test-credential-store")]
mod file_store;
#[cfg(target_os = "macos")]
mod keychain;

#[cfg(feature = "test-credential-store")]
pub use file_store::FileStore;
#[cfg(target_os = "macos")]
pub use keychain::{KeychainStore, KEYCHAIN_SERVICE};

/// The `AuthRequired` reason for a build with no credential store.
pub const NO_STORE_REASON: &str = "no credential store";

/// A credential provider; the key every item is stored under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Provider {
    Github,
    Google,
}

impl Provider {
    /// Both providers, in a fixed order.
    pub const ALL: [Provider; 2] = [Provider::Github, Provider::Google];

    /// The stable name used as the Keychain account and the file-store key.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Github => "github",
            Provider::Google => "google",
        }
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A secret value. Not `Clone`, not `Serialize`; `Debug` and `Display`
/// print a fixed placeholder whatever the content. The bytes are zeroed
/// when it is dropped (best effort: the allocator may have copied them
/// during a grow, which this type never does).
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Secret(value.into())
    }

    /// The secret text. The one way to read it: hand it to the store or
    /// to the HTTP client's auth header, never to a log, an error or a
    /// file.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        for b in bytes.iter_mut() {
            *b = 0;
        }
        std::hint::black_box(&bytes);
    }
}

/// Why a store call failed. "No item" is not an error: `get` returns
/// `Ok(None)`, and `delete` of a missing item is `Ok(())`.
///
/// `Backend` carries a reason made only of fixed text and platform
/// status codes, never of a secret or of file content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialError {
    /// The store exists but is locked and could not be unlocked here.
    Locked,
    /// The user or the system refused access to the item.
    Denied,
    /// There is no credential store in this build or on this machine.
    Unavailable,
    /// Anything else the store reported.
    Backend(String),
}

impl fmt::Display for CredentialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CredentialError::Locked => f.write_str("the credential store is locked"),
            CredentialError::Denied => f.write_str("access to the credential store was denied"),
            CredentialError::Unavailable => f.write_str("no credential store is available"),
            CredentialError::Backend(reason) => write!(f, "credential store error: {reason}"),
        }
    }
}

impl std::error::Error for CredentialError {}

/// One secret per provider in some OS-level store.
pub trait CredentialStore: Send + Sync {
    /// The stored secret, or `Ok(None)` when there is none.
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError>;

    /// Stores `secret`, replacing any earlier one. On error the earlier
    /// item, if any, is untouched.
    fn set(&self, provider: Provider, secret: &Secret) -> Result<(), CredentialError>;

    /// Removes the item; removing one that is not there is `Ok(())`.
    fn delete(&self, provider: Provider) -> Result<(), CredentialError>;
}

/// The store of a build with no usable credential store: every call
/// fails with [`CredentialError::Unavailable`], nothing is written.
#[derive(Debug, Default, Clone, Copy)]
pub struct UnavailableStore;

impl CredentialStore for UnavailableStore {
    fn get(&self, _provider: Provider) -> Result<Option<Secret>, CredentialError> {
        Err(CredentialError::Unavailable)
    }

    fn set(&self, _provider: Provider, _secret: &Secret) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }

    fn delete(&self, _provider: Provider) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
}

/// Whether a provider has a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialState {
    /// A secret is stored.
    SignedIn,
    /// The store answered and has none.
    SignedOut,
    /// The store could not be read; the reason is user-facing text.
    AuthRequired(String),
}

/// The pure mapping from a `get` result to a state: `Ok(Some)` is
/// `SignedIn`, `Ok(None)` is `SignedOut`, and every error variant is
/// `AuthRequired` with a reason that names no secret.
pub fn state_from_get(result: &Result<Option<Secret>, CredentialError>) -> CredentialState {
    match result {
        Ok(Some(_)) => CredentialState::SignedIn,
        Ok(None) => CredentialState::SignedOut,
        Err(CredentialError::Unavailable) => {
            CredentialState::AuthRequired(NO_STORE_REASON.to_string())
        }
        Err(CredentialError::Locked) => {
            CredentialState::AuthRequired("keychain locked".to_string())
        }
        Err(CredentialError::Denied) => {
            CredentialState::AuthRequired("keychain access denied".to_string())
        }
        Err(CredentialError::Backend(reason)) => {
            CredentialState::AuthRequired(format!("credential store error: {reason}"))
        }
    }
}

/// Reads `provider` from `store` and maps the answer with
/// [`state_from_get`]. Reads only; never writes or tries another store.
pub fn credential_state(store: &dyn CredentialStore, provider: Provider) -> CredentialState {
    state_from_get(&store.get(provider))
}

/// Which store [`open_store`] chose, for the startup log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreKind {
    Keychain,
    TestFile,
    Unavailable,
}

impl StoreKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StoreKind::Keychain => "macos-keychain",
            StoreKind::TestFile => "test-file",
            StoreKind::Unavailable => "unavailable",
        }
    }
}

/// Chooses the store for this build.
///
/// - `test_credential_file` set (the `test_credential_file` config key):
///   the file store, which must lie inside `data_dir`. Only a build with
///   the `test-credential-store` feature can do this; any other build
///   returns an error naming the key. (Config loading refuses the key
///   earlier on such a build; this is the same rule one layer down.)
/// - otherwise on macOS: the Keychain store.
/// - otherwise: [`UnavailableStore`].
///
/// Never falls from one store to another.
pub fn open_store(
    test_credential_file: Option<&Path>,
    data_dir: &Path,
) -> Result<(Box<dyn CredentialStore>, StoreKind), CredentialError> {
    #[cfg(feature = "test-credential-store")]
    if let Some(file) = test_credential_file {
        let store = FileStore::new(data_dir, file)?;
        return Ok((Box::new(store), StoreKind::TestFile));
    }
    #[cfg(not(feature = "test-credential-store"))]
    {
        let _ = data_dir;
        if test_credential_file.is_some() {
            return Err(CredentialError::Backend(
                "test_credential_file needs a build with the test-credential-store feature"
                    .to_string(),
            ));
        }
    }
    #[cfg(target_os = "macos")]
    {
        Ok((Box::new(KeychainStore::new()), StoreKind::Keychain))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok((Box::new(UnavailableStore), StoreKind::Unavailable))
    }
}
