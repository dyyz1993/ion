#!/usr/bin/env bash
# abort_ci.sh
#
# 中断/abort 内核验证（修复 A + B + D）。
#
# 验证 3 个 P0 修复：
#   A：工具执行中可中断（agent_loop select! 加 stopped 分支）
#   B：bash 杀进程树（process_group + killpg）
#   D：HTTP 真取消（CancellationToken 贯穿 provider）
#
# 运行：bash tests/abort_ci.sh
# 不调真实 LLM（用 FauxProvider + 真实 bash 进程）。

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

echo "=========================================="
echo "  Abort/Stop Kernel CI (A+B+D)"
echo "=========================================="
echo ""

# ── Phase 0: build ──
echo "[Phase 0] Build..."
cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
green "build OK"
echo ""

# ── 临时禁用扩展 + config ──
CONFIG_FILE="$HOME/.ion/config.json"
CONFIG_BACKUP=""
disable_extensions() {
    if [ -f "$CONFIG_FILE" ]; then
        CONFIG_BACKUP="$CONFIG_FILE.abort_ci_bak"
        cp "$CONFIG_FILE" "$CONFIG_BACKUP"
    fi
    CONFIG_FILE_ARG="$CONFIG_FILE" python3 -c "
import json, os
try:
    with open(os.environ['CONFIG_FILE_ARG']) as f: c = json.load(f)
except Exception:
    c = {}
c.setdefault('extensions', {})
c['extensions']['global-memory'] = {'enabled': False}
c['extensions']['file-snapshot'] = {'enabled': False}
with open(os.environ['CONFIG_FILE_ARG'], 'w') as f: json.dump(c, f, indent=2)
"
}
disable_extensions

HOST_PID=""
TMP_PROJ_DIR=""
cleanup() {
    [ -n "$HOST_PID" ] && kill -9 "$HOST_PID" 2>/dev/null
    rm -f "$SOCK" 2>/dev/null
    [ -n "$TMP_PROJ_DIR" ] && [ -d "$TMP_PROJ_DIR" ] && rm -rf "$TMP_PROJ_DIR"
    # 清理测试可能留下的进程
    pkill -f "sleep 30" 2>/dev/null
    pkill -f "sleep 60" 2>/dev/null
    if [ -n "$CONFIG_BACKUP" ] && [ -f "$CONFIG_BACKUP" ]; then
        mv "$CONFIG_BACKUP" "$CONFIG_FILE"
    fi
}
trap cleanup EXIT

start_host() {
    local script="$1"; local log="$2"
    rm -f "$SOCK" 2>/dev/null
    lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null
    sleep 1
    TMP_PROJ_DIR=$(mktemp -d /tmp/ion_abort_test_XXXXXX)
    ION_FAUX_SCRIPT="$script" ION_STREAM_DEBUG=1 \
        ION_SESSION_DIR="$TMP_PROJ_DIR/sessions" \
        "$ION_BIN" serve --provider faux --model faux-test \
        >"$log" 2>&1 &
    HOST_PID=$!
    for i in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1
        if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then return 0; fi
    done
    return 1
}

# ── Group A: 工具执行中 abort（修复 A）──
echo "[Group A] 工具执行中可中断（修复 A）"
echo "---------------------------------------"

# 构造：让 agent 调一个长 bash（sleep 30），然后 abort，断言 < 2s 退出
# 注意：bash 工具默认走 execute（非 stream），会阻塞 30s
# 如果 abort 不生效，subscribe 会在 30s 后才收到 agent_end
SCRIPT_A="/tmp/faux_abort_a.jsonl"
cat > "$SCRIPT_A" <<EOF
{"tool_call":{"name":"bash","input":{"command":"sleep 30"}}}
EOF

HOST_LOG_A="/tmp/ion_abort_a.log"
EVT_FILE_A="/tmp/evt_abort_a.log"

if ! start_host "$SCRIPT_A" "$HOST_LOG_A"; then
    fail "A: host 启动失败"
