#!/usr/bin/env bash
# TUI Phase 1 smoke gate: real host + headless ion-tui prints run list.
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
TUI_BIN="${TUI_BIN:-$PROJECT_DIR/target/debug/ion-tui}"
ROOT="$(mktemp -d /tmp/ion-tui-smoke-XXXXXX)"
HOME_DIR="$ROOT/home"
PROJECT="$ROOT/project"
DB="$HOME_DIR/.ion/execution.sqlite3"
HOST_PID=""
PASS=0
FAIL=0

pass() { printf '  ok  %s\n' "$1"; PASS=$((PASS + 1)); }
fail() { printf '  FAIL %s\n' "$1"; FAIL=$((FAIL + 1)); }
cleanup() {
    [ -n "$HOST_PID" ] && kill -9 "$HOST_PID" >/dev/null 2>&1
    [ -n "$HOST_PID" ] && wait "$HOST_PID" >/dev/null 2>&1
    rm -rf "$ROOT"
}
trap cleanup EXIT

jget() {
    python3 -c '
import json,sys
try:
  d=json.loads(sys.argv[1]); print(eval(sys.argv[2]))
except Exception:
  print("null")
' "$1" "$2" 2>/dev/null
}
rpc() {
    HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock" timeout 10 "$ION_BIN" rpc "$@" 2>/dev/null
}
start_host() {
    rm -f "$ROOT/host.sock"
    (
        export HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock"
        cd "$PROJECT" && exec "$ION_BIN" serve --provider faux --model faux-test
    ) >"$ROOT/host.log" 2>&1 &
    HOST_PID=$!
    for _ in $(seq 1 40); do
        sleep 0.25
        rpc --method list_sessions >/dev/null 2>&1 && return 0
    done
    return 1
}

echo '[build]'
if ~/.cargo/bin/cargo build --bin ion >/dev/null 2>&1 && ~/.cargo/bin/cargo build -p ion-tui >/dev/null 2>&1; then
    pass 'cargo build ion + ion-tui'
else
    fail 'build'; exit 1
fi

mkdir -p "$HOME_DIR/.ion" "$PROJECT"
cat >"$HOME_DIR/.ion/config.json" <<'JSON'
{
  "default_provider": "faux",
  "default_model": "faux-test",
  "runtime": {"durable_execution": true},
  "extensions": {
    "global-memory": {"enabled": false},
    "memory": {"enabled": false},
    "learning": {"enabled": false}
  }
}
JSON

python3 - "$DB" <<'PY'
import json, os, sqlite3, sys, time
db = sys.argv[1]
os.makedirs(os.path.dirname(db), exist_ok=True)
conn = sqlite3.connect(db); now = int(time.time()*1000)
conn.executescript("""
CREATE TABLE store_meta(singleton INTEGER PRIMARY KEY CHECK(singleton=1), store_id TEXT NOT NULL, schema_version INTEGER NOT NULL, host_epoch INTEGER NOT NULL);
CREATE TABLE runs(run_id TEXT PRIMARY KEY,session_id TEXT NOT NULL,branch_id TEXT NOT NULL,state TEXT NOT NULL,reason_code TEXT,version INTEGER NOT NULL,generation INTEGER NOT NULL,next_event_seq INTEGER NOT NULL,recovery_plan_id TEXT,desired_state TEXT NOT NULL,auto_resume INTEGER NOT NULL,input_json TEXT NOT NULL,env_json TEXT NOT NULL,created_ms INTEGER NOT NULL,updated_ms INTEGER NOT NULL);
CREATE TABLE tasks(task_id TEXT PRIMARY KEY,run_id TEXT NOT NULL,parent_task_id TEXT,logical_key TEXT NOT NULL,kind TEXT NOT NULL,state TEXT NOT NULL,version INTEGER NOT NULL,checkpoint_version INTEGER NOT NULL,checkpoint_json TEXT NOT NULL,result_json TEXT,policy_json TEXT NOT NULL,UNIQUE(run_id,logical_key));
CREATE TABLE attempts(attempt_id TEXT PRIMARY KEY,task_id TEXT NOT NULL,attempt_no INTEGER NOT NULL,host_epoch INTEGER NOT NULL,generation INTEGER NOT NULL,fence INTEGER NOT NULL,worker_id TEXT,phase TEXT NOT NULL,started_ms INTEGER,lease_deadline_ms INTEGER,outcome_json TEXT,UNIQUE(task_id,attempt_no));
CREATE TABLE resume_ops(operation_id TEXT PRIMARY KEY,principal_id TEXT NOT NULL,run_id TEXT NOT NULL,request_id TEXT NOT NULL,request_hash TEXT NOT NULL,state TEXT NOT NULL,accepted_json TEXT NOT NULL,accepted_version INTEGER NOT NULL,UNIQUE(principal_id,run_id,request_id));
CREATE TABLE run_events(run_id TEXT NOT NULL,seq INTEGER NOT NULL,run_version INTEGER NOT NULL,kind TEXT NOT NULL,payload_json TEXT NOT NULL,PRIMARY KEY(run_id,seq));
CREATE TABLE outbox(outbox_id TEXT PRIMARY KEY,run_id TEXT NOT NULL,effect_key TEXT UNIQUE,kind TEXT NOT NULL,payload_json TEXT NOT NULL,delivered_ms INTEGER);
CREATE TABLE transcript_entries(entry_id TEXT PRIMARY KEY,run_id TEXT NOT NULL,ordinal INTEGER NOT NULL,payload_json TEXT NOT NULL,UNIQUE(run_id,ordinal));
CREATE TABLE task_edges(parent_id TEXT NOT NULL,child_id TEXT NOT NULL,edge_key TEXT NOT NULL,ownership TEXT NOT NULL,wait_policy TEXT NOT NULL,PRIMARY KEY(parent_id,edge_key),UNIQUE(parent_id,child_id));
CREATE TABLE queued_inputs(input_id TEXT PRIMARY KEY,run_id TEXT NOT NULL,payload_json TEXT NOT NULL,consumed_by_task TEXT);
CREATE TABLE approvals(approval_id TEXT PRIMARY KEY,run_id TEXT NOT NULL,task_id TEXT NOT NULL,action_digest TEXT NOT NULL,generation INTEGER NOT NULL,policy_version TEXT NOT NULL,status TEXT NOT NULL,expires_ms INTEGER,decision_json TEXT,version INTEGER NOT NULL);
""")
conn.execute("INSERT INTO store_meta VALUES(1,'exec-tui-ci',1,0)")
run_id='run-tui-ci-001'
conn.execute("INSERT INTO runs VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (
    run_id,'sess-tui-ci-001','main','running',None,0,0,1,None,'running',1,
    json.dumps({}),json.dumps({}),now,now))
