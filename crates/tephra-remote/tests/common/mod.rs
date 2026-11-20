//! Shared setup: an in-memory credential store, a fake clock,
//! and a fake server wired to a real adapter. No Keychain, no real
//! network: every request goes to a fake on 127.0.0.1.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tephra_remote::clock::FakeClock;
use tephra_remote::credentials::{CredentialError, CredentialStore, Provider, Secret};
use tephra_remote::drive::DriveClient;
use tephra_remote::github::{GithubClient, RepoRef};
use tephra_remote::testing::{FakeDrive, FakeGithub};

pub const GH_SENTINEL: &str = "ghp_SENTINEL";
pub const GOOGLE_SENTINEL: &str = "ya29.SENTINEL";

/// An in-memory credential store. The file store from PR-1 is behind the
/// `test-credential-store` feature, and turning that feature on here would
/// turn it on for `tephrad` in a workspace-wide `cargo test` too (features
/// unify), which `tephrad`'s "default build has no file store" checks
/// forbid. The adapters see only the `CredentialStore` trait, so they go
/// through the same calls either way; no Keychain is touched.
#[derive(Default)]
pub struct MemoryStore {
    items: Mutex<HashMap<&'static str, String>>,
}

impl CredentialStore for MemoryStore {
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError> {
        let items = self.items.lock().unwrap();
        Ok(items.get(provider.as_str()).map(Secret::new))
    }

    fn set(&self, provider: Provider, secret: &Secret) -> Result<(), CredentialError> {
        let mut items = self.items.lock().unwrap();
        items.insert(provider.as_str(), secret.expose().to_string());
        Ok(())
    }

    fn delete(&self, provider: Provider) -> Result<(), CredentialError> {
        self.items.lock().unwrap().remove(provider.as_str());
        Ok(())
    }
}

/// The store a test hands to an adapter, and the way it changes the token.
pub struct Creds {
    pub store: Arc<MemoryStore>,
}

impl Creds {
    pub fn new() -> Creds {
        Creds {
            store: Arc::new(MemoryStore::default()),
        }
    }

    pub fn with_token(provider: Provider, token: &str) -> Creds {
        let c = Creds::new();
        c.set(provider, token);
        c
    }

    pub fn set(&self, provider: Provider, token: &str) {
        self.store
            .set(provider, &Secret::new(token))
            .expect("store the token");
    }

    pub fn remove(&self, provider: Provider) {
        self.store.delete(provider).expect("delete the token");
    }
}

pub struct GhEnv {
    pub fake: FakeGithub,
    pub client: GithubClient,
    pub creds: Creds,
    pub clock: Arc<FakeClock>,
    pub repo: RepoRef,
}

/// A fake with repo `octo/hello` (default branch `main`) and a client
/// holding `token`, which the fake accepts.
pub async fn github_env(token: &str) -> GhEnv {
    let clock = Arc::new(FakeClock::at_epoch_2026());
    let fake = FakeGithub::start_with_clock(clock.clone()).await;
    fake.add_repo("octo/hello", "main");
    fake.put_file("octo/hello", "main", "README.md", b"# hello\n");
    fake.put_file("octo/hello", "main", "docs/setup.md", b"setup\n");
    let creds = Creds::with_token(Provider::Github, token);
    let client = GithubClient::new(Some(fake.base_url()), creds.store.clone(), clock.clone())
        .expect("loopback base URL");
    GhEnv {
        fake,
        client,
        creds,
        clock,
        repo: RepoRef::parse("octo/hello").unwrap(),
    }
}

pub struct DrvEnv {
    pub fake: FakeDrive,
    pub client: DriveClient,
    pub creds: Creds,
    pub clock: Arc<FakeClock>,
}

/// A fake with a folder `fld` holding `notes.md` (`f1`) and a Doc (`d1`),
/// and a client holding `token`.
pub async fn drive_env(token: &str) -> DrvEnv {
    let clock = Arc::new(FakeClock::at_epoch_2026());
    let fake = FakeDrive::start_with_clock(clock.clone()).await;
    fake.add_folder("fld", "Notes", None);
    fake.add_file("f1", "notes.md", "text/markdown", Some("fld"), b"# notes\n");
    fake.add_doc("d1", "Plan", Some("fld"), "# Plan\n");
    let creds = Creds::with_token(Provider::Google, token);
    let client = DriveClient::new(Some(fake.base_url()), creds.store.clone(), clock.clone())
        .expect("loopback base URL");
    DrvEnv {
        fake,
        client,
        creds,
        clock,
    }
}
