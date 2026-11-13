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

// ---- unit.list ------------------------------------------------------

/// `ls`'s output (G1). The offline banner, when the response says
/// `offline: true`, comes first; then one row per entry:
///
/// ```text
/// KIND             SIZE MODIFIED         NAME
/// ```
///
/// (no header is printed; the columns are `kind` padded to 10, `size`
/// right-aligned to 10, local `YYYY-MM-DD HH:MM`, then the name).
/// Recursive output indents each name by two spaces per level below
/// the listed folder. A `prose` entry's `title`, when the response has
/// one, follows the name as `  # <title>`; entries without one print
/// nothing there. An unknown size or mtime prints `-`. An empty list
/// prints `(empty)`.
///
/// Names and titles come from the filesystem and from file contents, so
/// control characters in them (an ESC would let a file name drive the
/// terminal) are shown as `?`.
pub fn render_unit_list(result: &Value) -> String {
    render_unit_list_with(result, &local_time)
}

/// [`render_unit_list`] with the time formatter supplied, so tests don't
/// depend on the machine's time zone.
pub fn render_unit_list_with(result: &Value, format_time: &dyn Fn(i64) -> String) -> String {
    let mut out = String::new();
    if bool_field(result, "offline") {
        out.push_str(&format!(
            "source {} is offline (cached listing)\n",
            printable(str_field(result, "source"))
        ));
    }
    let entries = result
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if entries.is_empty() {
        out.push_str("(empty)\n");
        return out;
    }

    let base_depth = path_depth(str_field(result, "folder"));
    let recursive = bool_field(result, "recursive");
    for entry in entries {
        let kind = str_field(entry, "kind");
        let size = entry
            .get("size")
            .and_then(Value::as_i64)
            .map_or_else(|| "-".to_string(), |n| n.to_string());
        let modified = entry
            .get("mtime")
            .and_then(Value::as_i64)
            .map_or_else(|| "-".to_string(), format_time);
        let depth = if recursive {
            path_depth(str_field(entry, "rel_path")).saturating_sub(base_depth + 1)
        } else {
            0
        };
        let name = printable(str_field(entry, "name"));
        let title = match entry.get("title").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => format!("  # {}", printable(t)),
            _ => String::new(),
        };
        out.push_str(&format!(
            "{kind:<10} {size:>10} {modified:<16} {indent}{name}{title}\n",
            indent = "  ".repeat(depth),
        ));
    }
    out
}

// ---- unit.search ----------------------------------------------------

/// `search`'s output (G8): one row per result, in the daemon's rank order,
///
/// ```text
/// <id>  <kind>  <title>
/// ```
///
/// with `  (offline)` after the title when the result's source is offline.
/// No results prints the plain line `no matches`. Titles and ids come from
/// file names and file contents, so control characters in them are shown
/// as `?`, as in [`render_unit_list`].
pub fn render_search_results(result: &Value) -> String {
    let hits = result.as_array().map(Vec::as_slice).unwrap_or_default();
    if hits.is_empty() {
        return "no matches\n".to_string();
    }
    let mut out = String::new();
    for hit in hits {
        out.push_str(&format!(
            "{}  {}  {}{}\n",
            printable(str_field(hit, "id")),
            printable(str_field(hit, "kind")),
            printable(str_field(hit, "title")),
            if bool_field(hit, "offline") {
                "  (offline)"
            } else {
                ""
            },
        ));
    }
    out
}

/// The longest paragraph text [`render_in_file_results`] shows, in
/// characters; a longer one is cut with `...`. `--json` is never cut.
const MAX_SHOWN_TEXT_CHARS: usize = 120;

