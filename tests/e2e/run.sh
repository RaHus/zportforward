#!/usr/bin/env bash
# End-to-end test against a real container runtime.
#
#   tests/e2e/run.sh [path/to/zportforward]
#
# Starts a throwaway container labelled as a dev container, runs the daemon
# with --auto, and checks forwarding, removal, restart recovery and cleanup.
# It stops any running zportforward daemon first and leaves none running.
set -euo pipefail

BIN=$(realpath "${1:-target/debug/zportforward}")
RT=${ZPORTFORWARD_RUNTIME:-docker}
NAME=zportforward-e2e-$$
LOG=$(mktemp)

pass() { echo "ok   - $*"; }
fail() { echo "FAIL - $*" >&2; echo "--- daemon log" >&2; cat "$LOG" >&2; exit 1; }
cleanup() {
    "$RT" rm -f "$NAME" >/dev/null 2>&1 || true
    "$BIN" stop >/dev/null 2>&1 || true
    rm -f "$LOG"
}
trap cleanup EXIT

# Retry a command for up to $1 seconds.
eventually() {
    local deadline=$((SECONDS + $1)); shift
    until "$@" >/dev/null 2>&1; do
        [ $SECONDS -lt $deadline ] || return 1
        sleep 0.3
    done
}
serves() { [ "$(curl -s -m 2 "http://127.0.0.1:$1/")" = e2e-ok ]; }
closed() { ! curl -s -m 2 -o /dev/null "http://127.0.0.1:$1/"; }

"$BIN" stop >/dev/null
"$BIN" daemon --auto 2>"$LOG" &

# Two servers bound to the container's loopback only.
"$RT" run -d --name "$NAME" --label "devcontainer.local_folder=/nonexistent/$NAME" busybox sh -c \
    'mkdir -p /www && echo e2e-ok > /www/index.html && httpd -p 127.0.0.1:8099 -h /www && httpd -p 127.0.0.1:8098 -h /www; sleep infinity' \
    >/dev/null

eventually 30 serves 8099 || fail "auto-forward of a detected port"
pass "detected loopback-only port is auto-forwarded"

"$BIN" add 8098:18098 -c "$NAME" >/dev/null
eventually 5 serves 18098 || fail "manual forward on a chosen host port"
closed 8098 || fail "old host port still open after re-adding on another"
pass "manual add on a chosen host port replaces the auto forward"

"$BIN" rm 8099 -c "$NAME" >/dev/null
closed 8099 || fail "port still open after rm"
sleep 3
closed 8099 || fail "removed port came back"
pass "removed port stays removed"

"$RT" exec "$NAME" httpd -p 127.0.0.1:8097 -h /www
eventually 10 serves 8097 || fail "auto-forward of a listener started later"
"$RT" exec "$NAME" sh -c 'kill $(pgrep -f "[h]ttpd -p 127.0.0.1:8097")'
eventually 10 closed 8097 || fail "auto forward not closed when its listener went away"
pass "auto forwards follow listeners appearing and disappearing"

"$RT" restart -t 1 "$NAME" >/dev/null
eventually 30 serves 18098 || fail "manual forward did not recover after container restart"
pass "manual forward survives a container restart"

if command -v ss >/dev/null; then
    exposed=$(ss -tlnp 2>/dev/null | grep '"zportforward"' | grep -vc '127\.0\.0\.1:' || true)
    [ "$exposed" = 0 ] || fail "$exposed listener(s) not bound to 127.0.0.1"
    pass "every host listener is bound to 127.0.0.1"
fi

"$RT" rm -f "$NAME" >/dev/null
eventually 15 closed 18098 || fail "forward still open after the container was removed"
"$BIN" ls --json | grep -q "$NAME" && fail "removed container still listed"
pass "forwards close when the container is removed"

# Second phase: without --auto nothing is watched; ports are scanned on request.
"$BIN" stop >/dev/null
"$BIN" daemon 2>>"$LOG" &
"$RT" run -d --name "$NAME" --label "devcontainer.local_folder=/nonexistent/$NAME" busybox sh -c \
    'mkdir -p /www && echo e2e-ok > /www/index.html && httpd -p 127.0.0.1:8099 -h /www; sleep infinity' \
    >/dev/null
detected() { "$BIN" ls --json | tr -d ' \n' | grep -q "\"detected\":\[{\"port\":$1,"; }
eventually 30 detected 8099 || fail "listening port not reported on request"
closed 8099 || fail "port forwarded although auto mode is off"
pass "without --auto, a listening port is reported but not forwarded"

"$RT" exec "$NAME" httpd -p 127.0.0.1:8098 -h /www
"$BIN" ls --json | tr -d ' \n' | grep -q '"port":8098,"loopback":true' || fail "new listener missing from the next ls"
pass "a listener started later shows up in the next ls"

"$BIN" add 8099 -c "$NAME" >/dev/null
eventually 5 serves 8099 || fail "manual forward without --auto"
"$BIN" ls --json | tr -d ' \n' | grep -q '"port":8099,"host_port":8099,"origin":"manual","listening":true' \
    || fail "forward not reported as listening"
pass "manual forward works and is reported as listening"

echo "all end-to-end checks passed"
