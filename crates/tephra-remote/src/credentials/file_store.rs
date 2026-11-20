//! The test-only file store. Compiled only with the cargo feature
//! `test-credential-store`; a default build has none of this code.
//!
//! It exists so tests and the Linux container can run the daemon's
//! credential paths without a Keychain. It keeps the secrets of all
//! providers in one JSON file, written mode 0600 (a read refuses a file
//! with any group or other bit set), and it refuses to follow a symlink
//! or to use a path outside the directory it was given.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use super::{CredentialError, CredentialStore, Provider, Secret};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn backend(reason: &str) -> CredentialError {
    CredentialError::Backend(reason.to_string())
}

/// A file-backed [`CredentialStore`] for tests.
#[derive(Debug)]
pub struct FileStore {
    /// The file, as a path under the canonical form of the directory.
    path: PathBuf,
    /// Serialises read-modify-write cycles inside this process.
    lock: Mutex<()>,
}

impl FileStore {
    /// Opens the store file `file` under `dir`.
    ///
    /// `dir` must exist. A relative `file` is taken relative to `dir`.
    /// Refused with a [`CredentialError::Backend`] when `file` contains
    /// `..`, lies outside `dir`, or when `file` or any directory between
    /// `dir` and it is a symlink. `file` itself need not exist yet; its
    /// parent directory must.
    pub fn new(dir: &Path, file: &Path) -> Result<Self, CredentialError> {
        let canon_dir = dir
            .canonicalize()
            .map_err(|_| backend("the credential file directory does not exist"))?;
        let full = if file.is_absolute() {
            file.to_path_buf()
        } else {
            dir.join(file)
        };
        if full.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err(backend("the credential file path contains `..`"));
        }
        let rel = full
            .strip_prefix(dir)
            .or_else(|_| full.strip_prefix(&canon_dir))
            .map_err(|_| backend("the credential file is outside the allowed directory"))?
            .to_path_buf();
        if rel.as_os_str().is_empty() {
            return Err(backend("the credential file path is a directory"));
        }

        let mut walked = canon_dir.clone();
        for component in rel.components() {
            match component {
                Component::Normal(part) => walked.push(part),
                Component::CurDir => continue,
                _ => return Err(backend("the credential file path is not plain")),
            }
            match fs::symlink_metadata(&walked) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(backend("the credential file path goes through a symlink"));
                }
                Ok(_) => {}
                // Not there yet: nothing below it can be a symlink.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(backend("the credential file path cannot be inspected")),
            }
        }
        match fs::symlink_metadata(&walked) {
            Ok(meta) if meta.is_dir() => {
                return Err(backend("the credential file path is a directory"));
            }
            _ => {}
        }
        let parent_ok = walked.parent().map(|p| p.is_dir()).unwrap_or(false);
        if !parent_ok {
            return Err(backend("the credential file directory does not exist"));
        }
        Ok(FileStore {
            path: walked,
            lock: Mutex::new(()),
        })
    }

    fn read_all(&self) -> Result<BTreeMap<String, String>, CredentialError> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&self.path)
        {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
            Err(_) => return Err(backend("the credential file cannot be opened")),
        };
        let meta = file
            .metadata()
            .map_err(|_| backend("the credential file cannot be inspected"))?;
        if !meta.is_file() {
            return Err(backend("the credential file is not a regular file"));
        }
        if meta.mode() & 0o077 != 0 {
            return Err(backend("the credential file permissions are too open"));
        }
        let mut raw = String::new();
        file.read_to_string(&mut raw)
            .map_err(|_| backend("the credential file cannot be read"))?;
        if raw.trim().is_empty() {
            return Ok(BTreeMap::new());
        }
        serde_json::from_str(&raw).map_err(|_| backend("the credential file is not valid"))
    }

    fn write_all(&self, items: &BTreeMap<String, String>) -> Result<(), CredentialError> {
        let body =
            serde_json::to_string(items).map_err(|_| backend("cannot encode credentials"))?;
        let name = self
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = self.path.with_file_name(format!(
            ".{name}.{}.{}.tmp",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        // `create_new` fails on an existing path, symlink included, and
        // never follows one.
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|_| backend("the credential file cannot be written"))?;
        let written = out
            .write_all(body.as_bytes())
            .and_then(|_| out.sync_all())
            .and_then(|_| fs::rename(&tmp, &self.path));
        if written.is_err() {
            let _ = fs::remove_file(&tmp);
            return Err(backend("the credential file cannot be written"));
        }
        Ok(())
    }
}

impl CredentialStore for FileStore {
    fn get(&self, provider: Provider) -> Result<Option<Secret>, CredentialError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let items = self.read_all()?;
        Ok(items.get(provider.as_str()).map(|v| Secret::new(v.clone())))
    }

    fn set(&self, provider: Provider, secret: &Secret) -> Result<(), CredentialError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut items = self.read_all()?;
        items.insert(provider.as_str().to_string(), secret.expose().to_string());
        self.write_all(&items)
    }

    fn delete(&self, provider: Provider) -> Result<(), CredentialError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut items = self.read_all()?;
        if items.remove(provider.as_str()).is_none() {
            return Ok(());
        }
        self.write_all(&items)
    }
}
