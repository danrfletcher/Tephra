#!/usr/bin/env bash
# S1, git half: list, detect changes and read text through a blobless clone,
# and measure what lands in .git at each step.
#
#   git-half.sh <label> <url> <head-ref> <base-ref> [-- <advance command...>]
#
#   label        prefix for the keys written to $NUMBERS
#   url          clone source (a file:// URL for the fixture, https:// for a repo)
#   head-ref     ref or sha to list and read (e.g. origin/main)
#   base-ref     ref or sha to diff against; "" means "the tip the clone had
#                before the fetch step"
#   advance cmd  optional; runs between the clone and the fetch so the fetch
#                has something to bring (the fixture moves to its second commit)
#
# Everything runs in a throwaway clone made with mktemp. Only `clone`,
# `fetch`, `ls-tree`, `diff` and `cat-file` run, and none of them change the
# remote.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../lib/common.sh
. "$here/../lib/common.sh"

label="${1:?label}"; url="${2:?url}"; head_ref="${3:?head-ref}"; base_ref="${4-}"
shift 4 || true
if [ "${1-}" = "--" ]; then shift; fi

if ! command -v git >/dev/null 2>&1; then
  echo "not run: git is not installed"
  exit 0
fi

work="$(spike_mktemp)"
trap 'spike_cleanup_dir "$work"' EXIT
clone="$work/clone"
export GIT_TERMINAL_PROMPT=0

packs() { find "$clone/.git/objects/pack" -name '*.pack' 2>/dev/null | wc -l | tr -d ' '; }
blobs() { git -C "$clone" cat-file --batch-all-objects --batch-check='%(objecttype)' 2>/dev/null | grep -c '^blob$' || true; }
snap() { # snap <step>: record .git size, packs and blob objects physically present
  num "$label.$1.git_kb" "$(dir_kb "$clone/.git")"
  num "$label.$1.git_bytes" "$(dir_bytes "$clone/.git")"
  num "$label.$1.git_packs" "$(packs)"
  num "$label.$1.git_blob_objects" "$(blobs)"
}

echo "== S1 git half: $label ($url)"
echo "  git: $(git --version)"
num "$label.git_version" "$(git --version | sed 's/^git version //')"

# Before the clone there is no .git at all.
num "$label.before_clone.git_kb" 0

t0="$(now_ms)"
if ! git clone -q --filter=blob:none --no-checkout "$url" "$clone" 2>"$work/clone.err"; then
  echo "not run: clone of $url failed: $(head -c 300 "$work/clone.err")"
  exit 0
fi
num "$label.clone_ms" "$(( $(now_ms) - t0 ))"
snap after_blobless_clone

pre_fetch_tip="$(git -C "$clone" rev-parse origin/HEAD 2>/dev/null || git -C "$clone" rev-parse HEAD)"

if [ "$#" -gt 0 ]; then "$@" >/dev/null; fi

t0="$(now_ms)"
git -C "$clone" fetch -q origin
num "$label.fetch_ms" "$(( $(now_ms) - t0 ))"
snap after_fetch

base="${base_ref:-$pre_fetch_tip}"
head="$(git -C "$clone" rev-parse "$head_ref")"
num "$label.base" "${base:0:12}"
num "$label.head" "${head:0:12}"

# Listing: every path at the head commit, no content.
t0="$(now_ms)"
git -C "$clone" ls-tree -r "$head" > "$work/tree.txt"
num "$label.ls_tree_ms" "$(( $(now_ms) - t0 ))"
num "$label.ls_tree_entries" "$(wc -l < "$work/tree.txt" | tr -d ' ')"
num "$label.ls_tree_output_bytes" "$(wc -c < "$work/tree.txt" | tr -d ' ')"
snap after_ls_tree

# Change detection with rename detection. An identical-content rename needs
# no blob; a rename with an edit needs both blobs, so git fetches them from
# the promisor remote into .git.
t0="$(now_ms)"
git -C "$clone" diff -M --name-status "$base" "$head" > "$work/diff.txt"
num "$label.diff_m_ms" "$(( $(now_ms) - t0 ))"
num "$label.diff_m_lines" "$(wc -l < "$work/diff.txt" | tr -d ' ')"
snap after_diff_m
cp "$work/diff.txt" "$SPIKE_OUT/$label-git-diff.txt"
# Normalised rename list: "renamed <old> <new>", sorted, for the REST comparison.
awk -F'\t' '$1 ~ /^R/ { print "renamed " $2 " " $3 }' "$work/diff.txt" | LC_ALL=C sort > "$SPIKE_OUT/$label-git-renames.txt"
num "$label.rename_count" "$(wc -l < "$SPIKE_OUT/$label-git-renames.txt" | tr -d ' ')"
num "$label.rename_pure_found" "$(awk -F'\t' '$1 == "R100" { n++ } END { print n + 0 }' "$work/diff.txt")"
num "$label.rename_edited_found" "$(awk -F'\t' '$1 ~ /^R/ && $1 != "R100" { n++ } END { print n + 0 }' "$work/diff.txt")"

# First-index read: every text file at the head, through one cat-file batch.
text_re='\.(md|markdown|txt|rst|rs|py|js|ts|tsx|jsx|json|ya?ml|toml|html|css|sh|c|h|cpp|go|java|swift|lua|cfg|ini|lock)$'
awk -F'\t' '{ split($1, m, " "); print m[3] "\t" $2 }' "$work/tree.txt" | grep -E "$text_re" > "$work/text.txt" || true
num "$label.text_files" "$(wc -l < "$work/text.txt" | tr -d ' ')"
blobs_before_index="$(blobs)"
t0="$(now_ms)"
cut -f1 "$work/text.txt" | git -C "$clone" cat-file --batch > "$work/content.bin"
num "$label.index_read_ms" "$(( $(now_ms) - t0 ))"
num "$label.index_read_content_bytes" "$(wc -c < "$work/content.bin" | tr -d ' ')"
snap after_index_read
num "$label.blobs_landed_by_index_read" "$(( $(blobs) - blobs_before_index ))"

echo "  done: $label"
