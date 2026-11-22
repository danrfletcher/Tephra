//! G12: `source.unregister` of a promoted source IS an unpromote (same
//! rewrites, same log entry, same push); of a normal source it only
//! deletes rows; and it stays idempotent.

mod fix_world;
mod fixtures;

use std::time::Duration;

use fix_world::{fixes_of, real_writer, Daemon};
use tephra_cache::Cache;

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("proj/A.md", "# A\n\n[[Outside]] [[proj/B]]\n"),
        ("proj/B.md", "# B\n\n[[A]]\n"),
        ("Outside.md", "# O\n\n[[proj/A]] [[proj/B]]\n"),
        ("Plain.md", "# P\n"),
    ]
}

fn vault() -> Vec<(&'static str, &'static str)> {
    vec![("Vault.md", "[[@notes:proj/A#^x]] ![[@notes:proj/B]]\n")]
}

fn promoted() -> Daemon {
    let d = Daemon::start(&files(), &vault(), real_writer(), false);
    d.registry
        .promote("proj", d.notes_root().join("proj"), &d.engine)
        .expect("promote");
    d
}

fn state(d: &Daemon) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for root in [d.notes.root(), d.other.root()] {
        // `Warmup.md` is the daemon fixture's watcher check: how many
        // rounds it took (so its text) depends on the load, and the test
        // compares two daemons.
        let mut snap: Vec<_> = fixtures::tree_snapshot(root)
            .into_iter()
            .filter(|(p, _)| p != "Warmup.md")
            .map(|(p, (bytes, _))| (p, bytes))
            .collect();
        out.append(&mut snap);
    }
    out
}

#[test]
fn unregister_of_a_promoted_source_equals_unpromote_byte_for_byte() {
    let by_unpromote = promoted();
    by_unpromote
        .registry
        .unpromote("proj", &by_unpromote.engine)
        .expect("unpromote");
    let want = state(&by_unpromote);
    let want_entry = by_unpromote.entries();
    by_unpromote.close();

    let by_unregister = promoted();
    let mut client = by_unregister.connect();
    let _ = client.drain(Duration::from_millis(300));
    assert!(by_unregister
        .registry
        .unregister_promoted("proj", &by_unregister.engine)
        .expect("unregister"));
    assert_eq!(state(&by_unregister), want, "same files, same bytes");

    let got = by_unregister.entries();
    assert_eq!(got.len(), want_entry.len());
    assert_eq!(got[0].kind, "unpromote");
    assert_eq!(got[0].old, want_entry[0].old);
    assert_eq!(got[0].new, want_entry[0].new);
    assert_eq!(got[0].files_changed, want_entry[0].files_changed);
    assert_eq!(got[0].status, want_entry[0].status);

    let (push, _) = client.next_fix_push(Duration::from_secs(8));
    let push = push.expect("the push");
    assert_eq!(
        push["params"]["sources_changed"],
        serde_json::json!([{"kind": "unpromoted", "name": "proj"}])
    );
    assert_eq!(fixes_of(&push).len(), 1);
    assert!(client
        .drain(Duration::from_millis(600))
        .iter()
        .all(|p| p["params"].get("sources_changed").is_none()));
    by_unregister.close();
}

#[test]
fn unregister_of_a_normal_source_is_unchanged_rows_deleted_and_no_rewrites() {
    let d = promoted();
    let before = state(&d);
    let entries_before = d.entries().len();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    // `other` was never promoted: not an unpromote, and nothing rewrites.
    assert!(!d
        .registry
        .unregister_promoted("other", &d.engine)
        .expect("not promoted is not an error"));
    d.registry.unregister("other").expect("plain unregister");

    assert_eq!(state(&d), before, "no file was touched");
    assert_eq!(d.entries().len(), entries_before, "no log entry");
    let cache = Cache::open(&d.cache_path).unwrap();
    assert!(cache.get_source("other").unwrap().is_none());
    let (push, ordinary) = client.next_fix_push(Duration::from_millis(1500));
    assert!(push.is_none(), "no fix in the push: {push:?}");
    assert!(
        ordinary.iter().any(|p| p["params"]["sources_changed"]
            == serde_json::json!([{"kind": "removed", "name": "other"}])),
        "{ordinary:?}"
    );
    d.close();
}

#[test]
fn a_second_unregister_of_the_same_promoted_name_is_ok_and_silent() {
    let d = promoted();
    assert!(d
        .registry
        .unregister_promoted("proj", &d.engine)
        .expect("first"));
    let after_first = state(&d);
    let entries = d.entries().len();
    let mut client = d.connect();
    let _ = client.drain(Duration::from_millis(300));

    // The way the RPC does it: the promoted check says no, then the plain
    // unregister of a name that is not there succeeds and sends nothing.
    assert!(!d
        .registry
        .unregister_promoted("proj", &d.engine)
        .expect("second"));
    d.registry.unregister("proj").expect("plain, idempotent");

    assert_eq!(state(&d), after_first);
    assert_eq!(d.entries().len(), entries, "no second log entry");
    assert!(
        client.next_push(Duration::from_millis(800)).is_none(),
        "no second push"
    );
    d.close();
}
