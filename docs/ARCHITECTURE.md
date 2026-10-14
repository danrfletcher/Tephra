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
| `tephra-cache` | The SQLite database (WAL mode): sources, units, links, tags, blocks. Versioned: on a schema change it drops and rebuilds rather than migrating, and tries to keep the rows of offline sources. |
| `tephra-graph` | The in-memory link graph built from the cache: forward links, backlinks, tags, and the link resolver. |
| `tephractl` | The CLI client: `source add/remove/list`, `status`, `link resolve`. Holds no state and never touches your files itself. |

## How a source plugs in

1. `tephractl source add notes /path/to/notes` calls `source.register`. The name becomes a namespace: every note in it is addressed as `@notes:path/to/note`.
2. `tephrad` walks the folder. Ignored directories (`.git`, `node_modules`, `target`, plus anything listed in a `.tephraignore` at the source root) are pruned before the walk descends into them, and the walk streams rather than collecting every path first. Only Markdown files (`.md`, `.markdown`) are parsed and indexed; other files are seen but skipped.
3. Each Markdown file is parsed to a unit and written to the cache. A `notify` watcher keeps it current: filesystem events are debounced (250 ms) and coalesced, the file is re-parsed, and the graph is updated. Connected WebSocket clients get a `graph:changed` notification.
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

## Not built yet

- **Link refactoring on disk** (`link.preview_refactor`, `link.apply_refactor`): pull request 8, still in review. `tephractl link refactor` exists as a client and reports a clean "unknown method" error against this daemon.
- Links to anything inside non-Markdown files, for example a function in a Python file.
- Remote sources.
- Clients for Obsidian, VSCode and Chrome.
- A published benchmark.
