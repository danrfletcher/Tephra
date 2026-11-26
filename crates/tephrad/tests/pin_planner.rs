//! PR-12 (G11, E20): the pin planner. What a pin covers, the total it
//! shows, what is skipped as not fetchable, the path rules, and the
//! confirmation threshold of a fifth of the cache limit.
#![allow(dead_code)]

use tephra_cache::RemoteItemRow;
use tephrad::cache_limit::parse_cache_limit;
use tephrad::remote::pin::{needs_confirm, normalize_pin_path, plan_pin, PinKind, PlanError};

fn item(path: &str, kind: &str, size: i64) -> RemoteItemRow {
    RemoteItemRow {
        path: path.to_string(),
        kind: Some(kind.to_string()),
        sha: None,
        size: Some(size),
        file_id: Some(
            path.chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect(),
        ),
        revision: None,
        state: "remote".to_string(),
        reason: None,
    }
}

fn lfs(path: &str, size: i64) -> RemoteItemRow {
    RemoteItemRow {
        reason: Some(tephra_cache::REASON_LFS_POINTER.to_string()),
        ..item(path, "binary", size)
    }
}

fn tree() -> Vec<RemoteItemRow> {
    vec![
        item("Trip Plans/a.md", "document", 100),
        item("Trip Plans/b.pdf", "binary", 2_000),
        item("Trip Plans/Budget", "gsheet", 0),
        item("Trip Plans/Notes", "gdoc", 0),
        item("Trip Plans/Deck", "gslides", 0),
        item("Trip Plans/Survey", "gform", 0),
        item("Trip Plans/sub/c.md", "document", 30),
        item("Trip Plans/sub/big.bin", "binary", 4_000),
        item("Trip Plans-old/z.md", "document", 999_999),
        item("lib", "submodule", 0),
        item("assets/model.bin", "binary", 7),
        lfs("assets/huge.bin", 130),
        item("Trip Plans/Ideas (2).md", "document", 11),
        item("Trip Plans/Ideas.md", "document", 22),
    ]
}

#[test]
fn a_folder_counts_its_fetchable_files_and_their_listed_bytes() {
    let plan = plan_pin(&tree(), "Trip Plans", true).unwrap();
    assert_eq!(plan.kind, PinKind::Folder);
    assert_eq!(plan.path, "Trip Plans");
    // a.md, b.pdf, sub/c.md, sub/big.bin, Ideas (2).md, Ideas.md
    assert_eq!(plan.files, 6);
    assert_eq!(plan.total_bytes, 100 + 2_000 + 30 + 4_000 + 11 + 22);
}

#[test]
fn the_sibling_that_shares_the_prefix_is_not_in_the_folder() {
    let plan = plan_pin(&tree(), "Trip Plans", true).unwrap();
    assert!(plan.total_bytes < 999_999);
}

#[test]
fn native_google_files_are_skipped_and_left_out_of_the_total() {
    let plan = plan_pin(&tree(), "Trip Plans", true).unwrap();
    assert_eq!(plan.skipped_count(), 4);
    assert_eq!(plan.skipped.get("gsheet"), Some(&1));
    assert_eq!(plan.skipped.get("gdoc"), Some(&1));
    assert_eq!(plan.skipped.get("gslides"), Some(&1));
    assert_eq!(plan.skipped.get("gform"), Some(&1));
}

#[test]
fn lfs_content_and_submodules_are_skipped_too() {
    let mut rows = tree();
    rows.push(item("assets/lib", "submodule", 0));
    let plan = plan_pin(&rows, "assets", true).unwrap();
    assert_eq!(plan.files, 1);
    assert_eq!(plan.total_bytes, 7);
    assert_eq!(plan.skipped.get("lfs_pointer"), Some(&1));
    assert_eq!(plan.skipped.get("submodule"), Some(&1));
}

