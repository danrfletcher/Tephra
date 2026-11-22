//! The rows eviction reads and the one step that writes them (atlas-27
//! PR-8): pins (file, folder, whole source), the upload queue, the cache
//! flag, and the conditional `cached -> remote`.

mod common;

use tephra_cache::Cache;

const CUTOFF: i64 = 1_000;

fn cache_with_items(dir: &tempfile::TempDir) -> Cache {
    let cache = common::open_temp_cache(dir);
    cache
        .connection()
        .execute_batch(
            "INSERT INTO remote_sources (name, kind, target, created_at, cache) \
                VALUES ('g', 'github', 'o/r', 1, 1), ('off', 'github', 'o/s', 1, 0);
             INSERT INTO remote_items (source, path, size, state, last_used) VALUES
                ('g', 'a.md', 10, 'cached', 500),
                ('g', 'docs/b.md', 20, 'cached', 500),
                ('g', 'docs/deep/c.md', 30, 'cached', 500),
                ('g', 'docsx/d.md', 40, 'cached', 500),
                ('g', 'e.md', 50, 'cached', 500),
                ('g', 'f.md', 60, 'pending_upload', 500),
                ('g', 'g.md', 70, 'remote', NULL),
                ('g', 'h.md', 80, 'cached', 2000),
                ('off', 'x.md', 90, 'cached', 500);
             INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES
                ('g', 'docs', 1, 1),
                ('g', 'a.md', 0, 1);
             INSERT INTO upload_queue (source, path, created_at) VALUES ('g', 'e.md', 1);",
        )
        .expect("seed");
    cache
}

fn item<'a>(
    items: &'a [tephra_cache::EvictItem],
    source: &str,
    path: &str,
) -> &'a tephra_cache::EvictItem {
    items
        .iter()
        .find(|i| i.source == source && i.path == path)
        .unwrap_or_else(|| panic!("{source}:{path} is listed"))
}

#[test]
fn the_listing_has_every_copy_with_its_pin_queue_and_flag() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_items(&dir);
    let items = cache.remote_evict_items().unwrap();
    // `remote` has no copy and is not listed.
    assert!(!items.iter().any(|i| i.path == "g.md"));
    assert_eq!(items.len(), 8);

    assert!(item(&items, "g", "a.md").pinned, "a file pin");
    assert!(item(&items, "g", "docs/b.md").pinned, "under a folder pin");
    assert!(item(&items, "g", "docs/deep/c.md").pinned, "deep under it");
    assert!(
        !item(&items, "g", "docsx/d.md").pinned,
        "a folder pin is on the folder, not on a name that starts with it"
    );
    assert!(item(&items, "g", "e.md").edit_queued);
    assert!(!item(&items, "g", "h.md").edit_queued);
    assert!(item(&items, "g", "f.md").is_protected());
    assert!(item(&items, "g", "h.md").source_cache);
    assert!(!item(&items, "off", "x.md").source_cache);
}

#[test]
fn a_folder_pin_on_the_empty_path_covers_the_whole_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_items(&dir);
    cache
        .connection()
        .execute(
            "INSERT INTO remote_pins (source, path, is_folder, created_at) VALUES ('off', '', 1, 1)",
            [],
        )
        .unwrap();
    let items = cache.remote_evict_items().unwrap();
    assert!(item(&items, "off", "x.md").pinned);
    assert!(!item(&items, "g", "docsx/d.md").pinned);
}

#[test]
fn the_total_counts_every_copy_of_every_source() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_items(&dir);
    // 10+20+30+40+50+60+80+90; the `remote` row has no copy.
    assert_eq!(cache.remote_total_cache_bytes().unwrap(), 380);
    assert_eq!(cache.remote_cache_bytes("g").unwrap(), 290);
    // Pinned: a.md (cached with a pin) and the three under `docs`.
    assert_eq!(cache.remote_pinned_bytes().unwrap(), 10 + 20 + 30);
}

