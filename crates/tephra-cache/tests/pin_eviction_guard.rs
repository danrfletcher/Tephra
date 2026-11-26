//! A pinned file is never an eviction candidate (atlas-27 PR-12, guarding
//! PR-8): over every item state in the precedence table, with the source's
//! cache on or off, within the cap or over it. Real SQLite in a tempdir,
//! the planner fed from the rows the daemon feeds it.

mod common;

use tephra_cache::{plan_eviction, Cache, REMOTE_ITEM_STATES};

const NOW: i64 = 10_000;
const IDLE: i64 = 600;
/// Long past the idle window, so only a guard can keep a file.
const OLD: i64 = 1;

fn seeded(dir: &tempfile::TempDir, cache_on: bool, state: &str, pin: Pin) -> Cache {
    let cache = common::open_temp_cache(dir);
    cache
        .connection()
        .execute(
            "INSERT INTO remote_sources (name, kind, target, created_at, cache) \
             VALUES ('d', 'drive', 'Notes', 1, ?1)",
            [cache_on],
        )
        .unwrap();
    // A pinned file, an unpinned file next to it, and one in a sibling
    // folder that shares the pinned folder's name as a prefix.
    for (path, size) in [
        ("Trip/a.md", 100),
        ("Trip/b.md", 100),
        ("Trip-old/c.md", 100),
    ] {
        cache
            .connection()
            .execute(
                "INSERT INTO remote_items (source, path, size, state, last_used) \
                 VALUES ('d', ?1, ?2, ?3, ?4)",
                rusqlite::params![path, size, state, OLD],
            )
            .unwrap();
    }
    match pin {
        Pin::File => {
            cache.add_remote_pin("d", "Trip/a.md", false, 1).unwrap();
        }
        Pin::Folder => {
            cache.add_remote_pin("d", "Trip", true, 1).unwrap();
        }
        Pin::WholeSource => {
            cache.add_remote_pin("d", "", true, 1).unwrap();
        }
    }
    cache
}

#[derive(Clone, Copy, Debug)]
enum Pin {
    File,
    Folder,
    WholeSource,
}

#[test]
fn no_pinned_file_is_ever_a_candidate_in_any_state() {
    for state in REMOTE_ITEM_STATES {
        for cache_on in [true, false] {
            for pin in [Pin::File, Pin::Folder, Pin::WholeSource] {
                // Within the cap, at the cap and far over it.
                for limit in [10_000u64, 300, 0] {
                    let dir = tempfile::tempdir().unwrap();
                    let cache = seeded(&dir, cache_on, state, pin);
                    let items = cache.remote_evict_items().unwrap();
                    let plan = plan_eviction(&items, limit, NOW, IDLE);
                    let covered = |path: &str| match pin {
                        Pin::File => path == "Trip/a.md",
                        Pin::Folder => path.starts_with("Trip/"),
                        Pin::WholeSource => true,
                    };
                    for pick in &plan.picks {
                        assert!(
                            !covered(&pick.path),
                            "state {state}, cache {cache_on}, {pin:?}, limit {limit}: \
                             {} is pinned and was picked",
                            pick.path
                        );
                    }
                    // The rows say so too: every copy that a pin covers is
                    // listed as pinned.
                    for item in &items {
                        assert_eq!(item.pinned, covered(&item.path), "{}", item.path);
                    }
                }
            }
        }
    }
}

#[test]
fn an_unpinned_cached_file_next_to_a_pinned_one_is_still_dropped() {
    // The guard is the pin, not the folder: with the cache off, the same
    // old `cached` copy that no pin covers goes.
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir, false, "cached", Pin::File);
    let items = cache.remote_evict_items().unwrap();
    let plan = plan_eviction(&items, 10_000, NOW, IDLE);
    let picked: Vec<&str> = plan.picks.iter().map(|p| p.path.as_str()).collect();
    assert_eq!(picked, ["Trip-old/c.md", "Trip/b.md"]);
}

#[test]
fn a_file_unpinned_is_a_candidate_again() {
    let dir = tempfile::tempdir().unwrap();
    let cache = seeded(&dir, true, "cached", Pin::File);
    let items = cache.remote_evict_items().unwrap();
    assert!(plan_eviction(&items, 0, NOW, IDLE)
        .picks
        .iter()
        .all(|p| p.path != "Trip/a.md"));

    assert!(cache.remove_remote_pin("d", "Trip/a.md", false).unwrap());
    let items = cache.remote_evict_items().unwrap();
    assert!(plan_eviction(&items, 0, NOW, IDLE)
        .picks
        .iter()
        .any(|p| p.path == "Trip/a.md"));
}
