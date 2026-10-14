//! Human-readable rendering of JSON-RPC results (G8): every function
//! here takes the already-decoded `result`/`error` [`Value`]
//! PR-6's server returned and turns it into the text a terminal
//! user reads, or (the `--json` path, handled by `commands.rs`
//! simply skipping these functions) the raw structured value
//! untouched. No RPC call, no business logic, and no on-disk
//! mutation happens anywhere in this module — it only formats data
//! this crate's `transport` module already received.

use serde_json::Value;

use crate::transport::RpcCallError;

/// How many outgoing edges `render_resolve_result` prints by
/// default before truncating (edge case: a response with over
/// 10,000 edges must be truncated with a "use --json" notice). Kept
/// well under that threshold so the common case stays readable in a
/// terminal, while the truncation path itself is exercised by any
/// response exceeding it — including the >10,000 case the spec names
/// explicitly.
const MAX_EDGES_DISPLAYED: usize = 100;

fn str_field<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}

fn bool_field(value: &Value, field: &str) -> bool {
    value.get(field).and_then(Value::as_bool).unwrap_or(false)
}

// ---- source.* -------------------------------------------------------

/// `source add` success (G8): names the registered source and path,
/// plus the scan stats the daemon returned.
pub fn render_source_added(name: &str, path: &str, stats: &Value) -> String {
    let mut out = format!("Registered source \"{name}\" at {path}\n");
    out.push_str(&format!(
        "  files seen: {}, files ignored: {}, units written: {}, links written: {}\n",
        stats.get("files_seen").and_then(Value::as_u64).unwrap_or(0),
        stats
            .get("files_ignored")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        stats
            .get("units_written")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        stats
            .get("links_written")
            .and_then(Value::as_u64)
            .unwrap_or(0),
    ));
    out
}

pub fn render_source_removed(name: &str) -> String {
    format!("Removed source \"{name}\"\n")
}

pub fn render_source_not_found(name: &str) -> String {
    format!("source \"{name}\" not found\n")
}

/// Whether a `source.unregister`/`rescan` error message names a
/// missing source — `tephrad::registry::RegistryError::NotFound`'s
/// `Display` text is `no source named "<name>"`, matched here so the
/// CLI can tell "not found" apart from every other registry error
/// without this crate owning (or re-implementing) the registry's own
/// error type. T1: the real daemon's `source.unregister` is
/// idempotent and never actually sends this, so `commands::source_remove`
/// checks client-side via `source.list` before calling unregister at
/// all; this function (and the `RpcCallError::Rpc` arm that calls it)
/// remains only as a defensive fallback for a server that does error.
pub fn error_means_not_found(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("no source named") || lower.contains("not found")
}

/// `source list` / `status`'s shared table renderer. Rows are
/// `SourceRowWire`-shaped JSON (`name`, `path`, `status`,
/// `last_scanned_at`). Zero rows renders the "no sources registered"
/// line the spec requires instead of an empty table.
pub fn render_source_table(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "no sources registered\n".to_string();
    }

    let mut out = String::from("NAME                 STATUS    PATH\n");
    for row in rows {
        let name = str_field(row, "name");
        let path = str_field(row, "path");
        let status = str_field(row, "status");
        let offline_marker = if status == "offline" {
            " (offline)"
        } else {
            ""
        };
        out.push_str(&format!("{name:<20} {status:<9} {path}{offline_marker}\n"));
    }
    out
}

/// `status` (G8): bound address (known client-side — this crate
/// always dials the address it was told to, so there is nothing to
/// ask the daemon for here) plus per-source scan state from
/// `source.list`. Daemon uptime is deliberately *not* fabricated:
/// PR-6's RPC surface exposes no verb that reports it (see this
/// crate's top-level doc comment / the coder's summary for this PR),
/// so that line says so plainly instead of inventing a number.
pub fn render_status(addr: &str, rows: &[Value]) -> String {
    let mut out = format!("Daemon address: {addr}\n");
    out.push_str("Daemon uptime: unavailable (tephrad does not expose an uptime RPC method)\n");
    out.push_str(&render_source_table(rows));
    out
}

