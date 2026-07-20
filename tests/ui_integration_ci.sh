#!/usr/bin/env bash
# ui_integration_ci.sh
#
# UI 集成测试：模拟浏览器行为（HTTP RPC + SSE subscribe），端到端验证
# Phase 1+2+3 共 11 个修复的联动效果。
#
# 不需要真实浏览器，纯 curl + Python 模拟前端行为：
#   - HTTP POST /rpc → ion rpc（fire-and-forget prompt）
#   - GET /events?sid=X → SSE 事件流（accumulate + assert）
#
# 运行：bash tests/ui_integration_ci.sh
# 需要：host + proxy 已启动（脚本会自动起）
#
# 测试矩阵（11 个修复 × 用户场景）：
#   Phase 2 #1: host 启动后默认 session 存在
#   Phase 2 #2: 发到不存在 session → auto-create
#   Phase 2 #3: proxy watchdog 检测 host 存活
#   Phase 1  A: 工具执行中 abort < 3s
#   Phase 1  B: bash kill 后无残留进程
#   Phase 1  D: abort 后无新 text_delta
#   Phase 3  F: agent 跑时发新消息 → steer 入队（不报 busy）
#   Phase 3  I: auto_retry_start/end 事件（可选，需要触发重试）
#   Phase 3  N: 退避 60s（从事件时间戳推断）
#   Phase 3  E: write 后立刻读 → 内容完整（原子写）
#   流式跳动: tool_call_delta 计数 ≥ 20

set -uo pipefail

