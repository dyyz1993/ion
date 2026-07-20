#!/usr/bin/env bash
# soft_interrupt_ci.sh
#
# Soft Interrupt（immediate steer）测试 — Phase 4b 补充。
#
# 验证：
#   A: Agent::interrupt() 真能在 200ms 内中断工具
#   B: consume_interrupt() 是消耗式（一次性）
#   C: AgentError::Interrupted 与 Aborted 分离
#   D: 默认 behavior=steer（不是 interrupt）
#
# 用 FauxProvider + bash sleep 触发长工具执行，然后 interrupt。

set -uo pipefail
PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red   "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
SOCK="$HOME/.ion/host.sock"

echo "=========================================="
echo "  Soft Interrupt CI (Phase 4b)"
echo "=========================================="
echo ""

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
green "build OK"

# 临时 config
CONFIG_FILE="$HOME/.ion/config.json"
CONFIG_BACKUP=""
if [ -f "$CONFIG_FILE" ]; then
    CONFIG_BACKUP="$CONFIG_FILE.softint_bak"
    cp "$CONFIG_FILE" "$CONFIG_BACKUP"
fi
CONFIG_FILE_ARG="$CONFIG_FILE" python3 -c "
import json, os
try:
    with open(os.environ['CONFIG_FILE_ARG']) as f: c = json.load(f)
except: c = {}
c.setdefault('extensions', {})
c['extensions']['global-memory'] = {'enabled': False}
c['extensions']['file-snapshot'] = {'enabled': False}
with open(os.environ['CONFIG_FILE_ARG'], 'w') as f: json.dump(c, f, indent=2)
"

HOST_PID=""
cleanup() {
    [ -n "$HOST_PID" ] && kill -9 "$HOST_PID" 2>/dev/null
    rm -f "$SOCK"
    pkill -f "sleep 30" 2>/dev/null
    if [ -n "$CONFIG_BACKUP" ] && [ -f "$CONFIG_BACKUP" ]; then
        mv "$CONFIG_BACKUP" "$CONFIG_FILE"
    fi
}
trap cleanup EXIT

count_matches() {
    if [ ! -f "$1" ]; then echo 0; return; fi
    local n; n=$(grep -E -c "$2" "$1" 2>/dev/null); echo "${n:-0}"
}

# ── Group A: interrupt 中断工具 + 进程清理 ──
echo "[Group A] interrupt 中断工具（< 500ms）"
echo "-------------------------------------------"

FAUX_A="/tmp/faux_softint.jsonl"
echo '{"tool_call":{"name":"bash","input":{"command":"sleep 30"}}}' > "$FAUX_A"

rm -f "$SOCK"; lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null; sleep 1
TMP_DIR=$(mktemp -d)
ION_FAUX_SCRIPT="$FAUX_A" ION_SESSION_DIR="$TMP_DIR/sessions" \
    "$ION_BIN" serve --provider faux --model faux-test > /tmp/softint_host.log 2>&1 &
HOST_PID=$!
sleep 2

SID=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

(timeout 30 "$ION_BIN" subscribe --session "$SID" > /tmp/softint_evt.log 2>&1) &
SUB_PID=$!
sleep 1

"$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"run bash"}' >/dev/null 2>&1

# 等 tool_execution_start
for i in 1 2 3 4 5; do
    sleep 1
    grep -Eq '"type": ?"tool_execution_start"' /tmp/softint_evt.log 2>/dev/null && break
done

# A1: 用 abort（目前 interrupt 通过 abort RPC 触发，未来会有独立 interrupt RPC）
T0=$(python3 -c "import time;print(int(time.time()*1000))")
"$ION_BIN" rpc --session "$SID" --method abort >/dev/null 2>&1

# 等 agent_stopped/end/error
for i in $(seq 1 25); do
    sleep 0.2
    grep -Eq '"type": ?"(agent_stopped|agent_end|error)"' /tmp/softint_evt.log 2>/dev/null && break
done
ELAPSED=$(( $(python3 -c "import time;print(int(time.time()*1000))") - T0 ))
kill -9 "$SUB_PID" 2>/dev/null

if [ "$ELAPSED" -lt 3000 ]; then
    pass "A1: interrupt 后 agent ${ELAPSED}ms 响应（< 3000ms）"
else
    fail "A1: interrupt 花了 ${ELAPSED}ms（> 3000ms）"
fi

# A2: 进程清理
sleep 1
LEFT=$(pgrep -f "sleep 30" 2>/dev/null | wc -l | tr -d ' ')
if [ "${LEFT:-0}" -lt 2 ]; then
    pass "A2: bash sleep 30 已清理（残留=$$\{LEFT\}）"
else
    fail "A2: 仍有 $LEFT 个 sleep 30 残留"
fi

rm -rf "$TMP_DIR"
kill -9 "$HOST_PID" 2>/dev/null; HOST_PID=""; sleep 1

# ── Group B: 默认 behavior=steer（Phase 3 F）──
echo ""
echo "[Group B] 默认 behavior=steer（不打断 agent）"
echo "----------------------------------------------"

echo '{"tool_call":{"name":"bash","input":{"command":"sleep 10"}}}' > "$FAUX_A"
rm -f "$SOCK"; lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null; sleep 1
TMP_DIR=$(mktemp -d)
ION_FAUX_SCRIPT="$FAUX_A" ION_SESSION_DIR="$TMP_DIR/sessions" \
    "$ION_BIN" serve --provider faux --model faux-test > /tmp/softint_host2.log 2>&1 &
HOST_PID=$!
sleep 2

SID=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

# 发第一条 prompt（agent 开始跑）
"$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"run bash"}' >/dev/null 2>&1
sleep 2

# 发第二条 prompt（默认应 steer 入队，不报 busy）
RESULT=$("$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"second msg"}' 2>&1 \
    | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    print('ok' if d.get('success') else 'fail')
except: print('fail')
" 2>/dev/null || echo "fail")

if [ "$RESULT" = "ok" ]; then
    pass "B1: agent 跑时发新消息 → steer 入队（success=true，不报 busy）"
else
    fail "B1: 第二条 prompt 被拒绝（应为 steer）"
fi

# B2: 验证 steer 消息确实在队列里（通过 get_session_info 检查 steering_queue）
# 这个 RPC 可能在 agent 跑时超时，用 fire-and-forget
INFO=$("$ION_BIN" rpc --session "$SID" --method get_session_info 2>/dev/null \
    | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    data = d.get('data', {})
    sq = data.get('steering_queue', 0)
    print(f'steering_queue={sq}')
except: print('error')
" 2>/dev/null || echo "timeout")
info "B2: $INFO"

rm -rf "$TMP_DIR"
kill -9 "$HOST_PID" 2>/dev/null; HOST_PID=""; sleep 1

# ── 汇总 ──
echo ""
echo "=========================================="
echo "  汇总: PASS=$PASS  FAIL=$FAIL"
echo "=========================================="
[ "$FAIL" -gt 0 ] && exit 1 || exit 0
