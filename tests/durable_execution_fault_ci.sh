#!/usr/bin/env bash
# Durable execution crash/fault gate. All PIDs are created by this script.
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
ROOT="$(mktemp -d /tmp/ion-durable-fault-XXXXXX)"
PASS=0
FAIL=0
HOST_PID=""
function pass { printf "  ok %s\n" "$1"; PASS=$((PASS + 1)); }
function fail { printf "  FAIL %s\n" "$1"; FAIL=$((FAIL + 1)); }

cleanup() {
    [ -n "$HOST_PID" ] && kill -9 "$HOST_PID" >/dev/null 2>&1
    [ -n "$HOST_PID" ] && wait "$HOST_PID" >/dev/null 2>&1
    rm -rf "$ROOT"
}
trap cleanup EXIT

jget() { python3 -c 'import json,sys; d=json.loads(sys.argv[1]); print(eval(sys.argv[2]))' "$1" "$2" 2>/dev/null; }
rpc() {
    HOME="$ROOT/home" ION_HOST_SOCKET="$ROOT/host.sock" timeout 10 "$ION_BIN" rpc "$@" 2>/dev/null
}
start_host() {
    (
        export HOME="$ROOT/home" ION_HOST_SOCKET="$ROOT/host.sock"
        cd "$ROOT/project" && exec "$ION_BIN" serve
    ) >"$ROOT/host-$1.log" 2>&1 &
    HOST_PID=$!
    for _ in $(seq 1 20); do
        sleep 0.5
        rpc --method list_sessions >/dev/null 2>&1 && return 0
    done
    return 1
}

echo '[build]'
~/.cargo/bin/cargo build --bin ion >/dev/null 2>&1 || { fail 'build ion'; exit 1; }
pass 'build ion'

mkdir -p "$ROOT/home/.ion" "$ROOT/project"
cat >"$ROOT/home/.ion/config.json" <<'JSON'
{
  "runtime": {"durable_execution": true},
  "extensions": {"global-memory":{"enabled":false},"memory":{"enabled":false},"learning":{"enabled":false}}
}
JSON

echo '[F1] host crash preserves empty ledger identity'
start_host f1a || { fail 'first host starts'; exit 1; }
pass 'first host starts'
LIST1=$(rpc --method run.list --params '{}')
kill -9 "$HOST_PID"; wait "$HOST_PID" 2>/dev/null; HOST_PID=""
sleep 0.5
start_host f1b || { fail 'second host starts'; exit 1; }
pass 'second host starts after SIGKILL'
LIST2=$(rpc --method run.list --params '{}')
[ "$(jget "$LIST2" 'd["success"]')" = "True" ] && [ "$(jget "$LIST2" 'd["data"]')" = "[]" ] &&
    pass 'ledger survives host restart' || fail "ledger after restart: ${LIST2}"

echo '[F2] truncated ledger is fail-closed'
kill -9 "$HOST_PID"; wait "$HOST_PID" 2>/dev/null; HOST_PID=""
DB="$ROOT/home/.ion/execution.sqlite3"
rm -f "$DB-wal" "$DB-shm" 2>/dev/null || true
python3 - "$DB" <<'PY'
from pathlib import Path
import sys
p=Path(sys.argv[1]); b=p.read_bytes(); p.write_bytes(b[:32])
PY
start_host f2 || true
OUT=$(rpc --method run.list --params '{}' 2>/dev/null || true)
if [ "$(jget "$OUT" 'd["success"]')" = "False" ]; then
    pass 'truncated ledger reports storage error'
else
    fail 'truncated ledger response'
fi

printf 'PASS: %s, FAIL: %s' "$PASS" "$FAIL"
test "$FAIL" -eq 0
