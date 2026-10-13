# tephra-parser

A pure, synchronous Markdown AST parser for `tephrad`. Turns a path
and a string of file content into a `ParsedUnit`: YAML frontmatter, a
heading tree, `[[wikilinks]]` (including the federated
`[[@source:path]]` prefix form), `[markdown](links)`, `#tags`, and
`^block` anchors, plus a stable content hash.

This crate does no I/O, owns no filesystem watcher, and never opens
SQLite or resolves a link against a registry — it is a single-pass,
deterministic function of its inputs, consumed by the watcher (PR-4)
and the graph resolver (PR-5).

## Entry points

- `parse_unit(path: &Path, content: &str) -> ParsedUnit` — the main
  entry point.
- `parse_unit_bytes(path: &Path, bytes: &[u8]) -> Result<ParsedUnit, InvalidUtf8>`
  — for raw bytes read off disk that might not be valid UTF-8.

## Row-shaped output

`ParsedUnit::to_new_unit`/`to_new_tags`/`to_new_blocks`/`to_new_links`
convert a parsed result into `tephra-cache`'s own `NewUnit`/`NewTag`/
`NewBlock`/`NewLink` row types (imported from that crate rather than
redeclared here), once a caller knows the owning unit's row id.

## Tests

- `tests/unit/*.rs` — one file per extraction category (frontmatter,
  headings, wikilinks, markdown links, tags, blocks, determinism)
  plus `robustness.rs` for the empty-file/non-UTF-8 edge cases and
  `large_file.rs` for the bounded-time/bounded-memory, multi-megabyte
  (fence-free and fence-heavy) cases. Aggregated into a single test
  binary via `tests/unit.rs`.
- `tests/integration/parse_unit_fixtures.rs` — snapshot tests over the
  fixture corpus in `tests/fixtures/units/*.md`, each asserted against
  a committed golden JSON file in `tests/fixtures/units/__snapshots__/`.
  Aggregated via `tests/integration.rs`.
- `tests/fence_no_ui_deps.rs` — F3 regression: `cargo tree` must never
  show a UI/windowing/webview crate in this crate's dependency tree.

Regenerate every golden snapshot (after an intentional change to the
AST shape) with:

```
UPDATE_SNAPSHOTS=1 cargo test -p tephra-parser --test integration
```

Then diff the result and commit the updated files under
`tests/fixtures/units/__snapshots__/`.

## Example

```
cargo run -p tephra-parser --example dump_ast -- tests/fixtures/units/golden-path.md
```

Prints the full `ParsedUnit` as pretty JSON. Deterministic: re-running
against the same file produces byte-identical output.
