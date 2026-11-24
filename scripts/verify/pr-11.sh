#!/usr/bin/env bash
# PR-11 container verification: edits to a cached Google Drive file go back
# to Drive (G13, G14, G21, GP7, GP12, E3, E14).
#
# Builds the workspace, starts a throwaway tephrad (own XDG config/data dir, a
# free loopback port it picks itself, never the default 4812) against the
# PR-3 fake Drive (`tephra-remote/examples/drive_fake ... edits`, loopback
# only, driven through a pipe), and saves each transcript to
# verify/pr-11/*.txt (override the folder with OUT_DIR). No real Google call
# is made: the stored sign-in is a made-up token in a throwaway credential
# file, and the daemon is told to talk to the fake with the test-only
# `drive_api_url` key.
#
#   01-save-and-upload.txt   GP7: `fetch @gdrive:Plan.md` prints the cache path
#                            and `ls` says cached; the file is edited and
#                            saved: `ls` says pending_upload, then within
#                            10 s Drive has a new revision with the new
#                            bytes and `ls` says cached again
#   02-refused.txt           GP12: Drive changes while the edit is queued: the
#                            upload is refused, `ls` says needs_attention:
#                            changed on Drive, Drive got no write, the edit is
#                            still on disk
#   03-resolve-mine.txt      `resolve gdrive:Plan.md --mine`: his version is
#                            a new revision on top of the other change
#   04-resolve-theirs.txt    the same conflict again, `--theirs`: his edits
#                            are in Plan.md.mine, Plan.md is Drive's
#                            again; and every wrong use of `resolve`
#                            (no flag, both flags, a file with no conflict, a
#                            file that is not there) exits non-zero and
#                            changes nothing
#   05-killed-mid-upload.txt E14: the daemon is killed while the bytes are
#                            going (once before Drive applied them, once
#                            after), is started again, and Drive has exactly
#                            one new revision
#   06 (real Drive)          is a HUMAN step: this script cannot reach Google.
#                            With a real account: `tephractl auth google`,
#                            `tephractl source add drive gdrive "<a folder>"`,
#                            fetch a file, edit it, and watch `ls`.
#
# Every transcript ends with an "exit: <code>" line. The script exits non-zero
# if any result is wrong. It starts two processes (tephrad and the fake
# Drive), records their PIDs and stops only those PIDs on exit.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
OUT_DIR="${OUT_DIR:-$ROOT/verify/pr-11}"
# A made-up sign-in. It must appear nowhere in an output.
TOKEN="ya29.VERIFY_not_a_real_google_token"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/pr11-verify.XXXXXX")"
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
  echo "pr-11.sh: FAIL: $*" >&2
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
"$FAKE" "$WORK/url" "$WORK/requests.log" edits <"$WORK/fake.in" >"$WORK/fake.out" 2>"$WORK/fake.err" &
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
# exit code, in that order, appended to <file>. Returns 0 always; the code is
# in $CODE afterwards.
CODE=0
transcript() {
  local file="$1"
  shift
  local out
  CODE=0
  {
    printf '$ %s\n' "$*"
    out="$("$@" 2>&1)" || CODE=$?
    printf '%s\n' "$out"
    printf 'exit: %s\n' "$CODE"
  } >>"$OUT_DIR/$file"
  LAST="$out"
  return 0
}
# end <file>: the file's closing exit line, once its checks have passed.
end() { printf 'exit: 0\n' >>"$OUT_DIR/$1"; }

# fake_value <command...>: the answer of a fake command, after `ok `.
fake_value() {
  fake "$@"
  tail -n 1 "$WORK/fake.out" | sed 's/^ok //'
}

# state_of <name>: the state `ls` shows for a file of gdrive.
state_of() {
  ctl ls gdrive 2>/dev/null | grep -E "(^| )$1( |$)" | head -n 1
}

# until_state <seconds> <name> <extended regexp>: wait (bounded) until the
# `ls` line of a file matches.
until_state() {
  local seconds="$1" name="$2" pattern="$3" i
  for i in $(seq 1 "$seconds"); do
    if state_of "$name" | grep -Eq "$pattern"; then return 0; fi
    sleep 1
  done
  return 1
}

# save <file> <text>: what an editor does, in place.
save() { printf '%s\n' "$2" >"$1"; }

# ---- 00: the source ---------------------------------------------------------------

