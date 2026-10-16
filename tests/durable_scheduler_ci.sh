#!/usr/bin/env bash
# Durable scheduler E2E: SIGKILL host, restart, then the original session/run
# is resumed exactly once and converges to succeeded. All PIDs are script-owned.
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
ROOT="$(mktemp -d /tmp/ion-durable-scheduler-XXXXXX)"
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
        export HOME="$HOME_DIR" ION_HOST_SOCKET="$ROOT/host.sock" ION_FAUX_SCRIPT="$ROOT/faux.jsonl"
        cd "$PROJECT" && exec "$ION_BIN" serve --provider faux --model faux-test
    ) >"$ROOT/$1.log" 2>&1 &
    HOST_PID=$!
    for _ in $(seq 1 40); do
        sleep 0.25
        rpc --method list_sessions >/dev/null 2>&1 && return 0
    done
    return 1
}
wait_state() {
    local run_id="$1" expected="$2" out=""
    for _ in $(seq 1 80); do
        out=$(rpc --method run.get --params "{\"runId\":\"$run_id\"}")
        [ "$(jget "$out" 'd["data"]["state"]')" = "$expected" ] && { printf '%s' "$out"; return 0; }
        sleep 0.25
    done
    printf '%s' "$out"
    return 1
}

echo '[build]'
if cargo build --bin ion >/dev/null 2>&1; then
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

# The resumed worker's first model response reads this file. It must exist
# before the interrupted attempt so replay-safe recovery has real prior work.
echo 'durable-before-kill' >"$PROJECT/input.txt"
cat >"$ROOT/faux.jsonl" <<'JSONL'
{"tool_call":{"name":"read","input":{"file_path":"input.txt"}}}
{"text":"DURABLE_SCHEDULER_DONE"}
JSONL

