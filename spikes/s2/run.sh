#!/usr/bin/env bash
# S2: is rclone usable as Tephra's Drive cache/upload engine without a mount?
#
#   spikes/s2/run.sh
#
# Uses `rclone` from PATH, else the release binary spikes/.bin/rclone
# (spikes/s2/fetch-rclone.sh downloads one; spikes/.bin/ is gitignored).
# When neither runs, prints `not run` and the no-go stands.
#
# What it measures offline, with rclone's local backend and a throwaway
# directory (the compare logic lives in rclone's sync layer, not the backend):
#   - whether upload and read work with no mount (mount table unchanged)
#   - which flags give a cache at all, and in which commands
#   - what an upload does when the remote file changed under it, with no flag,
#     --immutable and --update
#   - whether the listing carries a file id
# What it cannot measure without a Google sign-in is marked `not run`.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/common.sh
. "$here/../lib/common.sh"
export NUMBERS="${NUMBERS:-$SPIKE_OUT/numbers-live.txt}"

echo "== S2 rclone"
rclone_bin=""
if command -v rclone >/dev/null 2>&1; then
  rclone_bin="$(command -v rclone)"
elif [ -x "$SPIKES_DIR/.bin/rclone" ]; then
  rclone_bin="$SPIKES_DIR/.bin/rclone"
fi
if [ -z "$rclone_bin" ] || ! "$rclone_bin" version >/dev/null 2>&1; then
  echo "not run: no rclone on PATH and no runnable spikes/.bin/rclone (run spikes/s2/fetch-rclone.sh)"
  num s2.status "not run: no rclone binary"
  num s2.verdict "no-go (kept: not run)"
  exit 0
fi

num s2.rclone_version "$("$rclone_bin" version | head -1)"
work="$(spike_mktemp)"
trap 'spike_cleanup_dir "$work"' EXIT
: > "$work/rclone.conf"
rc() { "$rclone_bin" --config "$work/rclone.conf" "$@"; }

# Cache flags: where do they exist?
count_flag() { rc "$1" --help 2>&1 | grep -c -e "$2" || true; }
if rc help flags vfs >/dev/null 2>&1; then
  num s2.cache_flag_vfs_cache_max_size.in_mount "$(rc mount --help 2>&1 | grep -c -e '--vfs-cache-max-size' || true)"
  num s2.cache_flag_vfs_cache_max_size.in_copyto "$(rc copyto --help 2>&1 | grep -c -e '--vfs-cache-max-size' || true)"
  num s2.cache_flag_vfs_cache_max_size.in_cat "$(rc cat --help 2>&1 | grep -c -e '--vfs-cache-max-size' || true)"
  num s2.cache_flag_vfs_cache_max_size.in_serve_nfs "$(rc serve nfs --help 2>&1 | grep -c -e '--vfs-cache-max-size' || true)"
fi

# Upload and read with no mount.
mkdir -p "$work/local" "$work/remote"
printf 'version one\n' > "$work/local/plan.md"
mounts_before="$(mount | wc -l | tr -d ' ')"
rc copyto "$work/local/plan.md" "$work/remote/plan.md"; up_rc=$?
read_back="$(rc cat "$work/remote/plan.md")"
mounts_after="$(mount | wc -l | tr -d ' ')"
num s2.no_mount.upload_exit "$up_rc"
num s2.no_mount.read_back_matches "$([ "$read_back" = "version one" ] && echo yes || echo no)"
num s2.no_mount.mount_table_lines_before "$mounts_before"
num s2.no_mount.mount_table_lines_after "$mounts_after"

# A remote that moved under a queued edit. Each case starts from the same
# state: remote holds "version one"; the remote then changes to
# "changed elsewhere" (newer mtime); the local edit "my edit" is uploaded.
try_upload() { # try_upload <name> [flags...]
  local name="$1"; shift
  printf 'version one\n' > "$work/remote/plan.md"
  touch -t 202601010000 "$work/remote/plan.md"
  printf 'my edit\n' > "$work/local/plan.md"
  touch -t 202601020000 "$work/local/plan.md"
  # remote moves: new content, newer than the local edit
  printf 'changed elsewhere\n' > "$work/remote/plan.md"
  touch -t 202601030000 "$work/remote/plan.md"
  local code=0
  rc copyto "$@" "$work/local/plan.md" "$work/remote/plan.md" >/dev/null 2>&1 || code=$?
  num "s2.moved_remote.$name.exit" "$code"
  num "s2.moved_remote.$name.remote_after" "$(tr -d '\n' < "$work/remote/plan.md" | tr ' ' '_')"
}
try_upload default
try_upload immutable --immutable
try_upload update --update

# And the same flags when the remote did NOT move: a revision check would let
# this upload through.
try_unmoved() {
  local name="$1"; shift
  printf 'version one\n' > "$work/remote/plan.md"
  printf 'my edit\n' > "$work/local/plan.md"
  touch -t 202601020000 "$work/local/plan.md"
  touch -t 202601010000 "$work/remote/plan.md"
  local code=0
  rc copyto "$@" "$work/local/plan.md" "$work/remote/plan.md" >/dev/null 2>&1 || code=$?
  num "s2.unmoved_remote.$name.exit" "$code"
  num "s2.unmoved_remote.$name.remote_after" "$(tr -d '\n' < "$work/remote/plan.md" | tr ' ' '_')"
}
try_unmoved default
try_unmoved immutable --immutable
try_unmoved update --update

# File ids in a listing: the local backend has none; Drive's would need a sign-in.
rc lsjson "$work/remote" > "$work/ls.json"
num s2.lsjson_local.has_nonempty_ID_field "$(grep -c '"ID": *"[^"]' "$work/ls.json" || true)"
num s2.drive_file_ids_and_renames "not run: no Drive sign-in in this script (rclone's Drive backend needs its own OAuth config)"
num s2.drive_revision_precondition "not run: no Drive sign-in; no rclone flag compares a Drive headRevisionId"

if grep -q '^Dan approved: yes (' "$SPIKES_DIR/../docs/spikes/S2-rclone.md" 2>/dev/null; then
  num s2.dan_approved "yes"
else
  num s2.dan_approved "no line found"
  num s2.verdict "no-go (no 'Dan approved: yes' line in docs/spikes/S2-rclone.md)"
fi
echo "  done: S2"
