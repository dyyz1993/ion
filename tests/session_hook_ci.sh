#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Session Hook CI — 命令行验证 on_session_before_switch 触发
#
# 验证链路：ion rpc call_tool branch_session → agent_loop 触发钩子
#           → SessionProbeExtension emit session_switch_seen 事件
#           → ion subscribe 收到事件
#
# 对齐 AGENTS.md「命令行可验证原则」：每个功能必须能从外部验证。
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0; SKIP=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"

echo "════════════════════════════════════════════════════"
echo "  Session Hook CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# ── 启动 host ──
SOCK="$HOME/.ion/host.sock"
rm -f "$SOCK" 2>/dev/null
pkill -9 -f "ion serve" 2>/dev/null; sleep 1

target/debug/ion serve >/tmp/ion_session_hook_host.log 2>&1 &
HOST_PID=$!
sleep 2

if ! kill -0 "$HOST_PID" 2>/dev/null; then
    fail "host 启动失败"; cat /tmp/ion_session_hook_host.log | tail -5; exit 1
fi
pass "SH0: host 启动成功"

cleanup() { kill "$HOST_PID" 2>/dev/null; pkill -9 -f "ion serve" 2>/dev/null; rm -f "$SOCK"; }
trap cleanup EXIT

CREATE_OUT=$($ION_BIN rpc --method create_session --params '{"agent":"build"}' 2>&1)
SID=$(echo "$CREATE_OUT" | grep -o 'sess_[a-z0-9]*' | head -1)
if [ -z "$SID" ]; then fail "SH0: create_session 失败"; exit 1; fi
pass "SH0: create_session 成功 ($SID)"

# 辅助：后台 subscribe 抓事件（超时 8s）
subscribe_capture() {
    local sid="$1"; local outfile="$2"; local wait_secs="$3"
    timeout 8 $ION_BIN subscribe --session "$sid" >"$outfile" 2>&1 &
    local sub_pid=$!
    sleep 1  # 等 subscribe 连上
    echo "$sub_pid"
}

# ════════════════════════════════════════════════════════
# Group A：branch_session 触发 session_switch_seen 事件
# ════════════════════════════════════════════════════════
echo ""
echo "── Group A：branch_session 触发 hook 事件 ──"

# A1：call_tool 调 branch_session → subscribe 收到 session_switch_seen
EVT_FILE=$(mktemp)
SUB_PID=$(subscribe_capture "$SID" "$EVT_FILE" 8)

# 触发 branch（entry 不存在没关系，hook 在工具执行前就触发）
$ION_BIN rpc --session "$SID" --method call_tool \
    --params '{"tool":"branch_session","args":{"from_entry":"entry_1","name":"ci-branch"}}' >/dev/null 2>&1
sleep 3
kill "$SUB_PID" 2>/dev/null; wait "$SUB_PID" 2>/dev/null

if grep -q "session_switch_seen" "$EVT_FILE"; then
    pass "A1: call_tool branch_session → subscribe 收到 session_switch_seen 事件"
else
    fail "A1: 未收到 session_switch_seen 事件"
    echo "    subscribe 输出: $(cat "$EVT_FILE" | head -5)"
fi

# A2：事件 action 字段 = "branch"
if grep -q '"action": "branch"' "$EVT_FILE" || grep -q '"action":"branch"' "$EVT_FILE"; then
    pass "A2: 事件 action = branch"
else
    fail "A2: action 字段不是 branch"
    echo "    事件内容: $(grep -o 'action[^,}]*' "$EVT_FILE" | head -1)"
fi

# A3：事件 branch_name 字段 = "ci-branch"
if grep -q '"ci-branch"' "$EVT_FILE"; then
    pass "A3: 事件 branch_name = ci-branch（从 tool args 透传）"
else
    fail "A3: branch_name 未透传"
fi

rm -f "$EVT_FILE"

# ════════════════════════════════════════════════════════
# Group B：rollback 时 action = "rollback"
# ════════════════════════════════════════════════════════
echo ""
echo "── Group B：rollback 触发 action=rollback ──"

EVT_FILE=$(mktemp)
SUB_PID=$(subscribe_capture "$SID" "$EVT_FILE" 8)

$ION_BIN rpc --session "$SID" --method call_tool \
    --params '{"tool":"branch_session","args":{"from_entry":"entry_1","is_rollback":true,"reason":"test"}}' >/dev/null 2>&1
sleep 3
kill "$SUB_PID" 2>/dev/null; wait "$SUB_PID" 2>/dev/null

if grep -q '"action": "rollback"' "$EVT_FILE" || grep -q '"action":"rollback"' "$EVT_FILE"; then
    pass "B1: rollback 时 action = rollback"
else
    fail "B1: rollback action 不对"
    echo "    事件: $(grep -o 'action[^,}]*' "$EVT_FILE" | head -1)"
fi
rm -f "$EVT_FILE"

# ════════════════════════════════════════════════════════
# Group C：非 session 工具不触发 hook 事件
# ════════════════════════════════════════════════════════
echo ""
echo "── Group C：其他工具不触发 session hook ──"

EVT_FILE=$(mktemp)
SUB_PID=$(subscribe_capture "$SID" "$EVT_FILE" 8)

# 调一个非 branch_session 的工具（echo 不存在的话用 get_skills）
$ION_BIN rpc --session "$SID" --method call_tool \
    --params '{"tool":"calculator","args":{"expression":"1+1"}}' >/dev/null 2>&1
sleep 3
kill "$SUB_PID" 2>/dev/null; wait "$SUB_PID" 2>/dev/null

if grep -q "session_switch_seen" "$EVT_FILE"; then
    fail "C1: 非 session 工具不应触发 session_switch_seen"
else
    pass "C1: 其他工具（calculator）不触发 session hook"
fi
rm -f "$EVT_FILE"

# ════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "════════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then echo "❌ session_hook_ci 有失败"; exit 1; fi
echo "✅ session_hook_ci 全部通过"
exit 0
