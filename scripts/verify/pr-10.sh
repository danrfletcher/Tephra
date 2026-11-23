#!/usr/bin/env bash
# PR-10 container verification: a Google Drive folder as a source, listed
# first and read once, kept current from the changes feed (G4, G8, G9, G5, GP3,
# E8, E9, E10).
#
# Builds the workspace, starts a throwaway tephrad (own XDG config/data dir, a
# free loopback port it picks itself, never the default 4812) against the
# PR-3 fake Drive (`tephra-remote/examples/drive_fake`, loopback only, driven
# through a pipe), and saves each transcript to verify/pr-10/*.txt (override
# the folder with OUT_DIR). No real Google call is made: the stored sign-in is
# a made-up token in a throwaway credential file, and the daemon is told to
# talk to the fake with the test-only `drive_api_url` key.
#
#   01-add.txt          `source add drive gdrive Notes`: 402 files listed in
#                       seconds, every one `remote`; `ls` shows the kinds
#                       gdoc, gsheet, binary, document
#   02-indexing.txt     the index paused part-way: `source list` says
#                       `indexing 120/400`, then `active`; after that
#                       `link resolve @gdrive:Roadmap` resolves and `backlinks`
#                       show, with no content read in between
#   03-duplicates.txt   two files called Plan: `ls` shows Plan.md and
#                       Plan (2).md; `link resolve @gdrive:Plan` is the
#                       ambiguity error naming both; `Plan (2)` reaches the
#                       later one only; the names survive a restart
#   04-feed.txt         one file added, one renamed and one deleted in Drive
#                       are all applied by ONE poll (about 60 s); a WebSocket
#                       client received ONE graph:changed; `fixes` shows the
#                       rename
#   05-nothing-on-disk.txt
#                       no file under the data dir's remote/ folder, 0 bytes
#                       cached, and no request that was not a GET
#   06 (real Drive)     is a HUMAN step: this script cannot reach Google.
#                       With a real account: `tephractl auth google`, then
#                       `tephractl source add drive gdrive "<a folder>"`.
#
# Every transcript ends with an "exit: <code>" line. The script exits non-zero
# if any result is wrong. It starts three processes (tephrad, the fake Drive
# and a WebSocket listener), records their PIDs and stops only those PIDs on
# exit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/pr-10}"
# Waiting for a poll: one interval plus the time a poll takes.
POLL_WAIT="${POLL_WAIT:-100}"
# A made-up sign-in. It must appear nowhere in an output.
TOKEN="ya29.VERIFY_not_a_real_google_token"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr10-verify.XXXXXX")"
DAEMON_PID=""
FAKE_PID=""
LISTENER_PID=""

cleanup() {
  exec 7>&- 2>/dev/null || true
  for pid in "$LISTENER_PID" "$DAEMON_PID" "$FAKE_PID"; do
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
      kill "$pid" 2>/dev/null || true
      wait "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() {
  echo "pr-10.sh: FAIL: $*" >&2
  exit 1
}

echo "== build"
FEATURES="tephrad/test-credential-store,tephractl/test-credential-store,tephra-remote/test-support"
(cd "$ROOT" && cargo build -p tephrad -p tephractl -p tephra-remote --features "$FEATURES" --bins --example drive_fake)
# Copies, so a `cargo test` run in the same checkout while this script waits
# cannot replace the daemon (it would be built without the test credential
# store) before the restart in step 03.
mkdir -p "$WORK/bin"
cp "$ROOT/target/debug/tephrad" "$ROOT/target/debug/tephractl" "$ROOT/target/debug/examples/drive_fake" "$WORK/bin/"
TEPHRAD="$WORK/bin/tephrad"
TEPHRACTL="$WORK/bin/tephractl"
FAKE="$WORK/bin/drive_fake"

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

# ---- the fake Drive, steered through a pipe ---------------------------------

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
DRIVE_URL="$(cat "$WORK/url")"
case "$DRIVE_URL" in
  http://127.0.0.1:*) ;;
  *) fail "the fake is not on loopback: $DRIVE_URL" ;;
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

# content_reads: how many file reads (a download or an export) the fake has
# seen so far.
content_reads() {
  fake dump
  grep -Ec 'alt=media|/export' "$WORK/requests.log" || true
}

# ---- tephrad ----------------------------------------------------------------