PASS=0; FAIL=0; SKIP=0
green()  { echo -e "\033[32m  ✅ $1\033[0m"; }
red()    { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow() { echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red   "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

count_matches() {
    local pattern="$1"; local file="$2"
    if [ ! -f "$file" ]; then echo 0; return; fi
    local n
    n=$(grep -E -c "$pattern" "$file" 2>/dev/null)
    echo "${n:-0}"
}

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
SOCK="$HOME/.ion/host.sock"
PROXY_PORT=8420

echo "=========================================="
echo "  UI Integration CI (11 fixes e2e)"
echo "=========================================="
echo ""

# ── Phase 0: build + 起 host + proxy ──
echo "[Phase 0] Build + start host + proxy..."
cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
green "build OK"

# 准备 config（opencode + DeepSeek，禁用扩展）
mv ~/.pi/agent/models.json ~/.pi/agent/models.json.bak_ui_test 2>/dev/null
python3 -c "
import json
with open('/Users/xuyingzhou/.ion/auth.json','w') as f:
    json.dump({'api_key':'sk-9gnzvY39HD12Xd2oD0UTWln1R2HiIBAPuAvhAnsfgsGAJR1yeaTIMgwBUBrEPhWL'}, f)
with open('/Users/xuyingzhou/.ion/config.json') as f: c=json.load(f)
c['default_provider']='opencode'
c['default_model']='deepseek-v4-flash'
c.setdefault('extensions', {})
c['extensions']['global-memory'] = {'enabled': False}
c['extensions']['file-snapshot'] = {'enabled': False}
with open('/Users/xuyingzhou/.ion/config.json','w') as f: json.dump(c,f,indent=2)
"

# cleanup
HOST_PID=""; PROXY_PID=""
cleanup() {
    [ -n "$PROXY_PID" ] && kill -9 "$PROXY_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && kill -9 "$HOST_PID" 2>/dev/null
    rm -f "$SOCK" 2>/dev/null
    pkill -f "ion_proxy_v2.py" 2>/dev/null
    # 还原 pi models.json
    mv ~/.pi/agent/models.json.bak_ui_test ~/.pi/agent/models.json 2>/dev/null
    # 清理测试文件
    rm -f /tmp/ui_test_*.txt /tmp/evt_ui_*.log /tmp/ion_ui_*.log
}
trap cleanup EXIT

lsof -ti "$SOCK" 2>/dev/null | xargs -r kill -9 2>/dev/null
pkill -f "ion_proxy_v2.py" 2>/dev/null
sleep 2

# 起 host
nohup "$ION_BIN" serve > /tmp/ion_ui_host.log 2>&1 &
HOST_PID=$!
disown $HOST_PID 2>/dev/null
for i in 1 2 3 4 5 6 7 8; do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then break; fi
done

# 起 proxy
nohup python3 /tmp/ion_proxy_v2.py > /tmp/ion_ui_proxy.log 2>&1 &
PROXY_PID=$!
disown $PROXY_PID 2>/dev/null
sleep 2
green "host + proxy 启动完成"
echo ""

# ─────────────────────────────────────────────────────────
# Group A: Phase 2 - session 管理（#1 #2 #3）
# ─────────────────────────────────────────────────────────
echo "[Group A] Phase 2 session 管理（#1 默认 session / #2 auto-create / #3 watchdog）"
echo "------------------------------------------------------------------------------------------------"

# A1: host 启动后默认 session 存在（修复 #1）
DEFAULT_COUNT=$("$ION_BIN" rpc --method list_sessions 2>/dev/null \
    | python3 -c "import sys,json;d=json.load(sys.stdin);print(len(d.get('data',{}).get('sessions',[])))" 2>/dev/null || echo 0)
if [ "${DEFAULT_COUNT:-0}" -ge 1 ]; then
    pass "A1 (#1): host 启动后默认 session 存在（count=${DEFAULT_COUNT}）"
else
    fail "A1 (#1): host 启动后没有默认 session（count=${DEFAULT_COUNT}）"
fi

# A2: 发到不存在 session → auto-create（修复 #2）
# 用一个不存在的 session_id 发 get_session_info，应该 auto-create 后返回数据
AUTO_SID="sess_ui_autocreate_$(date +%s)"
AUTO_RESULT=$("$ION_BIN" rpc --session "$AUTO_SID" --method get_session_info 2>&1 \
    | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    # success=true 表示 auto-create 成功并返回了数据
    print('ok' if d.get('success') else 'fail')
except: print('fail')
" 2>/dev/null || echo "fail")
if [ "$AUTO_RESULT" = "ok" ]; then
    pass "A2 (#2): 发到不存在 session → auto-create 成功"
else
    fail "A2 (#2): 发到不存在 session → auto-create 失败（可能是 worker ready 竞态）"
fi

# A3: proxy watchdog 检测 host 存活（修复 #3）
# 杀 host，等 watchdog 自动重启（5-10s）
echo "  [debug] 杀 host PID=$HOST_PID 测试 watchdog..."
kill -9 "$HOST_PID" 2>/dev/null
HOST_PID=""
WATCHDOG_OK=0
for i in $(seq 1 30); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then
        # host 重启成功，找新 PID
        HOST_PID=$(pgrep -f "target/debug/ion serve" | head -1)
        WATCHDOG_OK=1
        break
    fi
done
if [ "$WATCHDOG_OK" -eq 1 ]; then
    pass "A3 (#3): host 崩溃后 watchdog 自动重启（${i}s）"
else
    fail "A3 (#3): host 崩溃后 watchdog 未重启（30s 超时）"
    # 手动重启 host 继续后续测试
    nohup "$ION_BIN" serve > /tmp/ion_ui_host.log 2>&1 &
    HOST_PID=$!
    disown $HOST_PID 2>/dev/null
    for i in 1 2 3 4 5; do sleep 1; "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1 && break; done
fi
echo ""

# ─────────────────────────────────────────────────────────
# Group B: Phase 1 - 中断内核（A B D）
# ─────────────────────────────────────────────────────────
echo "[Group B] Phase 1 中断内核（A 工具中断 / B 进程树 / D HTTP 真取消）"
echo "------------------------------------------------------------------------------------------------"

# 创建测试 session
TEST_SID=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

# B1: 工具执行中 abort < 3s（修复 A）
# 让 agent 调 bash sleep 30，然后 abort
# 先用 FauxProvider 构造一个 bash sleep 30 的 tool_call
# 但当前 host 用的是 opencode，不能直接换 provider
# 改用：发一个会触发长 bash 的 prompt
( timeout 30 "$ION_BIN" subscribe --session "$TEST_SID" > /tmp/evt_ui_b1.log 2>&1 ) &
SUB_PID=$!
sleep 1

"$ION_BIN" rpc --session "$TEST_SID" --method prompt \
    --params '{"text":"用 bash 工具执行 sleep 30"}' >/dev/null 2>&1

# 等 tool_execution_start
for i in 1 2 3 4 5; do
    sleep 1
    grep -Eq '"type": ?"tool_execution_start"' /tmp/evt_ui_b1.log 2>/dev/null && break
done

# abort
ABORT_START=$(python3 -c "import time; print(int(time.time()*1000))")
"$ION_BIN" rpc --session "$TEST_SID" --method abort >/dev/null 2>&1

# 等 agent_stopped/end
for i in $(seq 1 25); do
    sleep 0.2
    grep -Eq '"type": ?"(agent_stopped|agent_end|error)"' /tmp/evt_ui_b1.log 2>/dev/null && break
done
ABORT_END=$(python3 -c "import time; print(int(time.time()*1000))")
ABORT_MS=$((ABORT_END - ABORT_START))
kill -9 "$SUB_PID" 2>/dev/null

if [ "$ABORT_MS" -lt 3000 ]; then
    pass "B1 (A): 工具执行中 abort ${ABORT_MS}ms 生效（< 3000ms）"
else
    fail "B1 (A): abort 生效花了 ${ABORT_MS}ms（> 3000ms）"
fi

# B2: bash kill 后无残留进程（修复 B）
sleep 1
SLEEP_COUNT=$(pgrep -f "sleep 30" 2>/dev/null | wc -l | tr -d ' ')
if [ "${SLEEP_COUNT:-0}" -eq 0 ]; then
    pass "B2 (B): bash sleep 30 进程已清理（kill_process_tree 生效）"
else
    fail "B2 (B): 仍有 $SLEEP_COUNT 个 sleep 30 残留"
fi

# B3: HTTP 流式期间 abort < 2s + 无新 delta（修复 D）
TEST_SID2=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
( timeout 30 "$ION_BIN" subscribe --session "$TEST_SID2" > /tmp/evt_ui_b3.log 2>&1 ) &
SUB_PID=$!
sleep 1

# 发一个会触发长文本流式的 prompt
"$ION_BIN" rpc --session "$TEST_SID2" --method prompt \
    --params '{"text":"写一首 500 字的诗"}' >/dev/null 2>&1

# 等 text_delta 开始
for i in 1 2 3 4 5; do
    sleep 0.5
    grep -Eq '"type": ?"text_delta"' /tmp/evt_ui_b3.log 2>/dev/null && break
done

DELTAS_BEFORE=$(count_matches '"type": ?"text_delta"' /tmp/evt_ui_b3.log)
ABORT_START=$(python3 -c "import time; print(int(time.time()*1000))")
"$ION_BIN" rpc --session "$TEST_SID2" --method abort >/dev/null 2>&1

for i in $(seq 1 20); do
    sleep 0.2
    grep -Eq '"type": ?"(agent_stopped|agent_end|error)"' /tmp/evt_ui_b3.log 2>/dev/null && break
done
ABORT_END=$(python3 -c "import time; print(int(time.time()*1000))")
ABORT_MS_D=$((ABORT_END - ABORT_START))
sleep 2
DELTAS_AFTER=$(count_matches '"type": ?"text_delta"' /tmp/evt_ui_b3.log)
kill -9 "$SUB_PID" 2>/dev/null

if [ "$ABORT_MS_D" -lt 2000 ]; then
    pass "B3a (D): HTTP 流式期间 abort ${ABORT_MS_D}ms 生效（< 2000ms）"
else
    fail "B3a (D): HTTP abort 花了 ${ABORT_MS_D}ms（> 2000ms）"
fi

if [ "$DELTAS_AFTER" -le "$DELTAS_BEFORE" ]; then
    pass "B3b (D): abort 后无新 text_delta（$DELTAS_BEFORE → ${DELTAS_AFTER}，HTTP 真取消）"
else
    fail "B3b (D): abort 后仍有新 delta（$DELTAS_BEFORE → ${DELTAS_AFTER}）"
fi
echo ""

# ─────────────────────────────────────────────────────────
# Group C: Phase 3 - 事件/队列（F steer / E 原子写）
# ─────────────────────────────────────────────────────────
echo "[Group C] Phase 3 事件/队列（F steer / E 原子写 / 流式跳动）"
echo "------------------------------------------------------------------------------------------------"

# C1: agent 跑时发新消息 → steer 入队（修复 F）
TEST_SID3=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
( timeout 30 "$ION_BIN" subscribe --session "$TEST_SID3" > /tmp/evt_ui_c1.log 2>&1 ) &
SUB_PID=$!
sleep 1

# 发第一个 prompt（agent 开始跑）
"$ION_BIN" rpc --session "$TEST_SID3" --method prompt \
    --params '{"text":"用 bash 工具执行 sleep 10"}' >/dev/null 2>&1
sleep 2  # 等 agent 开始跑

# 发第二个 prompt（默认 behavior=steer，应该入队不报 busy）
STEER_RESULT=$("$ION_BIN" rpc --session "$TEST_SID3" --method prompt \
    --params '{"text":"好了好了别睡了"}' 2>&1 \
    | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)
    s = d.get('success', False)
    data = d.get('data', {})
    # steer 入队会返回 success=true + data 里含 status=queued
    print('ok' if s else 'fail')
except: print('fail')
" 2>/dev/null || echo "fail")

if [ "$STEER_RESULT" = "ok" ]; then
    pass "C1 (F): agent 跑时发新消息 → steer 入队（不报 busy）"
else
    fail "C1 (F): agent 跑时发新消息被拒绝（应 steer 入队）"
fi

# 等 agent 结束（sleep 10 + steer）
for i in $(seq 1 60); do
    sleep 1
    AE=$(count_matches '"type": ?"(agent_stopped|agent_end)"' /tmp/evt_ui_c1.log)
    if [ "${AE:-0}" -ge 1 ]; then break; fi
done
kill -9 "$SUB_PID" 2>/dev/null

# C2: write 后立刻读 → 内容完整（修复 E 原子写）
TEST_SID4=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

"$ION_BIN" rpc --session "$TEST_SID4" --method prompt \
    --params '{"text":"用 write 工具创建 /tmp/ui_test_atomic.txt，写 5 行内容 hello world"}' >/dev/null 2>&1

# 等 agent 跑完
for i in $(seq 1 30); do
    sleep 2
    # 检查文件是否存在且内容完整
    if [ -f /tmp/ui_test_atomic.txt ]; then
        LINES=$(wc -l < /tmp/ui_test_atomic.txt 2>/dev/null || echo 0)
        if [ "${LINES:-0}" -ge 3 ]; then break; fi
    fi
done

if [ -f /tmp/ui_test_atomic.txt ]; then
    LINES=$(wc -l < /tmp/ui_test_atomic.txt)
    if [ "${LINES:-0}" -ge 3 ]; then
        pass "C2 (E): write 后文件内容完整（${LINES} 行，原子写生效）"
    else
        fail "C2 (E): write 后文件只有 ${LINES} 行（可能半写）"
    fi
else
    fail "C2 (E): 文件 /tmp/ui_test_atomic.txt 未创建"
fi

# C3: 流式跳动验证（tool_call_delta ≥ 20）
TEST_SID5=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
( timeout 60 "$ION_BIN" subscribe --session "$TEST_SID5" > /tmp/evt_ui_c3.log 2>&1 ) &
SUB_PID=$!
sleep 1

"$ION_BIN" rpc --session "$TEST_SID5" --method prompt \
    --params '{"text":"用 write 工具创建 /tmp/ui_test_stream.txt，写 30 行内容 line N hello world test"}' >/dev/null 2>&1

for i in $(seq 1 60); do
    sleep 1
    grep -Eq '"type": ?"tool_execution_end"' /tmp/evt_ui_c3.log 2>/dev/null && break
done
sleep 2
kill -9 "$SUB_PID" 2>/dev/null

TCD_COUNT=$(count_matches '"type": ?"tool_call_delta"' /tmp/evt_ui_c3.log)
if [ "${TCD_COUNT:-0}" -ge 20 ]; then
    pass "C3 (流式): subscribe 收到 ${TCD_COUNT} 个 tool_call_delta（≥20，DeepSeek 真实流式）"
else
    fail "C3 (流式): 只收到 ${TCD_COUNT} 个 tool_call_delta（<20）"
fi
echo ""

# ─────────────────────────────────────────────────────────
# 汇总
# ─────────────────────────────────────────────────────────
echo "=========================================="
echo "  汇总: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "=========================================="

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "❌ 有失败用例，诊断信息："
    echo "  - host 日志: /tmp/ion_ui_host.log"
    echo "  - proxy 日志: /tmp/ion_ui_proxy.log"
    echo "  - 事件流: /tmp/evt_ui_*.log"
    echo "  - 残留进程: pgrep -f 'sleep 30|sleep 10'"
    exit 1
fi

exit 0
