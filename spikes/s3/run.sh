#!/usr/bin/env bash
# S3: blobless clone plus sparse-checkout, with a `notify` watcher (the crate
# tephrad uses) logging every raw event under the clone, .git included.
#
#   spikes/s3/run.sh        (rerun command; see also spikes/run-offline.sh)
#
# Phases, each followed by a bounded wait for the event log to go quiet:
#   A  git sparse-checkout set docs
#   B  fixture moves to its second commit, then git fetch
#   C  git sparse-checkout add src
#   D  read one file outside the set (notes/) through git
#   E  protect checks: a working-tree edit and a local commit in the clone,
#      then narrow the set so docs/ leaves the working tree; also remove a
#      file by hand for comparison
# Everything runs in a throwaway clone made with mktemp. No remote is changed.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/common.sh
. "$here/../lib/common.sh"

echo "== S3 blobless clone + sparse-checkout under a notify watcher"
if ! command -v git >/dev/null 2>&1; then
  echo "not run: git is not installed"; num s3.status "not run: git is not installed"; exit 0
fi
echo "  git: $(git --version)"
num s3.git_version "$(git --version | sed 's/^git version //')"
if ! git_ok_for_sparse; then
  echo "not run: git older than 2.25 has no sparse-checkout"
  num s3.status "not run: git older than 2.25"; exit 0
fi
if ! command -v cargo >/dev/null 2>&1; then
  echo "not run: cargo is not installed, so the notify watcher cannot be built"
  num s3.status "not run: no cargo for the watcher"; exit 0
fi

watcher_bin="$here/watcher/target/debug/spike-watcher"
if ! cargo build --quiet --offline --manifest-path "$here/watcher/Cargo.toml" --target-dir "$here/watcher/target" 2>"$SPIKE_OUT/s3-build.err" \
   && ! cargo build --quiet --manifest-path "$here/watcher/Cargo.toml" --target-dir "$here/watcher/target" 2>>"$SPIKE_OUT/s3-build.err"; then
  echo "not run: the watcher did not build: $(head -c 300 "$SPIKE_OUT/s3-build.err")"
  num s3.status "not run: watcher build failed"; exit 0
fi

work="$(spike_mktemp)"
watcher_pid=""
cleanup() {
  [ -z "$watcher_pid" ] || kill "$watcher_pid" 2>/dev/null || true
  spike_cleanup_dir "$work"
}
trap cleanup EXIT
export GIT_TERMINAL_PROMPT=0

"$here/../s1/make-fixture.sh" "$work/fx" >/dev/null
clone="$work/clone"
gc() { git -C "$clone" -c user.name=spike -c user.email=spike@example.invalid "$@"; }

git clone -q --filter=blob:none --sparse "file://$work/fx/fixture.git" "$clone"
events="$work/events.tsv"; ready="$work/ready"; stop="$work/stop"; phases="$work/phases.txt"
: > "$events"; : > "$phases"
"$watcher_bin" "$clone" "$events" "$ready" "$stop" &
watcher_pid=$!
for _ in $(seq 1 50); do [ -e "$ready" ] && break; sleep 0.1; done
[ -e "$ready" ] || { echo "not run: the watcher never became ready"; num s3.status "not run: watcher not ready"; exit 0; }
num s3.watcher "notify 6.1.1 recursive on the clone root, no ignore filter"

blobs() { gc cat-file --batch-all-objects --batch-check='%(objecttype)' 2>/dev/null | grep -c '^blob$' || true; }
packs() { find "$clone/.git/objects/pack" -name '*.pack' | wc -l | tr -d ' '; }
wt_files() { find "$clone" -path "$clone/.git" -prune -o -type f -print | wc -l | tr -d ' '; }

# Wait until the event log has not grown for 0.8 s, at most 8 s.
settle() {
  local last=-1 cur same=0 i
  for i in $(seq 1 80); do
    cur="$(wc -c < "$events" | tr -d ' ')"
    if [ "$cur" = "$last" ]; then same=$((same + 1)); else same=0; fi
    last="$cur"
    [ "$same" -ge 8 ] && return 0
    sleep 0.1
  done
}

# collapse object ids and temp names so the transcript stays readable
tidy() {
  sed -E 's#objects/[0-9a-f]{2}/[0-9a-f]+#objects/<xx>/<oid>#; s#pack-[0-9a-f]+#pack-<id>#g; s#tmp_(pack|idx|obj|rev)_[A-Za-z0-9]+#tmp_\1_<rand>#g'
}