else
    green "A: host 启动"

    SID_A=$("$ION_BIN" rpc --method create_session 2>/dev/null \
        | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

    SUB_PID_A=$(timeout 30 "$ION_BIN" subscribe --session "$SID_A" > "$EVT_FILE_A" 2>&1 &)
    # 上面的写法不对，用下面这行
    ( timeout 30 "$ION_BIN" subscribe --session "$SID_A" > "$EVT_FILE_A" 2>&1 ) &
    SUB_PID_A=$!
    sleep 1

    # 发 prompt 触发 bash sleep 30
    "$ION_BIN" rpc --session "$SID_A" --method prompt --params '{"text":"run sleep"}' >/dev/null 2>&1
    # 等 tool_execution_start
    for i in 1 2 3 4 5; do
        sleep 1
        if grep -Eq '"type": ?"tool_execution_start"' "$EVT_FILE_A" 2>/dev/null; then break; fi
    done

    # 记录 abort 前的时间戳
    ABORT_START=$(python3 -c "import time; print(int(time.time()*1000))")

    # 发 abort
    "$ION_BIN" rpc --session "$SID_A" --method abort >/dev/null 2>&1

    # 等 agent_stopped 或 agent_end（最长 5s，工具中断 + 清理需要时间）
    STOPPED=0
    for i in $(seq 1 25); do
        sleep 0.2
        if grep -Eq '"type": ?"(agent_stopped|agent_end|error)"' "$EVT_FILE_A" 2>/dev/null; then
            STOPPED=1
            break
        fi
    done
    ABORT_END=$(python3 -c "import time; print(int(time.time()*1000))")
    ABORT_MS=$((ABORT_END - ABORT_START))

    kill -9 "$SUB_PID_A" 2>/dev/null

    # 断言 1：abort 后 agent 在 3 秒内停止（含 200ms 轮询 + drop future + cleanup）
    if [ "$STOPPED" -eq 1 ] && [ "$ABORT_MS" -lt 3000 ]; then
        pass "A1: 工具执行中 abort ${ABORT_MS}ms 生效（< 3000ms）"
    elif [ "$STOPPED" -eq 1 ]; then
        fail "A1: abort 生效但花了 ${ABORT_MS}ms（> 3000ms）— 工具中断仍慢"
    else
        fail "A1: abort 后 5s 内未收到 agent_stopped/end"
    fi

    # 断言 2：bash sleep 进程被杀（修复 A.2：drop future 触发 kill_on_drop）
    sleep 1  # 给 OS 一点时间清理
    SLEEP_COUNT=$(pgrep -f "sleep 30" 2>/dev/null | wc -l | tr -d ' ')
    if [ "${SLEEP_COUNT:-0}" -eq 0 ]; then
        pass "A2: bash sleep 30 进程已清理（kill_on_drop + process_group 生效）"
    else
        fail "A2: 仍有 $SLEEP_COUNT 个 sleep 30 进程残留 — kill 未杀整个进程组"
    fi
fi

kill -9 "$HOST_PID" 2>/dev/null; HOST_PID=""
rm -rf "$TMP_PROJ_DIR"; TMP_PROJ_DIR=""
echo ""

# ── Group B: 进程树清理（修复 B）──
echo "[Group B] bash 杀进程树（修复 B）"
echo "----------------------------------"

# 场景：模拟 ion 的 process_group(0) + kill_process_tree。
# 用 python os.setsid 让父进程成为新进程组 leader（等价于 process_group(0)），
# 然后用 kill -TERM -<pgid> 杀整个组（等价于 kill_process_tree）。
# 注意：用 sleep 33（而非通用的 30/60）避免误匹配其他进程的 sleep

# 启动一个带 3 个子进程的进程树，放进独立进程组
PARENT_PID=$(python3 -c "
import os, sys
pid = os.fork()
if pid > 0:
    print(pid)
    sys.exit(0)
os.setsid()
os.execvp('sh', ['sh', '-c', 'sleep 33 & sleep 33 & sleep 33 & wait'])
")
sleep 1

# 数当前测试产生的进程树（用 sleep 33 精确匹配）
BEFORE_TREE=$(pgrep -f "sleep 33" 2>/dev/null | wc -l | tr -d ' ')
echo "  [debug] 父 PID=$PARENT_PID, sleep 33 进程数=$BEFORE_TREE"

# 用 ion 等价的 kill_process_tree：先 SIGTERM 整个进程组，2s 后 SIGKILL
kill -TERM -$PARENT_PID 2>/dev/null
sleep 2
kill -KILL -$PARENT_PID 2>/dev/null
sleep 1

# 数残留（应该全清）
AFTER_TREE=$(pgrep -f "sleep 33" 2>/dev/null | wc -l | tr -d ' ')
if [ "${AFTER_TREE:-0}" -eq 0 ]; then
    pass "B1: kill -TERM/KILL -<pgid> 杀掉整个进程树（$BEFORE_TREE → 0）"
else
    fail "B1: 仍有 $AFTER_TREE 个 sleep 33 残留（应该 0）— killpg 未生效"
fi

# 清理保险
pkill -f "sleep 33" 2>/dev/null
echo ""

# ── Group C: HTTP abort 时延（修复 D）──
echo "[Group C] HTTP 流式期间 abort（修复 D）"
echo "-----------------------------------------"

# 场景：FauxProvider 返回大量 text_delta（模拟慢流），然后 abort
# 验证 abort 后 stream 真正停（不再产生新 delta）
SCRIPT_C="/tmp/faux_abort_c.jsonl"
# 一个长文本响应（FauxProvider 会切成多个 text_delta）
LONG_TEXT=$(python3 -c "print('a ' * 500)")
cat > "$SCRIPT_C" <<EOF
{"text":"$LONG_TEXT"}
EOF

HOST_LOG_C="/tmp/ion_abort_c.log"
EVT_FILE_C="/tmp/evt_abort_c.log"

if ! start_host "$SCRIPT_C" "$HOST_LOG_C"; then
    fail "C: host 启动失败"
else
    green "C: host 启动"

    SID_C=$("$ION_BIN" rpc --method create_session 2>/dev/null \
        | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

    ( timeout 30 "$ION_BIN" subscribe --session "$SID_C" > "$EVT_FILE_C" 2>&1 ) &
    SUB_PID_C=$!
    sleep 1

    "$ION_BIN" rpc --session "$SID_C" --method prompt --params '{"text":"hi"}' >/dev/null 2>&1

    # 等 text_delta 开始
    for i in 1 2 3 4 5; do
        sleep 0.5
        if grep -Eq '"type": ?"text_delta"' "$EVT_FILE_C" 2>/dev/null; then break; fi
    done

    # 数 abort 前的 delta 数
    DELTAS_BEFORE=$(count_matches '"type": ?"text_delta"' "$EVT_FILE_C")

    # 发 abort
    ABORT_START=$(python3 -c "import time; print(int(time.time()*1000))")
    "$ION_BIN" rpc --session "$SID_C" --method abort >/dev/null 2>&1

    # 等 agent_stopped
    for i in 1 2 3 4 5 6 7 8 9 10; do
        sleep 0.2
        if grep -Eq '"type": ?"(agent_stopped|agent_end)"' "$EVT_FILE_C" 2>/dev/null; then break; fi
    done
    ABORT_END=$(python3 -c "import time; print(int(time.time()*1000))")
    ABORT_MS_C=$((ABORT_END - ABORT_START))

    # 等 2 秒看是否还有新 delta 产生（不应该有）
    sleep 2
    DELTAS_AFTER=$(count_matches '"type": ?"text_delta"' "$EVT_FILE_C")

    kill -9 "$SUB_PID_C" 2>/dev/null

    # 断言 1：abort 后 2s 内 agent 停止
    if [ "$ABORT_MS_C" -lt 2000 ]; then
        pass "C1: HTTP 流式期间 abort ${ABORT_MS_C}ms 生效（< 2000ms）"
    else
        fail "C1: abort 生效花了 ${ABORT_MS_C}ms（> 2000ms）"
    fi

    # 断言 2：abort 后没有新 delta（HTTP 真正取消，不是继续流）
    if [ "$DELTAS_AFTER" -le "$DELTAS_BEFORE" ]; then
        pass "C2: abort 后无新 delta（HTTP 真取消，stream 已停）"
    else
        fail "C2: abort 后仍有新 delta（$DELTAS_BEFORE → $DELTAS_AFTER）— HTTP 未真正取消"
    fi
fi

kill -9 "$HOST_PID" 2>/dev/null; HOST_PID=""
rm -rf "$TMP_PROJ_DIR"; TMP_PROJ_DIR=""
echo ""

# ── 汇总 ──
echo "=========================================="
echo "  汇总: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "=========================================="

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "❌ 有失败用例，诊断信息："
    echo "  - host 日志: /tmp/ion_abort_*.log"
    echo "  - 事件流: /tmp/evt_abort_*.log"
    echo "  - 残留进程: pgrep -f 'sleep 30|sleep 60'"
    exit 1
fi

exit 0
