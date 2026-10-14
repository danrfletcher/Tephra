# What the review and test gates caught

Every pull request in this repo went through a coder agent, then a separate reviewer agent, then a tester agent that runs the real daemon in a container. Nothing was opened until both passed. Pull requests 1 to 7 went through 38 reviewer runs and 10 tester runs.

These are seven of the findings those runs raised, each with a fix that a later reviewer or tester run re-checked. The fixes are committed on the pull request branches. Those pull requests are still open, so none of this is on `main` yet. Findings from the review of pull request 8 (link refactoring) are not listed here; they will be added once their fixes are verified.

## Pull request 6: JSON-RPC server

### 1. Shutdown panic that survived its first fix
- **Wrong:** Stopping the daemon with SIGTERM panicked ("Cannot drop a runtime in a context where blocking is not allowed") while still exiting with code 0.
- **Found by:** the reviewer starting the compiled daemon and sending SIGTERM: 6 panics in 6 runs.
- **Fix:** the first fix stopped one background task. The next reviewer re-ran it with a WebSocket client connected and it still panicked, 3 of 3, because open sockets kept the registry alive. The final fix makes each socket watch the shutdown signal, close with a "going away" frame, and be waited for (bounded to 5 seconds) before the registry is released.
- **Verified:** clean in 3 of 3 runs with an open authenticated socket, and 10 of 10 with 32 threads hammering WebSocket handshakes across SIGTERM. A regression test holds a socket open across SIGTERM; the coder confirmed it fails against the old code by temporarily reverting the fix.

### 2. A revoked token kept working on open sockets
- **Wrong:** The bearer token was checked once, at the WebSocket handshake. After the token file was rotated, an already-open socket could still call every method, including `source.register`, `source.unregister` and `source.rescan`. The 10 MiB request cap also covered only HTTP; WebSocket messages defaulted to 64 MiB.
- **Found by:** the reviewer comparing the server code against the spec rule that every call over either transport is authenticated.
- **Fix:** the token is re-verified against the token file on every WebSocket message, closing the socket on a mismatch, and WebSocket message and frame size are capped to the same limit as HTTP.
- **Verified:** the next review confirmed each fix, each with its own regression test.

## Pull request 7: command-line client

### 3. Writes sent twice
- **Wrong:** After a 10-second timeout the client treated the slow reply as a dropped connection and re-sent the request, including writes such as `source.register`.
- **Found by:** the reviewer building a server that reads the request and never answers: one `source.register` reached the daemon twice and the user saw an error after 20 seconds, for a registration that might have succeeded.
- **Fix:** writes are never retried automatically; long operations get a 120-second budget; a timed-out write reports that the outcome is unknown instead of success or failure. A later review caught that read-only calls were wrongly given the same message and fixed it.
- **Verified:** the next review confirmed writes are no longer re-sent. A later review reproduced the read-only message fix against the built binary with a listener that never replies.

### 4. "Removed" for a source that never existed
- **Wrong:** `tephractl source remove <name>` printed "Removed" and exited 0 for a name that was never registered, because the daemon's unregister is deliberately idempotent.
- **Found by:** the tester running the CLI against a real daemon in the container.
- **Fix:** the client lists sources first and reports "not found" with exit 1 without calling unregister.
- **Verified:** the tester's re-run confirmed it and the full test run passed.

## Smaller catches

### 5. Pull request 1: an empty token file accepted as the auth token
A zero-byte token file was reused ("reusing existing auth token"), so the auth layer would have accepted an empty bearer token. The write was also not atomic. The reviewer found it by starting the built binary with an empty file. The token must now be 64 hex characters or startup fails, and it is written to a temporary 0600 file, synced, and renamed into place. A later review confirmed invalid tokens are rejected.

### 6. Pull request 2: a schema change could make the cache unopenable
The reviewer's probe seeded an old-shape database and `Cache::open` failed with "no such column" on every start, because the rebuild read columns the old tables did not have. The first fix was only partial: rows that did not fit the new constraints still aborted the rebuild. The final fix skips the carry-over on a snapshot error and runs the restore in a savepoint that rolls back on error, with four new tests. A third review confirmed it.

### 7. Pull request 4: a rescan racing a live edit lost most of a source
In a large synthetic source (250,000 files), an edit landing during a rescan made the rescan fail with a UNIQUE constraint error and left 1,520 of 250,000 rows, with the source marked stale. The scan now overwrites an existing row instead of blindly inserting. A later review ran 100,000 files with 1,000 live edits: the rescan finished, every row was present, all edits were indexed and the source was active. Mutation checks showed the new tests fail without the fix.