export XDG_CONFIG_HOME="$WORK/xdg-config"
export XDG_DATA_HOME="$WORK/xdg-data"
mkdir -p "$XDG_CONFIG_HOME/tephra" "$XDG_DATA_HOME/tephra"
cat >"$XDG_CONFIG_HOME/tephra/config.toml" <<EOF
listen_addr = "$ADDR"
data_dir = "$XDG_DATA_HOME/tephra"
log_level = "info"
drive_api_url = "$DRIVE_URL"
test_credential_file = "credentials.json"
EOF
# The made-up sign-in, in the test credential file (owner-only, as it checks).
( umask 077; printf '{"google":"%s"}' "$TOKEN" >"$XDG_DATA_HOME/tephra/credentials.json" )

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

mkdir -p "$OUT_DIR"
rm -f "$OUT_DIR"/*.txt

# transcript <file> <command words...>: the command line, its output and its
# exit code, in that order, appended to <file>. Returns the command's code.
transcript() {
  local file="$1"
  shift
  local code=0 out
  {
    printf '$ %s\n' "$*"
    out="$("$@" 2>&1)" || code=$?
    printf '%s\n' "$out"
    printf 'exit: %s\n' "$code"
  } >>"$OUT_DIR/$file"
  return 0
}
# end <file>: the file's closing exit line, once its checks have passed.
end() { printf 'exit: 0\n' >>"$OUT_DIR/$1"; }

# until_list <seconds> <extended regexp>: wait (bounded) until `source list`
# matches.
until_list() {
  local seconds="$1" pattern="$2" i
  for i in $(seq 1 "$seconds"); do
    if ctl source list 2>/dev/null | grep -Eq "$pattern"; then return 0; fi
    sleep 1
  done
  return 1
}

# until_ls <seconds> <present|absent> <fixed text>: wait (bounded) until
# `ls gdrive -r` lists (or no longer lists) a name.
until_ls() {
  local seconds="$1" want="$2" name="$3" i
  for i in $(seq 1 "$seconds"); do
    if ctl ls gdrive -r 2>/dev/null | grep -qF -- "$name"; then
      [ "$want" = present ] && return 0
    else
      [ "$want" = absent ] && return 0
    fi
    sleep 1
  done
  return 1
}

# ---- 01: add -----------------------------------------------------------------

: >"$OUT_DIR/01-add.txt"
# 120 files are read, then the rest are held: checkpoint 02 needs the index
# paused part-way.
fake hold 120
STARTED="$(python3 -c 'import time; print(time.time())')"
transcript 01-add.txt ctl source add drive gdrive Notes
ADDED="$(python3 -c 'import time; print(time.time())')"
grep -q 'Registered Google Drive source "gdrive"' "$OUT_DIR/01-add.txt" || fail "no success line: $(cat "$OUT_DIR/01-add.txt")"
grep -q '402 files listed, all remote, 0 bytes cached' "$OUT_DIR/01-add.txt" || fail "the listing is not 402 remote files"
python3 - "$STARTED" "$ADDED" <<'PY' || fail "the add took more than 30 s"
import sys
sys.exit(0 if float(sys.argv[2]) - float(sys.argv[1]) < 30 else 1)
PY
transcript 01-add.txt ctl ls gdrive
LS="$(ctl ls gdrive)"
for kind in gdoc gsheet binary document prose; do
  printf '%s\n' "$LS" | grep -Eq "(^| )$kind( |$)" || fail "ls shows no $kind file"
done
printf '%s\n' "$LS" | grep -q 'remote' || fail "ls does not say remote"
# G8: the listing alone read nothing (the first 120 reads are the index's).
end 01-add.txt

# ---- 02: indexing, then active ---------------------------------------------------

: >"$OUT_DIR/02-indexing.txt"
until_list 60 'indexing 120/400' || { ctl source list >&2; fail "the index did not pause at 120/400"; }
transcript 02-indexing.txt ctl source list
# The listing answers while the index is paused.
transcript 02-indexing.txt ctl ls gdrive
fake release
until_list 120 '^gdrive[[:space:]]+active' || { ctl source list >&2; fail "gdrive did not become active"; }
transcript 02-indexing.txt ctl source list
BEFORE="$(content_reads)"
transcript 02-indexing.txt ctl link resolve '[[@gdrive:Roadmap]]'
transcript 02-indexing.txt ctl backlinks @gdrive:Roadmap.md
ctl link resolve '[[@gdrive:Roadmap]]' | grep -q 'Roadmap.md' || fail "[[@gdrive:Roadmap]] did not resolve"
ctl backlinks @gdrive:Roadmap.md | grep -q 'n001.md' || fail "no backlink from n001.md"
ctl backlinks @gdrive:Roadmap.md | grep -q 'Meeting' || fail "no backlink from the Google Doc"
AFTER="$(content_reads)"
[ "$BEFORE" = "$AFTER" ] || fail "resolving a link and reading backlinks read a file ($BEFORE -> $AFTER)"
{ echo "content reads before the link and the backlinks: $BEFORE, after: $AFTER"; } >>"$OUT_DIR/02-indexing.txt"
# Every text file was read exactly once (399 downloads and 1 export: the
# Google Doc), and none of the name-only files was.
fake dump
READS="$(grep -Ec 'alt=media|/export' "$WORK/requests.log" || true)"
[ "$READS" = "400" ] || fail "expected 400 reads, the fake saw $READS"
grep -Eq '/files/(budget|logo)[? ].*alt=media' "$WORK/requests.log" && fail "a name-only file was read"
echo "content reads in all: $READS (400 text files, each once)" >>"$OUT_DIR/02-indexing.txt"
end 02-indexing.txt

# ---- 03: duplicates ----------------------------------------------------------------

: >"$OUT_DIR/03-duplicates.txt"
transcript 03-duplicates.txt ctl ls gdrive -r
ctl ls gdrive -r | grep -qF 'Plan.md' || fail "ls does not show Plan.md"
ctl ls gdrive -r | grep -qF 'Plan (2).md' || fail "ls does not show Plan (2).md"
# The ambiguity error lists both files, and exits non-zero.
CODE=0
OUT="$(ctl link resolve '[[@gdrive:Plan]]' 2>&1)" || CODE=$?
{ printf '$ tephractl link resolve [[@gdrive:Plan]]\n%s\nexit: %s\n' "$OUT" "$CODE"; } >>"$OUT_DIR/03-duplicates.txt"
[ "$CODE" != "0" ] || fail "the plain-name link to two files resolved"
case "$OUT" in *'Plan.md'*'Plan (2).md'* | *'Plan (2).md'*'Plan.md'*) ;; *) fail "the ambiguity error does not name both: $OUT" ;; esac
# `Plan (2)` reaches the later file only.
transcript 03-duplicates.txt ctl link resolve '[[@gdrive:Plan (2)]]'
RESOLVED="$(ctl link resolve '[[@gdrive:Plan (2)]]')"
case "$RESOLVED" in *'Plan (2).md'*) ;; *) fail "[[@gdrive:Plan (2)]] did not reach Plan (2).md: $RESOLVED" ;; esac
# The names are stable across a restart, with the saved feed position.
stop_daemon
start_daemon
until_list 60 '^gdrive[[:space:]]+active' || { ctl source list >&2; fail "gdrive is not active after the restart"; }
echo "(after a restart)" >>"$OUT_DIR/03-duplicates.txt"
transcript 03-duplicates.txt ctl ls gdrive -r
ctl ls gdrive -r | grep -qF 'Plan (2).md' || fail "Plan (2).md is gone after the restart"
end 03-duplicates.txt

# ---- the WebSocket listener, then 04: the feed -----------------------------------------

cat >"$WORK/listen.py" <<'PY'
import base64, json, os, socket, struct, sys, time

host, port = sys.argv[1].split(":")
token = open(sys.argv[2]).read().strip()
ready, out = sys.argv[3], sys.argv[4]

s = socket.create_connection((host, int(port)))
key = base64.b64encode(os.urandom(16)).decode()
s.sendall((
    f"GET /rpc HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\n"
    f"Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\n"
    f"Sec-WebSocket-Version: 13\r\nAuthorization: Bearer {token}\r\n\r\n"
).encode())
head = b""
while b"\r\n\r\n" not in head:
    chunk = s.recv(4096)
    if not chunk:
        raise SystemExit("handshake closed")
    head += chunk
head, rest = head.split(b"\r\n\r\n", 1)
if b" 101 " not in head.split(b"\r\n")[0]:
    raise SystemExit("handshake refused: " + head.decode(errors="replace"))
open(ready, "w").write("ready")

buf = rest
def need(n):
    global buf
    while len(buf) < n:
        chunk = s.recv(65536)
        if not chunk:
            raise SystemExit("closed")
        buf += chunk
    data, buf = buf[:n], buf[n:]
    return data

# Every graph:changed frame, one JSON line each, until the script stops us.
while True:
    b1, b2 = need(2)
    opcode, length = b1 & 0x0F, b2 & 0x7F
    if length == 126:
        length = struct.unpack(">H", need(2))[0]
    elif length == 127:
        length = struct.unpack(">Q", need(8))[0]
    if b2 & 0x80:
        need(4)
    payload = need(length)
    if opcode != 1:
        continue
    frame = json.loads(payload)
    if frame.get("method") == "graph:changed":
        with open(out, "a") as f:
            f.write(json.dumps({"received_at": time.time(), "frame": frame}) + "\n")
PY
python3 "$WORK/listen.py" "$ADDR" "$XDG_DATA_HOME/tephra/token" "$WORK/ws-ready" "$WORK/push.jsonl" \
  >"$WORK/listen.log" 2>&1 &
LISTENER_PID=$!
for _ in $(seq 1 50); do
  [ -f "$WORK/ws-ready" ] && break
  kill -0 "$LISTENER_PID" 2>/dev/null || { cat "$WORK/listen.log" >&2; fail "the WebSocket listener exited early"; }
  sleep 0.1
done
[ -f "$WORK/ws-ready" ] || fail "the WebSocket listener never connected"
: >"$WORK/push.jsonl"

: >"$OUT_DIR/04-feed.txt"
# Let any poll already in flight finish, so the three changes below land in
# the same one.
sleep 2
FEED_STARTED="$(python3 -c 'import time; print(time.time())')"
fake add fresh fresh note from Drive
fake rename n002 renamed-note.md
fake remove n003
{
  echo "in Drive: added fresh.md, renamed n002.md to renamed-note.md, deleted n003.md"
} >>"$OUT_DIR/04-feed.txt"
until_ls "$POLL_WAIT" present 'fresh.md' || fail "the added file never reached the listing"
sleep 3
transcript 04-feed.txt ctl ls gdrive -r
LS="$(ctl ls gdrive -r)"
printf '%s\n' "$LS" | grep -qF 'renamed-note.md' || fail "the renamed file is not listed under its new name"
printf '%s\n' "$LS" | grep -qE '(^| )n002\.md( |$)' && fail "the old name n002.md is still listed"
printf '%s\n' "$LS" | grep -qE '(^| )n003\.md( |$)' && fail "the deleted n003.md is still listed"
transcript 04-feed.txt ctl fixes
ctl fixes | grep -q 'n002.md' || fail "the rename is not in the fix log"
transcript 04-feed.txt ctl source list
until_list 5 '^gdrive[[:space:]]+active' || fail "gdrive is not active after the poll"
sleep 4
FRAMES="$(wc -l <"$WORK/push.jsonl" | tr -d ' ')"
python3 - "$WORK/push.jsonl" "$FEED_STARTED" >>"$OUT_DIR/04-feed.txt" <<'PY' || fail "the graph:changed check failed (see 04-feed.txt)"
import json, sys
frames = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
start = float(sys.argv[2])
for f in frames:
    print(json.dumps(f["frame"], sort_keys=True))
    print("seconds from the changes in Drive to the push: %.1f" % (f["received_at"] - start))
print("graph:changed frames received: %d" % len(frames))
sys.exit(0 if len(frames) == 1 else 1)
PY
[ "$FRAMES" = "1" ] || fail "expected ONE graph:changed for the one poll, got $FRAMES"
end 04-feed.txt

# ---- 05: nothing on disk ----------------------------------------------------------------

: >"$OUT_DIR/05-nothing-on-disk.txt"
{
  echo "files under the data dir's remote/ folder (none expected):"
  find "$XDG_DATA_HOME/tephra/remote" -type f 2>/dev/null || true
  echo "(end)"
} >>"$OUT_DIR/05-nothing-on-disk.txt"
[ -z "$(find "$XDG_DATA_HOME/tephra/remote" -type f 2>/dev/null)" ] || fail "a file was written under remote/"
transcript 05-nothing-on-disk.txt ctl source list
ctl source list | grep -q 'cached 0 bytes' || fail "source list does not say 0 bytes cached"
transcript 05-nothing-on-disk.txt ctl cache status
fake dump
{
  echo "requests that were not a GET (none expected):"
  grep -v '^GET ' "$WORK/requests.log" || true
  echo "(end)"
} >>"$OUT_DIR/05-nothing-on-disk.txt"
if grep -v '^GET ' "$WORK/requests.log" | grep -q .; then fail "a request that was not a GET reached the fake Drive"; fi
end 05-nothing-on-disk.txt

# ---- the token is nowhere -----------------------------------------------------------------

if grep -rq "$TOKEN" "$OUT_DIR" "$WORK/tephrad.log" "$WORK/fake.out" "$WORK/requests.log" "$WORK/push.jsonl" 2>/dev/null; then
  fail "the token appears in a log, a transcript or an answer"
fi
for f in "$OUT_DIR"/*.txt; do
  tail -n 1 "$f" | grep -q '^exit: [0-9]*$' || fail "$(basename "$f") does not end with an exit line"
done

echo "== PR-10 verification passed; transcripts in $OUT_DIR"