/// `search --in`'s output (G9): one row per result, in the daemon's
/// order, as `<what>  <where>  <text>`:
///
/// ```text
/// heading h2  line 9  Second café
/// symbol enum  line 73  ConflictReason  ^abc
/// paragraph  line 7  A paragraph about alpha.  ^dup
/// document paragraph  page 2 #3  Some text
/// ```
///
/// `<where>` is `line N`, or `page N`, `slide N` or `paragraph N` for a
/// document, with `#<index>` for a paragraph's place on its page or
/// slide. A `^id` the block already has follows the text; a row has no
/// trailing spaces (a paragraph with no text is just its place). Runs of
/// whitespace (a paragraph over several lines) become one space, and
/// control characters are shown as `?`, as in [`render_search_results`].
/// A leading note says when the file's source is offline or when the
/// paragraph text could not be read. No results prints `no matches`.
pub fn render_in_file_results(result: &Value) -> String {
    let mut out = String::new();
    if bool_field(result, "offline") {
        out.push_str(&format!(
            "{} is offline (cached results)\n",
            printable(str_field(result, "unit"))
        ));
    }
    if bool_field(result, "text_unavailable") {
        out.push_str("paragraph text could not be read; only an empty query lists paragraphs\n");
    }
    let hits = result
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if hits.is_empty() {
        out.push_str("no matches\n");
        return out;
    }
    for hit in hits {
        let kind = str_field(hit, "kind");
        let what = match kind {
            "heading" => match hit.get("level").and_then(Value::as_i64) {
                Some(level) => format!("heading h{level}"),
                None => "heading".to_string(),
            },
            "symbol" => match str_field(hit, "symbol_kind") {
                "" => "symbol".to_string(),
                symbol_kind => format!("symbol {}", printable(symbol_kind)),
            },
            other => printable(other),
        };
        let place = ["line", "page", "slide", "paragraph"]
            .into_iter()
            .find_map(|key| {
                hit.get(key)
                    .and_then(Value::as_i64)
                    .map(|n| format!("{key} {n}"))
            })
            .unwrap_or_else(|| "-".to_string());
        // The index within a page or slide; a docx paragraph's equals
        // its number, so it is left out there.
        let place = match (
            hit.get("index").and_then(Value::as_i64),
            hit.get("paragraph"),
        ) {
            (Some(index), None) => format!("{place} #{index}"),
            _ => place,
        };
        let text = hit
            .get("text")
            .or_else(|| hit.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let id = match hit.get("block_id").and_then(Value::as_str) {
            Some(id) => format!("  ^{}", printable(id)),
            None => String::new(),
        };
        let row = format!("{what}  {place}  {}{id}", shown_text(text));
        out.push_str(row.trim_end());
        out.push('\n');
    }
    out
}

/// `search "^^q"`'s output (G10): one row per result, in the daemon's
/// rank order. A hit with a line is `<id>:<line>  <what>  <text>`, a
/// location a terminal or editor can jump to; a document hit has no line,
/// so it is `<id>  <what>  <where>  <text>`:
///
/// ```text
/// @repo:src/lib.rs:73  symbol  enum ConflictReason
/// @repo:notes/a.md:7  paragraph  ...text around the match...  ^abc
/// @repo:deck.pptx  document paragraph  slide 2 #1  Some text
/// @old:b.md:3  comment  a note  (offline)
/// ```
///
/// `<where>` is `page N`, `slide N #i` or `paragraph N`, as in
/// [`render_in_file_results`]. A `^id` the block
/// already has follows the text, then `  (offline)` for a source that is
/// offline. A row has no trailing spaces. No results prints `no
/// matches`. Ids and text come from file names and contents, so control
/// characters are shown as `?`, and a long text is cut as in
/// [`render_in_file_results`]. A part with nothing in it (a heading with
/// no text) is left out, so a row has no double gaps.
pub fn render_all_blocks_results(result: &Value) -> String {
    let hits = result.as_array().map(Vec::as_slice).unwrap_or_default();
    if hits.is_empty() {
        return "no matches\n".to_string();
    }
    let mut out = String::new();
    for hit in hits {
        let mut unit = printable(str_field(hit, "unit"));
        let place = match hit.get("line").and_then(Value::as_i64) {
            Some(line) => {
                unit = format!("{unit}:{line}");
                String::new()
            }
            None => {
                let place = ["page", "slide", "paragraph"]
                    .into_iter()
                    .find_map(|key| {
                        hit.get(key)
                            .and_then(Value::as_i64)
                            .map(|n| format!("{key} {n}"))
                    })
                    .unwrap_or_else(|| "-".to_string());
                match (
                    hit.get("index").and_then(Value::as_i64),
                    hit.get("paragraph"),
                ) {
                    (Some(index), None) => format!("{place} #{index}"),
                    _ => place,
                }
            }
        };
        // A part with nothing in it (a heading with no text) leaves no
        // gap: the parts are joined by two spaces.
        let mut parts = vec![
            unit,
            printable(str_field(hit, "kind")),
            place,
            shown_text(str_field(hit, "snippet")),
        ];
        if let Some(id) = hit.get("block_id").and_then(Value::as_str) {
            parts.push(format!("^{}", printable(id)));
        }
        if bool_field(hit, "offline") {
            parts.push("(offline)".to_string());
        }
        parts.retain(|part| !part.is_empty());
        out.push_str(&parts.join("  "));
        out.push('\n');
    }
    out
}

/// `text` on one line: whitespace runs are one space, control
/// characters are `?`, and a long text is cut at
/// [`MAX_SHOWN_TEXT_CHARS`] characters.
fn shown_text(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let one_line = printable(&one_line);
    if one_line.chars().count() > MAX_SHOWN_TEXT_CHARS {
        let cut: String = one_line.chars().take(MAX_SHOWN_TEXT_CHARS).collect();
        format!("{}...", cut.trim_end())
    } else {
        one_line
    }
}

/// Number of components in a source-relative path (`""` is 0).
fn path_depth(rel: &str) -> usize {
    rel.split('/').filter(|s| !s.is_empty()).count()
}

/// `text` with every control character replaced by `?`.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// A Unix timestamp as local `YYYY-MM-DD HH:MM`. Falls back to the UTC
/// reading if the C library can't convert it.
pub fn local_time(secs: i64) -> String {
    let t = secs as libc::time_t;
    // SAFETY: `tm` is plain data that `localtime_r` fully initializes
    // when it returns non-null; both pointers are valid for the call.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return format!("{secs}");
        }
        tm
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        tm.tm_year as i64 + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min
    )
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
/// `last_scanned_at`). `status` is `active`, `stale`, `offline` or
/// `permission_denied` (macOS Full Disk Access not granted, E13).
/// Zero rows renders the "no sources registered"
/// line the spec requires instead of an empty table.
pub fn render_source_table(rows: &[Value]) -> String {
    if rows.is_empty() {
        return "no sources registered\n".to_string();
    }

    // `permission_denied` (E13) is longer than the other statuses;
    // widen the column for it instead of breaking the alignment.
    let status_width = rows
        .iter()
        .map(|row| str_field(row, "status").chars().count())
        .fold(9, usize::max);
    let mut out = format!("{:<20} {:<status_width$} PATH\n", "NAME", "STATUS");
    for row in rows {
        let name = str_field(row, "name");
        let path = str_field(row, "path");
        let status = str_field(row, "status");
        let offline_marker = if status == "offline" {
            " (offline)"
        } else {
            ""
        };
        out.push_str(&format!(
            "{name:<20} {status:<status_width$} {path}{offline_marker}\n"
        ));
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

// ---- block.ensure_id ------------------------------------------------

/// `block-id`'s output: the link, alone on one line, so a script can
/// take the whole of stdout. Nothing else is printed, whether the ID was
/// just written or was already there (`--json` says which).
pub fn render_block_id(result: &Value) -> String {
    format!("{}\n", result["link"].as_str().unwrap_or_default())
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

    // A block link (G13): the block it points at. A link to a file, a
    // heading or a symbol name has `block: null` and prints nothing.
    if let Some(block) = result.get("block").filter(|b| b.is_object()) {
        let moved = if bool_field(block, "moved") {
            " (moved: found in another file of the source)"
        } else {
            ""
        };
        out.push_str(&format!(
            "Block: ^{} at line {}{moved}\n",
            printable(str_field(block, "id")),
            block.get("line").and_then(Value::as_i64).unwrap_or(0),
        ));
    }

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
                // A block ID found in more than one place (E7/E8) also
                // lists every occurrence, one unit:line each.
                let lines = inner
                    .get("lines")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                if !lines.is_empty() {
                    out.push_str("found at:\n");
                    for place in lines {
                        out.push_str(&format!(
                            "  {}:{}\n",
                            printable(str_field(place, "unit")),
                            place.get("line").and_then(Value::as_i64).unwrap_or(0)
                        ));
                    }
                }
                return out;
            }
        }
        return format!("{message}\n");
    }
    format!("{err}\n")
}

