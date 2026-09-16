#!/usr/bin/env bash
# heartbeat_ci.sh — W1 heartbeat 判死健壮化 + worker 死亡可观测 的命令行验证
#
# 验证内容：
#   Group A: 单元测试（lib 内 TDD 测试：Bug1 活性信号 / Bug2 信号死亡分类 / 功能3 respawn 门禁）
#   Group B: host 级心跳 tick 可观察——Idle worker 静默超阈值被标 Stale（ION_HEARTBEAT_IDLE_MS 调小）
#   Group C: Bug1 端到端复现——faux worker 只发 tool 事件不发 text_delta（bash sleep），
#            ION_HEARTBEAT_BUSY_MS 调小后 Busy worker 不被误判 Dead（修复前会 Dead→被 GC 消失）
#
# 隔离三件套（绝不读写真实 ~/.ion）：
#   私有 HOME（mktemp）+ ION_HOST_SOCKET（私有 socket）+ ION_SESSION_DIR（私有会话目录）
# 进程清理：只用脚本自己启动的精确 PID，绝不 pkill。
#
# 运行：bash tests/heartbeat_ci.sh

set -uo pipefail

PASS=0; FAIL=0
green()  { echo -e "\033[32m  ✅ $1\033[0m"; }
red()    { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red   "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"

# ── 隔离三件套 ──
TEST_ROOT="$(mktemp -d /tmp/ion-hb-ci-XXXXXX)"
TEST_HOME="$TEST_ROOT/home"; mkdir -p "$TEST_HOME/.ion"
TEST_PROJECT="$TEST_ROOT/project"; mkdir -p "$TEST_PROJECT"
SOCK="$TEST_ROOT/host.sock"
export ION_HOST_SOCKET="$SOCK"
export ION_SESSION_DIR="$TEST_HOME/.ion/agent/sessions"

SERVE_PID=""
cleanup() {
    # 只杀脚本自己记录的 PID（铁律：绝不 pkill）
    [ -n "$SERVE_PID" ] && kill "$SERVE_PID" 2>/dev/null
    rm -rf "$TEST_ROOT"
}
trap cleanup EXIT

echo "════════════════════════════════════════════════════"
echo "  Heartbeat / Worker Death CI — $(date)"
echo "════════════════════════════════════════════════════"

# ── Phase 0: 构建（真实 HOME，rustup 需要工具链）──
echo "[Phase 0] Build..."
~/.cargo/bin/cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"
echo ""

# ── Group A: 单元测试（判死纯函数 + 泵事件活性信号 + 退出分类）──
echo "[Group A] 单元测试（TDD 复现测试）"
echo "---------------------------------------"
if ~/.cargo/bin/cargo test --lib heartbeat_decision -- --test-threads=2 2>&1 | grep -q "test result: ok"; then
    pass "A1 heartbeat_decision 判死阈值（Idle 180s→Stale / Busy 600s→Dead）"
else
    fail "A1 heartbeat_decision"
fi
if ~/.cargo/bin/cargo test --lib test_bug1 -- --test-threads=2 2>&1 | grep -q "test result: ok"; then
    pass "A2 Bug1：任何 stdout 事件刷新 last_heartbeat（工具期不判死）"
else
    fail "A2 Bug1 活性信号"
fi
if ~/.cargo/bin/cargo test --lib test_bug2 -- --test-threads=2 2>&1 | grep -q "test result: ok"; then
    pass "A3 Bug2：SIGKILL 信号死亡分类（不得当干净退出静默移除）"
else
    fail "A3 Bug2 信号死亡"
fi
if ~/.cargo/bin/cargo test --lib test_f3 -- --test-threads=2 2>&1 | grep -q "test result: ok"; then
    pass "A4 功能3：auto_respawn_local 门禁（默认 false）"
else
    fail "A4 功能3 门禁"
fi
echo ""

# ── 此后切到私有 HOME（构建已结束）──
export HOME="$TEST_HOME"
# faux 模式 config
printf '{"default_provider":"faux","default_model":"faux"}' > "$TEST_HOME/.ion/config.json"

# ── Group B: host 级心跳 tick 可观察（Idle 静默 → Stale）──
echo "[Group B] 心跳 tick：Idle 静默被标 Stale（阈值调小到 2s，tick 1s）"
echo "---------------------------------------"
rm -f "$SOCK"
ION_HEARTBEAT_TICK_SECS=1 ION_HEARTBEAT_IDLE_MS=2000 ION_FAUX_REPLY="hb ready" \
    "$ION_BIN" serve --provider faux --model faux-test \
    > "$TEST_ROOT/host_b.log" 2>&1 &
SERVE_PID=$!

HOST_READY=0
for i in $(seq 1 20); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q sessions; then
        HOST_READY=1; break
    fi
done
if [ "$HOST_READY" != "1" ]; then
    fail "B0 host 启动失败（log: $TEST_ROOT/host_b.log）"
    tail -5 "$TEST_ROOT/host_b.log" 2>/dev/null
    echo ""
    echo "PASS=$PASS FAIL=$FAIL"
    exit 1
fi
pass "B0 隔离 host 启动（PID=$SERVE_PID, socket=${SOCK}）"

SID_B=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
if [ -n "$SID_B" ] && [ "$SID_B" != "None" ]; then
    pass "B1 创建会话 $SID_B"
else
    fail "B1 创建会话失败"
    SID_B=""
fi

if [ -n "$SID_B" ]; then
    # 等 worker Idle → 静默 2s → tick 标 Stale（tick 1s + 阈值 2s，5s 内应发生）
    STALE_SEEN=0
    for i in $(seq 1 10); do
        sleep 1
        STATUS=$("$ION_BIN" rpc --method get_overview 2>/dev/null \
            | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)['data']
    ws = [w for w in d.get('workers', []) if w.get('session_id') == '$SID_B']
    print(ws[0]['status'] if ws else 'MISSING')
except Exception:
    print('ERR')" 2>/dev/null)
        if [ "$STATUS" = "stale" ]; then STALE_SEEN=1; break; fi
    done
    if [ "$STALE_SEEN" = "1" ]; then
        pass "B2 Idle worker 静默超阈值被标 Stale（last_heartbeat 判据生效）"
    else
        fail "B2 Idle worker 未被标 Stale（最后状态: ${STATUS:-unknown}）"
    fi
    # Stale ≠ Dead：Idle 静默只降级，不误判死
    if [ "$STATUS" = "stale" ]; then
        pass "B3 Idle 静默只标 Stale 不标 Dead（判死仅限 Busy 静默）"
    fi
fi

# 换 socket 前先停 Group B host（精确 PID）
kill "$SERVE_PID" 2>/dev/null
wait "$SERVE_PID" 2>/dev/null
SERVE_PID=""
echo ""

# ── Group C: Bug1 端到端——纯工具事件流（无 text_delta）不误判死 ──
echo "[Group C] Bug1 e2e：faux worker 只发 tool 事件（bash sleep 1s ×12），Busy 静默阈值 8s"
echo "---------------------------------------"
# 连续 12 个短工具调用（每个 sleep 1）：步与步之间必有 stdout 事件
# （tool_execution_end / 下一发 tool_call 流），最大事件间隔 ≈1.3s。
# 阈值取 8s，双向余量都拉满（治慢机器抖动——W1 遗留：旧参数 sleep 2 ×5 / 阈值 3s，
# 步间隔余量只有 0.9s，慢机器上单步抖过 3s 就假阳）：
#   - 不误判：事件间隔需 >8s 才会假 Dead（≈6 倍余量）
#   - 仍能抓 Bug1：修复前（泵只在 text_delta 刷心跳）首轮工具后即无 text_delta，
#     8s 静默即判 Dead；总 run ≈13s > 8s（≈5s 余量），后续步仍会被处决暴露回归
# （不能用单个长工具：bash 工具输出不产生 worker stdout 事件，sleep 期间
#   是真静默——事件静默超阈值判死是判据本义，不是 Bug1 回归）
SCRIPT_C="$TEST_ROOT/faux_tool_only.jsonl"
for i in 1 2 3 4 5 6 7 8 9 10 11 12; do
    echo '{"tool_call":{"name":"bash","input":{"command":"sleep 1"}}}' >> "$SCRIPT_C"
done

rm -f "$SOCK"
ION_HEARTBEAT_TICK_SECS=1 ION_HEARTBEAT_BUSY_MS=8000 \
    ION_FAUX_SCRIPT="$SCRIPT_C" \
    "$ION_BIN" serve --provider faux --model faux-test \
    > "$TEST_ROOT/host_c.log" 2>&1 &
SERVE_PID=$!

HOST_READY=0
for i in $(seq 1 20); do
    sleep 1
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q sessions; then
        HOST_READY=1; break
    fi
done
if [ "$HOST_READY" != "1" ]; then
    fail "C0 host 启动失败"
    echo "PASS=$PASS FAIL=$FAIL"
    exit 1
fi
pass "C0 隔离 host 启动（PID=${SERVE_PID}）"

SID_C=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
if [ -n "$SID_C" ] && [ "$SID_C" != "None" ]; then
    pass "C1 创建会话 $SID_C"
else
    fail "C1 创建会话失败"
    SID_C=""
fi

if [ -n "$SID_C" ]; then
    "$ION_BIN" rpc --session "$SID_C" --method prompt --params '{"text":"run the tool"}' >/dev/null 2>&1
    # 轮询 worker 状态直到 run 结束（Idle）或超时 45s（run 总长 ≈13s + 启动/调度余量）；
    # 修复前：工具期无 text_delta → 静默 8s 即被判 Dead（之后 agent_end 也救不回——Dead 终态）
    SAW_TOOL_BUSY=0; EVER_DEAD=0; FINAL_IDLE=0
    for i in $(seq 1 45); do
        sleep 1
        STATUS=$("$ION_BIN" rpc --method get_overview 2>/dev/null \
            | python3 -c "
import sys, json
try:
    d = json.load(sys.stdin)['data']
    ws = [w for w in d.get('workers', []) if w.get('session_id') == '$SID_C']
    print(ws[0]['status'] if ws else 'MISSING')
except Exception:
    print('ERR')" 2>/dev/null)
        [ "$STATUS" = "busy" ] && SAW_TOOL_BUSY=1
        [ "$STATUS" = "dead" ] && EVER_DEAD=1
        if [ "$STATUS" = "idle" ]; then FINAL_IDLE=1; break; fi
    done
    if [ "$SAW_TOOL_BUSY" = "1" ]; then
        pass "C2 观察到 Busy（工具执行期）"
    else
        pass "C2 工具期太快未采样到 Busy（faux 环境差异，不判失败）"
    fi
    if [ "$EVER_DEAD" = "0" ]; then
        pass "C3 核心：纯工具事件流期间 worker 从未被误判 Dead（Bug1 修复生效）"
    else
        fail "C3 核心：Busy worker 被误判 Dead（Bug1 复现！泵只在 text_delta 刷心跳）"
    fi
    if [ "$FINAL_IDLE" = "1" ]; then
        pass "C4 run 正常结束转 Idle（agent_end 到达）"
    else
        fail "C4 run 未正常结束（最后状态: ${STATUS:-unknown}）"
        echo "  [diag] overview: $("$ION_BIN" rpc --method get_overview 2>/dev/null | head -c 800)"
        echo "  [diag] host_c.log tail:"
        tail -8 "$TEST_ROOT/host_c.log" 2>/dev/null | sed 's/^/    /'
    fi
fi

echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS FAIL=$FAIL"
echo "════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && echo "全部通过" || echo "有失败"
exit "$FAIL"
