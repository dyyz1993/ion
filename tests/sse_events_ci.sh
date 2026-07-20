#!/usr/bin/env bash
# sse_events_ci.sh
#
# SSE 事件协议测试 — Phase 4a 补充。
#
# 验证新加的事件确实推到 subscribe：
#   A: agent_start / agent_end 事件
#   B: message_start / message_end 事件（含 usage）
#   C: tool_execution_end 含 result 字段
#   D: tool_execution_update 含 args 字段
#   E: auto_retry_start / auto_retry_end 事件（FauxProvider 模拟错误触发重试）
#
# 用 FauxProvider + subscribe 验证事件序列。

set -uo pipefail
PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
info()  { echo "  $*"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red   "$1"; }

count_matches() {
    if [ ! -f "$1" ]; then echo 0; return; fi
    local n; n=$(grep -E -c "$2" "$1" 2>/dev/null); echo "${n:-0}"
}

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
SOCK="$HOME/.ion/host.sock"

echo "=========================================="
echo "  SSE Events CI (Phase 4a)"
echo "=========================================="
echo ""

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
green "build OK"

CONFIG_FILE="$HOME/.ion/config.json"
CONFIG_BACKUP=""
if [ -f "$CONFIG_FILE" ]; then
    CONFIG_BACKUP="$CONFIG_FILE.sse_bak"
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
    if [ -n "$CONFIG_BACKUP" ] && [ -f "$CONFIG_BACKUP" ]; then
        mv "$CONFIG_BACKUP" "$CONFIG_FILE"
    fi
}
trap cleanup EXIT

# ── Group A+B: agent/message 事件 ──
echo "[Group A] agent_start/end + message_start/end 事件"
echo "---------------------------------------------------"

FAUX="/tmp/faux_sse.jsonl"
# 简单文本响应
echo '{"text":"hello world"}' > "$FAUX"

rm -f "$SOCK"; lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null; sleep 1
TMP_DIR=$(mktemp -d)
ION_FAUX_SCRIPT="$FAUX" ION_SESSION_DIR="$TMP_DIR/sessions" \
    "$ION_BIN" serve --provider faux --model faux-test > /tmp/sse_host.log 2>&1 &
HOST_PID=$!
sleep 2

SID=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

# subscribe + 发 prompt + 等结束
(timeout 15 "$ION_BIN" subscribe --session "$SID" > /tmp/sse_evt.log 2>&1) &
SUB_PID=$!
sleep 1
"$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"hi"}' >/dev/null 2>&1
# 等 agent_end
for i in $(seq 1 30); do
    sleep 0.5
    grep -Eq '"type": ?"agent_end"' /tmp/sse_evt.log 2>/dev/null && break
done
sleep 1
kill -9 "$SUB_PID" 2>/dev/null

# A1: agent_start 事件
AS=$(count_matches /tmp/sse_evt.log '"type": ?"agent_start"')
if [ "${AS:-0}" -ge 1 ]; then
    pass "A1: agent_start 事件收到（count=${AS}）"
else
    fail "A1: 未收到 agent_start 事件"
fi

# A2: agent_end 事件（含 willRetry + messages 字段）
AE=$(count_matches /tmp/sse_evt.log '"type": ?"agent_end"')
if [ "${AE:-0}" -ge 1 ]; then
    pass "A2: agent_end 事件收到（count=${AE}）"
    # 检查 willRetry 和 messages 字段
    if grep -q '"willRetry"' /tmp/sse_evt.log 2>/dev/null; then
        pass "A3: agent_end 含 willRetry 字段"
    else
        fail "A3: agent_end 缺 willRetry 字段"
    fi
    if grep -q '"messages"' /tmp/sse_evt.log 2>/dev/null; then
        pass "A4: agent_end 含 messages 字段"
    else
        fail "A4: agent_end 缺 messages 字段"
    fi
else
    fail "A2: 未收到 agent_end 事件"
fi

# B1: message_start 事件
MS=$(count_matches /tmp/sse_evt.log '"type": ?"message_start"')
if [ "${MS:-0}" -ge 1 ]; then
    pass "B1: message_start 事件收到（count=${MS}）"
else
    fail "B1: 未收到 message_start 事件"
fi

# B2: message_end 事件
ME=$(count_matches /tmp/sse_evt.log '"type": ?"message_end"')
if [ "${ME:-0}" -ge 1 ]; then
    pass "B2: message_end 事件收到（count=${ME}）"
    # 检查 usage 字段
    if grep -q '"usage"' /tmp/sse_evt.log 2>/dev/null; then
        pass "B3: message_end 含 usage 字段（token 用量）"
    else
        fail "B3: message_end 缺 usage 字段"
    fi
else
    fail "B2: 未收到 message_end 事件"
fi

rm -rf "$TMP_DIR"
kill -9 "$HOST_PID" 2>/dev/null; HOST_PID=""; sleep 1

# ── Group C+D: tool 事件字段 ──
echo ""
echo "[Group C] tool_execution_end 含 result + update 含 args"
echo "--------------------------------------------------------"

echo '{"tool_call":{"name":"echo","input":{"msg":"test hello"}}}' > "$FAUX"
rm -f "$SOCK"; lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null; sleep 1
TMP_DIR=$(mktemp -d)
ION_FAUX_SCRIPT="$FAUX" ION_SESSION_DIR="$TMP_DIR/sessions" \
    "$ION_BIN" serve --provider faux --model faux-test > /tmp/sse_host2.log 2>&1 &