// ---- graph.get_backlinks ----------------------------------------------

/// `backlinks`' output (G16): one row per link occurrence, in the
/// daemon's order: `from:line  [kind] to_raw`, with `(offline)` after
/// a link of an offline source. A link with no line (one in a document)
/// shows the unit alone. An empty list prints `no backlinks`.
///
/// Unit ids and link text come from file names and file contents, so
/// control characters in them are shown as `?`.
pub fn render_backlinks(result: &Value) -> String {
    let edges = result.as_array().map(Vec::as_slice).unwrap_or_default();
    if edges.is_empty() {
        return "no backlinks\n".to_string();
    }
    let mut out = String::new();
    for edge in edges {
        let mut place = printable(str_field(edge, "from"));
        if let Some(line) = edge.get("line").and_then(Value::as_i64) {
            place = format!("{place}:{line}");
        }
        let mut row = format!(
            "{place}  [{}] {}",
            printable(str_field(edge, "kind")),
            printable(str_field(edge, "to_raw"))
        );
        if bool_field(edge, "offline") {
            row.push_str("  (offline)");
        }
        out.push_str(&row);
        out.push('\n');
    }
    out
}

// ---- link.preview_refactor / link.apply_refactor ----------------------

/// `link refactor` preview rendering: a diff-style list of every
/// file and line that would be rewritten, followed (R10 review
/// finding) by any referring file the preview could not read or
/// stat at all — named explicitly rather than just missing from the
/// list above with no indication anything was left out. The exact
/// response shape here (`changes: [{file, line, before, after}]`,
/// `skipped: [{file, error}]`) is a judgement call pinned by this
/// PR's tests/fixtures, since PR-8 (which owns the real on-disk
/// refactor algorithm) had not yet frozen a wire contract for these
/// two methods at the time this PR was written — only the *request*
/// shape (`{source, old_path, new_path}`) was settled (ticket
/// round-3 delta, impl °). Falls back to a pretty JSON dump if the
/// response doesn't have the expected shape, so an eventual contract
/// mismatch degrades to "ugly but visible", never to silently
/// showing nothing.
pub fn render_refactor_preview(result: &Value) -> String {
    let changes = match result.get("changes").and_then(Value::as_array) {
        Some(c) => c,
        None => return format!("{}\n", pretty(result)),
    };

    let mut out = if changes.is_empty() {
        "no incoming links would be affected\n".to_string()
    } else {
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
    };

    if let Some(skipped) = result.get("skipped").and_then(Value::as_array) {
        if !skipped.is_empty() {
            out.push_str(&format!(
                "{} file(s) skipped (could not be read or checked):\n",
                skipped.len()
            ));
            for s in skipped {
                let file = str_field(s, "file");
                let error = str_field(s, "error");
                out.push_str(&format!("  {file}: {error}\n"));
            }
        }
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
    fn source_table_shows_permission_denied_and_keeps_the_columns_aligned() {
        let rows = vec![
            json!({"name": "notes", "path": "/Users/dan/Documents/Notes", "status": "permission_denied", "last_scanned_at": 1}),
            json!({"name": "work", "path": "/vault/work", "status": "active", "last_scanned_at": 1}),
        ];
        let rendered = render_source_table(&rows);
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(lines[0].starts_with("NAME                 STATUS            PATH"));
        assert!(lines[1].contains("permission_denied /Users/dan/Documents/Notes"));
        assert!(!lines[1].contains("(offline)"), "never reported as offline");
        let path_col = |l: &str| l.find('/').unwrap();
        assert_eq!(path_col(lines[1]), path_col(lines[2]));
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

    /// R10 review finding: a file the daemon couldn't read or stat
    /// must be named, with its error, in the rendered human-readable
    /// preview — never silently missing with no trace at all.
    #[test]
    fn refactor_preview_renders_skipped_files() {
        let result = json!({
            "changes": [
                {"file": "@notes:Journal.md", "line": 1, "before": "[[OldDoc]]", "after": "[[NewDoc]]"}
            ],
            "skipped": [
                {"file": "@notes:Secret.md", "error": "Permission denied (os error 13)"}
            ]
        });
        let rendered = render_refactor_preview(&result);
        assert!(rendered.contains("1 file(s) skipped"));
        assert!(rendered.contains("@notes:Secret.md: Permission denied (os error 13)"));
    }

    /// An empty `skipped` list must add nothing to the rendered
    /// output — the common case stays exactly as before.
    #[test]
    fn refactor_preview_with_empty_skipped_list_renders_nothing_extra() {
        let result = json!({"changes": [], "skipped": []});
        assert_eq!(
            render_refactor_preview(&result),
            "no incoming links would be affected\n"
        );
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

    fn fixed_time(secs: i64) -> String {
        format!("T{secs}")
    }

    fn sample_list(offline: bool, recursive: bool) -> Value {
        json!({
            "source": "notes", "folder": "docs", "recursive": recursive,
            "offline": offline, "needs_rescan": false,
            "entries": [
                {"id": "@notes:docs/img", "name": "img", "rel_path": "docs/img",
                 "kind": "folder", "size": 2, "mtime": 100},
                {"id": "@notes:docs/img/logo.png", "name": "logo.png",
                 "rel_path": "docs/img/logo.png", "kind": "binary", "size": 12345, "mtime": 200},
                {"id": "@notes:docs/img/deep", "name": "deep", "rel_path": "docs/img/deep",
                 "kind": "folder", "size": 1, "mtime": 300},
                {"id": "@notes:docs/img/deep/x.rs", "name": "x.rs",
                 "rel_path": "docs/img/deep/x.rs", "kind": "code", "size": 7, "mtime": 400},
                {"id": "@notes:docs/guide.md", "name": "guide.md", "rel_path": "docs/guide.md",
                 "kind": "prose", "size": 99, "mtime": 500, "title": "The Guide"},
            ]
        })
    }

    #[test]
    fn unit_list_columns_are_kind_size_modified_name() {
        let rendered = render_unit_list_with(&sample_list(false, false), &fixed_time);
        let expected = "\
folder              2 T100             img
binary          12345 T200             logo.png
folder              1 T300             deep
code                7 T400             x.rs
prose              99 T500             guide.md  # The Guide
";
        // Non-recursive: no indentation, whatever the paths look like.
        assert_eq!(rendered, expected);
    }

    #[test]
    fn unit_list_recursive_indents_two_spaces_per_level_below_the_listed_folder() {
        let rendered = render_unit_list_with(&sample_list(false, true), &fixed_time);
        let expected = "\
folder              2 T100             img
binary          12345 T200               logo.png
folder              1 T300               deep
code                7 T400                 x.rs
prose              99 T500             guide.md  # The Guide
";
        assert_eq!(rendered, expected);
    }

    #[test]
    fn unit_list_at_the_source_root_indents_by_absolute_depth() {
        let list = json!({
            "source": "s", "folder": "", "recursive": true, "offline": false,
            "entries": [
                {"name": "a", "rel_path": "a", "kind": "folder", "size": 1, "mtime": 1},
                {"name": "b.txt", "rel_path": "a/b.txt", "kind": "text-other", "size": 2, "mtime": 2},
            ]
        });
        let expected = "\
folder              1 T1               a
text-other          2 T2                 b.txt
";
        assert_eq!(render_unit_list_with(&list, &fixed_time), expected);
    }

    #[test]
    fn unit_list_offline_banner_comes_first_and_only_when_offline() {
        let offline = render_unit_list_with(&sample_list(true, false), &fixed_time);
        assert!(
            offline.starts_with("source notes is offline (cached listing)\nfolder "),
            "{offline}"
        );
        let online = render_unit_list_with(&sample_list(false, false), &fixed_time);
        assert!(!online.contains("offline"), "{online}");
    }

    #[test]
    fn unit_list_title_shows_only_on_entries_that_have_one() {
        let rendered = render_unit_list_with(&sample_list(false, false), &fixed_time);
        assert_eq!(rendered.matches('#').count(), 1);
        let blank = json!({"source": "s", "folder": "", "recursive": false, "entries": [
            {"name": "a.md", "rel_path": "a.md", "kind": "prose", "size": 1, "mtime": 1, "title": ""},
            {"name": "b.md", "rel_path": "b.md", "kind": "prose", "size": 1, "mtime": 1},
        ]});
        assert!(!render_unit_list_with(&blank, &fixed_time).contains('#'));
    }

    #[test]
    fn unit_list_empty_folder_prints_empty_and_keeps_the_offline_banner() {
        let empty = json!({"source": "s", "folder": "x", "recursive": false,
                           "offline": false, "entries": []});
        assert_eq!(render_unit_list_with(&empty, &fixed_time), "(empty)\n");
        let offline = json!({"source": "s", "folder": "x", "recursive": false,
                             "offline": true, "entries": []});
        assert_eq!(
            render_unit_list_with(&offline, &fixed_time),
            "source s is offline (cached listing)\n(empty)\n"
        );
    }

    #[test]
    fn unit_list_unknown_size_and_mtime_print_a_dash() {
        let list = json!({"source": "s", "folder": "", "recursive": false, "offline": true,
            "entries": [{"name": "d", "rel_path": "d", "kind": "folder", "size": 1, "mtime": null},
                        {"name": "f", "rel_path": "f", "kind": "text-other", "size": null, "mtime": 5}]});
        let rendered = render_unit_list_with(&list, &fixed_time);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[1], "folder              1 -                d");
        assert_eq!(lines[2], "text-other          - T5               f");
    }

    #[test]
    fn unit_list_control_characters_in_names_never_reach_the_terminal() {
        let list = json!({"source": "s", "folder": "", "recursive": false, "entries": [
            {"name": "evil\u{1b}[2Jname\n", "rel_path": "x", "kind": "prose", "size": 1,
             "mtime": 1, "title": "t\u{7}itle"}]});
        let rendered = render_unit_list_with(&list, &fixed_time);
        assert!(!rendered.contains('\u{1b}') && !rendered.contains('\u{7}'));
        assert!(rendered.contains("evil?[2Jname?  # t?itle"), "{rendered:?}");
        assert_eq!(rendered.matches('\n').count(), 1);
    }

    #[test]
    fn local_time_has_the_documented_shape_and_round_trips() {
        let secs = 1_700_000_040;
        let text = local_time(secs);
        assert_eq!(text.len(), 16, "{text}");
        let b = text.as_bytes();
        assert!(
            b[4] == b'-' && b[7] == b'-' && b[10] == b' ' && b[13] == b':',
            "{text}"
        );
        // Turn the local reading back into an instant: it must be the
        // original, to the minute.
        let num = |r: std::ops::Range<usize>| text[r].parse::<i32>().unwrap();
        let back = unsafe {
            let mut tm: libc::tm = std::mem::zeroed();
            tm.tm_year = num(0..4) - 1900;
            tm.tm_mon = num(5..7) - 1;
            tm.tm_mday = num(8..10);
            tm.tm_hour = num(11..13);
            tm.tm_min = num(14..16);
            tm.tm_isdst = -1;
            libc::mktime(&mut tm) as i64
        };
        assert_eq!(back, secs - secs % 60);
    }
}
