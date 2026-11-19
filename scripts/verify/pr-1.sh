#!/usr/bin/env bash
# PR-1 (E26) container verification: `tephrad` indexes and refactors file
# names whose multi-byte characters sit close to the extension.
#
# Builds the workspace, creates a source holding the crash-class names
# (`Résumé—v2.docx`, `Todo…ab`, `日本x`, `Plan…ab.md`), starts a throwaway
# tephrad (own XDG config/data dir, a free loopback port it picks itself,
# never the default 4812), registers the source with tephractl, and saves
# each transcript to verify/pr-1/*.txt (override the folder with OUT_DIR):
#
#   01-register-and-scan.txt  `source add`, then `source list` once the scan
#                             finished; ends with "exit: 0"
#   02-preview.txt            `link refactor ... --json`: a preview, exit 0
#   03-apply.txt              `link refactor ... --apply --yes`: rewrites the
#                             linking file (the file's new text is shown)
#   04-daemon-log.txt         the daemon's log; the script fails on "panicked"
#
# Every transcript ends with an "exit: <code>" line. The script starts one
# process (tephrad), records its PID and stops only that PID on exit.
#
# Before-the-fix run: PR1_TREE=<checkout of the unfixed code> runs the same
# steps against that tree (build, daemon and tephractl all from it). The
# source cannot be registered there, so checkpoint 01 fails: the script
# writes that transcript to 05-before-fix.txt (not 01) and exits non-zero
# with "checkpoint 01 failed", which is the expected result on unfixed code.
set -euo pipefail

SCRIPT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ROOT="${PR1_TREE:-$SCRIPT_ROOT}"
OUT_DIR="${OUT_DIR:-$SCRIPT_ROOT/verify/pr-1}"
BEFORE_FIX=""
[ -n "${PR1_TREE:-}" ] && BEFORE_FIX=1

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr1-verify.XXXXXX")"
DAEMON_PID=""

cleanup() {
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "pr-1.sh: FAIL: $*" >&2
  exit 1
}

echo "== build ($ROOT)"
(cd "$ROOT" && cargo build --workspace)
TEPHRAD="$ROOT/target/debug/tephrad"
TEPHRACTL="$ROOT/target/debug/tephractl"

# A free loopback port: bind it once to prove nothing holds it.
pick_port() {
  python3 - <<'PY'
import random, socket
for _ in range(200):
    port = random.randint(20000, 60000)
    s = socket.socket()
    try:
        s.bind(("127.0.0.1", port))
    except OSError:
        continue
    finally:
        s.close()
    print(port)
    break
else:
    raise SystemExit("no free port found")
PY
}
PORT="$(pick_port)"
ADDR="127.0.0.1:$PORT"

# Own XDG dirs: the daemon, its config, token and cache never touch Dan's.
export XDG_CONFIG_HOME="$WORK/xdg-config"
export XDG_DATA_HOME="$WORK/xdg-data"
mkdir -p "$XDG_CONFIG_HOME/tephra" "$XDG_DATA_HOME/tephra"
cat >"$XDG_CONFIG_HOME/tephra/config.toml" <<EOF
listen_addr = "$ADDR"
data_dir = "$XDG_DATA_HOME/tephra"
log_level = "info"
EOF

# The source: every crash-class name, and a note that links to each.
SRC="$WORK/notes-root"
mkdir -p "$SRC/dé/日本"
cp "$ROOT/crates/tephra-parser/tests/fixtures/documents/headings.docx" "$SRC/Résumé—v2.docx"
printf '# Todo\n' >"$SRC/Todo…ab"
printf '# Nihon\n' >"$SRC/日本x"
printf '# Plan\n' >"$SRC/Plan…ab.md"
printf '# Nested\n' >"$SRC/dé/日本/Notes—.txt"
printf '# Notes\n' >"$SRC/Notes😀ab.txt"
printf '# Linker\n\nCV: [[Résumé—v2.docx]]\nTodo: [[Todo…ab]] and [[日本x]]\nPlan: [[Plan…ab|the plan]]\n' \
  >"$SRC/Linker—.md"

echo "== start tephrad on $ADDR"
"$TEPHRAD" --config "$XDG_CONFIG_HOME/tephra/config.toml" >"$WORK/tephrad.log" 2>&1 &
DAEMON_PID=$!
echo "tephrad pid $DAEMON_PID"

ctl() { "$TEPHRACTL" --addr "$ADDR" "$@"; }

# Wait (bounded, ~20 s) for the daemon to answer.
ready=""
for _ in $(seq 1 100); do
  kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$WORK/tephrad.log" >&2; fail "tephrad exited early"; }
  if ctl source list >/dev/null 2>&1; then ready=1; break; fi
  sleep 0.2
done
[ -n "$ready" ] || fail "tephrad did not answer on $ADDR"

