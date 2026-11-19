#!/usr/bin/env bash
# Tests for scripts/linux-tephrad.sh: argument handling, non-zero exits,
# `--help`, and the refusals that must change nothing. `docker` is a stub
# on PATH that answers from environment variables and logs every call, so
# the tests need no Docker and no container. (The real run is in the
# Linux container: see the README, "Linux tephrad".)
#
# Run: scripts/tests/linux-tephrad.test.sh

set -u

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SCRIPT=$(cd "$HERE/.." && pwd)/linux-tephrad.sh
WORK=$(mktemp -d "${TMPDIR:-/tmp}/linux-tephrad-test.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

mkdir -p "$WORK/bin" "$WORK/out"
LOG="$WORK/docker.log"

# The stub. Every call is appended to $LOG as one line. What it answers is
# decided by STUB_* variables; `exec` answers by looking at the command.
cat >"$WORK/bin/docker" <<'STUB'
#!/usr/bin/env bash
echo "$*" >>"$STUB_LOG"
case "$1" in
  info) [ "${STUB_DOCKER:-up}" = up ]; exit $? ;;
  image) [ "${STUB_IMAGE:-present}" = present ]; exit $? ;;
  pull) [ "${STUB_PULL:-ok}" = ok ]; exit $? ;;
  inspect) echo "${STUB_RUNNING:-true}"; exit 0 ;;
  run) [ "${STUB_RUN:-ok}" = ok ]; exit $? ;;
  cp) exit 0 ;;
  exec)
    all="$*"
    case "$all" in
      *max_user_watches*) echo "${STUB_WATCHES:-1048576}"; exit 0 ;;
      *"df -PT"*) echo "${STUB_FS:-overlay}"; exit 0 ;;
      *"tephrad.new --help"*)
        if [ "${STUB_GLIBC:-ok}" = bad ]; then
          echo "tephrad.new: /lib/aarch64-linux-gnu/libc.so.6: version \`GLIBC_2.39' not found (required by tephrad.new)" >&2
          exit 1
        fi
        echo "tephrad USAGE"; exit 0 ;;
      *"tephrad.pid 2>/dev/null"*)
        # running_pid: up until its pid was killed, and again once a new
        # daemon was started.
        if [ "${STUB_UP:-no}" = yes ]; then
          if grep -q 'kill 4242' "$STUB_LOG" && ! grep -q 'exec -d' "$STUB_LOG"; then exit 1; fi
          echo 4242; exit 0
        fi
        exit 1 ;;
      *"grep -o 'listening on"*)
        [ "${STUB_NOLISTEN:-0}" = 1 ] || echo "127.0.0.1:${STUB_PORT:-43111}"
        exit 0 ;;
      *"tail -n 3"*) echo "${STUB_START_ERROR:-}"; exit 0 ;;
      *"source list"*) echo "${STUB_SOURCES:-}"; exit 0 ;;
      *) exit 0 ;;
    esac ;;
  *) exit 0 ;;
esac
STUB
chmod +x "$WORK/bin/docker"

PASS=0
FAIL=0
export STUB_LOG="$LOG"

# run <args...>: runs the script with the stub on PATH; stdout in
# $WORK/stdout, stderr in $WORK/stderr, the exit code in $CODE.
run() {
  : >"$LOG"
  PATH="$WORK/bin:$PATH" LINUX_TEPHRAD_OUT="${OUT:-$WORK/out}" "$SCRIPT" "$@" >"$WORK/stdout" 2>"$WORK/stderr"
  CODE=$?
}

ok() {
  PASS=$((PASS + 1))
  echo "ok   - $1"
}

bad() {
  FAIL=$((FAIL + 1))
  echo "FAIL - $1"
  echo "       exit=$CODE"
  sed 's/^/       stdout: /' "$WORK/stdout"
  sed 's/^/       stderr: /' "$WORK/stderr"
  sed 's/^/       docker: /' "$LOG"
}

# expect <name> <exit> [pattern in stdout or stderr]
expect() {
  local name=$1 code=$2 pattern=${3:-}
  if [ "$CODE" != "$code" ]; then
    bad "$name (wanted exit $code)"
    return
  fi
  if [ -n "$pattern" ] && ! cat "$WORK/stdout" "$WORK/stderr" | grep -qi -- "$pattern"; then
    bad "$name (output lacks '$pattern')"
    return
  fi
  ok "$name"
}

