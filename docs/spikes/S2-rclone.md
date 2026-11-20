# S2: rclone as the Drive cache and upload engine

Run date: 2026-11-19. Tools: rclone v1.76.0 (release binary for osx-arm64 in
`spikes/.bin/`, gitignored; fetched with `spikes/s2/fetch-rclone.sh`), git 2.50.1
(Apple Git-155). Raw numbers: `numbers/live.txt` (`s2.*`).

## Question

Can rclone serve as Tephra's Drive cache and upload engine without a mount
(FUSE or NFS), refuse an upload when the remote revision moved, and report
Drive file ids for renames? (Fence F13: no rclone, FUSE or mount unless this is
a go and Dan says yes.)

## Method

`spikes/s2/run.sh` (rerun command). It uses `rclone` from PATH, else
`spikes/.bin/rclone`; if neither runs it prints `not run` and keeps the no-go.
All checks use rclone's local backend in a `mktemp` directory with an empty
config; no Drive account is involved:

    rclone help ... | grep -c -e '--vfs-cache-max-size'   in mount, copyto, cat, serve nfs
    rclone copyto <local> <remote-dir>/plan.md ; rclone cat <remote-dir>/plan.md
        (compare `mount | wc -l` before and after)
    remote holds "version one"; the remote then changes to "changed elsewhere"
        (newer mtime); upload "my edit" with no flag, with --immutable, with --update
    the same three with a remote that did not move
    rclone lsjson <dir>   (is an ID field present)

## Measurements

    rclone version                                  rclone v1.76.0
    --vfs-cache-max-size mentioned in:  mount 4, serve nfs 4, copyto 0, cat 0
    upload with no mount: exit 0; read back matches: yes
    mount table lines before / after: 13 / 13

    remote moved under the edit      exit   remote afterwards
      no flag                          0    "my edit"        (overwrote the newer remote)
      --immutable                      6    "changed elsewhere"
      --update                         0    "changed elsewhere"  (skipped silently)
    remote not moved
      no flag                          0    "my edit"
      --immutable                      6    "version one"    (refused although nothing moved)
      --update                         0    "my edit"

    lsjson on the local backend: ID field present: 0
    Drive file ids for renames:                 not run: no Drive sign-in in this script
    Drive revision precondition (headRevisionId): not run: no Drive sign-in; no rclone flag
                                                compares a revision id

## Verdict

Verdict: no-go

Dan approved: no (no `Dan approved: yes (<date>)` line in this file). Reason, one
paragraph: rclone's `copyto` and `cat` work with no mount, but its cache flags
(`--vfs-cache-*`) exist only in `mount` and `serve nfs`, so the cache itself
needs a FUSE or NFS mount, which F13 forbids; its only upload guards are
`--immutable` (refuses every overwrite, moved or not) and `--update` (compares
modification times and skips silently with exit 0), and neither is a revision
check, so a Drive-side move between read and write is not something it can
refuse the way PR-11's `headRevisionId` compare needs. Drive file ids for renames
were not measured (not run: no Drive sign-in), and that does not change the
verdict: the cache and upload findings above already fail it.

## What changes in the plan

Nothing. F13 stays in force: Drive is read and written through Tephra's own
adapter (PR-10, PR-11). This file only becomes a go if Dan answers yes and a
line `Dan approved: yes (<date>)` is added here by him.
