#!/usr/bin/env bash
# PR-7 container verification: `tephractl fetch` (unit.fetch) downloads one
# file of a GitHub source into Tephra's cache, atomically, once (G10, GP5,
# E6, E7, E27, F12).
#
# Builds the workspace, starts a throwaway tephrad (own XDG config/data dir, a
# free loopback port it picks itself, never the default 4812) against the PR-3
# fake GitHub (`tephra-remote/examples/poll_fake`, loopback only, driven
# through a pipe), and saves each transcript to verify/pr-7/*.txt (override the
# folder with OUT_DIR):
#
#   01-fetch.txt       README.md is `remote`; a fetch on the slow route runs in
#                      the background while a second `ls` shows `downloading`;
#                      it prints its path, the file is there, byte for byte
#                      (`cmp`), and it is `cached`
#   02-second.txt      a second fetch prints the same path, keeps the inode and
#                      the mtime, and the server saw one content request
#   03-cut-off.txt     a download cut part way: exit 1 with a reason, no file
#                      and no temp file, `remote` with the reason in `ls`; a
#                      retry works
#   04-concurrent.txt  twenty fetches at once: one content request
#   05-not-fetchable.txt a Google Doc and a Sheet (seeded Drive items): exit
#                      1, `not_fetchable` and the URL, nothing written, no
#                      request to the server
#   06-nothing-else.txt no file but the one asked for; no write request
#
# Every transcript ends with an "exit: <code>" line. The script exits non-zero
# if any result is wrong. It starts two processes (tephrad and the fake),
# records their PIDs and stops only those PIDs on exit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/pr-7}"
# Waiting for a poll: one interval plus the time a poll takes.
POLL_WAIT="${POLL_WAIT:-100}"
# A made-up sign-in: the fake accepts any token. It must appear nowhere.
TOKEN="ghp_verifyScriptSentinelNotARealToken0000"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/remote-pr7-verify.XXXXXX")"
DAEMON_PID=""
FAKE_PID=""

cleanup() {
  exec 7>&- 2>/dev/null || true
  for pid in "$DAEMON_PID" "$FAKE_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "pr-7.sh: FAIL: $*" >&2
  exit 1
}

echo "== build"
FEATURES="tephrad/test-credential-store,tephractl/test-credential-store,tephra-remote/test-support"
(cd "$ROOT" && cargo build -p tephrad -p tephractl -p tephra-remote --features "$FEATURES" --bins --example poll_fake)
# Copies, so a `cargo test` run in the same checkout while this script waits
# cannot replace the daemon (it would be built without the test credential
# store) before the restart of step 05.
mkdir -p "$WORK/bin"
cp "$ROOT/target/debug/tephrad" "$ROOT/target/debug/tephractl" "$ROOT/target/debug/examples/poll_fake" "$WORK/bin/"
TEPHRAD="$WORK/bin/tephrad"
TEPHRACTL="$WORK/bin/tephractl"
FAKE="$WORK/bin/poll_fake"

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
[ "$PORT" != "4812" ] || fail "picked the product's default port"
ADDR="127.0.0.1:$PORT"

# ---- the fake, steered through a pipe ---------------------------------------

mkfifo "$WORK/fake.in"
"$FAKE" "$WORK/url" "$WORK/requests.log" <"$WORK/fake.in" >"$WORK/fake.out" 2>"$WORK/fake.err" &
FAKE_PID=$!
exec 7>"$WORK/fake.in"
echo "fake pid $FAKE_PID"
for _ in $(seq 1 100); do
  [ -s "$WORK/url" ] && break
  kill -0 "$FAKE_PID" 2>/dev/null || { cat "$WORK/fake.err" >&2; fail "the fake exited early"; }
  sleep 0.1
done
[ -s "$WORK/url" ] || fail "the fake never wrote its URL"
GITHUB_URL="$(cat "$WORK/url")"
case "$GITHUB_URL" in
  http://127.0.0.1:*) ;;
  *) fail "the fake is not on loopback: $GITHUB_URL" ;;
