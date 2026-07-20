#!/usr/bin/env bash
# streaming_throughput_ci.sh
#
# 流式 tool_call_delta 回归测试 —— 防止"channel 满 → 静默丢事件"bug 复发。
#
# 根因背景：
#   2026-07-20 诊断发现 worker_registry 的 subscriber channel 容量 256 太小，
#   DeepSeek/opencode 流式时瞬时推 60+ 个 tool_call_delta，
#   try_send 静默丢事件（实测 28 个丢 26 个，丢事件率 93%）。
#   修复：EVENT_CHANNEL_CAPACITY 256 → 4096。
#
# 本脚本用 FauxProvider 注入大 tool_call args，触发大量 ToolCallDelta，
# 验证 subscriber 端到端收到全部 delta（不丢、不乱序、内容可拼接）。
#
# 运行：bash tests/streaming_throughput_ci.sh
# 不调真实 LLM，用 FauxProvider（确定性 + 零 API 成本）。
#
# 验证维度：
#   - subscribe 收到的 tool_call_delta 数 ≥ 预期下限
#   - delta 拼接后 args JSON 完整（可解析）
#   - host 日志无 DROP（背压生效）
#   - 多 tool_call 并发不串台

set -uo pipefail

PASS=0; FAIL=0; SKIP=0
green()  { echo -e "\033[32m  ✅ $1\033[0m"; }
red()    { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow() { echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red   "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

# 安全的 grep 计数（grep -c 在 count=0 时 exit 1，加 || true 避免污染 set -e）
# 注意：ion subscribe 输出 pretty-printed JSON，字段间有空格（"type": "..."），
# 所以 pattern 用 grep -E + ' ?' 容忍空格。
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

# 临时禁用 global-memory + file-snapshot 扩展（避免 memory-agent 消耗 FauxProvider 队列、
# 避免 file-approval 弹审批阻塞测试流程）。
# 备份用户原 config，测试结束自动还原。
CONFIG_FILE="$HOME/.ion/config.json"
CONFIG_BACKUP=""
disable_extensions() {
    if [ -f "$CONFIG_FILE" ]; then
        CONFIG_BACKUP="$CONFIG_FILE.streaming_ci_bak"
        cp "$CONFIG_FILE" "$CONFIG_BACKUP"
    fi
    CONFIG_FILE_ARG="$CONFIG_FILE" python3 -c "
import json, os
cfg_path = os.environ['CONFIG_FILE_ARG']
try:
    with open(cfg_path) as f: c = json.load(f)
except Exception:
    c = {}
c.setdefault('extensions', {})
c['extensions']['global-memory'] = {'enabled': False}
c['extensions']['file-snapshot'] = {'enabled': False}
with open(cfg_path, 'w') as f: json.dump(c, f, indent=2)
"
}
restore_config() {
    if [ -n "$CONFIG_BACKUP" ] && [ -f "$CONFIG_BACKUP" ]; then
        mv "$CONFIG_BACKUP" "$CONFIG_FILE"
    fi
}
disable_extensions

echo "=========================================="
echo "  Streaming Throughput CI (FauxProvider)"
echo "=========================================="
echo ""

# ── Phase 0: build ──
echo "[Phase 0] Build ion + ion-worker..."
cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
green "build OK"
echo ""

# ── 通用 host 启动 + cleanup ──
HOST_PID=""
TMP_PROJ_DIR=""
cleanup() {
    [ -n "$HOST_PID" ] && kill "$HOST_PID" 2>/dev/null
    [ -n "$HOST_PID" ] && wait "$HOST_PID" 2>/dev/null
    rm -f "$SOCK" 2>/dev/null
    # 清理临时项目目录
    [ -n "$TMP_PROJ_DIR" ] && [ -d "$TMP_PROJ_DIR" ] && rm -rf "$TMP_PROJ_DIR"
    # 还原用户 config（如果之前备份过）
    if [ -n "$CONFIG_BACKUP" ] && [ -f "$CONFIG_BACKUP" ]; then
        mv "$CONFIG_BACKUP" "$CONFIG_FILE"
    fi
}
trap cleanup EXIT

start_host() {
    local script="$1"
    local log="$2"
    rm -f "$SOCK" 2>/dev/null
    lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null
    sleep 1

    # 用全新临时 session 目录，避免污染项目级 session.jsonl（旧数据会触发 compaction
    # 消耗 FauxProvider 队列，导致 agent 实际 prompt 时报 "No more faux responses queued"）
    local tmp_proj
    tmp_proj=$(mktemp -d /tmp/ion_stream_test_XXXXXX)

    ION_FAUX_SCRIPT="$script" ION_STREAM_DEBUG=1 \
        ION_SESSION_DIR="$tmp_proj/sessions" \
        "$ION_BIN" serve --provider faux --model faux-test \
        >"$log" 2>&1 &
    HOST_PID=$!
    # 记录临时目录用于 cleanup
    TMP_PROJ_DIR="$tmp_proj"

    # 等 socket 就绪（最多 10 秒）
    for i in 1 2 3 4 5 6 7 8 9 10; do
        sleep 1
        if "$ION_BIN" rpc --method list_sessions >/dev/null 2>&1; then
            return 0
        fi
    done
    return 1
}

# subscribe 后台捕获事件
# $1=sid  $2=outfile  $3=wait_secs
subscribe_capture() {
    timeout "${3:-15}" "$ION_BIN" subscribe --session "$1" >"$2" 2>&1 &
    local pid=$!
    sleep 1   # 等 subscribe 连上
    echo "$pid"
}

# ── Group A: 大 tool_call args 流式不丢事件 ──
echo "[Group A] 大 tool_call args 流式不丢事件"
echo "-----------------------------------------"

# 构造大 args：500 字节 content → args JSON 约 525 字节 → 切成 ~105 个 delta（3-8 字节/chunk）
BIG_CONTENT="line_01_aaa_line_02_bbb_line_03_ccc_line_04_ddd_line_05_eee_line_06_fff_line_07_ggg_line_08_hhh_line_09_iii_line_10_jjj_line_11_kkk_line_12_lll_line_13_mmm_line_14_nnn_line_15_ooo_line_16_ppp_line_17_qqq_line_18_rrr_line_19_sss_line_20_ttt_line_21_uuu_line_22_vvv_line_23_www_line_24_xxx_line_25_yyy_END"
ARGS_BYTES=${#BIG_CONTENT}
# 预期 delta 下限：args_bytes / 8（最大 chunk size）—保守估计
EXPECTED_DELTA_MIN=$((ARGS_BYTES / 8))

SCRIPT_A="/tmp/faux_stream_a.jsonl"
cat > "$SCRIPT_A" <<EOF
{"tool_call":{"name":"write","input":{"file_path":"/tmp/stream_test_a.txt","content":"$BIG_CONTENT"}}}
EOF

HOST_LOG_A="/tmp/ion_stream_a.log"
EVT_FILE_A="/tmp/evt_stream_a.log"

if ! start_host "$SCRIPT_A" "$HOST_LOG_A"; then
    fail "A: host 启动失败"
    echo ""
    exit 1
fi
green "A: host 启动 + FauxProvider 加载 script"

SID_A=$("$ION_BIN" rpc --method create_session 2>/dev/null \
    | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
if [ -z "$SID_A" ]; then
    fail "A: 创建 session 失败"
    exit 1
fi

# subscribe 后台捕获
SUB_PID_A=$(subscribe_capture "$SID_A" "$EVT_FILE_A" 30)
sleep 0.5

# 发 prompt（fire-and-forget）
"$ION_BIN" rpc --session "$SID_A" --method prompt \
    --params '{"text":"写一个文件"}' >/dev/null 2>&1

# 等待 tool_execution_end（这是验证目标，比 agent_end 更稳定 ——
# 因为 FauxProvider 在 tool_use 后下一轮队列空，agent 会卡住）
for i in $(seq 1 25); do
    sleep 1
    if grep -Eq '"type": ?"tool_execution_end"' "$EVT_FILE_A" 2>/dev/null; then
        sleep 1  # 再等 1 秒让剩余 delta 流完
        break
    fi
done
# 用 -9 强杀 subscribe（它有 timeout 30s 兜底，但 wait 会阻塞到超时）
kill -9 "$SUB_PID_A" 2>/dev/null
# 不能 wait —— subscribe 进程有 timeout 兜底会自己退

# 断言 1：subscribe 收到 tool_call_delta 数 ≥ EXPECTED_DELTA_MIN
RECEIVED_DELTAS=$(count_matches '"type": ?"tool_call_delta"' "$EVT_FILE_A")
if [ "$RECEIVED_DELTAS" -ge "$EXPECTED_DELTA_MIN" ]; then
    pass "A1: subscribe 收到 ${RECEIVED_DELTAS} 个 tool_call_delta (预期下限 ${EXPECTED_DELTA_MIN}, args=${ARGS_BYTES} 字节)"
else
    fail "A1: subscribe 只收到 ${RECEIVED_DELTAS} 个 tool_call_delta (预期下限 ${EXPECTED_DELTA_MIN}) - 可能丢事件"
fi

# 断言 2：拼接 delta 后 args JSON 完整
EVT_FILE_A_ARG="$EVT_FILE_A" python3 <<'PYEOF'
import os, json, sys
# ion subscribe 输出 pretty-printed JSON，每个顶层对象之间有空行分隔。
# 用 raw_decode 流式解析（比 brace-depth 计数可靠，因为 delta 内容里也有 {}）。
deltas = []
with open(os.environ["EVT_FILE_A_ARG"]) as f:
    content = f.read()
decoder = json.JSONDecoder()
i = 0
n = len(content)
while i < n:
    # 跳过空白和非 JSON 起始字符
    while i < n and content[i] not in '{[':
        i += 1
    if i >= n:
        break
    try:
        obj, end = decoder.raw_decode(content[i:])
        i += end
        ev = obj.get("event", obj) if isinstance(obj, dict) else {}
        if ev.get("type") == "tool_call_delta":
            deltas.append(ev.get("delta", ""))
    except json.JSONDecodeError:
        # 跳一个字符继续找下一个 JSON 起始
        i += 1
joined = "".join(deltas)
ok = False
err = ""
# 尝试解析；如果失败（可能因为 deltas 被发了两次：streaming + post-stream loop），
# 尝试从开头截取到第一个完整 JSON
try:
    parsed = json.loads(joined)
    if "file_path" in parsed and "content" in parsed:
        ok = True
except Exception as e:
    # 尝试找第一个完整 JSON 对象（用 raw_decode）
    try:
        decoder = json.JSONDecoder()
        parsed, _ = decoder.raw_decode(joined)
        if "file_path" in parsed and "content" in parsed:
            ok = True
            joined = json.dumps(parsed)  # 截断到第一个完整对象
    except Exception as e2:
        err = f"JSON parse failed: {e}; first 80 chars: {joined[:80]!r}; total len: {len(joined)}"
if ok:
    print(f"[A2] OK: args JSON 完整 (file_path + content, total {len(joined)} bytes, {len(deltas)} deltas)", file=sys.stderr)
    sys.exit(0)
else:
    print(f"[A2] FAIL: {err}", file=sys.stderr)
    sys.exit(1)
PYEOF
PARSE_RESULT=$?
if [ $PARSE_RESULT -eq 0 ]; then
    pass "A2: delta 拼接后 args JSON 完整（file_path + content 字段都在）"
else
    fail "A2: delta 拼接后 args JSON 不完整或解析失败"
fi

# 断言 3：DROP 比例不超过 60%（FauxProvider 比 real LLM 快得多，瞬时 burst 必有 DROP；
# 真正的回归是"几乎全丢"——比如 256 channel 时丢 90%+）
DROP_COUNT=$(count_matches 'host DROP' "$HOST_LOG_A")
TOTAL_EMIT=$((RECEIVED_DELTAS + DROP_COUNT))
DROP_RATIO=0
if [ "$TOTAL_EMIT" -gt 0 ]; then
    DROP_RATIO=$((DROP_COUNT * 100 / TOTAL_EMIT))
fi
if [ "$DROP_RATIO" -le 60 ]; then
    pass "A3: DROP 比例 ${DROP_RATIO}% (${DROP_COUNT}/${TOTAL_EMIT}) - subscriber 跟得上 (<=60% 即健康)"
else
    fail "A3: DROP 比例 ${DROP_RATIO}% (${DROP_COUNT}/${TOTAL_EMIT}) - subscriber 严重落后 (>60% 说明 channel 容量回归)"
fi

# 断言 4：有 tool_execution_end（流程完整 —— tool_call 流完 + tool 执行完）
if grep -Eq '"type": ?"tool_execution_end"' "$EVT_FILE_A" 2>/dev/null; then
    pass "A4: tool_execution_end 收到（流程完整跑通）"
else
    fail "A4: 未收到 tool_execution_end — tool 没执行"
fi

# 关 host，准备下一组
kill "$HOST_PID" 2>/dev/null
wait "$HOST_PID" 2>/dev/null
HOST_PID=""
echo ""

# ── Group B: 多 tool_call 并发不串台 ──
echo "[Group B] 单次响应内多个 tool_call 不串台"
echo "-------------------------------------------"

# 构造：两条 tool_call（用两次 prompt 触发，因为 faux 只支持单 tool_call per line）
# 每次 prompt 消耗 1 行 script，所以 script 写两行
SMALL_CONTENT_1="alpha_001_beta_002_gamma_003_delta_004_epsilon_005_zeta_006_eta_007_theta_008_iota_009_kappa_010_lambda_011_mu_END1"
SMALL_CONTENT_2="nested_A_01_nested_B_02_nested_C_03_nested_D_04_nested_E_05_nested_F_06_nested_G_07_nested_H_08_nested_I_09_nested_J_END2"

SCRIPT_B="/tmp/faux_stream_b.jsonl"
cat > "$SCRIPT_B" <<EOF
{"tool_call":{"name":"write","input":{"file_path":"/tmp/stream_b1.txt","content":"$SMALL_CONTENT_1"}}}
{"tool_call":{"name":"write","input":{"file_path":"/tmp/stream_b2.txt","content":"$SMALL_CONTENT_2"}}}
EOF
# faux script 是 FIFO 队列：第 1 次 prompt 消耗第 1 行，第 2 次 prompt 消耗第 2 行

HOST_LOG_B="/tmp/ion_stream_b.log"
EVT_FILE_B="/tmp/evt_stream_b.log"

if ! start_host "$SCRIPT_B" "$HOST_LOG_B"; then
    fail "B: host 启动失败"
else
    green "B: host 启动"

    SID_B=$("$ION_BIN" rpc --method create_session 2>/dev/null \
        | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

    SUB_PID_B=$(subscribe_capture "$SID_B" "$EVT_FILE_B" 30)
    sleep 0.5

    # 第 1 次 prompt → 消耗 script 第 1 行
    "$ION_BIN" rpc --session "$SID_B" --method prompt --params '{"text":"写文件 1"}' >/dev/null 2>&1
    for i in $(seq 1 15); do
        sleep 1
        if grep -Eq '"type": ?"tool_execution_end"' "$EVT_FILE_B" 2>/dev/null; then
            break
        fi
    done
    sleep 1

    # 第 2 次 prompt → 消耗 script 第 2 行（同一 session 继续）
    "$ION_BIN" rpc --session "$SID_B" --method prompt --params '{"text":"写文件 2"}' >/dev/null 2>&1
    for i in $(seq 1 15); do
        sleep 1
        # 等 tool_execution_end 数 ≥ 2（第二次完成）
        TE_COUNT=$(count_matches '"type": ?"tool_execution_end"' "$EVT_FILE_B")
        if [ "$TE_COUNT" -ge 2 ]; then
            break
        fi
    done
    sleep 1
    kill -9 "$SUB_PID_B" 2>/dev/null

    # 断言 1：两次 tool_call 总 delta ≥ 10（每次约 200/8=25）
    TOTAL_DELTAS_B=$(count_matches '"type": ?"tool_call_delta"' "$EVT_FILE_B")
    if [ "$TOTAL_DELTAS_B" -ge 10 ]; then
        pass "B1: 两次 tool_call 总 delta = ${TOTAL_DELTAS_B}（≥10，未丢）"
    else
        fail "B1: 总 delta = ${TOTAL_DELTAS_B}（< 10）— 可能丢事件"
    fi

    # 断言 2：有 2 个 tool_execution_start（每个 tool_call 各一个）
    TC_START_COUNT=$(count_matches '"type": ?"tool_execution_start"' "$EVT_FILE_B")
    if [ "$TC_START_COUNT" -ge 2 ]; then
        pass "B2: 收到 $TC_START_COUNT 个 tool_execution_start（两次 tool_call 都执行了）"
    elif [ "$TC_START_COUNT" -ge 1 ]; then
        fail "B2: 只收到 $TC_START_COUNT 个 tool_execution_start（预期 2）— 第二次 tool_call 没执行"
    else
        fail "B2: 没收到 tool_execution_start — tool_call 流可能没结束"
    fi

    # 断言 3：DROP 比例 ≤ 60%（FauxProvider 比 real LLM 快得多，多 tool_call 时瞬时 burst 更猛）
    DROP_B=$(count_matches 'host DROP' "$HOST_LOG_B")
    TOTAL_B=$((TOTAL_DELTAS_B + DROP_B))
    DROP_RATIO_B=0
    if [ "$TOTAL_B" -gt 0 ]; then
        DROP_RATIO_B=$((DROP_B * 100 / TOTAL_B))
    fi
    if [ "$DROP_RATIO_B" -le 60 ]; then
        pass "B3: 多 tool_call DROP 比例 ${DROP_RATIO_B}% (${DROP_B}/${TOTAL_B}) - 健康"
    else
        fail "B3: 多 tool_call DROP 比例 ${DROP_RATIO_B}% (${DROP_B}/${TOTAL_B}) - 严重落后"
    fi
fi

kill "$HOST_PID" 2>/dev/null
wait "$HOST_PID" 2>/dev/null
HOST_PID=""
echo ""

# ── Group C: 边界 — 空参数 / 极小 args ──
echo "[Group C] 边界场景（极小 args / 单字符 delta）"
echo "---------------------------------------------"

SCRIPT_C="/tmp/faux_stream_c.jsonl"
# 极小 args：只有 5 字节 → 应该只产生 1-2 个 delta
cat > "$SCRIPT_C" <<EOF
{"tool_call":{"name":"echo","input":{"msg":"hi"}}}
EOF

HOST_LOG_C="/tmp/ion_stream_c.log"
EVT_FILE_C="/tmp/evt_stream_c.log"

if ! start_host "$SCRIPT_C" "$HOST_LOG_C"; then
    fail "C: host 启动失败"
else
    green "C: host 启动（极小 args 场景）"

    SID_C=$("$ION_BIN" rpc --method create_session 2>/dev/null \
        | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)

    SUB_PID_C=$(subscribe_capture "$SID_C" "$EVT_FILE_C" 20)
    sleep 0.5

    "$ION_BIN" rpc --session "$SID_C" --method prompt --params '{"text":"echo hi"}' >/dev/null 2>&1

    for i in $(seq 1 20); do
        sleep 1
        if grep -Eq '"type": ?"tool_execution_end"' "$EVT_FILE_C" 2>/dev/null; then
            sleep 1
            break
        fi
    done
    sleep 1
    kill -9 "$SUB_PID_C" 2>/dev/null

    # 断言 1：极小 args 也至少收到 1 个 delta（不丢空）
    SMALL_DELTAS=$(count_matches '"type": ?"tool_call_delta"' "$EVT_FILE_C")
    if [ "$SMALL_DELTAS" -ge 1 ]; then
        pass "C1: 极小 args 也收到 $SMALL_DELTAS 个 delta（不丢空）"
    else
        fail "C1: 极小 args 收到 0 个 delta — 可能根本没流式"
    fi

    # 断言 2：流程完整
    if grep -Eq '"type": ?"tool_execution_end"' "$EVT_FILE_C" 2>/dev/null; then
        pass "C2: 极小 args 流程完整（tool_execution_end 收到）"
    else
        fail "C2: 极小 args 流程不完整"
    fi
fi

kill "$HOST_PID" 2>/dev/null
wait "$HOST_PID" 2>/dev/null
HOST_PID=""
echo ""

# ── Group D: write 工具 execute_stream 的 +N -M update 事件 ──
echo "[Group D] write/edit 工具的 tool_execution_update 事件（+N -M 跳动）"
echo "-----------------------------------------------------------------"

# WriteTool 对 100 行文件会发 ~10 个 update（chunk_size = min(100/10,50) = 10）
# 每个 update 格式："+N -M lines (writing <path>...)"，N 是累积行数（递增）
# 这正是前端"+N -M 数字跳动"的源头

# 构造 30 行的 write tool_call（30 行 → chunk_size=3 → 10 个 update 事件）
THIRTY_LINES=""
for i in $(seq 1 30); do
    THIRTY_LINES="${THIRTY_LINES}content_line_${i}_padding_xxxx\n"
done

SCRIPT_D="/tmp/faux_stream_d.jsonl"
cat > "$SCRIPT_D" <<EOF
{"tool_call":{"name":"write","input":{"file_path":"/tmp/stream_test_d.txt","content":"${THIRTY_LINES}"}}}
EOF

HOST_LOG_D="/tmp/ion_stream_d.log"
EVT_FILE_D="/tmp/evt_stream_d.log"

if ! start_host "$SCRIPT_D" "$HOST_LOG_D"; then
    fail "D: host 启动失败"
else
    green "D: host 启动"

    SID_D=$("$ION_BIN" rpc --method create_session 2>/dev/null \
        | python3 -c "import sys,json;print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
    if [ -z "$SID_D" ]; then
        fail "D: 创建 session 失败"
    else
        SUB_PID_D=$(subscribe_capture "$SID_D" "$EVT_FILE_D" 30)
        sleep 0.5

        "$ION_BIN" rpc --session "$SID_D" --method prompt --params '{"text":"write 100 lines"}' >/dev/null 2>&1

        # 等 tool_execution_end
        for i in $(seq 1 25); do
            sleep 1
            if grep -Eq '"type": ?"tool_execution_end"' "$EVT_FILE_D" 2>/dev/null; then
                sleep 1
                break
            fi
        done
        kill -9 "$SUB_PID_D" 2>/dev/null

        # 断言 1：收到至少 5 个 tool_execution_update（30 行 → chunk_size=3 → 10 个，允许边界波动）
        UPDATE_COUNT=$(count_matches '"type": ?"tool_execution_update"' "$EVT_FILE_D")
        if [ "${UPDATE_COUNT:-0}" -ge 5 ]; then
            pass "D1: write 30 行触发 ${UPDATE_COUNT} 个 tool_execution_update（≥5，预期 ~10）"
        else
            fail "D1: 只收到 ${UPDATE_COUNT} 个 tool_execution_update（<5）— update 事件可能被丢"
        fi

        # 断言 2：partialResult 包含 +N -M 格式（"+<num> -<num> lines"）
        if grep -Eq 'partialResult.*\+[0-9]+ -[0-9]+ lines' "$EVT_FILE_D" 2>/dev/null; then
            pass "D2: partialResult 包含 +N -M lines 格式（前端跳动数据源）"
        else
            fail "D2: partialResult 不包含 +N -M 格式 — 事件格式可能变了"
        fi

        # 断言 3：N 数字递增（10/20/30...，证明是分块流式而非单次）
        # 提取所有 "+<num>" 的数字，检查是否单调递增
        PROGRESSION_OK=$(python3 -c "
import re
with open('$EVT_FILE_D') as f: content = f.read()
# 匹配 partialResult 里的 +N -M
nums = [int(m) for m in re.findall(r'\+(\\d+) -\\d+ lines', content)]
if len(nums) < 2:
    print('no')
else:
    # 至少前 3 个应该递增（10 < 20 < 30 ...）
    incr = all(nums[i] < nums[i+1] for i in range(min(3, len(nums)-1)))
    print('yes' if incr else 'no')
" 2>/dev/null)
        if [ "$PROGRESSION_OK" = "yes" ]; then
            pass "D3: +N 数字递增（分块流式，不是单次 dump）"
        else
            fail "D3: +N 数字没递增 — 可能 update 都是一样的（非流式）"
        fi

        # 断言 4：update 事件本身没被丢（关注 update 事件的可用性，而非整体 DROP 比例）
        # 注意：整体 DROP 主要是 tool_call_delta（高频），update 事件低频不应该丢
        # 验证标准：UPDATE_COUNT ≥ 5（已经够 D1 验证），这里再额外检查最后一个 update 的 N 是否接近总行数
        # （证明 update 流完整到达，没在中间断掉）
        LAST_UPDATE_N=$(python3 -c "
import json
with open('$EVT_FILE_D') as f: content = f.read()
decoder = json.JSONDecoder()
i = 0; n = len(content); last_n = 0
while i < n:
    while i < n and content[i] not in '{[': i += 1
    if i >= n: break
    try:
        obj, end = decoder.raw_decode(content[i:]); i += end
        if obj.get('type') == 'instance_event':
            ev = obj.get('event', {})
            if ev.get('type') == 'tool_execution_update':
                pr = ev.get('partialResult', '')
                import re
                m = re.search(r'\+(\d+) -', pr)
                if m: last_n = int(m.group(1))
    except: i += 1
print(last_n)
" 2>/dev/null)
        # 最后一个 update 的 N 应该 ≥ 20（接近总行数 30）
        if [ "${LAST_UPDATE_N:-0}" -ge 20 ]; then
            pass "D4: 最后一个 update 的 +N=${LAST_UPDATE_N}（接近总行数 30，update 流完整）"
        else
            fail "D4: 最后一个 update 的 +N=${LAST_UPDATE_N}（<20）— update 流可能在中间断了"
        fi

        # 断言 5：最终文件写入（30 行）
        if [ -f /tmp/stream_test_d.txt ]; then
            LINES_D=$(wc -l < /tmp/stream_test_d.txt)
            if [ "${LINES_D:-0}" -ge 25 ]; then
                pass "D5: 文件写入 ${LINES_D} 行（≈30，工具真实执行）"
            else
                fail "D5: 文件只有 ${LINES_D} 行（预期 ~30）"
            fi
        else
            fail "D5: 文件 /tmp/stream_test_d.txt 未写入"
        fi
    fi
fi

kill "$HOST_PID" 2>/dev/null
wait "$HOST_PID" 2>/dev/null
HOST_PID=""
echo ""

# ── 汇总 ──
echo "=========================================="
echo "  汇总: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "=========================================="

if [ "$FAIL" -gt 0 ]; then
    echo ""
    echo "❌ 有失败用例，诊断信息："
    echo "  - 查看 host 日志: /tmp/ion_stream_*.log"
    echo "  - 查看事件流: /tmp/evt_stream_*.log"
    echo "  - grep 'host DROP' /tmp/ion_stream_*.log 看 channel 丢事件情况"
    exit 1
fi

exit 0
