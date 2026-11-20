# S3: blobless clone plus sparse-checkout under Tephra's watcher

Run date: 2026-11-19. Tools: git 2.50.1 (Apple Git-155) on macOS arm64 (FSEvents),
git 2.39.5 in a `rust:1-bookworm` Linux container (inotify), `notify` 6.1.1 (the
version `tephrad` resolves in `Cargo.lock`), cargo. Raw numbers: `numbers/offline.txt`
(macOS) and `numbers/offline-linux.txt` (Linux), keys `s3.*`; the tables below are macOS; terminal transcripts:
`transcripts/S3.txt` (Linux container) and `transcripts/S3-macos.txt` (macOS); raw event log: `spikes/out/s3-events.tsv` (gitignored).

## Question

With `git clone --filter=blob:none --sparse` and `git sparse-checkout set docs`,
what does a `notify` watcher on the clone see for a fetch, a `sparse-checkout
add` and a read of a file outside the set? Does that read fetch bytes into
`.git`? Do `.git` writes reach the watcher? What must the clone do so a file
with unpushed commits or working-tree edits is never touched by Tephra?

## Method

`spikes/s3/run.sh` (rerun command; `spikes/run-offline.sh` also runs it). The
watcher is `spikes/s3/watcher/` (own cargo project, not a workspace member):
`notify` 6.1.1, recursive on the clone root, no ignore filter, so `.git` events
are logged too. Fixture: the S1 bare repo (50 files). Phases:

    A  git clone --filter=blob:none --sparse <fixture> ; git sparse-checkout set docs
    B  fixture advances to its second commit ; git fetch origin
    C  git sparse-checkout add src
    D  git cat-file -p origin/main:notes/note-01.md      (a file outside the set)
    E1 edit docs/doc-02.md in the working tree ; commit a change to docs/doc-03.md locally
    E2 git sparse-checkout set src         (docs/ leaves the set)
    E3 rm src/mod-05.txt by hand           (for comparison)

## Measurements

    phase  events  in .git  in working tree  working-tree files  .git kB  blob objects
    A        118      56         62                24               160        24
    B         61      61          0                24               188        24
    C        117      51         66                44               200        44
    D         24      24          0                44               212        45
    E1        92      90          2                44               232        46
    E2        40      21         19                25               232        46
    E3         1       0          1                24               232        46

    Read of a file outside the set (D): exit 0; 1 promisor blob and 1 promisor pack
    added; .git grew 12 kB; 24 events, all under .git; the file did not appear
    in the working tree (before: no, after: no).
    Fetch (B): 61 events, all under .git (FETCH_HEAD, refs/remotes, objects/pack/tmp_*);
    working tree 0 events.
    sparse-checkout add (C): 66 working-tree events (20 files created), 51 in .git.
    .git writes reach the watcher: yes. Phases A to E3 logged 453 events; 303 of
    them were under .git (the raw log holds 468 events in all; the 15 outside the phase counts were not attributed).

    Protect checks (E1, E2): before narrowing: 1 status change, 1 commit ahead of origin/main.
    After `sparse-checkout set src`:
      docs/doc-04.md (clean)                  absent (removed from disk)
      docs/doc-03.md (committed, not pushed)  absent (removed from disk; commit stays in history)
      docs/doc-02.md (edited, uncommitted)    present; git warned "not up to date and
                                              were left despite sparse patterns"
      status changes 1, commits ahead of origin/main 1 (unchanged)
    Hand removal (E3) of src/mod-05.txt: status shows ` D src/mod-05.txt`; status
    changes went from 1 to 2.

Linux container (git 2.39.5, inotify), same script, same fixture:

    phase  events  in .git  in working tree  working-tree files  .git kB  blob objects
    A         91      31         60                24               144        24
    B         41      41          0                24               168        24
    C         93      32         61                44               176        44
    D         16      16          0                44               184        45
    E1        60      56          4                44               204        46
    E2        41      22         19                25               204        46
    E3         1       0          1                24               204        46

    Read outside the set: 1 promisor blob, 1 promisor pack, .git grew 8 kB, 16 events
    all under .git, file not in the working tree. Protect checks: same outcome as on
    macOS (clean and committed files absent, edited file present, 1 commit ahead
    before and after, hand removal shows ` D src/mod-05.txt`). Raw event log: 359 events.
    Event counts differ by platform; the outcomes do not.

## Verdict

Verdict: go-with-conditions

Blobless clone plus sparse-checkout works and its events are readable, but the
watcher sees a lot of `.git` traffic, and `sparse-checkout` alone does not
protect a committed-but-unpushed file.

Conditions the clone needs:
1. Tephra's watcher must drop `.git/` events (303 of 453 were there). The
   daemon's default ignore list already names `.git`
   (`DEFAULT_IGNORED_NAMES` in `crates/tephrad/src/ignore.rs`, applied in
   `crates/tephrad/src/watcher.rs`), by reading the code. This spike ran a bare
   `notify` watcher, not `tephrad`'s, so that filtering is not measured here.
2. Evicting a GitHub file must go through `git sparse-checkout` (a path leaving
   the set is not a status change), never `rm` (shows as ` D`, a "change not pushed").
3. Before narrowing the set, Tephra must itself refuse any path with working-tree
   edits (git keeps those and warns, but only as a warning on stderr) and any path
   touched by a commit ahead of origin (git removed the file from disk; the
   commit survives, the working copy does not).
4. A read of a file outside the set (`cat-file -p`) is a promisor fetch: bytes go
   into `.git`, not the working tree, so it must be counted under the cap.

## What changes in the plan

- PR-8 (evict): remove through `sparse-checkout`, and check `git status --porcelain`
  and `git rev-list origin/<branch>..HEAD -- <path>` first, as conditions 2 and 3.
- PR-7 (fetch): a fetch of a single GitHub file by `cat-file` lands a blob in
  `.git`; use Tephra's own cache for content, consistent with S1.
- PR-8 should confirm with a test that a clone's `.git` writes produce no daemon
  events; the spike did not run `tephrad`'s watcher.