conn.commit(); conn.close()
PY

echo '[T1] headless ion-tui prints durable run list'
start_host || { fail 'host starts'; tail -30 "$ROOT/host.log"; exit 1; }
pass 'host starts'
OUT=$(HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock" ION_TUI_HEADLESS=1 timeout 10 "$TUI_BIN" 2>"$ROOT/tui.err")
if echo "$OUT" | grep -q 'run-tui-ci-001' && echo "$OUT" | grep -q 'running'; then
    pass 'headless TUI prints run list with seeded run'
else
    fail "headless output: $OUT / $(cat "$ROOT/tui.err")"
fi
echo "$OUT" | grep -q 'sess-tui-ci-001' && pass 'session id shown' || fail 'session id missing'

echo '[T2] pause then headless reflects new state'
rpc --method run.pause --params '{"runId":"run-tui-ci-001","reason":"tui smoke"}' >/dev/null
OUT=$(HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock" ION_TUI_HEADLESS=1 timeout 10 "$TUI_BIN" 2>/dev/null)
echo "$OUT" | grep -q 'paused' && pass 'paused state visible in headless TUI' || fail "state after pause: $OUT"

echo '[T3] interactive keys pause/resume via stdin script'
# Select first run (j), pause (p), then resume (r), then quit (q).
{ printf 'j\n'; sleep 1; printf 'p\n'; sleep 1; printf 'r\n'; sleep 2; printf 'q\n'; sleep 1; } | HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock" timeout 20 "$TUI_BIN" >"$ROOT/t3.out" 2>"$ROOT/t3.err"
# Reconnect windows can swallow the in-band readback line; host state is the source of truth.
STATE=$(rpc --method run.get --params '{"runId":"run-tui-ci-001"}')
[ "$(jget "$STATE" 'd["data"]["state"]')" = 'recovering' ] &&
    pass 'interactive keys drove pause+resume (host state recovering)' ||
    fail "host state after interactive keys: $STATE"
grep -qE 'run-tui-ci-001 .* (paused|recovering)' "$ROOT/t3.out" &&
    pass 'TUI reflected at least one write state' ||
    { fail "no write state reflected in TUI output: $(tail -5 "$ROOT/t3.out")"; }

echo '[T4] TUI pause -> SIGKILL host -> restart -> scheduler auto resume -> TUI reads new state'
rpc --method run.pause --params '{"runId":"run-tui-ci-001","reason":"tui kill recovery"}' >/dev/null
STATE=$(rpc --method run.get --params '{"runId":"run-tui-ci-001"}')
[ "$(jget "$STATE" 'd["data"]["state"]')" = 'paused' ] &&
    pass 'run paused via RPC before kill' || fail "pre-kill state: $STATE"
kill -9 "$HOST_PID"; wait "$HOST_PID" 2>/dev/null; HOST_PID=""
sleep 0.5
start_host recovery || { fail 'host restarts after TUI pause + SIGKILL'; tail -30 "$ROOT/recovery.log"; exit 1; }
pass 'host restarts after SIGKILL'
# Scheduler runs 200ms after startup; give it a window to auto-resume.
for _ in $(seq 1 20); do
    STATE=$(rpc --method run.get --params '{"runId":"run-tui-ci-001"}')
    S=$(jget "$STATE" 'd["data"]["state"]')
    [ "$S" = 'queued' ] || [ "$S" = 'recovering' ] && break
    sleep 0.5
done
[ "$S" = 'queued' ] || [ "$S" = 'recovering' ] &&
    pass 'scheduler auto-resumed paused run after restart' || fail "state after restart: $STATE"
# Now TUI must show the new state, not the stale pre-kill snapshot.
OUT=$(HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock" ION_TUI_HEADLESS=1 timeout 10 "$TUI_BIN" 2>"$ROOT/tui-recovery.err")
if echo "$OUT" | grep -qE 'run-tui-ci-001.*(queued|recovering|running)'; then
    pass 'TUI reads post-recovery state (not stale paused)'
else
    fail "TUI stale after recovery: $OUT / $(cat "$ROOT/tui-recovery.err")"
fi
echo "$OUT" | grep -q 'sess-tui-ci-001' && pass 'session id preserved across recovery' || fail 'session id lost'
echo "$OUT" | grep -vq 'paused' && pass 'stale paused label gone from TUI output' || fail 'stale paused still shown'

printf 'PASS: %s, FAIL: %s\n' "$PASS" "$FAIL"
test "$FAIL" -eq 0
