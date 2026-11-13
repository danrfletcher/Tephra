//! A process-wide lock per file path, held for the whole read, check and
//! rewrite of one source file.
//!
//! Every write Tephra makes to a user's file (`link.apply_refactor`'s
//! link rewrites and `block.ensure_id`'s block marker) is a read of the
//! file, a computed new content, then an atomic replace. Two of those on
//! the same file at once would each start from the same old bytes and the
//! later rename would silently drop the earlier one's change. Both writers
//! take this lock around their read-to-rename window, so a second writer
//! starts only after the first has finished, and then re-reads the file
//! (and re-checks its mtime) as it now is.
//!
//! Keyed by path, like [`crate::writer_gate::for_path`]: the key is the
//! canonical path when the file exists (so two spellings of one file share
//! a lock), else the path as given. Two unrelated files never contend. An
//! entry is dropped from the map when its last holder lets go, so the map
//! stays as small as the set of files being written right now.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

struct Slot {
    held: Mutex<bool>,
    freed: Condvar,
}

fn slots() -> &'static Mutex<HashMap<PathBuf, Arc<Slot>>> {
    static SLOTS: OnceLock<Mutex<HashMap<PathBuf, Arc<Slot>>>> = OnceLock::new();
    SLOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Held while one file is being rewritten; releases on drop.
pub struct FileLockGuard {
    key: PathBuf,
    slot: Arc<Slot>,
}

/// Blocks until no other caller holds the lock for `path`, then returns a
/// guard holding it.
pub fn lock(path: &Path) -> FileLockGuard {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let slot = {
        let mut map = slots().lock().unwrap_or_else(|p| p.into_inner());
        Arc::clone(map.entry(key.clone()).or_insert_with(|| {
            Arc::new(Slot {
                held: Mutex::new(false),
                freed: Condvar::new(),
            })
        }))
    };
    let mut held = slot.held.lock().unwrap_or_else(|p| p.into_inner());
    while *held {
        held = slot.freed.wait(held).unwrap_or_else(|p| p.into_inner());
    }
    *held = true;
    drop(held);
    FileLockGuard { key, slot }
}

impl Drop for FileLockGuard {
    fn drop(&mut self) {
        {
            let mut held = self.slot.held.lock().unwrap_or_else(|p| p.into_inner());
            *held = false;
        }
        self.slot.freed.notify_one();
        // Forget the slot when nobody else holds or waits on it: the map
        // has one `Arc`, this guard one, and a waiter or a just-arrived
        // caller another each.
        let mut map = slots().lock().unwrap_or_else(|p| p.into_inner());
        if Arc::strong_count(&self.slot) == 2 {
            map.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn a_second_holder_waits_until_the_first_lets_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        std::fs::write(&path, "x").unwrap();

        let inside = Arc::new(AtomicUsize::new(0));
        let max_inside = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..6)
            .map(|_| {
                let (path, inside, max_inside) =
                    (path.clone(), Arc::clone(&inside), Arc::clone(&max_inside));
                std::thread::spawn(move || {
                    let _guard = lock(&path);
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    max_inside.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(15));
                    inside.fetch_sub(1, Ordering::SeqCst);
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(max_inside.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn different_files_do_not_wait_on_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.md"), dir.path().join("b.md"));
        std::fs::write(&a, "x").unwrap();
        std::fs::write(&b, "x").unwrap();
        let _held_a = lock(&a);
        // Would hang forever if `b` shared `a`'s lock.
        let _held_b = lock(&b);
    }

    #[test]
    fn the_map_forgets_a_file_once_its_lock_is_free() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("only-once.md");
        std::fs::write(&path, "x").unwrap();
        let key = path.canonicalize().unwrap();
        drop(lock(&path));
        assert!(!slots().lock().unwrap().contains_key(&key));
    }
}
