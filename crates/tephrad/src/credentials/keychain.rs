//! The macOS Keychain store: one generic-password item per provider,
//! service [`KEYCHAIN_SERVICE`], account the provider name.
//!
//! By default it uses the login keychain through the system search list.
//! [`KeychainStore::in_keychain`] pins it to one keychain, which the
//! live test uses so it never touches the real login keychain.
//!
//! Every Keychain status maps to a [`CredentialError`] by
//! [`map_status`]. The `Backend` reason carries the numeric status
//! only: no text from the call, so nothing of the secret can end up in
//! it.

use security_framework::base::Error as SecError;
use security_framework::os::macos::keychain::SecKeychain;
use security_framework::passwords::{
    delete_generic_password, generic_password, set_generic_password, PasswordOptions,
};

use super::{CredentialError, CredentialStore, Provider, Secret};

/// The Keychain service name of the daemon's items.
pub const KEYCHAIN_SERVICE: &str = "com.danrfletcher.tephra";

const ERR_USER_CANCELED: i32 = -128;
const ERR_AUTH_FAILED: i32 = -25293;
const ERR_NO_SUCH_KEYCHAIN: i32 = -25294;
const ERR_NO_DEFAULT_KEYCHAIN: i32 = -25307;
const ERR_INTERACTION_NOT_ALLOWED: i32 = -25308;
const ERR_ITEM_NOT_FOUND: i32 = -25300;
// Authorization Services statuses a keychain call surfaces when it needs
// to unlock the keychain or ask the user and may not show a dialog.
const ERR_AUTH_DENIED: i32 = -60005;
const ERR_AUTH_CANCELED: i32 = -60006;
const ERR_AUTH_INTERACTION_NOT_ALLOWED: i32 = -60007;
/// What a locked keychain returned with user interaction switched off,
/// seen live by `tests/keychain_live.rs` on macOS 26.
const ERR_AUTH_INTERNAL: i32 = -60008;

/// What a Keychain status means to the caller.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Mapped {
    /// "No such item".
    NotFound,
    Error(CredentialError),
}

/// Maps a nonzero Keychain `OSStatus`.
pub(crate) fn map_status(code: i32) -> Mapped {
    match code {
        ERR_ITEM_NOT_FOUND => Mapped::NotFound,
        ERR_INTERACTION_NOT_ALLOWED | ERR_AUTH_INTERACTION_NOT_ALLOWED | ERR_AUTH_INTERNAL => {
            Mapped::Error(CredentialError::Locked)
        }
        ERR_AUTH_FAILED | ERR_USER_CANCELED | ERR_AUTH_DENIED | ERR_AUTH_CANCELED => {
            Mapped::Error(CredentialError::Denied)
        }
        ERR_NO_DEFAULT_KEYCHAIN | ERR_NO_SUCH_KEYCHAIN => {
            Mapped::Error(CredentialError::Unavailable)
        }
        other => Mapped::Error(CredentialError::Backend(format!("keychain status {other}"))),
    }
}

fn as_error(e: SecError) -> Mapped {
    map_status(e.code())
}

fn bytes_to_secret(bytes: &[u8]) -> Result<Secret, CredentialError> {
    match std::str::from_utf8(bytes) {
        Ok(text) => Ok(Secret::new(text)),
        Err(_) => Err(CredentialError::Backend(
            "the stored item is not valid text".to_string(),
        )),
    }
}

/// The Keychain-backed [`CredentialStore`].
pub struct KeychainStore {
    service: String,
    /// `None`: the default keychain search list. `Some`: that keychain only.
    keychain: Option<SecKeychain>,
}

impl KeychainStore {
    /// The store for the daemon: service [`KEYCHAIN_SERVICE`], login keychain.
    pub fn new() -> Self {
        KeychainStore {
            service: KEYCHAIN_SERVICE.to_string(),
            keychain: None,
        }
    }

    /// A store under `service` in the default keychain; for probes and
    /// examples that must not share the daemon's items.
    pub fn with_service(service: &str) -> Self {
        KeychainStore {
            service: service.to_string(),
            keychain: None,
        }
    }

    /// A store under `service` that reads and writes only `keychain`.
    pub fn in_keychain(service: &str, keychain: SecKeychain) -> Self {
        KeychainStore {
            service: service.to_string(),
            keychain: Some(keychain),
        }
    }
}

impl Default for KeychainStore {
    fn default() -> Self {
        Self::new()
    }
}

impl CredentialStore for KeychainStore {
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError> {
        let account = provider.as_str();
        let found = match &self.keychain {
            Some(kc) => kc
                .find_generic_password(&self.service, account)
                .map(|(pw, _item)| pw.as_ref().to_vec()),
            None => generic_password(PasswordOptions::new_generic_password(
                &self.service,
                account,
            )),
        };
        match found {
            Ok(bytes) => bytes_to_secret(&bytes).map(Some),
            Err(e) => match as_error(e) {
                Mapped::NotFound => Ok(None),
                Mapped::Error(err) => {
                    tracing::debug!(provider = account, error = %err, "keychain get failed");
                    Err(err)
                }
            },
        }
    }

    fn set(&self, provider: Provider, secret: &Secret) -> Result<(), CredentialError> {
        let account = provider.as_str();
        let done = match &self.keychain {
            Some(kc) => kc.set_generic_password(&self.service, account, secret.expose().as_bytes()),
            None => set_generic_password(&self.service, account, secret.expose().as_bytes()),
        };
        done.map_err(|e| match as_error(e) {
            // A store that reports "no item" on a write has failed.
            Mapped::NotFound => CredentialError::Backend("keychain status -25300".to_string()),
            Mapped::Error(err) => {
                tracing::debug!(provider = account, error = %err, "keychain set failed");
                err
            }
        })
    }

    fn delete(&self, provider: Provider) -> Result<(), CredentialError> {
        let account = provider.as_str();
        let done = match &self.keychain {
            Some(kc) => match kc.find_generic_password(&self.service, account) {
                Ok((_pw, item)) => {
                    item.delete();
                    Ok(())
                }
                Err(e) => Err(e),
            },
            None => delete_generic_password(&self.service, account),
        };
        match done {
            Ok(()) => Ok(()),
            Err(e) => match as_error(e) {
                Mapped::NotFound => Ok(()),
                Mapped::Error(err) => {
                    tracing::debug!(provider = account, error = %err, "keychain delete failed");
                    Err(err)
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_the_error_variants() {
        assert_eq!(map_status(-25300), Mapped::NotFound);
        assert_eq!(map_status(-25308), Mapped::Error(CredentialError::Locked));
        assert_eq!(map_status(-25293), Mapped::Error(CredentialError::Denied));
        assert_eq!(map_status(-128), Mapped::Error(CredentialError::Denied));
        assert_eq!(map_status(-60008), Mapped::Error(CredentialError::Locked));
        assert_eq!(map_status(-60007), Mapped::Error(CredentialError::Locked));
        assert_eq!(map_status(-60005), Mapped::Error(CredentialError::Denied));
        assert_eq!(map_status(-60006), Mapped::Error(CredentialError::Denied));
        assert_eq!(
            map_status(-25307),
            Mapped::Error(CredentialError::Unavailable)
        );
        assert_eq!(
            map_status(-25299),
            Mapped::Error(CredentialError::Backend("keychain status -25299".into()))
        );
    }
}