transcript 00-setup.txt ctl source add drive gdrive Notes
[ "$CODE" = 0 ] || fail "source add failed: $LAST"
for _ in $(seq 1 60); do
  ctl source list 2>/dev/null | grep -Eq '^gdrive[[:space:]]+active' && break
  sleep 1
done
ctl source list | grep -Eq '^gdrive[[:space:]]+active' || fail "gdrive did not become active"
echo "(setup of the source; transcript in 00-setup.txt)" >/dev/null
end 00-setup.txt

# ---- 01: GP7 save and upload ---------------------------------------------------------

: >"$OUT_DIR/01-save-and-upload.txt"
transcript 01-save-and-upload.txt ctl fetch @gdrive:Plan.md
[ "$CODE" = 0 ] || fail "fetch failed: $LAST"
PLAN="$(printf '%s\n' "$LAST" | grep -Eo '/[^ ]*Plan\.md' | head -n 1)"
[ -f "$PLAN" ] || fail "fetch did not name a file: $LAST"
case "$PLAN" in "$XDG_DATA_HOME"/tephra/remote/gdrive/*) ;; *) fail "the cache file is not in the content cache: $PLAN" ;; esac
transcript 01-save-and-upload.txt ctl ls gdrive
state_of Plan.md | grep -q 'cached' || fail "Plan.md is not cached after the fetch: $(state_of Plan.md)"
HEAD_BEFORE="$(fake_value head plan)"
UPLOADS_BEFORE="$(fake_value uploads plan)"
[ "$UPLOADS_BEFORE" = 0 ] || fail "an upload before any edit"

# Keep the upload from going for a moment, so `pending_upload` can be seen.
fake hold-upload before
echo "(edit and save Plan.md in the content cache)" >>"$OUT_DIR/01-save-and-upload.txt"
SAVED_AT="$(python3 -c 'import time; print(time.time())')"
save "$PLAN" "# Plan

second version, edited here"
until_state 20 Plan.md 'pending_upload' || { ctl ls gdrive >&2; fail "the saved edit never became pending_upload"; }
transcript 01-save-and-upload.txt ctl ls gdrive
state_of Plan.md | grep -q 'pending_upload' || fail "Plan.md is not pending_upload"
transcript 01-save-and-upload.txt ctl source list
fake release-uploads
until_state 20 Plan.md 'cached' || { ctl ls gdrive >&2; fail "the edit did not go up and Plan.md did not become cached"; }
DONE_AT="$(python3 -c 'import time; print(time.time())')"
transcript 01-save-and-upload.txt ctl ls gdrive
python3 - "$SAVED_AT" "$DONE_AT" >>"$OUT_DIR/01-save-and-upload.txt" <<'PY' || fail "the upload took more than 10 s after the hold was released"
import sys
secs = float(sys.argv[2]) - float(sys.argv[1])
print("seconds from the save to cached (the hold included): %.1f" % secs)
PY
[ "$(fake_value uploads plan)" = 1 ] || fail "expected one upload"
HEAD_AFTER="$(fake_value head plan)"
[ "$HEAD_BEFORE" != "$HEAD_AFTER" ] || fail "Drive's revision did not change"
fake_value text plan | grep -q 'second version, edited here' || fail "Drive does not have the edited text"
{
  echo "Drive's head revision before: $HEAD_BEFORE, after: $HEAD_AFTER"
  echo "Drive's text now: $(fake_value text plan)"
} >>"$OUT_DIR/01-save-and-upload.txt"
end 01-save-and-upload.txt

# The time check, without the hold: save again and time it.
: >"$OUT_DIR/01b-timed.txt"
SAVED_AT="$(python3 -c 'import time; print(time.time())')"
save "$PLAN" "# Plan

third version"
until_state 20 Plan.md 'pending_upload|cached' || fail "the third save was not seen"
sleep 1
until_state 20 Plan.md 'cached' || fail "the third save did not go up"
DONE_AT="$(python3 -c 'import time; print(time.time())')"
python3 - "$SAVED_AT" "$DONE_AT" >>"$OUT_DIR/01b-timed.txt" <<'PY' || fail "a plain save took 10 s or more to reach Drive"
import sys
secs = float(sys.argv[2]) - float(sys.argv[1])
print("seconds from save to cached: %.1f" % secs)
sys.exit(0 if secs < 10 else 1)
PY
[ "$(fake_value uploads plan)" = 2 ] || fail "expected two uploads in all"
end 01b-timed.txt

# ---- 02: GP12 refused -------------------------------------------------------------------

: >"$OUT_DIR/02-refused.txt"
fake_value writes >"$WORK/writes.before"
fake edit plan "# Plan\n\nsomebody else changed this"
fake_value text plan | grep -q 'somebody else changed this' || fail "the change in Drive did not apply"
HEAD_THEIRS="$(fake_value head plan)"
save "$PLAN" "# Plan

my edit, made after theirs"
until_state 30 Plan.md 'needs_attention' || { ctl ls gdrive >&2; fail "the upload was not refused"; }
transcript 02-refused.txt ctl ls gdrive
state_of Plan.md | grep -q 'changed on Drive' || fail "ls does not say why: $(state_of Plan.md)"
transcript 02-refused.txt ctl source list
[ "$(fake_value uploads plan)" = 2 ] || fail "bytes were sent to Drive for a changed file"
[ "$(fake_value head plan)" = "$HEAD_THEIRS" ] || fail "Drive's revision moved"
grep -q 'my edit, made after theirs' "$PLAN" || fail "the edit is not on disk any more"
echo "Drive still holds: $(fake_value text plan)" >>"$OUT_DIR/02-refused.txt"
echo "the file on disk still holds: $(tr '\n' ' ' <"$PLAN")" >>"$OUT_DIR/02-refused.txt"
end 02-refused.txt

# ---- 03: resolve --mine -------------------------------------------------------------------

: >"$OUT_DIR/03-resolve-mine.txt"
transcript 03-resolve-mine.txt ctl resolve gdrive:Plan.md --mine
[ "$CODE" = 0 ] || fail "resolve --mine failed: $LAST"
transcript 03-resolve-mine.txt ctl ls gdrive
state_of Plan.md | grep -q 'cached' || fail "Plan.md is not cached after --mine"
fake_value text plan | grep -q 'my edit, made after theirs' || fail "Drive does not have my version"
[ "$(fake_value uploads plan)" = 3 ] || fail "expected one more upload"
[ "$(fake_value head plan)" != "$HEAD_THEIRS" ] || fail "no new revision on top of theirs"
echo "Drive's head revision: $HEAD_THEIRS -> $(fake_value head plan)" >>"$OUT_DIR/03-resolve-mine.txt"
end 03-resolve-mine.txt

# ---- 04: resolve --theirs, and every wrong use ------------------------------------------------

: >"$OUT_DIR/04-resolve-theirs.txt"
fake edit plan "# Plan\n\nsomebody else, once more"
HEAD_THEIRS="$(fake_value head plan)"
save "$PLAN" "# Plan

my edit, to be kept aside"
until_state 30 Plan.md 'needs_attention' || { ctl ls gdrive >&2; fail "the second conflict was not raised"; }
transcript 04-resolve-theirs.txt ctl ls gdrive

# Wrong uses first: nothing changes.
ctl ls gdrive >"$WORK/ls.before"
UPLOADS_NOW="$(fake_value uploads plan)"
for args in "gdrive:Plan.md" "gdrive:Plan.md --mine --theirs" "gdrive:Other.md --mine" "gdrive:Nope.md --theirs" "nowhere:Plan.md --mine"; do
  # A usage error (exit 2) prints the usage text after its first line: the
  # transcript keeps the first line only.
  CODE=0
  # shellcheck disable=SC2086
  OUT="$(ctl resolve $args 2>&1)" || CODE=$?
  { printf '$ ctl resolve %s\n%s\nexit: %s\n' "$args" "$(printf '%s\n' "$OUT" | head -n 1)" "$CODE"; } >>"$OUT_DIR/04-resolve-theirs.txt"
  [ "$CODE" != 0 ] || fail "resolve $args exited 0: $OUT"
done
ctl ls gdrive >"$WORK/ls.after"
cmp -s "$WORK/ls.before" "$WORK/ls.after" || fail "a wrong resolve changed the listing"
[ "$(fake_value uploads plan)" = "$UPLOADS_NOW" ] || fail "a wrong resolve sent bytes"
grep -q 'my edit, to be kept aside' "$PLAN" || fail "a wrong resolve touched the file"
state_of Plan.md | grep -q 'needs_attention' || fail "a wrong resolve cleared the conflict"

transcript 04-resolve-theirs.txt ctl resolve gdrive:Plan.md --theirs
[ "$CODE" = 0 ] || fail "resolve --theirs failed: $LAST"
[ -f "$PLAN.mine" ] || fail "no Plan.md.mine"
grep -q 'my edit, to be kept aside' "$PLAN.mine" || fail "Plan.md.mine does not hold my edit"
grep -q 'somebody else, once more' "$PLAN" || fail "Plan.md is not Drive's again"
state_of Plan.md | grep -q 'cached' || fail "Plan.md is not cached after --theirs"
[ "$(fake_value uploads plan)" = "$UPLOADS_NOW" ] || fail "--theirs sent bytes"
transcript 04-resolve-theirs.txt ctl ls gdrive
{
  echo "Plan.md.mine: $(tr '\n' ' ' <"$PLAN.mine")"
  echo "Plan.md:      $(tr '\n' ' ' <"$PLAN")"
} >>"$OUT_DIR/04-resolve-theirs.txt"
# A second resolve of the same conflict has nothing to settle.
transcript 04-resolve-theirs.txt ctl resolve gdrive:Plan.md --mine
[ "$CODE" != 0 ] || fail "a second resolve exited 0"
end 04-resolve-theirs.txt

# ---- 05: E14 killed mid-upload --------------------------------------------------------------

: >"$OUT_DIR/05-killed-mid-upload.txt"
kill_and_restart() {
  echo "(tephrad killed: pid $DAEMON_PID)" >>"$OUT_DIR/05-killed-mid-upload.txt"
  kill -9 "$DAEMON_PID" 2>/dev/null || true
  wait "$DAEMON_PID" 2>/dev/null || true
  DAEMON_PID=""
  fake release-uploads
  start_daemon
  echo "(tephrad started again)" >>"$OUT_DIR/05-killed-mid-upload.txt"
}
for when in before after; do
  echo "== killed with the upload held $when Drive applied it" >>"$OUT_DIR/05-killed-mid-upload.txt"
  UPLOADS_NOW="$(fake_value uploads plan)"
  HEAD_NOW="$(fake_value head plan)"
  fake hold-upload "$when"
  save "$PLAN" "# Plan

edit while killed $when"
  for _ in $(seq 1 30); do
    [ "$(fake_value held)" -ge 1 ] && break
    sleep 1
  done
  [ "$(fake_value held)" -ge 1 ] || fail "the upload never reached the fake ($when)"
  transcript 05-killed-mid-upload.txt ctl ls gdrive
  kill_and_restart
  until_state 40 Plan.md 'cached' || { ctl ls gdrive >&2; fail "after the restart Plan.md did not become cached ($when)"; }
  transcript 05-killed-mid-upload.txt ctl ls gdrive
  [ "$(fake_value uploads plan)" = "$((UPLOADS_NOW + 1))" ] || fail "not exactly one new revision after a kill $when: $(fake_value uploads plan) uploads, was $UPLOADS_NOW"
  fake_value text plan | grep -q "edit while killed $when" || fail "Drive does not have the edit ($when)"
  {
    echo "uploads applied by Drive: $UPLOADS_NOW -> $(fake_value uploads plan) (exactly one more)"
    echo "Drive's head revision: $HEAD_NOW -> $(fake_value head plan)"
  } >>"$OUT_DIR/05-killed-mid-upload.txt"
done
end 05-killed-mid-upload.txt

# ---- the write the daemon makes, and the token ---------------------------------------------------

: >"$OUT_DIR/06-only-writes.txt"
fake dump
{
  echo "requests to Drive that were not a GET:"
  grep -v '^GET ' "$WORK/requests.log" | sort | uniq -c
  echo "(end)"
} >>"$OUT_DIR/06-only-writes.txt"
if grep -v '^GET ' "$WORK/requests.log" | grep -vq '^PATCH /upload/drive/v3/files/plan '; then
  fail "a request other than the media upload of the one file was not a GET"
fi
end 06-only-writes.txt

if grep -rq "$TOKEN" "$OUT_DIR" "$WORK/tephrad.log" "$WORK/fake.out" "$WORK/requests.log" 2>/dev/null; then
  fail "the token appears in a log, a transcript or an answer"
fi
for f in "$OUT_DIR"/*.txt; do
  tail -n 1 "$f" | grep -q '^exit: [0-9]*$' || fail "$(basename "$f") does not end with an exit line"
done

echo "== PR-11 verification passed; transcripts in $OUT_DIR"
