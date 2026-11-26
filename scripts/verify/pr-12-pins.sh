#!/usr/bin/env bash
# PR-12 (pins) container verification: a FOLDER pin on Google Drive (G11, GP9,
# E4, F12), live, against the PR-3 fake Drive in its `pins` tree (see
# `tephra-remote/examples/drive_fake.rs`: the folder `Trip Plans` with 40
# files of about 8 KB and a Google Sheet, the folder `Elsewhere` with 5).
#
# Builds the binaries, starts a throwaway tephrad (own XDG config/data dir, a
# free loopback port it picks itself, never the default 4812, `cache_limit =
# "1MB"`, `cache_idle_seconds = 3`) against the fake Drive on loopback, and
# saves each transcript to verify/pr-12-pins/*.txt (override the folder with
# OUT_DIR). No real Google call is made: the sign-in is a made-up token in a
# throwaway credential file.
#
#   01-pin-folder.txt     screenshot 1 and 2: `pin '@gdrive:Trip Plans/'` with no
#                         terminal (refused, size line first), then on a
#                         pseudo-terminal: the size line and the confirm prompt
#                         (`n` aborts, nothing pinned)
#   02-mid-download.txt   screenshot 3: with Drive holding the content reads,
#                         `pin --yes` returns at once; `ls` shows pinned and
#                         downloading rows and `cache status` a part of the total
#   03-all-pinned.txt     the reads are released: all 40 files `pinned`, `cache
#                         status` pinned bytes equal the total printed first,
#                         `Elsewhere` not requested by the pin (F12), the
#                         Sheet never requested (F5), a second `pin` says nothing changed
#   04-future-file.txt    screenshot 4: a 41st file is added in Trip Plans on
#                         Drive; the next sync fetches it (state pinned, two
#                         new content reads, both of it: the index read and
#                         the pin download) with no new `pin` call
#   05-over-cap.txt       screenshot 5: the cap lowered under the pinned bytes:
#                         `over limit` and the pins-exceed-cap warning, all 41
#                         files still on disk after an eviction pass
#   06-unpin-evict.txt    screenshot 6: `unpin`; the 41 files stay cached; after
#                         the idle window an eviction pass drops the least
#                         recently used first, then all of them; the folder
#                         listing and the backlinks of t02.md stay
#   07-restart.txt        a restart: the folder pin and a file added later survive
#   08-no-writes.txt      Drive saw no request that was not a GET
#
# Every transcript ends with an "exit: <code>" line. The script exits non-zero
# if any result is wrong. It starts two processes (tephrad and the fake
# Drive), records their PIDs and stops only those PIDs on exit. It waits for
# one sync poll (about 60 s) in step 04; every wait is bounded.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/pr-12-pins}"
POLL_WAIT="${POLL_WAIT:-120}"
IDLE=3
# A made-up sign-in. It must appear nowhere in an output.
TOKEN="ya29.V...oken"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr12pins-verify.XXXXXX")"
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
  echo "pr-12-pins.sh: FAIL: $*" >&2
  exit 1
}

echo "== build"
FEATURES="tephrad/test-credential-store,tephractl/test-credential-store,tephra-remote/test-support"
(cd "$ROOT" && cargo build -p tephrad -p tephractl -p tephra-remote --features "$FEATURES" --bins --example drive_fake)
# Copies, so a `cargo test` run in the same checkout cannot replace the daemon
# (built without the test credential store) before the restart in step 07.
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
"$FAKE" "$WORK/url" "$WORK/requests.log" pins <"$WORK/fake.in" >"$WORK/fake.out" 2>"$WORK/fake.err" &
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

# content_reads [id regexp]: how many file reads (a download or an export) the
# fake has seen so far, optionally only of the files whose id matches.
content_reads() {
  fake dump
  if [ -n "${1:-}" ]; then
    grep -E 'alt=media|/export' "$WORK/requests.log" | grep -Ec "/files/($1)[/? ]|/files/($1)\$" || true
  else
    grep -Ec 'alt=media|/export' "$WORK/requests.log" || true
  fi
}

# ---- tephrad ----------------------------------------------------------------