mkdir -p "$OUT_DIR"
if [ -n "$BEFORE_FIX" ]; then rm -f "$OUT_DIR/05-before-fix.txt"; else rm -f "$OUT_DIR"/0[1-4]*.txt; fi

# -- 01: register and scan ----------------------------------------------------
if [ -n "$BEFORE_FIX" ]; then T01="$OUT_DIR/05-before-fix.txt"; else T01="$OUT_DIR/01-register-and-scan.txt"; fi
code=0
{
  echo '$ tephractl source add notes <source with multi-byte names>'
  out="$(ctl source add notes "$SRC" 2>&1)" || code=$?
  printf '%s\n' "$out"
  if [ "$code" -eq 0 ]; then
    # `source add` returns once the scan is queued or done; wait (bounded,
    # ~20 s) for the source to be active with a finished scan.
    scanned=""
    for _ in $(seq 1 100); do
      if ctl source list --json 2>/dev/null | python3 -c '
import json, sys
rows = json.load(sys.stdin)
row = next((r for r in rows if r["name"] == "notes"), None)
sys.exit(0 if row and row["status"] == "active" and row["last_scanned_at"] is not None else 1)
'; then scanned=1; break; fi
      sleep 0.2
    done
    echo '$ tephractl source list'
    ctl source list 2>&1 || true
    if [ -z "$scanned" ]; then code=1; echo "scan did not finish"; fi
  fi
  printf 'exit: %s\n' "$code"
} >"$T01"
cat "$T01"
if [ "$code" -ne 0 ]; then
  if grep -q 'panicked' "$WORK/tephrad.log"; then
    echo "-- daemon log, panic lines:" | tee -a "$T01"
    grep -A1 'panicked' "$WORK/tephrad.log" | tee -a "$T01"
  fi
  fail "checkpoint 01 failed: the source with multi-byte names did not register and scan (transcript: $T01)"
fi
[ -z "$BEFORE_FIX" ] || fail "the unfixed tree registered the source: PR1_TREE does not hold the bug"

# -- 02/03: preview and apply -------------------------------------------------
transcript() {
  local file="$1"
  shift
  {
    echo "\$ tephractl $*"
    local code=0
    local out
    out="$(ctl "$@" 2>&1)" || code=$?
    printf '%s\n' "$out"
    printf 'exit: %s\n' "$code"
  } >"$OUT_DIR/$file"
}

transcript 02-preview.txt link refactor --source notes --old "Résumé—v2.docx" --new "Résumé—v3.docx" --json
transcript 03-apply.txt link refactor --source notes --old "Résumé—v2.docx" --new "Résumé—v3.docx" --apply --yes
{
  echo '$ cat Linker—.md'
  cat "$SRC/Linker—.md"
} >>"$OUT_DIR/03-apply.txt"

# The bare, extensionless and Markdown targets too. `link refactor --apply`
# needs a preview of the same rename first (F2), so each gets one.
transcript 03-preview-bare.txt link refactor --source notes --old "Todo…ab" --new "Todo…ac" --json
transcript 03-apply-bare.txt link refactor --source notes --old "Todo…ab" --new "Todo…ac" --apply --yes
transcript 03-preview-md.txt link refactor --source notes --old "Plan…ab.md" --new "Plan…ac.md" --json
transcript 03-apply-md.txt link refactor --source notes --old "Plan…ab.md" --new "Plan…ac.md" --apply --yes

cp "$WORK/tephrad.log" "$OUT_DIR/04-daemon-log.txt"

echo "== checks"
grep -q '^exit: 0$' "$OUT_DIR/01-register-and-scan.txt" || fail "01 did not exit 0"
grep -q ' active ' "$OUT_DIR/01-register-and-scan.txt" || fail "01 does not show the scan finished"
grep -q '^exit: 0$' "$OUT_DIR/02-preview.txt" || fail "02 did not exit 0"
grep -qF 'Résumé—v3.docx' "$OUT_DIR/02-preview.txt" || fail "02 does not show the new name"
for f in 03-apply 03-apply-bare 03-apply-md; do
  grep -q '^exit: 0$' "$OUT_DIR/$f.txt" || fail "$f did not exit 0"
done
grep -qF '[[Résumé—v3.docx]]' "$SRC/Linker—.md" || fail "03: the linking file was not rewritten"
grep -qF '[[Todo…ac]]' "$SRC/Linker—.md" || fail "03: [[Todo…ab]] was not rewritten"
grep -qF '[[Plan…ac|the plan]]' "$SRC/Linker—.md" || fail "03: [[Plan…ab|the plan]] was not rewritten"
grep -qF '[[日本x]]' "$SRC/Linker—.md" || fail "03: an unrelated link changed"
if grep -q 'panicked' "$OUT_DIR/04-daemon-log.txt"; then fail "the daemon log holds a panic"; fi
kill -0 "$DAEMON_PID" 2>/dev/null || fail "tephrad is no longer running"

echo "pr-1.sh: OK (transcripts in $OUT_DIR)"
