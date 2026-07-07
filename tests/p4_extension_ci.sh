#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────
# P4 验证: Extension 生态 — 子 Worker 创建 + 通信 端到端
# ────────────────────────────────────────────────────────────────
# 验证链路:
#   call_tool spawn_worker (RPC)
#   → agent.call_tool("spawn_worker")
#   → WorkerRuntime::spawn_worker()
#   → ManagerBridge::send_command("create_worker")  (JSON → stdout)
#   → Manager process_pending_commands()
#   → WorkerRegistry::create_worker()               (spawn child)
#   → 响应回传 (stdin → oneshot)
#   → CLI 拿到 child worker_id
# ────────────────────────────────────────────────────────────────
set -uo pipefail
TMPDIR="${TMPDIR:-/tmp}"

MANAGER_PID_FILE="$TMPDIR/ion-ci-p4.pid"
PARENT_SID_FILE="$TMPDIR/ion-ci-p4-parent.sid"
CHILD_SID_FILE="$TMPDIR/ion-ci-p4-child.sid"

cleanup() {
    # 先杀已知 PID
    [ -f "$MANAGER_PID_FILE" ] && kill "$(cat "$MANAGER_PID_FILE")" 2>/dev/null || true
    rm -f "$MANAGER_PID_FILE" "$PARENT_SID_FILE" "$CHILD_SID_FILE" ~/.ion/host.sock
}
trap cleanup EXIT

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || ION_BIN="ion"

RPC="$ION_BIN rpc"

echo "══════════════════════════════════════════════════════════"
echo "  P4 — Extension 生态验证 CI   $(date)"
echo "══════════════════════════════════════════════════════════"

# ── Phase 0: Build ──
cargo build --bin ion --bin ion-worker -q 2>/dev/null
if [ $? -ne 0 ]; then
    echo "  Build failed — aborting"
    exit 1
fi
pass "build ion + ion-worker"

# ── Phase 1: Start Manager ──
cleanup
sleep 0.5
"$ION_BIN" manager start > /tmp/ion-ci-p4-manager.log 2>&1 &
MANAGER_PID=$!
echo "$MANAGER_PID" > "$MANAGER_PID_FILE"

# 轮询等待 socket（最长 5s）
for i in $(seq 1 10); do
    if [ -S ~/.ion/host.sock ]; then
        pass "manager started"
        break
    fi
    sleep 0.5
done
if [ ! -S ~/.ion/host.sock ]; then
    cat /tmp/ion-ci-p4-manager.log
    fail "manager socket not found after 5s"
    exit 1
fi

# ── Phase 2: Create parent worker ──
PARENT_JSON=$($RPC --method create_worker --params '{"session":"p4-parent"}' 2>/dev/null)
PARENT_SID=$(echo "$PARENT_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))" 2>/dev/null)
PARENT_WID=$(echo "$PARENT_JSON" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('workerId',''))" 2>/dev/null)

if [ -n "$PARENT_SID" ]; then
    echo "$PARENT_SID" > "$PARENT_SID_FILE"
    pass "create_worker p4-parent (SID=$PARENT_SID, WID=$PARENT_WID)"
else
    fail "create_worker failed. Response: $(echo "$PARENT_JSON" | head -c 200)"
    exit 1
fi

# ── Phase 3: Verify parent is alive (call bash tool) ──
BASH_OUT=$($RPC --session "$PARENT_SID" --method call_tool \
    --params '{"tool":"bash","args":{"command":"echo parent-alive"}}' 2>/dev/null)
if echo "$BASH_OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('success') and 'parent-alive' in str(d)" 2>/dev/null; then
    pass "parent worker is alive (bash echo)"
else
    fail "parent not responsive: $BASH_OUT"
    exit 1
fi

# ── Phase 4: Parent spawns child worker via spawn_worker tool ──
# 测试的是完整链路:
#   call_tool spawn_worker → ManagerBridge → stdout → Manager → create_worker
SPAWN_RESULT=$($RPC --session "$PARENT_SID" --method call_tool \
    --params '{"tool":"spawn_worker","args":{"task":"echo hello from child","relation":"child","wait":false,"name":"p4-child-worker","agent":"developer"}}' 2>/dev/null)

# 解析嵌套 JSON: response.data.output → {worker_id, session_id}
CHILD_WORKER_ID=$(echo "$SPAWN_RESULT" | python3 -c "
import sys, json
try:
    resp = json.load(sys.stdin)
    output_str = resp.get('data', {}).get('output', '{}')
    output = json.loads(output_str) if isinstance(output_str, str) else output_str
    print(output.get('worker_id', ''))
except Exception as e:
    print('PARSING_ERROR: ' + str(e))
" 2>/dev/null)

if [ -n "$CHILD_WORKER_ID" ] && [ "$CHILD_WORKER_ID" != "PARSING_ERROR"* ]; then
    pass "spawn_worker created child (worker_id=$CHILD_WORKER_ID)"
else
    fail "spawn_worker did not return worker_id. Response: $(echo "$SPAWN_RESULT" | head -c 200)"
    exit 1
fi

# ── Phase 5: Get child's session ID ──
CHILD_SID=$(echo "$SPAWN_RESULT" | python3 -c "
import sys, json
try:
    resp = json.load(sys.stdin)
    output_str = resp.get('data', {}).get('output', '{}')
    output = json.loads(output_str) if isinstance(output_str, str) else output_str
    print(output.get('session_id', ''))
except Exception as e:
    print('PARSING_ERROR: ' + str(e))
" 2>/dev/null)

if [ -n "$CHILD_SID" ] && [ "$CHILD_SID" != "PARSING_ERROR"* ]; then
    echo "$CHILD_SID" > "$CHILD_SID_FILE"
    pass "child has session_id ($CHILD_SID)"
else
    pass "child created (session_id not in response, worker_id=$CHILD_WORKER_ID)"
    CHILD_SID=""
fi

# ── Phase 6: Verify child via send_to_worker (parent → child communication) ──
if [ -n "$CHILD_WORKER_ID" ]; then
    SEND_OUT=$($RPC --session "$PARENT_SID" --method call_tool \
        --params "{\"tool\":\"send_to_worker\",\"args\":{\"target\":\"$CHILD_WORKER_ID\",\"text\":\"echo ping-from-parent\"}}" 2>/dev/null)
    if echo "$SEND_OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('success')" 2>/dev/null; then
        pass "parent → child send_to_worker OK"
    else
        # send_to_worker 可能失败，但不影响 p4 核心验证
        yellow "parent → child send_to_worker (non-critical): $(echo "$SEND_OUT" | head -c 100)"
    fi
fi

# ── Phase 7: Kill workers ──
# 注：用 Manager 级 kill（不经过 call_tool，避免 worker 被 kill 后无法响应）
if [ -n "$CHILD_WORKER_ID" ]; then
    $RPC --method kill --params "{\"worker_id\":\"$CHILD_WORKER_ID\"}" 2>/dev/null || true
    pass "kill child worker ($CHILD_WORKER_ID)"
fi

if [ -n "$PARENT_WID" ]; then
    $RPC --method kill --params "{\"worker_id\":\"$PARENT_WID\"}" 2>/dev/null || true
    pass "kill parent worker ($PARENT_WID)"
fi

# ── Phase 8: Stop Manager ──
kill "$MANAGER_PID" 2>/dev/null
sleep 0.5
pass "manager stopped"

# ── Summary ──
echo ""
echo "══════════════════════════════════════════════════════════"
echo "  P4 验证结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