export XDG_CONFIG_HOME="$WORK/xdg-config"
export XDG_DATA_HOME="$WORK/xdg-data"
mkdir -p "$XDG_CONFIG_HOME/tephra" "$XDG_DATA_HOME/tephra"
cat >"$XDG_CONFIG_HOME/tephra/config.toml" <<EOF
listen_addr = "$ADDR"
data_dir = "$XDG_DATA_HOME/tephra"
log_level = "info"
cache_limit = "1MB"
cache_idle_seconds = $IDLE
drive_api_url = "$DRIVE_URL"
test_credential_file = "credentials.json"
EOF
( umask 077; printf '{"google":"%s"}' "$TOKEN" >"$XDG_DATA_HOME/tephra/credentials.json" )

ctl() { "$TEPHRACTL" --addr "$ADDR" "$@"; }

start_daemon() {
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then fail "a tephrad of this script is still running (pid $DAEMON_PID)"; fi
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
# exit code, in that order, appended to <file>. The code is in $CODE after.
CODE=0
LAST=""
transcript() {
  local file="$1"
  shift
  CODE=0
  {
    printf '$ %s\n' "$*"
    LAST="$("$@" 2>&1)" || CODE=$?
    printf '%s\n' "$LAST"
    printf 'exit: %s\n' "$CODE"
  } >>"$OUT_DIR/$file"
  return 0
}
note() { printf '%s\n' "$2" >>"$OUT_DIR/$1"; }
end() { printf 'exit: 0\n' >>"$OUT_DIR/$1"; }

# states <dir>: the `ls` state column of every file of a folder, one per line.
states() { ctl ls gdrive "$1" | awk '$1 == "prose" {print $3}'; }
count_state() { states "$1" | grep -c "^$2\$" || true; }
# until_count <seconds> <folder> <state> <n>: wait (bounded) for n files to be in a state.
until_count() {
  local seconds="$1" folder="$2" state="$3" n="$4" i
  for i in $(seq 1 "$seconds"); do
    [ "$(count_state "$folder" "$state")" = "$n" ] && return 0
    sleep 1
  done
  return 1
}
cached_files_on_disk() { find "$XDG_DATA_HOME/tephra/remote/gdrive" -type f -name 't[0-9][0-9].md' 2>/dev/null | wc -l | tr -d ' '; }
# pinned_bytes: the number on the `pinned` line of `cache status`.
pinned_bytes() { ctl cache status | awk '$1 == "pinned" {print $2}'; }

# ---- the source ---------------------------------------------------------------

ctl source add drive gdrive Notes >"$WORK/add.out" 2>&1 || { cat "$WORK/add.out" >&2; fail "source add failed"; }
for _ in $(seq 1 60); do
  ctl source list 2>/dev/null | grep -Eq '^gdrive[[:space:]]+active' && break
  sleep 1
done
ctl source list | grep -Eq '^gdrive[[:space:]]+active' || fail "gdrive did not become active"
[ "$(count_state 'Trip Plans' remote)" = 40 ] || fail "expected 40 remote files in Trip Plans: $(ctl ls gdrive 'Trip Plans')"

# ---- 01: the size line and the confirm prompt -------------------------------------

: >"$OUT_DIR/01-pin-folder.txt"
transcript 01-pin-folder.txt ctl pin '@gdrive:Trip Plans/'
[ "$CODE" != 0 ] || fail "a folder pin over a fifth of the cap went through with no terminal and no --yes"
echo "$LAST" | grep -q '40 files' || fail "the size line does not say 40 files: $LAST"
echo "$LAST" | grep -qi 'skipped' || fail "the size line does not mention the skipped Sheet: $LAST"
TOTAL="$(echo "$LAST" | grep -Eo '[0-9]+ bytes' | head -n 1 | grep -Eo '[0-9]+')"
[ "$TOTAL" -gt 200000 ] || fail "the folder is not over a fifth of the 1 MB cap ($TOTAL)"
note 01-pin-folder.txt "(the same command on a pseudo-terminal; the answer is typed)"
python3 - "$TEPHRACTL" "$ADDR" >>"$OUT_DIR/01-pin-folder.txt" <<'PY'
import os, pty, sys, select, time

tephractl, addr = sys.argv[1], sys.argv[2]
pid, fd = pty.fork()
if pid == 0:
    os.execv(tephractl, [tephractl, "--addr", addr, "pin", "@gdrive:Trip Plans/"])
out = b""
sent = False
deadline = time.time() + 20
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r:
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            break
        if not chunk:
            break
        out += chunk
        if b"[y/N]" in out and not sent:
            os.write(fd, b"n\n")
            sent = True
_, status = os.waitpid(pid, 0)
print("$ tephractl pin '@gdrive:Trip Plans/'   (terminal; typed: n)")
print(out.decode(errors="replace").replace("\r\n", "\n").rstrip())
print("exit: %d" % os.waitstatus_to_exitcode(status))
PY
tail -n 3 "$OUT_DIR/01-pin-folder.txt" | grep -q 'exit: 1' || fail "answering n did not exit 1"
grep -q 'Pin it and download these files? \[y/N\]' "$OUT_DIR/01-pin-folder.txt" || fail "the confirm prompt is not in the transcript"
transcript 01-pin-folder.txt ctl cache status
[ "$(pinned_bytes)" = 0 ] || fail "bytes are pinned after the pin was refused"
[ "$(count_state 'Trip Plans' remote)" = 40 ] || fail "a refused pin changed a state"
end 01-pin-folder.txt

# ---- 02: pin --yes, with Drive holding the reads: mid-download ----------------------

: >"$OUT_DIR/02-mid-download.txt"
READS_BEFORE="$(content_reads)"
OUTSIDE_BEFORE="$(content_reads 'e0[0-4]|budget')"
fake hold 12
transcript 02-mid-download.txt ctl pin '@gdrive:Trip Plans/' --yes
[ "$CODE" = 0 ] || fail "pin --yes failed: $LAST"
echo "$LAST" | grep -q 'download in the background' || fail "no background line: $LAST"
until_count 30 'Trip Plans' downloading 1 || { ctl ls gdrive 'Trip Plans' >&2; fail "no file reached downloading"; }
transcript 02-mid-download.txt ctl ls gdrive 'Trip Plans'
transcript 02-mid-download.txt ctl cache status
PINNED_MID="$(count_state 'Trip Plans' pinned)"
[ "$PINNED_MID" -ge 1 ] && [ "$PINNED_MID" -lt 40 ] || fail "expected some, not all, files pinned mid-download: $PINNED_MID"
[ "$(count_state 'Trip Plans' downloading)" = 1 ] || fail "expected exactly one downloading file"
[ "$(pinned_bytes)" -gt 0 ] && [ "$(pinned_bytes)" -lt "$TOTAL" ] || fail "pinned bytes mid-download are not a part of the total: $(pinned_bytes)"
end 02-mid-download.txt

# ---- 03: all pinned ------------------------------------------------------------------

: >"$OUT_DIR/03-all-pinned.txt"
fake release
until_count 60 'Trip Plans' pinned 40 || { ctl ls gdrive 'Trip Plans' >&2; fail "the 40 files did not all become pinned"; }
transcript 03-all-pinned.txt ctl ls gdrive 'Trip Plans'
transcript 03-all-pinned.txt ctl cache status
[ "$(pinned_bytes)" = "$TOTAL" ] || fail "pinned bytes $(pinned_bytes) are not the total printed first ($TOTAL)"
[ "$(cached_files_on_disk)" = 40 ] || fail "expected 40 files on disk: $(cached_files_on_disk)"
note 03-all-pinned.txt "content reads by Drive since before the pin: $(( $(content_reads) - READS_BEFORE )) (40 files)"
[ "$(( $(content_reads) - READS_BEFORE ))" = 40 ] || fail "expected exactly 40 content reads for the pin"
OUTSIDE_NOW="$(( $(content_reads 'e0[0-4]|budget') - OUTSIDE_BEFORE ))"
note 03-all-pinned.txt "content reads of Elsewhere (e00..e04) and the Sheet (budget) since the pin: $OUTSIDE_NOW (must be 0; the first-index read before the pin read the Markdown ones once)"
[ "$OUTSIDE_NOW" = 0 ] || fail "a file outside the pin, or the Sheet, was requested"
transcript 03-all-pinned.txt ctl ls gdrive Elsewhere
[ "$(count_state Elsewhere remote)" = 5 ] || fail "Elsewhere changed: $(ctl ls gdrive Elsewhere)"
transcript 03-all-pinned.txt ctl pin '@gdrive:Trip Plans/' --yes
[ "$CODE" = 0 ] || fail "the second pin exited non-zero"
echo "$LAST" | grep -q 'nothing changed' || fail "the second pin did not say nothing changed: $LAST"
end 03-all-pinned.txt

# ---- 04: a 41st file ---------------------------------------------------------------------

: >"$OUT_DIR/04-future-file.txt"
note 04-future-file.txt "(Drive gets a new file t40.md in Trip Plans; no pin command is run from here on)"
READS_BEFORE="$(content_reads)"
fake add t40 t40 trip file added later
transcript 04-future-file.txt ctl ls gdrive 'Trip Plans'
echo "$LAST" | grep -q 't40.md' && fail "t40.md was listed before the sync"
for _ in $(seq 1 "$POLL_WAIT"); do
  [ "$(count_state 'Trip Plans' pinned)" = 41 ] && break
  sleep 1
done
transcript 04-future-file.txt ctl ls gdrive 'Trip Plans'
[ "$(count_state 'Trip Plans' pinned)" = 41 ] || fail "the 41st file was not fetched and pinned within ${POLL_WAIT}s: $(ctl ls gdrive 'Trip Plans')"
echo "$LAST" | grep -q 't40.md' || fail "t40.md is not listed"
transcript 04-future-file.txt ctl cache status
READS_AFTER="$(content_reads)"
# A new Drive text file is read once for the index (PR-10, links and headings;
# the bytes are not kept) and, when a pin covers it, once more as the pin
# download (the cache copy). Both are allowed reads (F12); nothing else is.
note 04-future-file.txt "content reads since the file was added: $(( READS_AFTER - READS_BEFORE )) (2: one index read and one pin download, both of t40.md)"
[ "$(( READS_AFTER - READS_BEFORE ))" = 2 ] || { grep -E 'alt=media|/export' "$WORK/requests.log" | tail -n 4 >&2; fail "expected two content reads (index, pin download) after the 41st file"; }
[ "$(content_reads t40)" = 2 ] || fail "the reads are not both of t40"
[ "$(cached_files_on_disk)" = 41 ] || fail "expected 41 files on disk"
note 04-future-file.txt "pin requests sent to the daemon since: 0 (this step only listed and waited)"
end 04-future-file.txt

# ---- 05: pinned bytes over the cap ---------------------------------------------------------

: >"$OUT_DIR/05-over-cap.txt"
transcript 05-over-cap.txt ctl cache set-limit 0.15MB
[ "$CODE" = 0 ] || fail "set-limit 0.15MB failed: $LAST"
sleep $((IDLE + 2))
transcript 05-over-cap.txt ctl cache set-limit 0.15MB
transcript 05-over-cap.txt ctl cache status
ctl cache status | grep -q '^over limit' || fail "cache status does not say over limit"
ctl cache status | grep -q '^warning:' || fail "cache status has no pins-exceed-cap warning"
[ "$(cached_files_on_disk)" = 41 ] || fail "an eviction pass removed a pinned file: $(cached_files_on_disk) left"
[ "$(count_state 'Trip Plans' pinned)" = 41 ] || fail "a pinned file changed state"
note 05-over-cap.txt "files on disk after the pass: $(cached_files_on_disk) of 41"
end 05-over-cap.txt

# ---- 06: unpin, then eviction -----------------------------------------------------------------

: >"$OUT_DIR/06-unpin-evict.txt"
LISTING_BEFORE="$(ctl ls gdrive 'Trip Plans' | awk '{print $NF}' | LC_ALL=C sort)"
transcript 06-unpin-evict.txt ctl backlinks '@gdrive:Trip Plans/t02.md'
BACKLINKS_BEFORE="$LAST"
echo "$BACKLINKS_BEFORE" | grep -q 't01' || fail "t02.md has no backlink from t01.md: $BACKLINKS_BEFORE"
transcript 06-unpin-evict.txt ctl unpin '@gdrive:Trip Plans/'
[ "$CODE" = 0 ] || fail "unpin failed: $LAST"
[ "$(count_state 'Trip Plans' cached)" = 41 ] || fail "after unpin the 41 files are not cached: $(ctl ls gdrive 'Trip Plans')"
[ "$(cached_files_on_disk)" = 41 ] || fail "unpin removed a file"
transcript 06-unpin-evict.txt ctl ls gdrive 'Trip Plans'
note 06-unpin-evict.txt "(idle window: $IDLE s, then a pass at the 0.15MB cap)"
sleep $((IDLE + 2))
transcript 06-unpin-evict.txt ctl cache set-limit 0.15MB
for _ in $(seq 1 20); do
  [ "$(count_state 'Trip Plans' remote)" -gt 0 ] && break
  sleep 1
done
transcript 06-unpin-evict.txt ctl cache status
transcript 06-unpin-evict.txt ctl ls gdrive 'Trip Plans'
DROPPED="$(count_state 'Trip Plans' remote)"
KEPT="$(count_state 'Trip Plans' cached)"
[ "$DROPPED" -gt 0 ] && [ "$KEPT" -gt 0 ] || fail "expected an LRU drop of some, not all: dropped $DROPPED kept $KEPT"
ctl cache status | grep -q '^over limit' && fail "still over limit after the pass"
# least recently used first: t00 was fetched first and goes before t40.
[ "$(states 'Trip Plans' | head -n 1)" = remote ] || fail "t00.md (the oldest) was not dropped first"
echo "$(ctl ls gdrive 'Trip Plans' | grep -c 't40.md')" | grep -q 1 || fail "t40.md vanished from the listing"
ctl ls gdrive 'Trip Plans' | grep 't40.md' | grep -q cached || fail "t40.md (the newest) was dropped before the oldest"
transcript 06-unpin-evict.txt ctl cache set-limit 0
for _ in $(seq 1 20); do
  [ "$(count_state 'Trip Plans' remote)" = 41 ] && break
  sleep 1
done
transcript 06-unpin-evict.txt ctl cache status
transcript 06-unpin-evict.txt ctl ls gdrive 'Trip Plans'
[ "$(count_state 'Trip Plans' remote)" = 41 ] || fail "not every unpinned file was dropped under pressure"
[ "$(cached_files_on_disk)" = 0 ] || fail "files are still on disk"
[ "$(ctl ls gdrive 'Trip Plans' | awk '{print $NF}' | LC_ALL=C sort)" = "$LISTING_BEFORE" ] || fail "the folder listing changed" 
transcript 06-unpin-evict.txt ctl backlinks '@gdrive:Trip Plans/t02.md'
[ "$LAST" = "$BACKLINKS_BEFORE" ] || fail "the backlinks changed after eviction"
end 06-unpin-evict.txt

# ---- 07: restart ---------------------------------------------------------------------------------

: >"$OUT_DIR/07-restart.txt"
ctl cache set-limit 1MB >/dev/null
transcript 07-restart.txt ctl pin '@gdrive:Trip Plans/' --yes
[ "$CODE" = 0 ] || fail "re-pin failed: $LAST"
until_count 60 'Trip Plans' pinned 41 || fail "the re-pin did not finish"
stop_daemon
start_daemon
transcript 07-restart.txt ctl ls gdrive 'Trip Plans'
transcript 07-restart.txt ctl cache status
[ "$(count_state 'Trip Plans' pinned)" = 41 ] || fail "pins did not survive the restart"
[ "$(pinned_bytes)" -gt "$TOTAL" ] || fail "pinned bytes did not survive the restart"
transcript 07-restart.txt ctl pin '@gdrive:Trip Plans/' --yes
echo "$LAST" | grep -q 'already pinned' || fail "the folder pin is not there after the restart: $LAST"
end 07-restart.txt

# ---- 08: nothing was written to Drive -----------------------------------------------------------------

: >"$OUT_DIR/08-no-writes.txt"
fake dump
WRITES="$(grep -Evc '^GET ' "$WORK/requests.log" || true)"
note 08-no-writes.txt "requests Drive saw: $(wc -l <"$WORK/requests.log" | tr -d ' ')"
note 08-no-writes.txt "requests that were not a GET: $WRITES"
[ "$WRITES" = 0 ] || fail "Drive saw a write"
grep -q "$TOKEN" "$OUT_DIR"/*.txt && fail "the made-up sign-in is in an output"
end 08-no-writes.txt

for f in "$OUT_DIR"/*.txt; do
  tail -n 1 "$f" | grep -q '^exit: [0-9]*$' || fail "$(basename "$f") does not end with an exit line"
done

echo "== PR-12 pins verification passed; transcripts in $OUT_DIR"
