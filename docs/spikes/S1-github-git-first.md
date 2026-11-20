# S1: GitHub reads, git-first versus REST

Run date: 2026-11-19. Tools: git 2.50.1 (Apple Git-155), python3 with urllib, macOS arm64.
Raw numbers: `numbers/offline.txt` (generated fixture, macOS), `numbers/offline-linux.txt`
(same fixture in a `rust:1-bookworm` container, git 2.39.5) and `numbers/live.txt` (public repo,
macOS). Terminal transcripts of the fixture run: `transcripts/S1.txt` (Linux container) and
`transcripts/S1-macos.txt`.

## Question

Can Tephra list a GitHub repo and detect changes (including renames) with a
blobless git clone (`fetch`, `ls-tree -r`, `diff -M`) instead of the REST API,
without file content landing in `.git` outside the cache cap? And what does the
same work cost through REST?

## Method

Exact commands. Every git command runs inside a directory made with `mktemp -d`
and removed afterwards. Nothing is ever sent to a remote.

Fixture (no network): `spikes/s1/make-fixture.sh <dir>` builds a bare repo of 50
text files (20 `docs/`, 20 `src/`, 6 `notes/`, 4 at the root). `... <dir> advance`
adds a second commit with one pure rename (`docs/doc-01.md` to `docs/guide-01.md`)
and one rename with a small edit (`src/mod-01.txt` to `src/engine-01.txt`).

Git half, `spikes/s1/git-half.sh <label> <url> <head> <base>`, with `du -sk .git`
(and the byte total, pack count and blob-object count of `.git`) recorded after
each step:

    git clone --filter=blob:none --no-checkout <url> <tmp>/clone
    git fetch origin                      (after the repo moved to its second commit)
    git ls-tree -r <head>
    git diff -M --name-status <base> <head>
    git cat-file --batch-all-objects --batch-check   (counts blobs in .git)
    git cat-file -p <head>:<path>         (the first-index read, every text file)

REST half, `spikes/s1/rest.py` against `https://api.github.com`:

    GET /repos/<repo>/git/trees/<head>?recursive=1
    GET /repos/<repo>/compare/<base>...<head>
    GET /repos/<repo>/commits
    GET /repos/<repo>/git/blobs/<sha>      (10 sampled files, to price the read)
    GET /repos/<repo>/commits?per_page=1   twice, the second with If-None-Match (60 s apart)
    GET /rate_limit                        (units used, before and after)

Public repo: `danrfletcher/Tephra`, pinned range `30b5b5750b1c...39f5a8e697aa`
(it holds 5 pure renames and 2 renames with edits). Rerun the fixture half with
`spikes/run-offline.sh`; rerun the live half with `spikes/s1/live.sh --unauthenticated`.

Credential state of this run: the Keychain item `tephra-spike-github` was not
found (`security` exit 44), so the live half ran unauthenticated (60 requests per
hour). Authenticated numbers: `not run: no credential`.

## Measurements

Generated fixture (50 text files), `du -sk .git`, then blob objects in `.git`:

    step                          .git kB   blob objects
    before clone                        0   -
    after blobless clone              120   0
    after git fetch                   148   0
    after git ls-tree -r              148   0
    after git diff -M                 160   2   (both sides of the edited rename)
    after first-index read            752   51  (49 blobs landed by the read)

    clone 114 ms, fetch 105 ms, ls-tree 65 ms (50 entries), diff -M 145 ms (2 lines),
    first-index read of 50 files 4033 ms, 48131 content bytes.

Same fixture in the Linux container (git 2.39.5): .git kB 112 after the blobless
clone, 136 after fetch, 136 after ls-tree, 144 after diff -M (2 blobs), 544 after
the first-index read (51 blobs, 49 landed by the read); 2 of 2 renames found.

Public repo `danrfletcher/Tephra` (git half), 33 text files at the head:

    step                          .git kB   blob objects
    after blobless clone              340   0
    after git fetch                   344   0
    after git ls-tree -r              344   0
    after git diff -M                 364   4
    after first-index read            756   35  (31 blobs landed by the read)

    clone 1008 ms, fetch 421 ms, ls-tree 77 ms (34 entries), diff -M 513 ms,
    first-index read 13135 ms, 210252 content bytes.

Public repo, REST half (unauthenticated, 15 requests, 366834 body bytes, 15
rate-limit units for the whole run):

    tree ?recursive=1     1 request    10566 body bytes   44 entries, truncated=false
    compare               1 request   184207 body bytes   28 files, 7 commits
    commits               1 request    95320 body bytes   23 commits listed
    blob sample (10)     10 requests   72089 body bytes
    first-index read of all 33 text files through REST: 33 blob requests
        (10 measured; 33 needed; the unauthenticated limit is 60 per hour)
    idle poll, 60 s apart: first request 4652 body bytes; conditional request
        (If-None-Match, ETag) returned 304 with 0 body bytes and cost 1
        rate-limit unit, by both the `/rate_limit` endpoint and the headers

Rename detection, same range, both methods:

    git diff -M --name-status:   7 renames: 5 pure (R100), 2 with edits
    REST compare, status "renamed": 7 renames: 5 pure, 2 with edits
    disagreements: 0
    fixture: git found 2 of 2 (1 pure, 1 edited)

Not measured: the rate-limit cost of a 304 poll with a token (the 1 unit above
is for an unauthenticated request; GitHub documents that an authenticated 304
costs nothing, and this run did not check that).

## Verdict

Verdict: go-with-conditions

Rule (written before the run): git is preferred only if no file content lands in
`.git` outside the cap; otherwise the first-index read uses the API.

Applying it: the blobless clone, `git fetch`, `git ls-tree -r` put zero file
content in `.git` (0 blob objects after each), so listing and change detection
can be git-first. `git diff -M` is the exception: for a rename with an edit, git
fetches the blobs of both sides (2 on the fixture, 4 on the public repo), so
content lands in `.git` there too. The first-index read through git lands one
blob per text file in `.git` (49 of 50, and 31 of 33), growing it from 148 kB to
752 kB and 344 kB to 756 kB, none of it under the cache cap. By the rule the
first-index read uses the API, and the blobs go into Tephra's own cache, not `.git`.

Condition: the REST first-index read of N text files is N blob requests; at 33
files that already needs more than half of an unauthenticated hour. It is
acceptable only with a token (5,000 per hour), and that number was not measured
here.

## What changes in the plan

- PR-5 (G9 first index): read every text file through the API, not
  `git cat-file`; `.git` is never the content store. This is the "if git leaves
  blobs outside the cap" branch the ticket already names.
- Listing and change detection (PR-5, PR-7): may use `git fetch` plus `ls-tree -r`
  with a blobless clone. Treat `git diff -M` as a blob-fetching call and count its
  bytes under the cap, or use REST compare for rename detection.
- `cache status` shows `.git` size separately (G12), because `.git` still grows
  by trees, commits and the `diff -M` blobs.
- Both methods agree on renames in this range (0 disagreements), so choosing one
  is a cost question, not a correctness one.