HOST_PID=$!
sleep 2

SID=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

(timeout 15 "$ION_BIN" subscribe --session "$SID" > /tmp/sse_evt2.log 2>&1) &
SUB_PID=$!
sleep 1
"$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"run tool"}' >/dev/null 2>&1
for i in $(seq 1 30); do
    sleep 0.5
    grep -Eq '"type": ?"tool_execution_end"' /tmp/sse_evt2.log 2>/dev/null && break
done
sleep 1
kill -9 "$SUB_PID" 2>/dev/null

# C1: tool_execution_end 含 result（用 python3 解析 JSON）
C1_OK=$(python3 -c "
import json
with open('/tmp/sse_evt2.log') as f: content = f.read()
decoder = json.JSONDecoder()
i=0; n=len(content)
while i<n:
    while i<n and content[i] not in '{[': i+=1
    if i>=n: break
    try:
        obj,end=decoder.raw_decode(content[i:]); i+=end
        if obj.get('type')=='instance_event':
            ev=obj.get('event',{})
            if ev.get('type')=='tool_execution_end' and 'result' in ev:
                print('ok'); break
    except: i+=1
" 2>/dev/null || echo "fail")
if [ "$C1_OK" = "ok" ]; then
    pass "C1: tool_execution_end 含 result 字段"
else
    fail "C1: tool_execution_end 缺 result 字段"
fi

# D1: tool_execution_update 含 args
D1_OK=$(python3 -c "
import json
with open('/tmp/sse_evt2.log') as f: content = f.read()
decoder = json.JSONDecoder()
i=0; n=len(content)
while i<n:
    while i<n and content[i] not in '{[': i+=1
    if i>=n: break
    try:
        obj,end=decoder.raw_decode(content[i:]); i+=end
        if obj.get('type')=='instance_event':
            ev=obj.get('event',{})
            if ev.get('type')=='tool_execution_update' and 'args' in ev:
                print('ok'); break
    except: i+=1
" 2>/dev/null || echo "fail")
if [ "$D1_OK" = "ok" ]; then
    pass "D1: tool_execution_update 含 args 字段"
else
    fail "D1: tool_execution_update 缺 args 字段"
fi

rm -rf "$TMP_DIR"
kill -9 "$HOST_PID" 2>/dev/null; HOST_PID=""; sleep 1

# ── Group E: auto_retry 事件 ──
echo ""
echo "[Group E] auto_retry_start/end 事件"
echo "-------------------------------------"

# FauxProvider 错误响应 → 触发重试
echo '{"error":"500 Internal Server Error"}' > "$FAUX"
rm -f "$SOCK"; lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null; sleep 1
TMP_DIR=$(mktemp -d)
# 只放 1 个错误响应，agent 第一次失败 → retry → 队列空 → 最终失败
ION_FAUX_SCRIPT="$FAUX" ION_SESSION_DIR="$TMP_DIR/sessions" \
    ION_MAX_TURNS=1 \
    "$ION_BIN" serve --provider faux --model faux-test > /tmp/sse_host3.log 2>&1 &
HOST_PID=$!
sleep 2

SID=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

(timeout 30 "$ION_BIN" subscribe --session "$SID" > /tmp/sse_evt3.log 2>&1) &
SUB_PID=$!
sleep 1
"$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"hi"}' >/dev/null 2>&1
# 等错误 / agent_end / agent_stopped
for i in $(seq 1 40); do
    sleep 0.5
    if grep -Eq '"type": ?"(error|agent_stopped|agent_end)"' /tmp/sse_evt3.log 2>/dev/null; then break; fi
done
sleep 1
kill -9 "$SUB_PID" 2>/dev/null

# E1: auto_retry_start 事件
ARS=$(count_matches /tmp/sse_evt3.log '"type": ?"auto_retry_start"')
if [ "${ARS:-0}" -ge 1 ]; then
    pass "E1: auto_retry_start 事件收到（count=${ARS}）"
    # 检查 attempt + maxRetries 字段
    if grep -q '"attempt"' /tmp/sse_evt3.log 2>/dev/null && \
       grep -q '"maxRetries"' /tmp/sse_evt3.log 2>/dev/null; then
        pass "E2: auto_retry_start 含 attempt + maxRetries 字段"
    else
        fail "E2: auto_retry_start 缺 attempt/maxRetries 字段"
    fi
else
    fail "E1: 未收到 auto_retry_start 事件（可能错误未被判定为可重试）"
fi

# E3: auto_retry_end 事件（success=false 因为队列空了）
ARE=$(count_matches /tmp/sse_evt3.log '"type": ?"auto_retry_end"')
if [ "${ARE:-0}" -ge 1 ]; then
    pass "E3: auto_retry_end 事件收到（count=${ARE}）"
else
    fail "E3: 未收到 auto_retry_end 事件"
fi

rm -rf "$TMP_DIR"

# ── 汇总 ──
echo ""
echo "=========================================="
echo "  汇总: PASS=$PASS  FAIL=$FAIL"
echo "=========================================="
[ "$FAIL" -gt 0 ] && exit 1 || exit 0