esac

# fake <command...>: one command, and wait (bounded) for its answer line.
fake() {
  local before after
  before="$(wc -l <"$WORK/fake.out")"
  echo "$*" >&7
  for _ in $(seq 1 100); do
    after="$(wc -l <"$WORK/fake.out")"
    [ "$after" -gt "$before" ] && break
    sleep 0.1
  done
  [ "$after" -gt "$before" ] || fail "the fake did not answer: $*"
  tail -n 1 "$WORK/fake.out" | grep -q '^ok ' || fail "the fake refused: $(tail -n 1 "$WORK/fake.out")"
}

# ---- tephrad ----------------------------------------------------------------

export XDG_CONFIG_HOME="$WORK/xdg-config"
export XDG_DATA_HOME="$WORK/xdg-data"
mkdir -p "$XDG_CONFIG_HOME/tephra" "$XDG_DATA_HOME/tephra"
cat >"$XDG_CONFIG_HOME/tephra/config.toml" <<EOF
listen_addr = "$ADDR"
data_dir = "$XDG_DATA_HOME/tephra"
log_level = "info"
github_api_url = "$GITHUB_URL"
test_credential_file = "credentials.json"
EOF

ctl() { "$TEPHRACTL" --addr "$ADDR" "$@"; }

start_daemon() {
  "$TEPHRAD" --config "$XDG_CONFIG_HOME/tephra/config.toml" >>"$WORK/tephrad.log" 2>&1 &
  DAEMON_PID=$!
  echo "tephrad pid $DAEMON_PID"
  local ready=""
  for _ in $(seq 1 100); do
    kill -0 "$DAEMON_PID" 2>/dev/null || { cat "$WORK/tephrad.log" >&2; fail "tephrad exited early"; }
    if ctl source list >/dev/null 2>&1; then ready=1; break; fi
    sleep 0.2
  done
  [ -n "$ready" ] || fail "tephrad did not answer on $ADDR"
}

stop_daemon() {
  kill "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
  DAEMON_PID=""
}

echo "== start tephrad on $ADDR"
start_daemon
# `auth` commands take no --addr: they read the same config.toml.
printf '%s\n' "$TOKEN" | "$TEPHRACTL" auth github - >"$WORK/auth.out" 2>&1 || { cat "$WORK/auth.out" >&2; fail "sign-in failed"; }