#[test]
fn a_file_pin_is_one_file() {
    let plan = plan_pin(&tree(), "Trip Plans/b.pdf", false).unwrap();
    assert_eq!(plan.kind, PinKind::File);
    assert_eq!((plan.files, plan.total_bytes), (1, 2_000));
    assert_eq!(plan.skipped_count(), 0);
}

#[test]
fn duplicate_drive_names_are_pinned_by_their_own_path() {
    let plan = plan_pin(&tree(), "Trip Plans/Ideas (2).md", false).unwrap();
    assert_eq!((plan.files, plan.total_bytes), (1, 11));
    assert_eq!(plan.path, "Trip Plans/Ideas (2).md");
}

#[test]
fn a_native_doc_or_sheet_is_not_pinnable_and_carries_its_url() {
    for (path, kind, url_part) in [
        ("Trip Plans/Notes", "gdoc", "docs.google.com/document"),
        (
            "Trip Plans/Budget",
            "gsheet",
            "docs.google.com/spreadsheets",
        ),
    ] {
        match plan_pin(&tree(), path, false) {
            Err(PlanError::NotFetchable(e)) => {
                assert_eq!(e.reason, "not_fetchable");
                assert_eq!(e.kind.as_deref(), Some(kind));
                let url = e.url.expect("a url");
                assert!(url.contains(url_part), "{url}");
            }
            other => panic!("{path}: {other:?}"),
        }
    }
}

#[test]
fn a_submodule_and_an_lfs_file_are_not_pinnable() {
    for path in ["lib", "assets/huge.bin"] {
        assert!(
            matches!(
                plan_pin(&tree(), path, false),
                Err(PlanError::NotFetchable(_))
            ),
            "{path}"
        );
    }
}

#[test]
fn a_path_that_is_not_there_is_not_found() {
    for (path, folder_only) in [
        ("Nope", false),
        ("Trip Plans/zzz", false),
        ("Trip", true),
        ("Trip Plans/a.md", true),
    ] {
        assert_eq!(
            plan_pin(&tree(), path, folder_only),
            Err(PlanError::NotFound),
            "{path}"
        );
    }
}

#[test]
fn a_file_deleted_on_the_remote_and_kept_for_its_edits_is_not_planned() {
    let mut rows = tree();
    rows.iter_mut()
        .find(|r| r.path == "Trip Plans/a.md")
        .unwrap()
        .reason = Some(tephra_cache::REASON_DELETED_ON_REMOTE.to_string());
    let plan = plan_pin(&rows, "Trip Plans", true).unwrap();
    assert_eq!(plan.files, 5);
    assert_eq!(
        plan_pin(&rows, "Trip Plans/a.md", false),
        Err(PlanError::NotFound)
    );
}

#[test]
fn a_file_with_no_listed_size_counts_as_zero_bytes() {
    let mut rows = tree();
    rows.iter_mut()
        .find(|r| r.path == "Trip Plans/a.md")
        .unwrap()
        .size = None;
    let plan = plan_pin(&rows, "Trip Plans/a.md", false).unwrap();
    assert_eq!((plan.files, plan.total_bytes), (1, 0));
}

#[test]
fn an_empty_folder_of_only_skipped_files_is_a_pin_of_zero_files() {
    let rows = vec![item("D/x", "gdoc", 0), item("D/y", "gsheet", 0)];
    let plan = plan_pin(&rows, "D", true).unwrap();
    assert_eq!((plan.files, plan.total_bytes), (0, 0));
    assert_eq!(plan.skipped_count(), 2);
}

// ---- the path rules --------------------------------------------------------------

#[test]
fn paths_are_normalized_and_a_trailing_slash_asks_for_a_folder() {
    assert_eq!(
        normalize_pin_path("Trip Plans/"),
        Ok(("Trip Plans".to_string(), true))
    );
    assert_eq!(
        normalize_pin_path("docs/setup.md"),
        Ok(("docs/setup.md".to_string(), false))
    );
    assert_eq!(
        normalize_pin_path("Café/日本/メモ (2).md"),
        Ok(("Café/日本/メモ (2).md".to_string(), false))
    );
}

