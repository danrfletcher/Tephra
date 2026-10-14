//! Tiny CLI harness for container verification (PR-2 checkpoints 1-3):
//! opens (or creates) a `cache.db` at the given path, optionally seeds
//! a demo source with cached units, and prints a short report. Not
//! part of the production `tephrad` binary — this crate is a pure
//! library consumed by later PRs.
//!
//! Usage:
//!   cargo run -p tephra-cache --example init_cache -- <db-path> [seed-offline]
//!
//! With `seed-offline`, registers a source named "demo" with two
//! cached units/links/tags/blocks, then immediately marks it
//! `offline` — standing in for checkpoint 3's "seed one offline source
//! row with cached units" step.

use std::path::PathBuf;

use tephra_cache::{Cache, NewBlock, NewLink, NewTag, NewUnit, SourceStatus};

fn main() {
    let mut args = std::env::args().skip(1);
    let db_path: PathBuf = match args.next() {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("usage: init_cache <db-path> [seed-offline]");
            std::process::exit(2);
        }
    };
    let seed_offline = args.next().as_deref() == Some("seed-offline");

    let mut cache = Cache::open(&db_path).expect("open cache");
    println!("opened cache at {}", cache.path().display());
    println!(
        "schema_version = {:?} (CURRENT_SCHEMA_VERSION = {})",
        cache.schema_version().unwrap(),
        tephra_cache::CURRENT_SCHEMA_VERSION
    );
    println!("journal_mode = {}", cache.journal_mode().unwrap());
    println!("synchronous = {}", cache.synchronous_mode().unwrap());
    println!("foreign_keys = {}", cache.foreign_keys_enabled().unwrap());

    if seed_offline {
        let source_id = match cache.get_source("demo") {
            Ok(Some(row)) => row.id,
            _ => cache
                .register_source("demo", "/tmp/demo-source", SourceStatus::Active)
                .expect("register_source"),
        };
        let unit_id = cache
            .insert_unit(&NewUnit {
                source_id,
                rel_path: "demo-note.md",
                mtime: Some(0),
                frontmatter_json: None,
            })
            .expect("insert_unit");
        cache
            .insert_link(&NewLink {
                from_unit_id: unit_id,
                to_raw: "[[Other]]",
                to_unit_id: None,
                kind: "wikilink",
            })
            .expect("insert_link");
        cache
            .insert_tag(&NewTag {
                unit_id,
                tag: "demo",
            })
            .expect("insert_tag");
        cache
            .insert_block(&NewBlock {
                unit_id,
                block_id: "demo-block",
                line: 1,
            })
            .expect("insert_block");
        cache
            .transition_source_status("demo", SourceStatus::Offline)
            .expect("active -> offline");
        println!("seeded source \"demo\" with 1 unit/link/tag/block, now offline");
    }

    // Exercise the F2 rebuild path explicitly, as checkpoint 2/3 ask
    // for, so re-running this binary against the same cache.db is
    // enough to demonstrate the mismatch-triggered rebuild without
    // needing a separate throwaway branch.
    if args.next().as_deref() == Some("rebuild") {
        cache
            .rebuild(&tephra_cache::FaultInjector::none())
            .expect("rebuild");
        println!(
            "rebuild complete; schema_version = {:?}",
            cache.schema_version().unwrap()
        );
    }

    println!("sources row count = {}", cache.source_row_count().unwrap());
}