# The whole failure is one line on stderr.
one_line() {
  local name=$1 lines
  lines=$(grep -c . "$WORK/stderr")
  if [ "$lines" = 1 ]; then ok "$name: one line on stderr"; else bad "$name: $lines lines on stderr"; fi
}

# Nothing that changes the container (or the image) reached Docker.
changed_nothing() {
  local name=$1
  if grep -Eq '^(run|cp|pull|volume|rm|stop) |exec -d|mkdir|rm -f|kill |mv -f' "$LOG"; then
    bad "$name: changed something"
  else
    ok "$name: changed nothing"
  fi
}

# in_log <name> <pattern>; not_in_log <name> <pattern>
in_log() { grep -q -- "$2" "$LOG" && ok "$1" || bad "$1"; }
not_in_log() { grep -q -- "$2" "$LOG" && bad "$1" || ok "$1"; }
in_stdout() { grep -q -- "$2" "$WORK/stdout" && ok "$1" || bad "$1"; }

# --- --help and usage -------------------------------------------------

run --help
expect "--help exits 0 and shows the usage" 0 "start <slug>"
for word in build start stop status test "--repeat" glibc; do
  grep -q -- "$word" "$WORK/stdout" && ok "--help mentions $word" || bad "--help mentions $word"
done
[ ! -s "$LOG" ] && ok "--help does not call docker" || bad "--help does not call docker"
run -h
expect "-h is --help" 0 "USAGE"

run
expect "no arguments is a usage error" 2 "USAGE"
run frobnicate
expect "an unknown command is a usage error" 2 "unknown command"
run build extra
expect "build with an argument is a usage error" 2
run start
expect "start with no slug" 2 "slug"
run stop
expect "stop with no slug" 2 "slug"
run status
expect "status with no slug" 2 "slug"
run start "../etc"
expect "a slug with a path in it is refused" 2 "not a valid slug"
run start ".hidden"
expect "a slug starting with a dot is refused" 2 "not a valid slug"
run start "a b"
expect "a slug with a space is refused" 2 "not a valid slug"
run stop s extra
expect "stop with a second argument" 2
run start s --port
expect "--port without a value" 2 "--port"
run start s --port abc
expect "--port with letters" 2 "1 to 65535"
run start s --port 70000
expect "--port above 65535" 2 "1 to 65535"
run start s --source relative/dir
expect "--source must be absolute" 2 "absolute"
run start s --source /a --no-source
expect "--source with --no-source" 2 "together"
run start s --bogus
expect "start with an unknown option" 2 "unknown option"
run test --repeat abc s
expect "--repeat with letters" 2 "whole number"
run test --repeat 0 s
expect "--repeat 0" 2 "whole number"
run test --repeat
expect "--repeat without a value" 2 "--repeat"
run test --nope s
expect "test with an unknown option" 2 "unknown option"
run test a b
expect "test with two slugs" 2 "one slug"
run test
expect "test with no slug and no LINUX_TEPHRAD_SLUG" 2 "slug"
[ ! -s "$LOG" ] && ok "usage errors never reach docker" || bad "usage errors never reach docker"

# --- Docker not running: one line, nothing changed ----------------------

export STUB_DOCKER=down
for cmd in "build" "start s" "stop s" "status s" "test s"; do
  # shellcheck disable=SC2086
  run $cmd
  expect "docker down: $cmd" 1 "Docker is not running"
  one_line "docker down: $cmd"
  changed_nothing "docker down: $cmd"
done
unset STUB_DOCKER

# --- the image cannot be pulled ----------------------------------------

export STUB_IMAGE=missing STUB_PULL=fail
run build
expect "pull fails: build" 1 "cannot pull"
one_line "pull fails: build"
not_in_log "pull fails: no container was run" '^run '
unset STUB_IMAGE STUB_PULL

# --- the build itself fails --------------------------------------------

export STUB_RUN=fail
run build
expect "cargo fails: build" 1 "build failed"
one_line "cargo fails: build"
unset STUB_RUN

# --- start: refusals that change nothing -------------------------------

export STUB_RUNNING=false
run start s
expect "container not running" 1 "desktop-s is not running"
one_line "container not running"
changed_nothing "container not running"
unset STUB_RUNNING

OUT="$WORK/empty-out"
mkdir -p "$OUT"
run start s
expect "no built binaries" 1 "build"
changed_nothing "no built binaries"
unset OUT