phase() { # phase <id> <description> <command...>
  local id="$1" desc="$2" start end
  shift 2
  echo "-- phase $id: $desc"
  settle
  start="$(now_ms)"
  last_rc=0
  "$@" > "$work/phase.out" 2>&1 || last_rc=$?
  settle
  end="$(now_ms)"
  awk -F'\t' -v s="$start" -v e="$end" '$1 >= s && $1 <= e' "$events" > "$work/phase.ev"
  local total git_n wt_n
  total="$(wc -l < "$work/phase.ev" | tr -d ' ')"
  git_n="$(awk -F'\t' '$3 ~ /^\.git(\/|$)/' "$work/phase.ev" | wc -l | tr -d ' ')"
  wt_n=$((total - git_n))
  num "s3.$id.events_total" "$total"
  num "s3.$id.events_in_dot_git" "$git_n"
  num "s3.$id.events_in_working_tree" "$wt_n"
  num "s3.$id.working_tree_files" "$(wt_files)"
  num "s3.$id.git_kb" "$(dir_kb "$clone/.git")"
  num "s3.$id.git_blob_objects" "$(blobs)"
  num "s3.$id.git_packs" "$(packs)"
  echo "  working-tree events (kind, path, count; first 12):"
  awk -F'\t' '$3 !~ /^\.git(\/|$)/ { print $2 "\t" $3 }' "$work/phase.ev" | sort | uniq -c | sort -rn | head -12 | sed 's/^/    /'
  echo "  .git events (collapsed paths, top 8):"
  awk -F'\t' '$3 ~ /^\.git(\/|$)/ { print $3 }' "$work/phase.ev" | tidy | sort | uniq -c | sort -rn | head -8 | sed 's/^/    /'
}

num s3.initial.working_tree_files "$(wt_files)"
num s3.initial.git_blob_objects "$(blobs)"
num s3.initial.sparse_patterns "$(gc sparse-checkout list | tr '\n' ' ' | sed 's/ $//')"

phase A "git sparse-checkout set docs" gc sparse-checkout set docs

advance() { "$here/../s1/make-fixture.sh" "$work/fx" advance; gc fetch -q origin; }
phase B "fixture moves to its second commit, then git fetch" advance

phase C "git sparse-checkout add src" gc sparse-checkout add src

# D: a read of a file outside the set. `notes/` is not in the set, so the
# file is not in the working tree; the read goes through git.
num s3.outside_file_on_disk_before_read "$([ -e "$clone/notes/note-01.md" ] && echo yes || echo no)"
blobs_before_read="$(blobs)"; packs_before_read="$(packs)"; kb_before_read="$(dir_kb "$clone/.git")"
read_outside() { gc cat-file -p origin/main:notes/note-01.md > /dev/null; }
phase D "read notes/note-01.md (outside the set) through git" read_outside
num s3.read_outside.exit_code "$last_rc"
num s3.read_outside.promisor_blobs_added "$(( $(blobs) - blobs_before_read ))"
num s3.read_outside.promisor_packs_added "$(( $(packs) - packs_before_read ))"
num s3.read_outside.git_kb_growth "$(( $(dir_kb "$clone/.git") - kb_before_read ))"
num s3.outside_file_on_disk_after_read "$([ -e "$clone/notes/note-01.md" ] && echo yes || echo no)"

# E: protect checks.
protect_setup() {
  echo "edited by hand" >> "$clone/docs/doc-02.md"
  echo "committed locally" >> "$clone/docs/doc-03.md"
  gc commit -q -m "local commit, never sent anywhere" -- docs/doc-03.md
}
phase E1 "working-tree edit to docs/doc-02.md and a local commit on docs/doc-03.md" protect_setup
num s3.E.status_changes_before_narrowing "$(gc status --porcelain | wc -l | tr -d ' ')"
num s3.E.local_commits_ahead_of_origin "$(gc rev-list --count origin/main..HEAD)"

narrow() { gc sparse-checkout set src; }
phase E2 "git sparse-checkout set src (docs/ leaves the set)" narrow
cp "$work/phase.out" "$work/narrow.out"
echo "  git said: $(tr '\n' ' ' < "$work/narrow.out" | head -c 400)"
exists() { [ -e "$clone/$1" ] && echo present || echo absent; }
num s3.E.after_narrowing.docs_doc_02_dirty_file "$(exists docs/doc-02.md)"
num s3.E.after_narrowing.docs_doc_03_unpushed_commit_file "$(exists docs/doc-03.md)"
num s3.E.after_narrowing.docs_doc_04_clean_file "$(exists docs/doc-04.md)"
num s3.E.after_narrowing.status_changes "$(gc status --porcelain | wc -l | tr -d ' ')"
num s3.E.after_narrowing.local_commits_ahead_of_origin "$(gc rev-list --count origin/main..HEAD)"

by_hand() { rm "$clone/src/mod-05.txt"; }
phase E3 "remove src/mod-05.txt by hand (for comparison)" by_hand
num s3.E.after_hand_removal.status_changes "$(gc status --porcelain | wc -l | tr -d ' ')"
num s3.E.after_hand_removal.status_line "$(gc status --porcelain | grep mod-05 | tr -s ' ' '_' | head -1 || true)"

touch "$stop"
wait "$watcher_pid" 2>/dev/null || true
watcher_pid=""
cp "$events" "$SPIKE_OUT/s3-events.tsv"
echo "  raw event log: $(wc -l < "$SPIKE_OUT/s3-events.tsv" | tr -d ' ') events, saved to spikes/out/s3-events.tsv"
echo "  done: S3"
