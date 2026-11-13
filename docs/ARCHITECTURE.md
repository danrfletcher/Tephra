# Architecture

Tephra is a federated link and graph engine for plain-text knowledge: a background daemon (`tephrad`) that indexes any number of folders ("sources") into one link graph, and a command-line client (`tephractl`) that talks to it. Obsidian vaults, code repositories and plain folders are treated as equal siblings in one namespace.

This page describes the code on the branch `vector/atlas-3/pr-7` (pull requests 1 to 7). Link refactoring is not part of it yet; see [Not built yet](#not-built-yet).

```
 tephractl  ──┐                          ┌─ source "notes"      (a folder of notes)
 (CLI)        │  JSON-RPC 2.0            ├─ source "atlas-repo" (a code repository)
 future       ├─ HTTP + WebSocket ──►  tephrad ──► watcher ─► parser ─► SQLite cache ─► in-memory graph
 clients      │  127.0.0.1:4812,        (daemon)        ▲                                   │
              │  bearer token                           └──────── graph:changed (push) ◄────┘
              ┘
```

## Crates

| Crate | Job |
|---|---|
| `tephrad` | The daemon: config and auth token, source registry, filesystem watcher, JSON-RPC server. Headless by design: no UI of any kind. |
| `tephra-parser` | Pure Markdown parsing (`pulldown-cmark`). Extracts frontmatter, headings, `[[wikilinks]]` (including `[[@source:path]]`), `[markdown](links)`, `#tags` and `^block` ids. No I/O, no resolving. |
| `tephra-cache` | The SQLite database (WAL mode): sources, units (each with a kind, size and `too_large` flag), links, tags, blocks, headings, symbols, a trigram full-text table and per-source scan state. Schema version 2. Versioned: on a schema change it drops and rebuilds rather than migrating, and tries to keep the rows of offline sources. |
| `tephra-graph` | The in-memory link graph built from the cache: forward links, backlinks, tags, and the link resolver. |
| `tephractl` | The CLI client: `source add/remove/list`, `status`, `link resolve`. Holds no state and never touches your files itself. |

## How a source plugs in

1. `tephractl source add notes /path/to/notes` calls `source.register`. The name becomes a namespace: every note in it is addressed as `@notes:path/to/note`.
2. `tephrad` walks the folder. Ignored names (`.git`, `node_modules`, `target`, `.venv`, `__pycache__`, `.DS_Store`, `Library` and `.Trash` directly under `$HOME`, plus anything listed in a `.tephraignore` at the source root) are pruned before the walk descends into them, and the walk streams rather than collecting every path first. Every other file becomes a unit with a kind: `prose`, `code`, `document`, `text-other` or `binary`. Files over 20 MiB are indexed by name only.
3. Each Markdown file is parsed to a unit and written to the cache; every other file is written as a unit with its name, kind, size and mtime and no links, tags or blocks. A scan records when it starts and finishes, and the daemon rescans any source whose scan never finished (for example after a `kill -9`) on its next start. A `notify` watcher keeps it current: filesystem events are debounced (250 ms) and coalesced, the file is re-parsed, and the graph is updated. Connected WebSocket clients get a `graph:changed` notification.
4. If a source's folder goes missing (an unmounted drive), the source is marked offline. Its cached index is kept, and links into it resolve with an `(offline)` marker until it returns.

Sources are independent: a missing or broken source does not corrupt another.

## How links resolve

- `[[Note]]` (no prefix) resolves within the note's own source, by file name or path, shortest path first. If two files tie, the resolver returns an error listing every candidate rather than guessing.
- `[[@atlas-repo:README]]` resolves in the named source. Reaching across sources needs the explicit `@source:` prefix; there is no search across all sources for an unprefixed link.
- A target that matches nothing is reported as unresolved, which is a different result from ambiguous.

## The RPC surface

JSON-RPC 2.0 over HTTP (`POST /rpc`) and WebSocket (`GET /rpc`) on `127.0.0.1:4812`. Every call needs the local bearer token (a file in the daemon's data directory, mode 0600); it is re-read and checked on every HTTP request and every WebSocket message. The method set is a closed list:

- `source.register`, `source.unregister`, `source.list`, `source.rescan`
- `unit.resolve_link`, `graph.get_backlinks`, `graph.get_forward_links`, `graph.get_edges`
- `unit.list`: one folder of a source (`{source, folder?, recursive?}`), each entry `{id, name, rel_path, kind, size, mtime, title?}`, folders first then by name. Files come from the cache through a read-only connection; folders of an online source come from the disk (so empty folders show), and an offline source is listed from the cache alone with `offline: true`. `tephractl ls <source> [folder] [-r]` prints it.
- `unit.search`: a name search across every source (`{query, scope?, limit?}`; `scope` is `names` (this bullet), `headings` or `blocks` (inside one file, with `in`), or `all_blocks` (the `^^` search, below), and `limit` is 1 to 100000, default 50). Each result is `{id, source, title, kind, score, offline}`, best first. A file matches on its name, its title (front-matter title, else first heading, else the name) or its path, case-insensitively and literally (a character of the query is that character, never a wildcard): an exact name scores 1.0, then a prefix of the name, then a looser match on the name or title, then a match on the path alone. Every kind of file is searchable, including ones too large to read and those of an offline source (`offline: true`). It runs on its own read-only connection in one pass, never takes the writer gate or the shared cache lock, and never writes. It does not feed link resolution: a tie in `[[Name]]` stays an error whatever a search would rank first (a test fences the resolver against importing the search). `tephractl search <query> [--limit n]` prints one row per result; a query starting with a single `#` or `^` is refused client-side unless `--in <id>` picks the file, and one starting with `^^` is the all-blocks search (a bare or blank `^^` is refused as an empty query, with no request). It prints one row per hit as `<unit>:<line>  <kind>  <snippet>` (`<unit>  <kind>  page N|slide N #i|paragraph N  <snippet>` for a document).
- `unit.search` with `scope: "all_blocks"` (`tephractl search "^^text"`): a full-text search of every block of every source. The `block_fts` table (an FTS5 trigram index, so part of a word matches) has one row per paragraph, heading and `^id` of a note, per symbol and comment block of a code file, per blank-line-separated block of a `text-other` file and per paragraph of a document (by page, slide or paragraph, never with a block ID). A binary, a too-large file or a document with no readable text has none. The scan writes a unit's rows in the same transaction as the unit, and every delete path removes them (a unit's rows are the rowid range `unit_id << 24 ..`). A symbol's text also carries its name split into words, so `conflict` finds `ConflictReason`. The query is never FTS5 syntax: it is sent as one quoted phrase, so `NEAR`, `AND`, `"` and `*` are plain text, and one under three characters uses a literal `LIKE`. Ranked: a symbol or heading named exactly like the query first (found through `block_names`, an indexed table of those names, not by scanning matches), then bm25, then shorter path, path, source and line. The work is bounded: bm25 is computed for at most the first 10,000 rows FTS5 matches (in index order), so a query matching more than that ranks those, and a 100k-file index answers a common word in tens of milliseconds, not seconds. A row over 16K characters is not cut off: a block over that is several rows of about 2K characters, each at the line it starts on. Each result is `{unit, source, kind, line | page | slide | paragraph, index?, snippet, block_id?, offline}`. It reads one snapshot on its own read-only connection and never takes the writer gate. A cache built before this index is flagged for a rescan on start. The daemon fails to start with a clear error if SQLite has no FTS5 or trigram tokenizer.

## Not built yet

- **Link refactoring on disk** (`link.preview_refactor`, `link.apply_refactor`): pull request 8, still in review. `tephractl link refactor` exists as a client and reports a clean "unknown method" error against this daemon.
- Links to anything inside non-Markdown files, for example a function in a Python file.
- Remote sources.
- Clients for Obsidian, VSCode and Chrome.
- A published benchmark.