mkdir -p "$OUT_DIR"
rm -f "$OUT_DIR"/*.txt

# transcript <file> <command words...>: the command line, its output and its
# exit code, in that order, appended to <file>.
transcript() {
  local file="$1"
  shift
  {
    local code=0
    local out
    printf '$ %s\n' "$*"
    out="$("$@" 2>&1)" || code=$?
    printf '%s\n' "$out"
    printf 'exit: %s\n' "$code"
  } >>"$OUT_DIR/$file"
  return 0
}

# until_list <seconds> <extended regexp> : wait (bounded) until `source list`
# matches.
until_list() {
  local seconds="$1" pattern="$2" i
  for i in $(seq 1 "$seconds"); do
    if ctl source list 2>/dev/null | grep -Eq "$pattern"; then return 0; fi
    sleep 1
  done
  return 1
}

# until_ls <seconds> <present|absent> <name> : wait (bounded) until `ls g -r`
# lists (or no longer lists) a file (`ls -r` prints names, not paths).
until_ls() {
  local seconds="$1" want="$2" name="$3" i
  for i in $(seq 1 "$seconds"); do
    if ctl ls g -r 2>/dev/null | grep -q "$name"; then
      [ "$want" = present ] && return 0
    else
      [ "$want" = absent ] && return 0
    fi
    sleep 1
  done
  return 1
}


# ---- helpers for this PR ----------------------------------------------------

REMOTE_DIR="$XDG_DATA_HOME/tephra/remote/g"
# the number of content requests (blob or contents) the fake has seen since the
# last `fake clear`
reads_since_clear() {
  fake dump
  grep -Ec '/git/blobs/|/contents/' "$WORK/requests.log" || true
}
state_of() { ctl ls g -r | grep -F "$1" | grep -Eo '(remote|downloading|cached)' | head -n 1; }
# inode and mtime of a file: GNU stat (Linux) takes -c; BSD stat (macOS) -f.
# (On Linux `stat -f` is the file-system status, so GNU is tried first.)
stat_of() {
  if stat -c '%i %Y' "$1" 2>/dev/null; then return 0; fi
  stat -f '%i %m' "$1"
}

# ---- 01: fetch --------------------------------------------------------------

: >"$OUT_DIR/01-fetch.txt"
transcript 01-fetch.txt ctl source add github g danrfletcher/atlas
until_list 60 '^g[[:space:]]+active' || { ctl source list >&2; fail "g did not become active"; }
sleep 2
[ ! -e "$REMOTE_DIR/clone" ] || fail "something is in clone/ before any fetch"
transcript 01-fetch.txt ctl ls g -r
[ "$(state_of README.md)" = "remote" ] || fail "README.md is not remote before the fetch: $(ctl ls g -r)"
# The fetch runs on the slow route in the background; a second `ls` while it
# is running must show README.md `downloading`.
fake latency 1500
fake clear
( code=0; ctl fetch @g:README.md >"$WORK/first.out" 2>"$WORK/first.err" || code=$?; echo "$code" >"$WORK/first.code" ) &
FETCH_PID=$!
SAW_DOWNLOADING=""
for _ in $(seq 1 40); do
  if [ "$(state_of README.md)" = "downloading" ]; then SAW_DOWNLOADING=1; break; fi
  sleep 0.05
done
[ -n "$SAW_DOWNLOADING" ] || fail "no ls showed README.md downloading while the slow fetch ran: $(ctl ls g -r)"
transcript 01-fetch.txt ctl ls g -r
wait "$FETCH_PID" || true
fake latency 0
{
  printf '$ tephractl fetch @g:README.md   (the server is slow; the ls above ran meanwhile)\n'
  cat "$WORK/first.out"
  printf 'exit: %s\n' "$(cat "$WORK/first.code")"
} >>"$OUT_DIR/01-fetch.txt"
[ "$(cat "$WORK/first.code")" = "0" ] || fail "the slow fetch failed: $(cat "$WORK/first.err")"
PATH_OUT="$(cat "$WORK/first.out")"
[ "$(wc -l <"$WORK/first.out" | tr -d ' ')" = "1" ] || fail "stdout of fetch is not exactly one line: $PATH_OUT"
EXPECTED="$REMOTE_DIR/clone/README.md"
[ "$PATH_OUT" = "$EXPECTED" ] || fail "fetch printed '$PATH_OUT', expected '$EXPECTED'"
[ -f "$EXPECTED" ] || fail "no file at $EXPECTED"
printf '# Atlas\n\nStart with [[docs/Guide.md]] and see [[Guide]].\n\n## Setup\n' >"$WORK/README.expected"
cmp "$EXPECTED" "$WORK/README.expected" || fail "the fetched README.md differs from the server's"
{ printf '$ cmp %s <server bytes>\n' "$EXPECTED"; printf 'identical\nexit: 0\n'; } >>"$OUT_DIR/01-fetch.txt"
[ "$(state_of README.md)" = "cached" ] || fail "README.md is not cached after the fetch: $(ctl ls g -r)"
transcript 01-fetch.txt ctl ls g -r
transcript 01-fetch.txt ctl source list
ctl source list | grep -Eq 'cached [1-9][0-9]* bytes' || fail "source list does not show the bytes now cached"
[ -z "$(find "$REMOTE_DIR" -name '%tmp-*')" ] || fail "a temp file is left behind"

# ---- 02: the second fetch ---------------------------------------------------

: >"$OUT_DIR/02-second.txt"
BEFORE="$(stat_of "$EXPECTED")"
fake clear
sleep 1
transcript 02-second.txt ctl fetch @g:README.md
SECOND="$(ctl fetch @g:README.md)"
[ "$SECOND" = "$EXPECTED" ] || fail "the second fetch printed '$SECOND'"
AFTER="$(stat_of "$EXPECTED")"
{ echo "inode and mtime before: $BEFORE"; echo "inode and mtime after:  $AFTER"; } >>"$OUT_DIR/02-second.txt"
[ "$BEFORE" = "$AFTER" ] || fail "the second fetch rewrote the file ($BEFORE -> $AFTER)"
N="$(reads_since_clear)"
{ echo "content requests the server saw for two more fetches: $N"; cat "$WORK/requests.log"; } >>"$OUT_DIR/02-second.txt"
[ "$N" = "0" ] || fail "a second fetch asked the server for content again ($N)"
printf 'exit: 0\n' >>"$OUT_DIR/02-second.txt"

# ---- 03: cut off ------------------------------------------------------------

: >"$OUT_DIR/03-cut-off.txt"
BIG="$REMOTE_DIR/clone/data/big.bin"
fake cut 1
CODE=0; OUT="$(ctl fetch @g:data/big.bin 2>"$WORK/cut.err")" || CODE=$?
{ echo '$ tephractl fetch @g:data/big.bin   (the server cuts the body part way)'; echo "stdout: '$OUT'"; echo "stderr: $(cat "$WORK/cut.err")"; echo "exit: $CODE"; } >>"$OUT_DIR/03-cut-off.txt"
[ "$CODE" != "0" ] || fail "a cut-off download exited 0"
[ -s "$WORK/cut.err" ] || fail "no reason on stderr for the cut-off download"
[ -z "$OUT" ] || fail "something was printed on stdout for a failure: $OUT"
[ ! -e "$BIG" ] || fail "a file is left at $BIG"
[ -z "$(find "$REMOTE_DIR" -name '%tmp-*')" ] || fail "a temp file is left behind"
transcript 03-cut-off.txt ctl ls g -r
ctl ls g -r | grep -F big.bin | grep -q remote || fail "big.bin is not remote after the failure"
transcript 03-cut-off.txt ctl fetch @g:data/big.bin
[ "$(ctl fetch @g:data/big.bin)" = "$BIG" ] || fail "the retry did not print the path"
[ "$(wc -c <"$BIG" | tr -d ' ')" = "$((6 * 1024 * 1024))" ] || fail "the retried file is the wrong size"

# ---- 04: twenty at once -----------------------------------------------------

: >"$OUT_DIR/04-concurrent.txt"
fake latency 1500
fake clear
FETCHERS=""
for i in $(seq 1 20); do
  ( code=0; ctl fetch @g:src/lib.rs >"$WORK/c$i.out" 2>"$WORK/c$i.err" || code=$?; echo "$code" >"$WORK/c$i.code" ) &
  FETCHERS="$FETCHERS $!"
done
# Wait for these twenty only: a bare `wait` would also wait for tephrad.
for pid in $FETCHERS; do wait "$pid" || true; done
fake latency 0
BAD=0
for i in $(seq 1 20); do
  [ "$(cat "$WORK/c$i.code")" = "0" ] || BAD=$((BAD + 1))
done
[ "$BAD" = "0" ] || fail "$BAD of the 20 simultaneous fetches failed"
[ "$(cat "$WORK"/c*.out | sort -u | wc -l | tr -d ' ')" = "1" ] || fail "the 20 fetches did not all print one path"
N="$(reads_since_clear)"
{ echo "20 fetches at once, all exit 0, all printed: $(head -n 1 "$WORK/c1.out")"; echo "content requests the server saw: $N"; cat "$WORK/requests.log"; printf 'exit: 0\n'; } >>"$OUT_DIR/04-concurrent.txt"
[ "$N" = "1" ] || fail "20 simultaneous fetches made $N content requests"

# ---- 05: a Google Doc -------------------------------------------------------

: >"$OUT_DIR/05-not-fetchable.txt"
# Drive sources arrive with PR-10, so a Doc is seeded into the cache database
# the way PR-10's sync will store it.
sqlite3 "$XDG_DATA_HOME/tephra/cache.db" "INSERT INTO remote_sources (name, kind, target, created_at, status) VALUES ('gd', 'drive', 'folder-id', 1795132000, 'active'); INSERT INTO remote_items (source, path, kind, file_id, size, state) VALUES ('gd', 'Plan', 'gdoc', 'doc-1', 0, 'remote'), ('gd', 'Budget', 'gsheet', 'sheet-1', 0, 'remote');" || fail "could not seed the Drive items"
fake clear
transcript 05-not-fetchable.txt ctl fetch @gd:Plan
transcript 05-not-fetchable.txt ctl fetch @gd:Budget
CODE=0; OUT="$(ctl fetch @gd:Plan 2>"$WORK/doc.err")" || CODE=$?
[ "$CODE" != "0" ] || fail "fetching a Google Doc exited 0"
[ -z "$OUT" ] || fail "something was printed on stdout for a Doc: $OUT"
grep -q 'https://docs.google.com/document/d/doc-1' "$WORK/doc.err" || fail "the Doc's URL is not in the message: $(cat "$WORK/doc.err")"
grep -q 'https://docs.google.com/spreadsheets/d/sheet-1' < <(ctl fetch @gd:Budget 2>&1 || true) || fail "the Sheet's URL is not in the message"
[ ! -e "$XDG_DATA_HOME/tephra/remote/gd" ] || [ -z "$(find "$XDG_DATA_HOME/tephra/remote/gd" -type f)" ] || fail "something was written for a Doc"
N="$(reads_since_clear)"
{ echo "requests the server saw for the Doc and the Sheet: $N"; } >>"$OUT_DIR/05-not-fetchable.txt"
[ "$N" = "0" ] || fail "a Doc fetch reached the server"
printf 'exit: 0\n' >>"$OUT_DIR/05-not-fetchable.txt"

# ---- 06: nothing else was downloaded ---------------------------------------

: >"$OUT_DIR/06-nothing-else.txt"
fake dump
{
  echo "files under $REMOTE_DIR:"
  (cd "$REMOTE_DIR" && find . -type f | LC_ALL=C sort)
} >>"$OUT_DIR/06-nothing-else.txt"
FILES="$(cd "$REMOTE_DIR" && find . -type f | LC_ALL=C sort | tr '\n' ' ')"
[ "$FILES" = "./clone/README.md ./clone/data/big.bin ./clone/src/lib.rs " ] || fail "files other than the three asked for exist: $FILES"
if grep -v '^GET ' "$WORK/requests.log" | grep -q .; then fail "a request that was not a GET reached the server"; fi
printf 'exit: 0\n' >>"$OUT_DIR/06-nothing-else.txt"

# ---- the token is nowhere ---------------------------------------------------

if grep -rq "$TOKEN" "$OUT_DIR" "$WORK/tephrad.log" "$WORK/fake.out" "$WORK/requests.log" "$WORK/auth.out" 2>/dev/null; then
  fail "the token appears in a log, a transcript or an answer"
fi
if grep -rq "$TOKEN" "$XDG_DATA_HOME/tephra/remote" 2>/dev/null; then fail "the token is in a cached file"; fi

for f in "$OUT_DIR"/*.txt; do
  tail -n 1 "$f" | grep -q '^exit: [0-9]*$' || fail "$(basename "$f") does not end with an exit line"
done

echo "== PR-7 verification passed; transcripts in $OUT_DIR"
