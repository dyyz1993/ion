#!/usr/bin/env bash
# Durable execution PR3 RPC gate.
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
TEST_ROOT="$(mktemp -d /tmp/ion-durable-XXXXXX)"
HOME_DIR="$TEST_ROOT/home"
SOCK="$TEST_ROOT/host.sock"
HOST_PID=""
PASS=0
FAIL=0

pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
cleanup() {
    [ -n "$HOST_PID" ] && kill "$HOST_PID" >/dev/null 2>&1
    [ -n "$HOST_PID" ] && wait "$HOST_PID" >/dev/null 2>&1
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

jget() {
    python3 -c '
import json, sys
try:
    d = json.loads(sys.argv[1])
    print(eval(sys.argv[2]))
except Exception:
    print("null")
' "$1" "$2" 2>/dev/null
}

rpc() {
    HOME="$HOME_DIR" ION_HOST_SOCKET="$SOCK" timeout 10 "$ION_BIN" rpc "$@" 2>/dev/null
}

mkdir -p "$HOME_DIR/.ion" "$TEST_ROOT/project"
cat > "$HOME_DIR/.ion/config.json" <<'JSON'
{
  "runtime": {"durable_execution": true},
  "extensions": {
    "global-memory": {"enabled": false},
    "memory": {"enabled": false},
    "learning": {"enabled": false}
  }
}
JSON

echo "[build]"
if ~/.cargo/bin/cargo build --bin ion >/dev/null 2>&1; then
    pass "cargo build --bin ion"
else
    fail "cargo build --bin ion"; exit 1
fi

echo "[host]"
(
    export HOME="$HOME_DIR" ION_HOST_SOCKET="$SOCK"
    cd "$TEST_ROOT/project" && exec "$ION_BIN" serve
) >"$TEST_ROOT/host.log" 2>&1 &
HOST_PID=$!
for _ in $(seq 1 20); do
    sleep 0.5
    if rpc --method list_sessions >/dev/null 2>&1; then
        break
    fi
done
if rpc --method list_sessions >/dev/null 2>&1; then
    pass "isolated host starts with durable execution"
else
    fail "isolated host starts with durable execution"
    tail -20 "$TEST_ROOT/host.log"
    exit 1
fi

# SQLite CLI is not required: use ion's existing execution store tests to seed a run.
# For CLI observability, a fresh ledger starts empty and errors are explicit.
NOT_FOUND=$(rpc --method run.get --params '{"runId":"missing"}')
[ "$(jget "$NOT_FOUND" 'd["success"]')" = "False" ] && pass "run.get missing is explicit error" ||
    fail "run.get missing: $NOT_FOUND"

DISABLED=$(HOME="$TEST_ROOT/disabled-home" ION_HOST_SOCKET="$SOCK" timeout 10 "$ION_BIN" rpc \
    --method run.get --params '{"runId":"missing"}' 2>/dev/null || true)
[ "$(jget "$DISABLED" 'd["error"]')" = "run not found: missing" ] &&
    pass "per-host config remains stable" || fail "disabled config: $DISABLED"

EVENTS=$(rpc --method events.subscribe --params '{"runId":"missing","afterCursor":{"seq":0}}')
[ "$(jget "$EVENTS" 'd["success"]')" = "False" ] && pass "events.subscribe validates run" ||
    fail "events.subscribe validates run: $EVENTS"

RESUME=$(rpc --method run.resume --params '{"runId":"missing","requestId":"r1","expectedStateVersion":0}')
[ "$(jget "$RESUME" 'd["success"]')" = "False" ] && pass "run.resume validates run" ||
    fail "run.resume validates run: $RESUME"
DURABLE_APPROVALS=$(rpc --method durable_approvals_pending --params '{}')
[ "$(jget "$DURABLE_APPROVALS" 'd["data"]["checkpointed"]')" = "True" ] &&
    pass "durable approvals bridge returns explicit checkpoint status" ||
    fail "durable approvals bridge: $DURABLE_APPROVALS"

RUN_LIST=$(rpc --method run.list --params '{}')
[ "$(jget "$RUN_LIST" 'd["success"]')" = "True" ] && [ "$(jget "$RUN_LIST" 'd["data"]')" = "[]" ] &&
    pass "run.list returns empty durable ledger" || fail "run.list: $RUN_LIST"

STREAM_MISSING=$(python3 - "$SOCK" <<'PYEOF'
import json, socket, sys
s=socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); s.connect(sys.argv[1])
req={"id":"sub-missing","method":"events.subscribe","params":{"runId":"missing"}}
s.sendall((json.dumps(req)+"\n").encode()); s.settimeout(2)
try: print(s.recv(65536).decode())
except Exception as e: print("STREAM_ERROR", e)
PYEOF
)
echo "$STREAM_MISSING" | grep -q '"success":false' && echo "$STREAM_MISSING" | grep -q 'run not found' &&
    pass "events.subscribe stream validates run" || fail "events.subscribe stream: $STREAM_MISSING"

if [ "$FAIL" -eq 0 ]; then
    echo "PASS: $PASS, FAIL: $FAIL"
    exit 0
fi
echo "PASS: $PASS, FAIL: $FAIL"
exit 1