# Seed a realistic interrupted durable graph through the product store schema.
# run: non-terminal and auto-resumable; read task: interrupted while executing.
python3 - "$DB" "$PROJECT" <<'PY'
import json, os, sqlite3, sys, time
db, cwd = sys.argv[1:]
os.makedirs(os.path.dirname(db), exist_ok=True)
conn = sqlite3.connect(db)
store_id = 'exec-scheduler-ci'; now = int(time.time()*1000)
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
CREATE UNIQUE INDEX one_active_attempt ON attempts(task_id) WHERE phase IN ('claimed','executing');
CREATE UNIQUE INDEX one_active_resume ON resume_ops(run_id) WHERE state IN ('accepted','reconciling','dispatching');
""")
conn.execute("INSERT INTO store_meta VALUES(1,?,1,0)", (store_id,))
run_id="run-scheduler-ci-001"; session_id="sess-scheduler-ci-001"
conn.execute("INSERT INTO runs VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (
    run_id,session_id,"main","running",None,0,0,1,None,"running",1,
    json.dumps({"prompt":"read input and finish"}),json.dumps({}),now,now
))
checkpoint=json.dumps({"arguments":{"file_path":"input.txt"},"name":"read","callId":"call-before-kill"})
conn.execute("INSERT INTO tasks VALUES(?,?,?,?,?,?,?,?,?,?,?)", (
    "task-read-before-kill",run_id,None,"call-before-kill:0","tool","running",0,0,
    checkpoint,None,"\"replay_safe\""
))
conn.execute("INSERT INTO attempts VALUES(?,?,?,?,?,?,?,?,?,?,?)", (
    "attempt-before-kill","task-read-before-kill",0,0,0,0,"old-host-worker","executing",
    now,now+60000,None
))
conn.execute("INSERT INTO run_events VALUES(?,?,?,?,?)", (run_id,0,0,"run.created",json.dumps({"runId":run_id})))
conn.commit(); conn.close()
PY

echo '[S1] SIGKILL -> restart -> auto resume -> succeeded'
start_host before-kill || { fail 'host starts before SIGKILL'; tail -40 "$ROOT/before-kill.log"; exit 1; }
pass 'host starts before SIGKILL'

# Give scheduler 500ms injection delay plus worker startup margin.
sleep 2
kill -9 "$HOST_PID"; wait "$HOST_PID" 2>/dev/null; HOST_PID=""
pass 'host SIGKILL after durable worker started'

start_host after-kill || { fail 'host restarts after SIGKILL'; tail -40 "$ROOT/after-kill.log"; exit 1; }
pass 'host restarts after SIGKILL'

RUN_ID='run-scheduler-ci-001'
FINAL=$(wait_state "$RUN_ID" succeeded) ||
    { fail 'run converges to succeeded'; rpc --method run.get --params "{\"runId\":\"$RUN_ID\"}"; tail -80 "$ROOT/after-kill.log"; exit 1; }
pass 'run converges to succeeded'
[ "$(jget "$FINAL" 'd["data"]["session_id"]')" = 'sess-scheduler-ci-001' ] &&
    pass 'original sessionId preserved' || fail "sessionId changed: $FINAL"

TOOL_COMPLETED=$(python3 - "$DB" "$RUN_ID" <<'PY'
import sqlite3,sys
c=sqlite3.connect(sys.argv[1])
print(sum(1 for (x,) in c.execute("select payload_json from run_events where run_id=? and kind='tool.completed'", (sys.argv[2],))))
PY
)
[ "$TOOL_COMPLETED" -eq 1 ] && pass 'tool completed exactly once' ||
    fail "tool.completed count=$TOOL_COMPLETED"

# The worker may be gone by the time list_workers is polled, so the durable
# side effect proves dispatch: the second model turn only exists after restart.
FINAL_TEXT=$(python3 - "$DB" "$RUN_ID" <<'PY'
import sqlite3,sys
c=sqlite3.connect(sys.argv[1])
print(c.execute("select count(*) from tasks where run_id=? and state='succeeded'", (sys.argv[2],)).fetchone()[0])
PY
)
[ "$FINAL_TEXT" -ge 1 ] && pass 'scheduler resumed original run' ||
    { fail 'scheduler produced no resumed work'; tail -80 "$ROOT/after-kill.log"; }

echo '[S2] unsafe interrupted effect remains blocked'
# Seed an unverified shell effect that reached executing before SIGKILL. Reopen
# must mark it uncertain, block the run, and prevent scheduler dispatch.
UNSAFE_ID='run-scheduler-unsafe-001'
python3 - "$DB" "$UNSAFE_ID" <<'PY'
import json, sqlite3, sys, time
db, run_id = sys.argv[1:]
conn=sqlite3.connect(db); now=int(time.time()*1000)
conn.execute("INSERT INTO runs VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)", (
    run_id,"sess-scheduler-unsafe-001","main","running",None,0,0,0,None,"running",1,
    json.dumps({"prompt":"external effect"}),json.dumps({}),now,now
))
checkpoint=json.dumps({"arguments":{"command":"echo unsafe-side-effect"},"name":"bash","callId":"call-unsafe"})
conn.execute("INSERT INTO tasks VALUES(?,?,?,?,?,?,?,?,?,?,?)", (
    "task-unsafe-before-kill",run_id,None,"call-unsafe:0","tool","running",0,0,
    checkpoint,None,"\"unsafe\""
))
conn.execute("INSERT INTO attempts VALUES(?,?,?,?,?,?,?,?,?,?,?)", (
    "attempt-unsafe-before-kill","task-unsafe-before-kill",0,0,0,0,"old-host-worker","executing",
    now,now+60000,None
))
conn.commit(); conn.close()
PY
start_host unsafe-restart || { fail 'unsafe restart host'; exit 1; }
pass 'unsafe host restarts'
sleep 2
UNSAFE=$(rpc --method run.get --params "{\"runId\":\"$UNSAFE_ID\"}")
[ "$(jget "$UNSAFE" 'd["data"]["state"]')" = 'blocked' ] &&
    pass 'unsafe run is blocked' || fail "unsafe run not blocked: $UNSAFE"
[ "$(jget "$UNSAFE" 'd["data"]["reason_code"]')" = 'uncertain_tool_effect' ] &&
    pass 'unsafe reason is uncertain_tool_effect' ||
    fail "unsafe reason missing: $UNSAFE"
[ "$(jget "$UNSAFE" 'len([t for t in d["data"]["tasks"] if t["state"]=="uncertain"])')" = '1' ] &&
    pass 'unsafe attempt task remains uncertain' || fail "unsafe tasks: $UNSAFE"
UNSAFE_WORKERS=$(grep -c "resumed $UNSAFE_ID" "$ROOT/unsafe-restart.log" || true)
[ "$UNSAFE_WORKERS" = '0' ] && pass 'unsafe run is not auto-dispatched' ||
    fail "unsafe run spawned workers: $UNSAFE_WORKERS"

printf 'PASS: %s, FAIL: %s\n' "$PASS" "$FAIL"
test "$FAIL" -eq 0
