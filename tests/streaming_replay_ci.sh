#!/usr/bin/env bash
# streaming_replay_ci.sh
#
# 流式 tool_call_delta 回归测试（Record/Replay 版）。
#
# 与 streaming_throughput_ci.sh 的区别：
#   - throughput_ci 用 FauxProvider + 手工构造的 BIG_CONTENT（合成数据）
#   - replay_ci 用真实 DeepSeek 录制的响应（真实业务数据 + 真实 LLM 生成的内容）
#
# 录制来源：tests/fixtures/recordings/stream_demo/trace.jsonl
#   - 由 DeepSeek V4 Flash 经 opencode.ai 真实生成
#   - 包含 3 个响应：write tool_call（30 行文件）+ read tool_call + 总结文本
#   - stop_reason 已持久化（record.rs bug 已修）
#
# 回放机制：
#   - --model replay/stream_demo 加载 trace.jsonl
#   - faux_stream_blocks 把 args 重新切成 3-8 字节 chunks（token 级粒度）
#   - 走完整 agent loop（tool 真实执行，写文件到 /tmp）
#
# 运行：bash tests/streaming_replay_ci.sh
# 不调真实 LLM（用录制回放），但内容是真实 LLM 生成的。

set -uo pipefail

PASS=0; FAIL=0; SKIP=0
green()  { echo -e "\033[32m  ✅ $1\033[0m"; }
red()    { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow() { echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red   "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

# 安全的 grep 计数
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
FIXTURE_DIR="$PROJECT_DIR/tests/fixtures/recordings/stream_demo"
RECORDING_DIR="$HOME/.ion/recordings/stream_demo"

echo "=========================================="
echo "  Streaming Replay CI (Recorded DeepSeek)"
echo "=========================================="
echo ""

# ── Phase 0: build ──
echo "[Phase 0] Build ion + ion-worker..."
cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
green "build OK"
echo ""

# ── 准备：把 fixture 复制到 ~/.ion/recordings/ ──
# （replay 只从 ~/.ion/recordings/<id>/ 读，不从 repo fixtures 读）
if [ ! -f "$FIXTURE_DIR/trace.jsonl" ]; then
    echo "❌ fixture 不存在：$FIXTURE_DIR/trace.jsonl"
    echo "   需要先录制：ION_RECORD=stream_demo ion -p '...' 然后 cp 到 fixtures/"
    exit 1
fi
mkdir -p "$RECORDING_DIR"
cp "$FIXTURE_DIR"/* "$RECORDING_DIR/" 2>/dev/null
green "fixture 已加载到 $RECORDING_DIR"
echo ""

# ── 临时禁用扩展（避免 memory-agent 干扰）──
CONFIG_FILE="$HOME/.ion/config.json"
CONFIG_BACKUP=""
disable_extensions() {
    if [ -f "$CONFIG_FILE" ]; then
        CONFIG_BACKUP="$CONFIG_FILE.replay_ci_bak"
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

# ── 通用 host 启动 + cleanup ──
HOST_PID=""
TMP_PROJ_DIR=""
cleanup() {
    [ -n "$HOST_PID" ] && kill -9 "$HOST_PID" 2>/dev/null
    rm -f "$SOCK" 2>/dev/null
    [ -n "$TMP_PROJ_DIR" ] && [ -d "$TMP_PROJ_DIR" ] && rm -rf "$TMP_PROJ_DIR"
    if [ -n "$CONFIG_BACKUP" ] && [ -f "$CONFIG_BACKUP" ]; then
        mv "$CONFIG_BACKUP" "$CONFIG_FILE"
    fi
}
trap cleanup EXIT

start_host() {
    local log="$1"
    rm -f "$SOCK" 2>/dev/null
    lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null
    sleep 1

    TMP_PROJ_DIR=$(mktemp -d /tmp/ion_replay_test_XXXXXX)

    ION_STREAM_DEBUG=1 \
        ION_SESSION_DIR="$TMP_PROJ_DIR/sessions" \
        ION_MAX_TURNS=3 \
        "$ION_BIN" serve --model replay/stream_demo \
        >"$log" 2>&1 &
    HOST_PID=$!

    for i in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1
        if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then
            return 0
        fi
    done
    return 1
}

subscribe_capture() {
    timeout "${3:-15}" "$ION_BIN" subscribe --session "$1" >"$2" 2>&1 &
    local pid=$!
    sleep 1
    echo "$pid"
}

# ── Group A: 回放真实 DeepSeek 响应，验证流式 delta ──
echo "[Group A] 回放真实 DeepSeek write tool_call 流式"
echo "---------------------------------------------------"

HOST_LOG_A="/tmp/ion_replay_a.log"
EVT_FILE_A="/tmp/evt_replay_a.log"
rm -f /tmp/record_demo.txt  # 清掉旧文件

if ! start_host "$HOST_LOG_A"; then
    fail "A: host 启动失败"
else
    green "A: host 启动 + 加载录制 stream_demo"

    SID_A=$("$ION_BIN" rpc --method create_worker --params \
        '{"agent":"build","model":"replay/stream_demo","provider":"replay"}' 2>/dev/null \
        | python3 -c "import sys,json;d=json.load(sys.stdin)['data'];print(d.get('sessionId') or d.get('session_id') or '')" 2>/dev/null)
    if [ -z "$SID_A" ]; then
        fail "A: 创建 session 失败"
        exit 1
    fi

    SUB_PID_A=$(subscribe_capture "$SID_A" "$EVT_FILE_A" 30)
    sleep 0.5

    "$ION_BIN" rpc --session "$SID_A" --method prompt --params '{"text":"写文件"}' >/dev/null 2>&1

    # 等 tool_execution_end（录制有 2 个 tool_call：write + read）
    for i in $(seq 1 25); do
        sleep 1
        if grep -Eq '"type": ?"tool_execution_end"' "$EVT_FILE_A" 2>/dev/null; then
            sleep 1
            break
        fi
    done
    kill -9 "$SUB_PID_A" 2>/dev/null

    # 断言 1：subscribe 收到 tool_call_delta（真实 DeepSeek 内容被切成多个 chunks）
    RECEIVED_DELTAS=$(count_matches '"type": ?"tool_call_delta"' "$EVT_FILE_A")
    if [ "${RECEIVED_DELTAS:-0}" -ge 30 ]; then
        pass "A1: subscribe 收到 ${RECEIVED_DELTAS} 个 tool_call_delta (≥30，真实 DeepSeek 内容流式)"
    else
        fail "A1: subscribe 只收到 ${RECEIVED_DELTAS} 个 tool_call_delta (<30) — 可能丢事件"
    fi

    # 断言 2：有 tool_execution_end（write 工具真实执行）
    TE_COUNT=$(count_matches '"type": ?"tool_execution_end"' "$EVT_FILE_A")
    if [ "${TE_COUNT:-0}" -ge 1 ]; then
        pass "A2: 收到 ${TE_COUNT} 个 tool_execution_end（write 工具真实执行）"
    else
        fail "A2: 没收到 tool_execution_end — write 工具没执行"
    fi

    # 断言 3：文件真实写入（30 行）
    if [ -f /tmp/record_demo.txt ]; then
        LINE_COUNT=$(wc -l < /tmp/record_demo.txt 2>/dev/null || echo 0)
        if [ "${LINE_COUNT:-0}" -ge 30 ]; then
            pass "A3: 文件真实写入 ${LINE_COUNT} 行（DeepSeek 生成的内容落地）"
        else
            fail "A3: 文件只有 ${LINE_COUNT} 行（预期 ≥30）"
        fi
    else
        fail "A3: 文件 /tmp/record_demo.txt 没写入"
    fi

    # 断言 4：DROP 比例 ≤ 60%（同 throughput_ci 标准）
    DROP_COUNT=$(count_matches 'host DROP' "$HOST_LOG_A")
    TOTAL_EMIT=$((RECEIVED_DELTAS + DROP_COUNT))
    DROP_RATIO=0
    if [ "$TOTAL_EMIT" -gt 0 ]; then
        DROP_RATIO=$((DROP_COUNT * 100 / TOTAL_EMIT))
    fi
    if [ "$DROP_RATIO" -le 60 ]; then
        pass "A4: DROP 比例 ${DROP_RATIO}% (${DROP_COUNT}/${TOTAL_EMIT}) - 健康"
    else
        fail "A4: DROP 比例 ${DROP_RATIO}% (${DROP_COUNT}/${TOTAL_EMIT}) - 严重落后"
    fi
fi

kill -9 "$HOST_PID" 2>/dev/null
HOST_PID=""
echo ""

# ── Group B: 回放内容正确性（DeepSeek 真实生成的 30 行内容）──
echo "[Group B] 回放内容正确性（真实 DeepSeek 生成的内容）"
echo "------------------------------------------------------"

# 录制中 write tool_call 的 content 是 DeepSeek 真实生成的：
# "line 1 hello world test content number 1\nline 2 ...\n... line 30 ..."
# 验证回放后文件内容包含真实 DeepSeek 生成的行

if [ -f /tmp/record_demo.txt ]; then
    # 检查是否包含 DeepSeek 生成的关键行
    if grep -q "line 1 hello world test content number 1" /tmp/record_demo.txt \
       && grep -q "line 30 hello world test content number 30" /tmp/record_demo.txt; then
        pass "B1: 文件包含 DeepSeek 真实生成的内容（line 1 + line 30 都在）"
    else
        fail "B1: 文件内容不匹配 DeepSeek 录制 — 可能回放了错误的内容"
    fi

    # 检查行数精确匹配
    LINE_COUNT_B=$(wc -l < /tmp/record_demo.txt)
    if [ "$LINE_COUNT_B" -eq 30 ] || [ "$LINE_COUNT_B" -eq 31 ]; then
        pass "B2: 文件行数 ${LINE_COUNT_B}（DeepSeek 录制的 30 行 + 可能的尾换行）"
    else
        fail "B2: 文件行数 ${LINE_COUNT_B}（预期 30）"
    fi
else
    fail "B1/B2: 文件不存在（Group A 失败导致）"
    FAIL=$((FAIL+1))
fi
echo ""

# ── Group C: 多次回放一致性（确定性）──
echo "[Group C] 多次回放一致性（录制是确定性的）"
echo "--------------------------------------------"

# 录制应该是确定性的——同样的 trace.jsonl 回放两次应该产生相同内容
rm -f /tmp/record_demo_run2.txt
# 用 scene-1 直接跑（不需要 host）
timeout 30 "$ION_BIN" --model replay/stream_demo -p "test" >/dev/null 2>&1
if [ -f /tmp/record_demo.txt ]; then
    HASH1=$(md5 -q /tmp/record_demo.txt 2>/dev/null || md5sum /tmp/record_demo.txt | awk '{print $1}')
    cp /tmp/record_demo.txt /tmp/record_demo_run1.txt

    rm -f /tmp/record_demo.txt
    timeout 30 "$ION_BIN" --model replay/stream_demo -p "test" >/dev/null 2>&1
    if [ -f /tmp/record_demo.txt ]; then
        HASH2=$(md5 -q /tmp/record_demo.txt 2>/dev/null || md5sum /tmp/record_demo.txt | awk '{print $1}')
        if [ "$HASH1" = "$HASH2" ]; then
            pass "C1: 两次回放产生相同内容（hash: ${HASH1:0:8}...）"
        else
            fail "C1: 两次回放 hash 不同（${HASH1:0:8} vs ${HASH2:0:8}）— 录制不确定性"
        fi
    else
        fail "C1: 第二次回放没生成文件"
    fi
else
    fail "C1: 第一次回放没生成文件"
fi
rm -f /tmp/record_demo_run1.txt /tmp/record_demo_run2.txt
echo ""

# ── 汇总 ──
echo "=========================================="
echo "  汇总: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "=========================================="

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "❌ 有失败用例，诊断信息："
    echo "  - 查看 host 日志: /tmp/ion_replay_*.log"
    echo "  - 查看事件流: /tmp/evt_replay_*.log"
    echo "  - 查看录制内容: cat $FIXTURE_DIR/trace.jsonl"
    exit 1
fi

exit 0
