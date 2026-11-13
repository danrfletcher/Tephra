# Architecture

Tephra is a federated link and graph engine for plain-text knowledge: a background daemon (`tephrad`) that indexes any number of folders ("sources") into one link graph, and a command-line client (`tephractl`) that talks to it. Obsidian vaults, code repositories and plain folders are treated as equal siblings in one namespace.

This page describes `main`. What is not built yet is listed in [Not built yet](#not-built-yet).

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
| `tephra-parser` | Pure parsing, no I/O and no resolving. Markdown (`pulldown-cmark`): frontmatter, headings, `[[wikilinks]]` (including `[[@source:path]]` and `[[path#^id]]`), `[markdown](links)`, `#tags` and `^block` ids, with every occurrence of an ID kept. Code (tree-sitter, 13 languages): symbols, comment blocks and `// ^id`, `# ^id` and `-- ^id` block IDs. Text of `.docx`, `.pptx` and PDF documents by paragraph, slide or page. |
| `tephra-cache` | The SQLite database (WAL mode): sources, units (each with a kind, size and `too_large` flag), links, tags, blocks, headings, symbols, a trigram full-text table and per-source scan state. Schema version 2. Versioned: on a schema change it drops and rebuilds rather than migrating, and tries to keep the rows of offline sources. |
| `tephra-graph` | The in-memory link graph built from the cache: forward links, backlinks, tags, and the link resolver. |
| `tephractl` | The CLI client: `source add/remove/list`, `status`, `ls`, `search`, `link resolve`, `link refactor`, `block-id`, `backlinks` and `daemon install/uninstall/status`. Holds no state; every change to your files is made by the daemon (see [What Tephra writes](#what-tephra-writes)). |

## How a source plugs in

1. `tephractl source add notes /path/to/notes` calls `source.register`. The name becomes a namespace: every note in it is addressed as `@notes:path/to/note`.
2. `tephrad` walks the folder. Ignored names (`.git`, `node_modules`, `target`, `.venv`, `__pycache__`, `.DS_Store`, `Library` and `.Trash` directly under `$HOME`, plus anything listed in a `.tephraignore` at the source root) are pruned before the walk descends into them, and the walk streams rather than collecting every path first. Every other file becomes a unit with one of five kinds: `prose` (Markdown and other text notes), `code` (13 languages, read with tree-sitter), `document` (`.docx`, `.pptx` and PDF, read for their text), `text-other` or `binary`; a folder is listed as a `folder`. Files over 20 MiB (the 20 MB cap) are indexed by name only.
3. Each `prose` and `code` file is parsed to a unit and written to the cache with its links, tags, headings, symbols and blocks; a document is written with its text by page, slide or paragraph; a `text-other` file is searchable by blank-line-separated block; a binary or too-large file is written as a unit with its name, kind, size and mtime only. The cache is SQLite schema version 2: a cache from an older version is rebuilt, and every source is rescanned as if it were new. A scan records when it starts and finishes, and the daemon rescans any source whose scan never finished (for example after a `kill -9`) on its next start. A `notify` watcher keeps it current: filesystem events are debounced (250 ms) and coalesced, the file is re-parsed, and the graph is updated. Connected WebSocket clients get a `graph:changed` notification.
4. If a source's folder goes missing (an unmounted drive), the source is marked offline. Its cached index is kept, and links into it resolve with an `(offline)` marker until it returns.

Sources are independent: a missing or broken source does not corrupt another.

## How links resolve

- `[[Note]]` (no prefix) resolves within the note's own source, by file name or path, shortest path first. If two files tie, the resolver returns an error listing every candidate rather than guessing.
- `[[@atlas-repo:README]]` resolves in the named source. Reaching across sources needs the explicit `@source:` prefix; there is no search across all sources for an unprefixed link.
- A target that matches nothing is reported as unresolved, which is a different result from ambiguous.
- `[[@source:path#^id]]` links to a block (write block links as `[[path#^id]]`, or with the `@source:` prefix to reach another source). The named file is looked at first (`block {id, line, moved: false}`; for a code file `line` is the definition's first line, the one that carries the ID comment). If the ID is not there and is a Tephra ID (exactly 6 characters of `a-z0-9`), the one other file of the same source that has it is the answer, with `moved: true`; two or more files, or one file with the ID twice, is an ambiguity error that lists every file and line, never a guess. A typed ID such as `^summary` missing from its file is unresolved, whatever other files hold, and the search for a moved ID never leaves the source. `#Heading` and `#symbol_name` anchors resolve to the file and carry `block: null`. `[[path^id]]` (no `#`) is never a block link.
- **Duplicate block IDs.** Within one file, a duplicate is renamed (see [What Tephra writes](#what-tephra-writes)); across files it is allowed, and resolving it gives the ambiguity error above. A file that already held a duplicate when it was first scanned is left as it is, and resolving that ID lists every line.
- `graph.get_backlinks` takes a unit id (`@s:path`, every link to the file, anchored or not) or a block id (`@s:path#^id`, only links to that block, including links written to the block's old file that find it here as `moved`). Each row has the `line` of the link (1-based; `null` for a link in a document, which has pages, not lines), and there is one row per `(from, line)`. The cache keeps the line in `links.line`; a cache without it is rebuilt, and an offline source's links come back with `line` null until its next scan. `tephractl backlinks <id>` prints it.

## The RPC surface

JSON-RPC 2.0 over HTTP (`POST /rpc`) and WebSocket (`GET /rpc`) on `127.0.0.1:4812`. Every call needs the local bearer token (a file in the daemon's data directory, mode 0600); it is re-read and checked on every HTTP request and every WebSocket message. The method set is a closed list of 13 (`METHOD_NAMES` in `crates/tephrad/src/rpc/dispatch.rs`, with a test that the dispatch table is exactly that set):

- `source.register`, `source.unregister`, `source.list`, `source.rescan`
- `unit.resolve_link`, `graph.get_backlinks`, `graph.get_forward_links`, `graph.get_edges`
- `link.preview_refactor` and `link.apply_refactor` (`{source, old_path, new_path}`): the links a file move would break, and the rewrite that fixes them. See [What Tephra writes](#what-tephra-writes).
- `block.ensure_id` (`{unit, line}` or `{unit, symbol}`): gives a block an ID, or returns the one it has, as `{id, link, created}`. See [What Tephra writes](#what-tephra-writes).
- `unit.list`: one folder of a source (`{source, folder?, recursive?}`), each entry `{id, name, rel_path, kind, size, mtime, title?}`, folders first then by name. Files come from the cache through a read-only connection; folders of an online source come from the disk (so empty folders show), and an offline source is listed from the cache alone with `offline: true`. `tephractl ls <source> [folder] [-r]` prints it.
- `unit.search`: a name search across every source (`{query, scope?, limit?}`; `scope` is `names` (this bullet), `headings` or `blocks` (inside one file, with `in`), or `all_blocks` (the `^^` search, below), and `limit` is 1 to 100000, default 50). Each result is `{id, source, title, kind, score, offline}`, best first. A file matches on its name, its title (front-matter title, else first heading, else the name) or its path, case-insensitively and literally (a character of the query is that character, never a wildcard): an exact name scores 1.0, then a prefix of the name, then a looser match on the name or title, then a match on the path alone. Every kind of file is searchable, including ones too large to read and those of an offline source (`offline: true`). It runs on its own read-only connection in one pass, never takes the writer gate or the shared cache lock, and never writes. It does not feed link resolution: a tie in `[[Name]]` stays an error whatever a search would rank first (a test fences the resolver against importing the search). `tephractl search <query> [--limit n]` prints one row per result; a query starting with a single `#` or `^` is refused client-side unless `--in <id>` picks the file, and one starting with `^^` is the all-blocks search (a bare or blank `^^` is refused as an empty query, with no request). It prints one row per hit as `<unit>:<line>  <kind>  <snippet>` (`<unit>  <kind>  page N|slide N #i|paragraph N  <snippet>` for a document).
- `unit.search` with `scope: "all_blocks"` (`tephractl search "^^text"`): a full-text search of every block of every source. The `block_fts` table (an FTS5 trigram index, so part of a word matches) has one row per paragraph, heading and `^id` of a note, per symbol and comment block of a code file, per blank-line-separated block of a `text-other` file and per paragraph of a document (by page, slide or paragraph, never with a block ID). A binary, a too-large file or a document with no readable text has none. The scan writes a unit's rows in the same transaction as the unit, and every delete path removes them (a unit's rows are the rowid range `unit_id << 24 ..`). A symbol's text also carries its name split into words, so `conflict` finds `ConflictReason`. The query is never FTS5 syntax: it is sent as one quoted phrase, so `NEAR`, `AND`, `"` and `*` are plain text, and one under three characters uses a literal `LIKE`. Ranked: a symbol or heading named exactly like the query first (found through `block_names`, an indexed table of those names, not by scanning matches), then bm25, then shorter path, path, source and line. The work is bounded: bm25 is computed for at most the first 10,000 rows FTS5 matches (in index order), so a query matching more than that ranks those, and a 100k-file index answers a common word in tens of milliseconds, not seconds. A row over 16K characters is not cut off: a block over that is several rows of about 2K characters, each at the line it starts on. Each result is `{unit, source, kind, line | page | slide | paragraph, index?, snippet, block_id?, offline}`. It reads one snapshot on its own read-only connection and never takes the writer gate. A cache built before this index is flagged for a rescan on start. The daemon fails to start with a clear error if SQLite has no FTS5 or trigram tokenizer.

## What Tephra writes

Tephra reads your files and never changes them, except through exactly three writers. Nothing else (a scan, a search, a resolve, `ls`) writes to a source. Each of the three writes a temp file beside the original, syncs it and renames it into place, and checks the file's size and mtime first: if the file changed since it was read, nothing is written.

1. **`link.apply_refactor`** rewrites the links in other files that a moved file would break. It never moves the file itself.
2. **`block.ensure_id`** adds an ID to one block: ` ^id` at the end of a note paragraph, or `// ^id`, `# ^id` or `-- ^id` at the end of a code symbol's definition line. It changes nothing else, and never creates a duplicate.
3. **The duplicate block ID rename.** When a changed file is re-parsed and an ID that the cache already held for that file once now appears more than once, the copies are renamed in one write and the original keeps its ID, so links stay on it. The original is the occurrence whose text equals the cached block's text, then the one on the cached line; if none is on that line, the nearest one below it (a copy pasted above the original pushes the original down, never up); then the first.
   - A typed ID gets a numeric suffix: `^summary` becomes `^summary-1`, then `-2`, skipping any suffix the file already uses. A Tephra-format ID (6 characters of `a-z0-9`) gets a fresh 6-character ID that is unique in the file. In a code file the ID in the copy's comment changes and nothing else on the line does.
   - The cache records the new ID straight away, and the watcher drops the file events of Tephra's own write, so a rename never starts another batch.
   - Each rename logs one line with the unit id, the old ID, the new ID and the line.
   - Only `prose` and `code` files are ever written. A `^id` inside a code fence, an inline code span, frontmatter or a string literal is not a block and is never renamed. Documents, `text-other` and binary files are never written.
   - **Not renamed:** a file that already held duplicates the first time it was scanned (including after a schema rebuild), an ID that was not in the file's cache and appears twice (a block pasted twice from elsewhere), and the same ID in two different files. Resolving such an ID gives the ambiguity error.
   - **When it runs.** A rename happens when a changed file is re-parsed, and a file is re-parsed in two ways: by a watcher batch for a source registered in the current `tephrad` run, and by a full rescan of a source whose files already have cached rows. A duplicate added while `tephrad` was stopped is therefore renamed only by the next full rescan: an interrupted scan that `tephrad` redoes when it starts, or `source.rescan`. After a clean stop and start nothing is rescanned, and `tephrad` starts no watcher for sources that are already registered, so an edit made after such a start is not renamed until the source is rescanned (or registered again).
   - If the write fails (read-only file, full disk), the file is left as it was, the reason is logged, the cache keeps the duplicate rows and the scan carries on. If another writer holds the file (`block.ensure_id`) or the file changed since it was read, nothing is written and the file is tried again, up to 8 times about 250 ms apart, reading it afresh each time; its cached rows stay as they were until then (in a rescan the file is indexed after those tries). If it is still not possible after the last try, the file is indexed as it is, with the duplicate, and `tephrad` keeps in memory the IDs the cache held for the file before that edit, so the file's next re-parse (an edit, or a full rescan) still renames the copy. That memory is dropped once the file is renamed, no longer has a duplicate or is deleted, and is lost when `tephrad` restarts: a file with a duplicate left like this then counts as one scanned with its duplicate, and is never renamed.

## Running the daemon on macOS

`tephractl daemon install` copies `tephrad` to `~/Library/Application Support/tephra/bin/`, signs the copy with a local "Tephra Local" certificate (created in your login keychain the first time) and loads it as a launchd agent (`com.danrfletcher.tephrad`), so it starts at login and is restarted if it exits with a failure. `daemon status` shows whether the agent is installed and running, and `daemon uninstall` removes the agent and the installed copy but keeps the certificate, so a later install keeps its permissions.

macOS needs a one-time Full Disk Access grant for the installed `tephrad` before it can read folders such as `~/Documents`. Until it is granted, a read the system refuses is recorded as `permission_denied` in the daemon's `access.json`, and `daemon install` and `daemon status` print the one line that fixes it: open System Settings > Privacy & Security > Full Disk Access, add `~/Library/Application Support/tephra/bin/tephrad`, then run `tephractl daemon install` again. These commands need no token and no running daemon. Off macOS, start `tephrad` by hand.

## Not built yet

- **Remote sources.**
- **Promoting a folder to a nested source.**
- **`source.rename`, rename detection and automatic link fixes.** `link.apply_refactor` fixes links when you ask it to; nothing notices a rename on its own, and renaming a block ID never renames a file or rewrites links in other files.
- **Links to a code symbol by name**, for example `#parse_config` in a Python file (code is linked by block ID only).
- **OCR** for scanned documents: a PDF with no text layer has no readable text.
- **Clients** for Obsidian, VSCode and Chrome.
- A published benchmark.
