#!/usr/bin/env bash
# Durable execution PR6.1 pause/resume lifecycle CLI gate.
# Seeded interrupted run -> run.pause -> run.get shows paused/resume_allowed=true
# -> run.resume -> run.get shows recovering -> run.auto_resume toggle -> run.get
# actions reflect reason. All PIDs are created and cleaned by this script.
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
ROOT="$(mktemp -d /tmp/ion-durable-pause-XXXXXX)"
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
    HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock" timeout 10 "$ION_BIN" rpc "$@" \
        2>"$ROOT/rpc-error.log"
}
start_host() {
    rm -f "$ROOT/host.sock"
    (
        export HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock"
        cd "$PROJECT" && exec "$ION_BIN" serve --provider faux --model faux-test
    ) >"$ROOT/$1.log" 2>&1 &
    HOST_PID=$!
    for _ in $(seq 1 40); do
        sleep 0.25
        rpc --method list_sessions >/dev/null 2>&1 && return 0
    done
    return 1
}

echo '[build]'
if ~/.cargo/bin/cargo build --bin ion >/dev/null 2>&1; then
    pass 'cargo build --bin ion'
else
    fail 'cargo build --bin ion'; exit 1
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
CREATE UNIQUE INDEX one_live_run_per_branch ON runs(session_id,branch_id) WHERE state NOT IN ('succeeded','failed','cancelled');
""")
conn.execute("INSERT INTO store_meta VALUES(1,'exec-pause-ci',1,0)")
run_id="run-pause-ci-001"; session_id="sess-pause-ci-001"
conn.execute("INSERT INTO runs VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (
    run_id,session_id,"main","running",None,0,0,1,None,"running",1,
    json.dumps({"prompt":"seed"}),json.dumps({}),now,now))
conn.execute("INSERT INTO run_events VALUES(?,?,?,?,?)", (run_id,0,0,"run.created",json.dumps({"runId":run_id})))
conn.commit(); conn.close()
PY

echo '[A] run.pause records reason and exposes resumable action'
start_host a || { fail 'host starts'; tail -40 "$ROOT/a.log"; exit 1; }
pass 'host starts'

RUN_ID='run-pause-ci-001'
BEFORE=$(rpc --method run.get --params "{\"runId\":\"$RUN_ID\"}")
[ "$(jget "$BEFORE" 'd["data"]["state"]')" = 'running' ] &&
    pass 'seeded run starts as running' || fail "seeded state: $BEFORE"
[ "$(jget "$BEFORE" 'd["data"]["actions"]["resume_allowed"]')" = 'False' ] &&
    pass 'running run is not resumable' || fail "running actions: $BEFORE"

PAUSED=$(rpc --method run.pause --params "{\"runId\":\"$RUN_ID\",\"reason\":\"simulated kill -9 for CI\"}")
[ "$(jget "$PAUSED" 'd["data"]["paused"]')" = 'True' ] &&
    pass 'run.pause returns paused=true' || fail "pause response: $PAUSED"
NOW=$(rpc --method run.get --params "{\"runId\":\"$RUN_ID\"}")
[ "$(jget "$NOW" 'd["data"]["state"]')" = 'paused' ] &&
    pass 'run.get shows paused' || fail "paused state: $NOW"
[ "$(jget "$NOW" 'd["data"]["reason_code"]')" = 'simulated kill -9 for CI' ] &&
    pass 'pause reason recorded' || fail "pause reason: $NOW"
[ "$(jget "$NOW" 'd["data"]["actions"]["resume_allowed"]')" = 'True' ] &&
    pass 'paused run exposes resume_allowed=true' || fail "resumable action: $NOW"

echo '[B] run.resume returns to recovering'
RESUMED=$(rpc --method run.resume --params "{\"runId\":\"$RUN_ID\"}")
[ "$(jget "$RESUMED" 'd["data"]["state"]')" = 'recovering' ] &&
    pass 'run.resume returns recovering' || fail "resume response: $RESUMED"
NOW=$(rpc --method run.get --params "{\"runId\":\"$RUN_ID\"}")
[ "$(jget "$NOW" 'd["data"]["state"]')" = 'recovering' ] &&
    pass 'run.get shows recovering' || fail "recovering state: $NOW"
[ "$(jget "$NOW" 'd["data"]["reason_code"]')" = 'null' ] &&
    pass 'reason cleared after resume' || fail "reason after resume: $NOW"

echo '[C] run.auto_resume toggle reflects in actions'
OFF=$(rpc --method run.auto_resume --params "{\"runId\":\"$RUN_ID\",\"enabled\":false}")
[ "$(jget "$OFF" 'd["data"]["autoResume"]')" = 'False' ] &&
    pass 'auto_resume toggle accepted' || fail "auto_resume response: $OFF"
NOW=$(rpc --method run.get --params "{\"runId\":\"$RUN_ID\"}")
[ "$(jget "$NOW" 'd["data"]["actions"]["resume_allowed"]')" = 'False' ] &&
    pass 'auto_resume=false disables resume action' || fail "auto_resume actions: $NOW"
echo "$NOW" | grep -q '"reason":"auto_resume is disabled' &&
    pass 'reason explains auto_resume disabled' || fail "auto_resume reason: $NOW"
rpc --method run.auto_resume --params "{\"runId\":\"$RUN_ID\",\"enabled\":true}" >/dev/null
NOW=$(rpc --method run.get --params "{\"runId\":\"$RUN_ID\"}")
[ "$(jget "$NOW" 'd["data"]["actions"]["resume_allowed"]')" = 'True' ] &&
    pass 'auto_resume=true restores resume action' || fail "restored actions: $NOW"

echo '[D] pause->restart->scheduler auto resume (full lifecycle)'
rpc --method run.pause --params "{\"runId\":\"$RUN_ID\",\"reason\":\"restart test\"}" >/dev/null
NOW=$(rpc --method run.get --params "{\"runId\":\"$RUN_ID\"}")
[ "$(jget "$NOW" 'd["data"]["state"]')" = 'paused' ] &&
    pass 'run paused before restart' || fail "restart pause: $NOW"
kill -9 "$HOST_PID"; wait "$HOST_PID" 2>/dev/null; HOST_PID=""
sleep 0.5
start_host restart || { fail 'host restarts after pause'; tail -40 "$ROOT/restart.log"; exit 1; }
pass 'host restarts after pause'
FINAL=$(rpc --method run.get --params "{\"runId\":\"$RUN_ID\"}")
STATE=$(jget "$FINAL" 'd["data"]["state"]')
[ "$STATE" = 'queued' ] || [ "$STATE" = 'recovering' ] &&
    pass 'scheduler picked up paused run after restart' || fail "restart state: $FINAL"

printf 'PASS: %s, FAIL: %s\n' "$PASS" "$FAIL"
test "$FAIL" -eq 0