# Fake binaries so start gets past that check.
printf '#!/bin/sh\n' >"$WORK/out/tephrad"
printf '#!/bin/sh\n' >"$WORK/out/tephractl"
chmod +x "$WORK/out/tephrad" "$WORK/out/tephractl"

export STUB_FS=virtiofs
run start s --source /config/workspace/vault
expect "a source on a bind mount is refused" 1 "bind mount"
one_line "bind mount"
changed_nothing "bind mount"
run start s
expect "the run dir on a bind mount is refused" 1 "virtiofs"
changed_nothing "run dir on a bind mount"
unset STUB_FS

export STUB_WATCHES=100
run start s
expect "inotify limit too low names the sysctl" 1 "fs.inotify.max_user_watches"
one_line "inotify limit"
changed_nothing "inotify limit"
unset STUB_WATCHES

# glibc: the binary is tried before anything is replaced or started.
export STUB_GLIBC=bad
run start s
expect "a binary that needs a newer glibc" 1 "glibc"
in_stdout_or_err() { cat "$WORK/stdout" "$WORK/stderr" | grep -q -- "$2" && ok "$1" || bad "$1"; }
in_stdout_or_err "the glibc message is shown" "GLIBC_2.39"
not_in_log "glibc: no daemon was started" 'exec -d'
not_in_log "glibc: nothing was signalled" 'kill '
in_log "glibc: the unusable copy was removed" 'rm -f .*tephrad.new'
unset STUB_GLIBC

# --- start: the happy path, and twice ----------------------------------

export STUB_UP=no
run start s --no-source
expect "start prints where it is" 0 "127.0.0.1:43111"
in_stdout "start prints the docker exec command" 'docker exec'
in_stdout "start names the container" 'desktop-s'
in_stdout "start names the token path" 'xdg-data/tephra/token'
in_log "start runs tephrad in the background" 'exec -d'
grep -c 'exec -d' "$LOG" | grep -qx 1 && ok "one daemon started" || bad "one daemon started"
not_in_log "a first start signals nothing" 'kill '
in_log "the config listens on loopback" '127.0.0.1:%s'
in_log "its own XDG_DATA_HOME" 'XDG_DATA_HOME=/tmp/tephra-linux/s/xdg-data'
not_in_log "never the real ~/.local/share/tephra" '\.local/share/tephra'

# Again with the daemon up: its own pid is stopped, then one new daemon
# starts; never two.
export STUB_UP=yes
run start s --no-source
expect "a second start" 0 "127.0.0.1:43111"
in_log "the second start stops the first daemon's pid" 'kill 4242'
grep -c 'exec -d' "$LOG" | grep -qx 1 && ok "still one daemon started" || bad "still one daemon started"
not_in_log "only its own pid is signalled" 'kill [^4]'
unset STUB_UP

# A port in use: the daemon exits at start with the bind error.
export STUB_UP=no STUB_NOLISTEN=1 STUB_START_ERROR="tephrad: listener failed: Address already in use (os error 98)"
run start s --port 4812 --no-source
expect "a port in use is reported" 1 "port 4812 is in use"
one_line "port in use"
unset STUB_UP STUB_NOLISTEN STUB_START_ERROR

# --- stop and status ---------------------------------------------------

export STUB_UP=no
run stop s
expect "stop when it is not running" 0 "not running"
not_in_log "stop signals nothing when it is down" 'kill '
run status s
expect "status when down" 3 "down"
export STUB_UP=yes
run status s
expect "status when up" 0 "up in desktop-s"
run stop s
expect "stop when it is running" 0 "stopped"
in_log "stop signals the run's own pid" 'kill 4242'
not_in_log "stop signals only that pid" 'kill [^4]'
unset STUB_UP

# --- test --------------------------------------------------------------

run test s --repeat 7
expect "test runs" 0
in_log "test passes --repeat on" 'RACE_REPEAT=7'
in_log "test runs the six race_ tests by default" 'rename_races race_'
run test s --all
not_in_log "--all drops the filter" 'rename_races race_'
LINUX_TEPHRAD_SLUG=s run test
in_log "test takes the slug from LINUX_TEPHRAD_SLUG" 'desktop-s'

echo
echo "$PASS passed, $FAIL failed"
[ "$FAIL" = 0 ]