#[test]
fn the_cache_off_condition_follows_the_flag_as_it_is_now() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_items(&dir);
    // 'g' has its cache on: a drop that needs it off is refused.
    assert!(!cache
        .drop_remote_copy("g", "docsx/d.md", CUTOFF, true)
        .unwrap());
    // Turning the cache back on stops the time-based drop of 'off'.
    assert_eq!(
        cache.set_remote_source_cache("off", true).unwrap(),
        Some(false)
    );
    assert!(!cache.drop_remote_copy("off", "x.md", CUTOFF, true).unwrap());
    cache.set_remote_source_cache("off", false).unwrap();
    assert!(cache.drop_remote_copy("off", "x.md", CUTOFF, true).unwrap());
}

#[test]
fn drop_remote_copy_changes_only_a_row_that_still_qualifies() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_items(&dir);
    let state = |p: &str| -> String {
        cache
            .connection()
            .query_row(
                "SELECT state FROM remote_items WHERE source = 'g' AND path = ?1",
                [p],
                |r| r.get(0),
            )
            .unwrap()
    };
    for p in ["a.md", "docs/b.md", "e.md", "f.md", "h.md", "nothing.md"] {
        assert!(
            !cache.drop_remote_copy("g", p, CUTOFF, false).unwrap(),
            "{p}"
        );
    }
    assert_eq!(state("h.md"), "cached", "used since the pass looked");
    assert_eq!(state("g.md"), "remote");
    assert_eq!(state("a.md"), "cached");
    assert_eq!(state("f.md"), "pending_upload");

    assert!(cache
        .drop_remote_copy("g", "docsx/d.md", CUTOFF, false)
        .unwrap());
    assert_eq!(state("docsx/d.md"), "remote");
    assert!(
        !cache
            .drop_remote_copy("g", "docsx/d.md", CUTOFF, false)
            .unwrap(),
        "a second drop finds nothing to do"
    );
    // The size stays (it is the remote's size); the bytes stop counting.
    assert_eq!(cache.remote_cache_bytes("g").unwrap(), 290 - 40);

    // A drop that could not delete its file is undone, once.
    assert!(cache.restore_remote_copy("g", "docsx/d.md").unwrap());
    assert_eq!(state("docsx/d.md"), "cached");
    assert!(!cache.restore_remote_copy("g", "docsx/d.md").unwrap());
    assert!(
        !cache.restore_remote_copy("g", "e.md").unwrap(),
        "only from remote"
    );
}

#[test]
fn a_finished_download_notes_its_use_in_the_same_statement() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_items(&dir);
    let c = cache.connection();
    c.execute(
        "UPDATE remote_items SET state = 'downloading', sha = 's', last_used = 1 \
         WHERE source = 'g' AND path = 'g.md'",
        [],
    )
    .unwrap();
    assert!(!cache
        .finish_remote_download("g", "g.md", Some("other"), Some(70), 99)
        .unwrap());
    assert!(cache
        .finish_remote_download("g", "g.md", Some("s"), Some(70), 99)
        .unwrap());
    let (state, used): (String, i64) = c
        .query_row(
            "SELECT state, last_used FROM remote_items WHERE source = 'g' AND path = 'g.md'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), used), ("cached", 99));
}

#[test]
fn a_fetch_that_touches_last_used_defeats_a_drop_that_was_picked() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_items(&dir);
    cache
        .touch_remote_last_used("g", "docsx/d.md", CUTOFF + 5)
        .unwrap();
    assert!(!cache
        .drop_remote_copy("g", "docsx/d.md", CUTOFF, false)
        .unwrap());
}

#[test]
fn the_cache_flag_is_set_read_back_and_unknown_sources_are_none() {
    let dir = tempfile::tempdir().unwrap();
    let cache = cache_with_items(&dir);
    assert_eq!(
        cache.set_remote_source_cache("g", false).unwrap(),
        Some(true)
    );
    assert!(!cache.remote_source("g").unwrap().unwrap().cache);
    assert_eq!(
        cache.set_remote_source_cache("g", false).unwrap(),
        Some(false)
    );
    assert_eq!(
        cache.set_remote_source_cache("g", true).unwrap(),
        Some(false)
    );
    assert!(cache.remote_source("g").unwrap().unwrap().cache);
    assert_eq!(cache.set_remote_source_cache("nope", true).unwrap(), None);
}
