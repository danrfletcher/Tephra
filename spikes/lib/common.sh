#!/usr/bin/env bash
# Shared helpers for the spike scripts. Source this file; do not run it.
#
# Rules every spike script follows (they are checked by
# crates/tephrad/tests/spike_scripts_fence.rs):
#   - git runs only inside a throwaway directory made with mktemp, and only
#     read-side git commands run against a remote.
#   - A token is read from the Keychain with `security ... -w` and goes
#     straight into an HTTP header. It never appears on a command line, in a
#     file or in output.

SPIKES_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SPIKE_OUT="${TEPHRA_SPIKE_OUT:-$SPIKES_DIR/out}"
mkdir -p "$SPIKE_OUT"

# Numbers file: one `key=value` per line. Callers pick the file by setting
# NUMBERS before they call `num`.
NUMBERS="${NUMBERS:-$SPIKE_OUT/numbers.txt}"

num() {
  printf '%s=%s\n' "$1" "$2" >> "$NUMBERS"
  printf '  %-46s %s\n' "$1" "$2"
}

now_ms() {
  perl -MTime::HiRes=time -e 'printf "%d\n", time()*1000'
}

# Size of a directory in KiB, as `du -sk` reports it (block-granular).
dir_kb() {
  du -sk "$1" | cut -f1
}

# Sum of the file sizes under a directory, in bytes (exact, not block-granular).
dir_bytes() {
  find "$1" -type f -exec wc -c {} + | awk '$2 != "total" { s += $1 } END { print s + 0 }'
}

# Echo "ok" when `git` exists and is at least 2.25 (sparse-checkout).
git_ok_for_sparse() {
  command -v git >/dev/null 2>&1 || return 1
  local v major minor
  v="$(git --version | sed -E 's/^git version ([0-9]+)\.([0-9]+).*/\1 \2/')"
  major="${v% *}"
  minor="${v#* }"
  [ "$major" -gt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -ge 25 ]; }
}

# Read the secret stored under Keychain service $1 on stdout. Exit 1 when the
# item is missing, there is no `security` tool (Linux), or
# TEPHRA_SPIKE_NO_KEYCHAIN=1 (tests). The secret is never echoed by this
# function except to the caller's stdout capture.
spike_credential() {
  [ "${TEPHRA_SPIKE_NO_KEYCHAIN:-0}" = "1" ] && return 1
  command -v security >/dev/null 2>&1 || return 1
  security find-generic-password -s "$1" -w 2>/dev/null
}

# True when the Keychain item exists. Never prints the secret.
spike_credential_present() {
  [ "${TEPHRA_SPIKE_NO_KEYCHAIN:-0}" = "1" ] && return 1
  command -v security >/dev/null 2>&1 || return 1
  security find-generic-password -s "$1" >/dev/null 2>&1
}

# Make a throwaway working directory under the scratch dir and print it. The
# caller removes it with `spike_cleanup_dir`, which refuses to remove
# anything that was not made here.
spike_mktemp() {
  mktemp -d "${TMPDIR:-/tmp}/tephra-spike.XXXXXX"
}

spike_cleanup_dir() {
  case "$1" in
    */tephra-spike.*) [ -d "$1" ] && rm -rf "$1" ;;
  esac
  return 0
}

# Strip the user's home directory from a transcript.
spike_redact() {
  sed "s#${HOME}#~#g"
}

# S1: forget the rename lists and comparison files of an earlier run, so a run
# that stops early can never be compared against, or report, old results.
spike_clear_rename_lists() {
  rm -f "$SPIKE_OUT/$1-git-renames.txt" "$SPIKE_OUT/$1-git-renames.txt.sorted" \
        "$SPIKE_OUT/$1-rest-renames.txt" "$SPIKE_OUT/$1-rest-renames.txt.sorted" \
        "$SPIKE_OUT/$1-rename-disagreements.txt"
}

# S1: compare the git rename list with the REST one and record
# `<label>.rename_disagreements`. Both lists are written only by a half that
# finished in this run (see spike_clear_rename_lists), so the comparison runs
# only when both exist. An empty list is a real answer (no renames) and is
# compared; a missing list is not: that half did not finish (rate limit, no
# network, failed clone) and no number is invented for it.
spike_compare_renames() {
  local label="$1"
  local git_list="$SPIKE_OUT/$label-git-renames.txt"
  local rest_list="$SPIKE_OUT/$label-rest-renames.txt"
  local out="$SPIKE_OUT/$label-rename-disagreements.txt"
  if [ ! -e "$git_list" ] || [ ! -e "$rest_list" ]; then
    local missing="REST"
    [ -e "$git_list" ] || missing="git"
    [ -e "$git_list" ] || [ -e "$rest_list" ] || missing="git and REST"
    echo "not run: rename comparison needs both halves, the $missing half did not finish"
    num "$label.rename_disagreements" "not run: the $missing half did not finish"
    return 0
  fi
  LC_ALL=C sort "$git_list" > "$git_list.sorted"
  LC_ALL=C sort "$rest_list" > "$rest_list.sorted"
  if diff "$git_list.sorted" "$rest_list.sorted" > "$out"; then
    num "$label.rename_disagreements" 0
  else
    num "$label.rename_disagreements" "$(grep -c '^[<>]' "$out")"
    echo "  disagreements (< git, > REST):"
    sed 's/^/    /' "$out"
  fi
}
