//! The spike write-ups under `docs/spikes/` (PR-2): five files exist, each has
//! the five headings and a `Verdict: <value>` line with an allowed value, the
//! README has a row for every spike with its date, tool versions and PRs,
//! S2 is `no-go` unless it holds `Dan approved: yes (<date>)`, no Measurements
//! section is empty, and anything not run says `not run: <reason>` and is
//! never `go`. Checks the real files; nothing is mocked.

use std::fs;
use std::path::{Path, PathBuf};

const FILES: [(&str, &str); 5] = [
    ("S1", "S1-github-git-first.md"),
    ("S2", "S2-rclone.md"),
    ("S3", "S3-partial-clone-sparse.md"),
    ("S4", "S4-docs-api-and-token.md"),
    ("S5", "S5-drive-changes-feed.md"),
];
const HEADINGS: [&str; 5] = [
    "Question",
    "Method",
    "Measurements",
    "Verdict",
    "What changes in the plan",
];
const VERDICTS: [&str; 3] = ["go", "no-go", "go-with-conditions"];

fn spikes_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/spikes")
}

fn read(name: &str) -> String {
    let path = spikes_dir().join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("{} must exist: {e}", path.display()))
}

/// The text under `## <heading>` up to the next `## ` heading, if the heading exists.
fn section<'a>(text: &'a str, heading: &str) -> Option<&'a str> {
    let marker = format!("## {heading}");
    let start = text.lines().position(|l| l.trim_end() == marker)?;
    let lines: Vec<&str> = text.lines().collect();
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.starts_with("## "))
        .map_or(lines.len(), |i| start + 1 + i);
    let body_start: usize = lines[..=start].iter().map(|l| l.len() + 1).sum();
    let body_end: usize = lines[..end].iter().map(|l| l.len() + 1).sum();
    Some(&text[body_start.min(text.len())..body_end.min(text.len())])
}

/// The value on the one line that starts `Verdict: ` (`None` when absent).
fn verdict_value(text: &str) -> Option<&str> {
    text.lines()
        .find_map(|l| l.strip_prefix("Verdict: "))
        .map(str::trim)
}

