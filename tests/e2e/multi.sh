#!/usr/bin/env bash
# Several editor windows and several dev containers at once.
#
#   tests/e2e/multi.sh [path/to/zportforward]
#
# Checks that clients starting the daemon at the same moment get exactly one,
# that two containers wanting the same port both get a forward, that requests
# reach the right container, and that the ports page enforces its guards.
# It stops any running zportforward daemon first and leaves none running.
set -euo pipefail

BIN=$(realpath "${1:-target/debug/zportforward}")
HERE=$(cd "$(dirname "$0")" && pwd)
RT=${ZPORTFORWARD_RUNTIME:-docker}
A=zportforward-multi-a-$$
B=zportforward-multi-b-$$
WORK=$(mktemp -d)
export ZPORTFORWARD_UI_PORT=44801
UI=http://127.0.0.1:$ZPORTFORWARD_UI_PORT

pass() { echo "ok   - $*"; }
fail() { echo "FAIL - $*" >&2; "$BIN" ls >&2 || true; exit 1; }
cleanup() {
    "$RT" rm -f "$A" "$B" >/dev/null 2>&1 || true
    "$BIN" stop >/dev/null 2>&1 || true
    [ -d "$WORK" ] && find "$WORK" -delete
}
trap cleanup EXIT
eventually() {
    local deadline=$((SECONDS + $1)); shift
    until "$@" >/dev/null 2>&1; do
        [ $SECONDS -lt $deadline ] || return 1
        sleep 0.3
    done
}
says() { [ "$(curl -s -m 2 "http://127.0.0.1:$1/")" = "$2" ]; }
closed() { ! curl -s -m 2 -o /dev/null "http://127.0.0.1:$1/"; }
daemons() { ps -C zportforward -o args= | grep -c ' daemon' || true; }
mcp() { python3 "$HERE/mcp_call.py" "$BIN" "$@"; }
host_port() { # container name, container port -> host port
    "$BIN" ls --json | python3 -c '
import json, sys
name, port = sys.argv[1], int(sys.argv[2])
for c in json.load(sys.stdin)["containers"]:
    if c["name"] == name:
        print(next(f["host_port"] for f in c["forwards"] if f["port"] == port))' "$1" "$2"
}

# A project folder with a devcontainer.json, and a container serving its own
# name on the container's loopback, on ports 8099 (declared) and 8098.
project() { # name
    mkdir -p "$WORK/$1/.devcontainer"
    echo '{ "forwardPorts": [8099] }' > "$WORK/$1/.devcontainer/devcontainer.json"
    "$RT" run -d --name "$1" \
        --label "devcontainer.local_folder=$WORK/$1" \
        --label "devcontainer.config_file=$WORK/$1/.devcontainer/devcontainer.json" \
        busybox sh -c "mkdir -p /www && echo $1 > /www/index.html && httpd -p 127.0.0.1:8099 -h /www && httpd -p 127.0.0.1:8098 -h /www; sleep infinity" \
        >/dev/null
}

"$BIN" stop >/dev/null
project "$A"
project "$B"

# --- many clients start the daemon at the same moment
for _ in $(seq 12); do "$BIN" ls --json >/dev/null 2>&1 & done
wait
[ "$(daemons)" = 1 ] || fail "expected one daemon, found $(daemons)"
holders=$(ss -tlnp 2>/dev/null | grep '"zportforward"' | grep -oE 'pid=[0-9]+' | sort -u | wc -l)
[ "$holders" = 1 ] || fail "$holders processes hold zportforward ports"
pass "12 simultaneous clients start exactly one daemon"

# --- two containers declare the same port
eventually 30 says 8099 "$A" || eventually 5 says 8099 "$B" || fail "declared port 8099 not forwarded"
pa=$(host_port "$A" 8099); pb=$(host_port "$B" 8099)
[ "$pa" != "$pb" ] || fail "both containers got host port $pa"
eventually 10 says "$pa" "$A" || fail "host port $pa does not reach $A"
eventually 10 says "$pb" "$B" || fail "host port $pb does not reach $B"
pass "both containers forward their declared port 8099 (host ports $pa and $pb), each to itself"

# --- picking the container
(cd "$WORK/$A" && "$BIN" add 8098:18098 >/dev/null) || fail "cwd did not select the container"
says 18098 "$A" || fail "cwd-selected forward reached the wrong container"
if (cd / && "$BIN" add 8098 2>/dev/null); then fail "ambiguous request was accepted"; fi
(cd / && "$BIN" add 8098 2>&1 || true) | grep -q "$WORK/$B" || fail "ambiguity error does not list the project folders"
pass "the project directory selects the container; elsewhere the request is refused with the choices"

# --- three editor windows act at the same moment
(cd / && mcp forward_port "{\"port\":8098,\"host_port\":28098,\"container\":\"$B\"}" >/dev/null) &
(cd / && mcp forward_port "{\"port\":8099,\"host_port\":28099,\"container\":\"$A\"}" >/dev/null) &
(cd / && mcp forward_port "{\"port\":8099,\"host_port\":28100,\"container\":\"$B\"}" >/dev/null) &
wait
says 28098 "$B" || fail "window 1's forward is wrong"
says 28099 "$A" || fail "window 2's forward is wrong"
says 28100 "$B" || fail "window 3's forward is wrong"
[ "$(daemons)" = 1 ] || fail "expected one daemon, found $(daemons)"
(cd / && mcp list_ports | grep -q "$A") || fail "list_ports does not show $A"
if (cd / && mcp forward_port '{"port":8098}' >/dev/null 2>&1); then fail "agent request without a container was accepted"; fi
pass "three concurrent agent clients each forward to the right container"

# --- an editor configured with auto turns it on in the running daemon
"$BIN" ls --json | grep -q '"auto": false' || fail "daemon unexpectedly in auto mode"
(cd / && ZPORTFORWARD_AUTO=1 mcp list_ports >/dev/null)
eventually 5 sh -c "'$BIN' ls --json | grep -q '\"auto\": true'" || fail "auto mode was not turned on"
pass "a client with auto enabled switches the running daemon to auto mode"

# --- the ports page
post() { curl -s -m 5 -o /dev/null -w '%{http_code}' -X POST -H 'content-type: application/json' "$@"; }
curl -s -m 3 "$UI/" | grep -q "<title>zportforward ports</title>" || fail "ports page not served"
[ "$("$BIN" ui --no-open)" = "$UI/" ] || fail "zportforward ui prints the wrong address"
[ "$(post -H 'x-zportforward: 1' -d "{\"container\":\"$A\",\"port\":8098,\"host_port\":38098}" "$UI/api/forward")" = 200 ] || fail "forward through the page"
says 38098 "$A" || fail "page forward reached the wrong container"
[ "$(post -H 'x-zportforward: 1' -d "{\"container\":\"$A\",\"port\":8098}" "$UI/api/stop")" = 200 ] || fail "stop through the page"
closed 38098 || fail "port still open after stopping it on the page"
pass "the ports page lists, forwards and stops"
body="{\"container\":\"$A\",\"port\":8098,\"host_port\":38098}"
[ "$(post -d "$body" "$UI/api/forward")" = 403 ] || fail "POST without the X-Zportforward header was accepted"
[ "$(post -H 'x-zportforward: 1' -H 'Origin: http://evil.example' -d "$body" "$UI/api/forward")" = 403 ] || fail "cross-origin POST was accepted"
[ "$(post -H 'x-zportforward: 1' -H 'Host: evil.example' -d "$body" "$UI/api/forward")" = 403 ] || fail "foreign Host header was accepted"
[ "$(curl -s -m 3 -o /dev/null -w '%{http_code}' -H 'Host: evil.example' "$UI/api/state")" = 403 ] || fail "state readable under a foreign Host"
closed 38098 || fail "a refused request still opened a port"
pass "the page refuses cross-site and rebinding requests"

exposed=$(ss -tlnp 2>/dev/null | grep '"zportforward"' | grep -vc '127\.0\.0\.1:' || true)
[ "$exposed" = 0 ] || fail "$exposed listener(s) not bound to 127.0.0.1"
pass "every listener, including the page, is bound to 127.0.0.1"

# --- one container goes away
"$RT" rm -f "$A" >/dev/null
eventually 15 closed 28099 || fail "forward of the removed container still open"
says 28098 "$B" || fail "the other container's forward broke"
pass "removing one container closes only its forwards"

"$BIN" stop >/dev/null
[ "$(daemons)" = 0 ] || fail "daemon still running after stop"
echo "all multi-window checks passed"