#[test]
fn paths_that_climb_out_or_are_malformed_are_refused() {
    for bad in [
        "",
        "/",
        "../x",
        "docs/../x",
        "..",
        "./x",
        "docs/./x",
        "/etc/passwd",
        "a//b",
        "docs/..",
        "x\0y",
        "x\ny",
        "..//",
    ] {
        assert!(normalize_pin_path(bad).is_err(), "{bad:?} must be refused");
    }
}

// ---- the confirmation threshold --------------------------------------------------

#[test]
fn the_threshold_is_strictly_more_than_a_fifth_of_the_cap() {
    let caps = [
        "1 MB", "5 MB", "50 MB", "100 MB", "1 GB", "2 GB", "10 GB", "1 MiB", "1 GiB", "10 GiB",
        "1MB", "1GB",
    ];
    for cap_text in caps {
        let cap = parse_cache_limit(cap_text).unwrap();
        // The cap is a multiple of 5 for the decimal units; for the
        // binary ones the fifth is not whole, so test around the exact
        // boundary the rule states: total * 5 > cap.
        let fifth_floor = cap / 5;
        for total in [
            fifth_floor.saturating_sub(1),
            fifth_floor,
            fifth_floor + 1,
            fifth_floor + 2,
        ] {
            assert_eq!(
                needs_confirm(total, cap),
                u128::from(total) * 5 > u128::from(cap),
                "{cap_text}: {total} of {cap}"
            );
        }
        if cap.is_multiple_of(5) {
            assert!(!needs_confirm(cap / 5 - 1, cap), "{cap_text}: a fifth - 1");
            assert!(!needs_confirm(cap / 5, cap), "{cap_text}: exactly a fifth");
            assert!(needs_confirm(cap / 5 + 1, cap), "{cap_text}: a fifth + 1");
        }
    }
}

#[test]
fn the_threshold_for_the_named_caps_matches_the_spec_table() {
    // (cap, fifth)
    for (cap_text, fifth) in [
        ("1 MB", 200_000u64),
        ("5 MB", 1_000_000),
        ("100 MB", 20_000_000),
        ("1 GB", 200_000_000),
        ("10 GB", 2_000_000_000),
    ] {
        let cap = parse_cache_limit(cap_text).unwrap();
        assert!(!needs_confirm(fifth - 1, cap), "{cap_text}");
        assert!(!needs_confirm(fifth, cap), "{cap_text}");
        assert!(needs_confirm(fifth + 1, cap), "{cap_text}");
    }
}

#[test]
fn mb_gb_and_mib_inputs_mean_what_the_limit_parser_says() {
    // 1000-based and 1024-based inputs give different fifths.
    let mb = parse_cache_limit("200 MB").unwrap();
    let mib = parse_cache_limit("200 MiB").unwrap();
    assert_eq!(mb, 200_000_000);
    assert_eq!(mib, 209_715_200);
    // 40_000_000 is exactly a fifth of 200 MB, and under a fifth of 200 MiB.
    assert!(!needs_confirm(40_000_000, mb));
    assert!(needs_confirm(40_000_001, mb));
    assert!(!needs_confirm(41_943_040, mib));
    assert!(needs_confirm(41_943_041, mib));
    // A GB cap.
    let gb = parse_cache_limit("2 GB").unwrap();
    assert!(!needs_confirm(400_000_000, gb));
    assert!(needs_confirm(400_000_001, gb));
}

#[test]
fn a_zero_total_never_needs_confirmation_and_huge_totals_do_not_overflow() {
    assert!(!needs_confirm(0, 0));
    assert!(!needs_confirm(0, 1));
    assert!(needs_confirm(1, 0));
    assert!(needs_confirm(u64::MAX, u64::MAX));
    assert!(!needs_confirm(u64::MAX / 5, u64::MAX));
}