// ---- unit.resolve_link ------------------------------------------------

fn render_edge(edge: &Value) -> String {
    let kind = str_field(edge, "kind");
    let to_raw = str_field(edge, "to_raw");
    let offline = bool_field(edge, "offline");
    let target = match edge.get("to") {
        Some(Value::Object(map)) => map
            .get("resolved")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| "dangling".to_string()),
        Some(Value::String(s)) if s == "dangling" => "dangling".to_string(),
        _ => "dangling".to_string(),
    };
    let offline_marker = if offline { " (offline)" } else { "" };
    format!("  [{kind}] {to_raw} -> {target}{offline_marker}\n")
}

/// `link resolve`'s success rendering (GP5): visibly distinguishes
/// the source the lookup was resolved *from* (`requested_default_source`,
/// when given) from the source the link actually resolved *to*
/// (`unit.source`), so cross-source federation is legible rather than
/// just another field in an undifferentiated dump.
pub fn render_resolve_result(result: &Value, requested_default_source: Option<&str>) -> String {
    let unit = result.get("unit").cloned().unwrap_or(Value::Null);
    let edges = result
        .get("edges")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let target_source = str_field(&unit, "source");
    let resolving_from = requested_default_source
        .map(str::to_string)
        .unwrap_or_else(|| "(none — the link was already fully-qualified)".to_string());

    let mut out = String::new();
    out.push_str(&format!("Resolving from source: {resolving_from}\n"));
    out.push_str(&format!("Target source:         {target_source}\n"));
    out.push_str(&format!("Resolved unit: {}\n", str_field(&unit, "id")));
    out.push_str(&format!("  path: {}\n", str_field(&unit, "rel_path")));

    if let Some(frontmatter) = unit.get("frontmatter_json").and_then(Value::as_str) {
        out.push_str(&format!("  frontmatter: {frontmatter}\n"));
    }

    let tags: Vec<String> = unit
        .get("tags")
        .and_then(Value::as_array)
        .map(|v| {
            v.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    out.push_str(&format!(
        "  tags: {}\n",
        if tags.is_empty() {
            "(none)".to_string()
        } else {
            tags.join(", ")
        }
    ));

    out.push_str(&format!(
        "  offline: {}\n",
        if bool_field(&unit, "offline") {
            "yes"
        } else {
            "no"
        }
    ));

    out.push_str(&format!("Outgoing edges ({}):\n", edges.len()));
    let shown = edges.iter().take(MAX_EDGES_DISPLAYED);
    for edge in shown {
        out.push_str(&render_edge(edge));
    }
    if edges.len() > MAX_EDGES_DISPLAYED {
        out.push_str(&format!(
            "  ... and {} more, use --json for full output\n",
            edges.len() - MAX_EDGES_DISPLAYED
        ));
    }
    out
}

/// Renders a failed `link resolve` call. An ambiguous target (E4)
/// renders the daemon's tied-candidate list as a numbered, readable
/// list rather than the raw error blob; every other failure mode
/// (unresolved, unknown source, missing/empty target, auth, transport)
/// prints its own already-human-readable message.
pub fn render_resolve_error(err: &RpcCallError) -> String {
    if let RpcCallError::Rpc {
        data: Some(data),
        message,
        ..
    } = err
    {
        if let Some(inner) = data.get("data") {
            if data.get("error").and_then(Value::as_str) == Some("ambiguous") {
                let source = str_field(inner, "source");
                let target = str_field(inner, "target");
                let candidates: Vec<&str> = inner
                    .get("candidates")
                    .and_then(Value::as_array)
                    .map(|v| v.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();
                let mut out =
                    format!("\"{target}\" is ambiguous in source \"{source}\"; candidates:\n");
                for (i, candidate) in candidates.iter().enumerate() {
                    out.push_str(&format!("  {}. {candidate}\n", i + 1));
                }
                return out;
            }
        }
        return format!("{message}\n");
    }
    format!("{err}\n")
}

// ---- link.preview_refactor / link.apply_refactor ----------------------

/// `link refactor` preview rendering: a diff-style list of every
/// file and line that would be rewritten. The exact response shape
/// here (`changes: [{file, line, before, after}]`) is a judgement
/// call pinned by this PR's tests/fixtures, since PR-8 (which owns
/// the real on-disk refactor algorithm) had not yet frozen a wire
/// contract for these two methods at the time this PR was written —
/// only the *request* shape (`{source, old_path, new_path}`) was
/// settled (ticket round-3 delta, impl °). Falls back to a pretty
/// JSON dump if the response doesn't have the expected shape, so an
/// eventual contract mismatch degrades to "ugly but visible", never
/// to silently showing nothing.
pub fn render_refactor_preview(result: &Value) -> String {
    let changes = match result.get("changes").and_then(Value::as_array) {
        Some(c) => c,
        None => return format!("{}\n", pretty(result)),
    };

    if changes.is_empty() {
        return "no incoming links would be affected\n".to_string();
    }

    let mut out = format!("{} affected link(s):\n", changes.len());
    for change in changes {
        let file = str_field(change, "file");
        let line = change.get("line").and_then(Value::as_u64).unwrap_or(0);
        let before = str_field(change, "before");
        let after = str_field(change, "after");
        out.push_str(&format!("  {file}:{line}  - {before}\n"));
        out.push_str(&format!("  {file}:{line}  + {after}\n"));
    }
    out
}

/// `link refactor --apply`'s result rendering: prints the daemon's
/// success/abort result. See [`render_refactor_preview`]'s doc
/// comment on the response-shape judgement call this also relies on.
pub fn render_refactor_apply(result: &Value) -> String {
    match result.get("status").and_then(Value::as_str) {
        Some("applied") => {
            let files_changed = result
                .get("files_changed")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            format!("Refactor applied: {files_changed} file(s) changed\n")
        }
        Some("aborted") => {
            let reason = str_field(result, "reason");
            format!("Refactor aborted: {reason}\n")
        }
        // R5 review finding: this PR introduced "partial" (some files
        // rewritten, a later one failed) and owns the end-to-end CLI
        // wiring for it, so it must be rendered explicitly — falling
        // through to the raw-JSON-dump fallback arm below would print
        // an unreadable blob instead of naming exactly which files
        // succeeded and which failed.
        Some("partial") => {
            let files_changed = result
                .get("files_changed")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let mut out = format!(
                "Refactor partially applied: {files_changed} file(s) changed before a failure\n"
            );
            let succeeded = result
                .get("succeeded")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !succeeded.is_empty() {
                out.push_str("  succeeded:\n");
                for file in &succeeded {
                    out.push_str(&format!("    {}\n", file.as_str().unwrap_or("")));
                }
            }
            let failed = result
                .get("failed")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !failed.is_empty() {
                out.push_str("  failed:\n");
                for failure in &failed {
                    let file = str_field(failure, "file");
                    let error = str_field(failure, "error");
                    out.push_str(&format!("    {file}: {error}\n"));
                }
            }
            out
        }
        _ => format!("{}\n", pretty(result)),
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_table_renders_rows_with_offline_marker() {
        let rows = vec![
            json!({"name": "notes", "path": "/vault/notes", "status": "active", "last_scanned_at": 1}),
            json!({"name": "old", "path": "/vault/old", "status": "offline", "last_scanned_at": null}),
        ];
        let rendered = render_source_table(&rows);
        assert!(rendered.contains("notes"));
        assert!(rendered.contains("active"));
        assert!(rendered.contains("old"));
        assert!(rendered.contains("(offline)"));
    }

    #[test]
    fn source_table_empty_prints_no_sources_registered() {
        assert_eq!(render_source_table(&[]), "no sources registered\n");
    }

    #[test]
    fn error_means_not_found_matches_registry_not_found_text() {
        assert!(error_means_not_found("no source named \"old\""));
        assert!(!error_means_not_found("duplicate source"));
    }

    #[test]
    fn resolve_result_distinguishes_resolving_and_target_source() {
        let result = json!({
            "unit": {
                "id": "@atlas-repo:README.md",
                "source": "atlas-repo",
                "rel_path": "README.md",
                "frontmatter_json": null,
                "tags": [],
                "blocks": [],
                "offline": false
            },
            "edges": []
        });
        let rendered = render_resolve_result(&result, Some("notes"));
        let resolving_line = rendered
            .lines()
            .find(|l| l.contains("Resolving from"))
            .unwrap();
        let target_line = rendered
            .lines()
            .find(|l| l.contains("Target source"))
            .unwrap();
        assert!(resolving_line.contains("notes"));
        assert!(target_line.contains("atlas-repo"));
        assert!(!resolving_line.contains("atlas-repo"));
    }

    #[test]
    fn resolve_result_truncates_large_edge_lists() {
        let edges: Vec<Value> = (0..10_050)
            .map(|i| {
                json!({
                    "from": "@vault:a.md",
                    "to": {"resolved": format!("@vault:{i}.md")},
                    "to_raw": format!("[[{i}]]"),
                    "kind": "wikilink",
                    "offline": false
                })
            })
            .collect();
        let result = json!({
            "unit": {
                "id": "@vault:a.md", "source": "vault", "rel_path": "a.md",
                "frontmatter_json": null, "tags": [], "blocks": [], "offline": false
            },
            "edges": edges
        });
        let rendered = render_resolve_result(&result, None);
        assert!(rendered.contains("use --json for full output"));
        assert!(rendered.contains("9950 more"));
    }

    #[test]
    fn ambiguous_resolve_error_renders_numbered_candidates() {
        let err = RpcCallError::Rpc {
            code: -32010,
            message: "ambiguous".to_string(),
            data: Some(json!({
                "error": "ambiguous",
                "data": {
                    "source": "vault",
                    "target": "guide",
                    "candidates": ["@vault:lib/guide.md", "@vault:api/guide.md"]
                }
            })),
        };
        let rendered = render_resolve_error(&err);
        assert!(rendered.contains("1. @vault:lib/guide.md"));
        assert!(rendered.contains("2. @vault:api/guide.md"));
        assert!(
            !rendered.contains("{\"error\""),
            "must not dump the raw error blob"
        );
    }

    #[test]
    fn unresolved_resolve_error_prints_plain_message() {
        let err = RpcCallError::Rpc {
            code: -32010,
            message: "no unit in source \"vault\" matches \"missing\"".to_string(),
            data: Some(
                json!({"error": "unresolved", "data": {"source": "vault", "target": "missing"}}),
            ),
        };
        let rendered = render_resolve_error(&err);
        assert!(rendered.contains("no unit in source \"vault\" matches \"missing\""));
    }

    #[test]
    fn refactor_preview_renders_diff_style_changes() {
        let result = json!({
            "changes": [
                {"file": "@notes:Journal.md", "line": 12, "before": "[[OldDoc]]", "after": "[[NewDoc]]"}
            ]
        });
        let rendered = render_refactor_preview(&result);
        assert!(rendered.contains("@notes:Journal.md:12"));
        assert!(rendered.contains("- [[OldDoc]]"));
        assert!(rendered.contains("+ [[NewDoc]]"));
    }

    #[test]
    fn refactor_apply_renders_success() {
        let result = json!({"status": "applied", "files_changed": 3});
        assert_eq!(
            render_refactor_apply(&result),
            "Refactor applied: 3 file(s) changed\n"
        );
    }

    #[test]
    fn refactor_apply_renders_abort_reason() {
        let result = json!({"status": "aborted", "reason": "file modified since preview"});
        assert_eq!(
            render_refactor_apply(&result),
            "Refactor aborted: file modified since preview\n"
        );
    }
}