fn text_without_markers(s: &str) -> String {
    s.lines()
        .filter(|l| !l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn five_files_exist_each_with_the_five_headings() {
    for (id, file) in FILES {
        let text = read(file);
        let mut last = 0;
        for heading in HEADINGS {
            let at = text
                .lines()
                .position(|l| l.trim_end() == format!("## {heading}"))
                .unwrap_or_else(|| panic!("{id} ({file}) lacks the heading `## {heading}`"));
            assert!(at >= last, "{id}: heading `{heading}` is out of order");
            last = at;
        }
    }
}

#[test]
fn each_file_has_one_verdict_line_with_an_allowed_value() {
    for (id, file) in FILES {
        let text = read(file);
        let lines: Vec<&str> = text
            .lines()
            .filter(|l| l.starts_with("Verdict: "))
            .collect();
        assert_eq!(
            lines.len(),
            1,
            "{id}: expected exactly one `Verdict:` line, got {lines:?}"
        );
        let value = verdict_value(&text).unwrap();
        assert!(
            VERDICTS.contains(&value),
            "{id}: verdict {value:?} is not one of {VERDICTS:?}"
        );
        // The verdict sits under the Verdict heading.
        let verdict_section =
            section(&text, "Verdict").unwrap_or_else(|| panic!("{id}: no Verdict section"));
        assert!(
            verdict_section.contains(lines[0]),
            "{id}: the Verdict line is outside `## Verdict`"
        );
    }
}

#[test]
fn no_measurements_section_is_empty() {
    for (id, file) in FILES {
        let text = read(file);
        let body = section(&text, "Measurements")
            .unwrap_or_else(|| panic!("{id}: no Measurements section"));
        let body = text_without_markers(body);
        assert!(
            body.len() >= 40,
            "{id}: Measurements is empty or a stub: {body:?}"
        );
        // Either numbers, or an explicit `not run: <reason>`.
        let has_digit = body.chars().any(|c| c.is_ascii_digit());
        let has_not_run = body.contains("not run: ")
            && body
                .split("not run: ")
                .nth(1)
                .is_some_and(|r| r.trim().len() > 3);
        assert!(
            has_digit || has_not_run,
            "{id}: Measurements has no number and no `not run: <reason>`"
        );
    }
}

#[test]
fn anything_not_run_says_so_and_is_never_go() {
    for (id, file) in FILES {
        let text = read(file);
        let measurements = section(&text, "Measurements").unwrap();
        let value = verdict_value(&text).unwrap();
        // A spike whose Measurements section is `not run` has no numbers to stand on.
        if measurements.trim_start().starts_with("not run: ") {
            assert_ne!(value, "go", "{id}: a spike that was not run cannot be `go`");
            assert_ne!(
                value, "go-with-conditions",
                "{id}: a spike that was not run cannot be go-with-conditions"
            );
        }
    }
    // S4 and S5 depend on a Google sign-in. With no credential they must say so.
    for (id, file) in [("S4", FILES[3].1), ("S5", FILES[4].1)] {
        let text = read(file);
        let value = verdict_value(&text).unwrap();
        if value == "go" || value == "go-with-conditions" {
            let m = section(&text, "Measurements").unwrap();
            assert!(
                !m.contains("not run:"),
                "{id}: `{value}` next to `not run:` in Measurements"
            );
        }
    }
}

#[test]
fn s2_is_no_go_without_the_dan_approved_line() {
    let text = read(FILES[1].1);
    let approved = text.lines().any(|l| {
        l.strip_prefix("Dan approved: yes (")
            .is_some_and(|rest| rest.ends_with(')') && rest.len() > 4)
    });
    if !approved {
        assert_eq!(
            verdict_value(&text),
            Some("no-go"),
            "S2 must be no-go without `Dan approved: yes (<date>)`"
        );
    }
    // The reason is one paragraph: a non-empty paragraph follows the Verdict line.
    let verdict = section(&text, "Verdict").unwrap();
    let paragraphs = verdict
        .split("\n\n")
        .filter(|p| p.trim().len() > 60)
        .count();
    assert!(paragraphs >= 1, "S2 Verdict needs a one-paragraph reason");
}

#[test]
fn readme_has_a_row_per_spike_with_verdict_date_versions_and_prs() {
    let readme = read("README.md");
    let rows: Vec<&str> = readme
        .lines()
        .filter(|l| l.starts_with("| S") && !l.starts_with("| Spike "))
        .collect();
    assert_eq!(
        rows.len(),
        5,
        "README must have one row per spike S1 to S5, got {rows:?}"
    );
    for ((id, file), row) in FILES.iter().zip(&rows) {
        assert!(
            row.starts_with(&format!("| {id} ")),
            "row for {id} out of order: {row}"
        );
        assert!(row.contains(file), "{id} row must link {file}");
        let cells: Vec<&str> = row.split('|').map(str::trim).collect();
        // | S1 .. | link | verdict | date | versions | PRs |
        assert_eq!(
            cells.len(),
            8,
            "{id} row has the wrong number of columns: {row}"
        );
        let verdict = cells[3];
        assert!(
            VERDICTS.contains(&verdict),
            "{id}: README verdict {verdict:?} not allowed"
        );
        assert_eq!(
            Some(verdict),
            verdict_value(&read(file)),
            "{id}: README verdict differs from its file"
        );
        let date = cells[4];
        assert!(
            date.len() == 10 && date.as_bytes()[4] == b'-' && date.as_bytes()[7] == b'-',
            "{id}: run date {date:?} is not YYYY-MM-DD"
        );
        assert!(!cells[5].is_empty(), "{id}: tool versions missing");
        assert!(!cells[6].is_empty(), "{id}: PRs it changes missing");
    }
    assert!(
        readme.contains("git --version"),
        "README must name the `git --version` it recorded"
    );
    assert!(
        readme.contains("rclone version"),
        "README must name the `rclone version` it recorded"
    );
    assert!(
        readme.lines().any(|l| l.starts_with("Verdict: ")),
        "README needs a `Verdict:` line"
    );
}

#[test]
fn spike_code_is_not_a_workspace_member_and_each_spike_has_a_readme() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let manifest = fs::read_to_string(root.join("Cargo.toml")).expect("Cargo.toml");
    assert!(
        !manifest.contains("spikes"),
        "spikes/ must not be a workspace member"
    );
    for id in ["s1", "s2", "s3", "s4", "s5"] {
        assert!(
            root.join("spikes").join(id).join("README.md").exists(),
            "spikes/{id}/README.md missing"
        );
    }
}

#[test]
fn section_helper_returns_only_its_own_section() {
    let text = "# T\n\n## A\none\n\n## B\ntwo\nthree\n\n## C\nfour\n";
    assert_eq!(section(text, "A"), Some("one\n\n"));
    assert_eq!(section(text, "B"), Some("two\nthree\n\n"));
    assert_eq!(section(text, "C"), Some("four\n"));
    assert_eq!(section(text, "D"), None);
    assert_eq!(verdict_value("x\nVerdict: no-go\n"), Some("no-go"));
    assert_eq!(verdict_value("Verdicts: x"), None);
}
