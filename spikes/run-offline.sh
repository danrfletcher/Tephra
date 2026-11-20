#!/usr/bin/env bash
# Offline spike run: S1 git half and S3 on a generated fixture. No network,
# no credentials. This is what `cargo test` runs (crates/tephrad/tests/
# spike_offline_run.rs); live REST, S2, S4 and S5 are in run-live.sh.
#
#   spikes/run-offline.sh
#
# Output: $TEPHRA_SPIKE_OUT (default spikes/out/) holds numbers.txt, one
# key=value per line. With TEPHRA_SPIKE_TRANSCRIPTS=<dir> each spike's
# terminal output is also saved as <dir>/S1.txt and <dir>/S3.txt.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
. "$here/lib/common.sh"

: > "$NUMBERS"
transcripts="${TEPHRA_SPIKE_TRANSCRIPTS:-}"
[ -z "$transcripts" ] || mkdir -p "$transcripts"

# Run "$@" and, when transcripts are wanted, save a redacted copy of its output.
step() {
  local id="$1"; shift
  if [ -n "$transcripts" ]; then
    "$@" 2>&1 | tee >(spike_redact > "$transcripts/$id.txt")
  else
    "$@" 2>&1
  fi
}

if ! command -v git >/dev/null 2>&1; then
  echo "not run: git is not installed (S1 git half and S3 need it)"
  num offline.status "not run: git is not installed"
  exit 0
fi
echo "git: $(git --version)"
num offline.git_version "$(git --version | sed 's/^git version //')"

s1() {
  local dir fixture
  dir="$(spike_mktemp)"
  "$here/s1/make-fixture.sh" "$dir"
  num s1.fixture.files "$(git -C "$dir/work" ls-tree -r --name-only fixture-c1 | wc -l | tr -d ' ')"
  fixture="file://$dir/fixture.git"
  "$here/s1/git-half.sh" s1.fixture "$fixture" origin/main "" -- "$here/s1/make-fixture.sh" "$dir" advance
  spike_cleanup_dir "$dir"
}
step S1 s1

if git_ok_for_sparse; then
  step S3 "$here/s3/run.sh"
else
  echo "not run: S3 needs git 2.25 or newer for sparse-checkout ($(git --version))"
  num s3.status "not run: git older than 2.25"
  if [ -n "$transcripts" ]; then
    echo "not run: S3 needs git 2.25 or newer for sparse-checkout ($(git --version))" > "$transcripts/S3.txt"
  fi
fi

echo "numbers: $NUMBERS"
