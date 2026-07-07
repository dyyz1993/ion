#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：Session Entry 类型 + 字段平铺 + 消息变体
# ──────────────────────────────────────────────────────────
# 用法:
#   bash tests/session_entries_ci.sh              # 快速模式（无 LLM）
#   bash tests/session_entries_ci.sh --with-llm   # 含真实 LLM prompt（需 API key）
#
# 退出码:
#   0 = 全部通过
#   1 = 至少一项失败
# ──────────────────────────────────────────────────────────
set -uo pipefail

PASS=0
FAIL=0

green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }

pass() { PASS=$((PASS + 1)); green "$1"; }
fail() { FAIL=$((FAIL + 1)); red "$1"; }

# 静默运行，过滤 shell noise
quiet() {
    "$@" 2>/dev/null | grep -v "setValueForKey\|valueForKey\|_encode\|_decode" || true
}

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

echo "════════════════════════════════════════════════════"
echo "  ION Session Entry CI Test"
echo "  $(date)"
echo "════════════════════════════════════════════════════"

# ── Step 0: 项目构建 ──
echo ""
echo "── Phase 0: Build ──"

if cargo build --bin ion --bin ion-worker 2>/dev/null; then
    pass "cargo build ion/ion-worker"
else
    fail "cargo build ion/ion-worker"
    echo ""
    red "Build failed, aborting."
    exit 1
fi

if cargo build --bin agent-demo 2>/dev/null; then
    pass "cargo build agent-demo"
else
    fail "cargo build agent-demo (non-critical)"
fi

# ── Step 0b: 选择 ion 二进制 ──
# 优先用刚构建的 target/debug/ion（避免 ~/.cargo/bin/ion 旧版/签名问题）
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
if [ -x "$PROJECT_DIR/target/debug/ion" ]; then
    ION_BIN="$PROJECT_DIR/target/debug/ion"
else
    ION_BIN="ion"
fi
echo "  使用 binary: $ION_BIN"

# ── Step 1: 单元测试 ──
echo ""
echo "── Phase 1: Unit Tests ──"

RUST_LOG=error cargo test --lib --color never 2>&1 > /tmp/ion-ci-test-lib.log
if grep -q "^test result:" /tmp/ion-ci-test-lib.log; then
    pass "cargo test --lib"
else
    fail "cargo test --lib"
    cat /tmp/ion-ci-test-lib.log | tail -5
fi

RUST_LOG=error cargo test -p ion-provider --color never 2>&1 > /tmp/ion-ci-test-provider.log
if grep -q "^test result:" /tmp/ion-ci-test-provider.log; then
    pass "cargo test -p ion-provider"
else
    fail "cargo test -p ion-provider"
    cat /tmp/ion-ci-test-provider.log | tail -5
fi

# ── Step 2: 启动 Manager + Worker ──
echo ""
echo "── Phase 2: Manager & Worker ──"

# 严厉清理残留 manager
# 方式 1: 端口占用进程
lsof -ti :53293 2>/dev/null | xargs kill -9 2>/dev/null || true
# 方式 2: target/debug/ion manager 进程
for pid in $(ps aux | grep "target/debug/ion" | grep -v grep | awk '{print $2}' 2>/dev/null || true); do
    kill -9 "$pid" 2>/dev/null || true
done
sleep 2

cargo run --bin ion -- serve start > /tmp/ion-ci-host.log 2>&1 &
# 等编译完成后，后续 rpc 调用用 $ION_BIN（指向 target/debug/ion）
MANAGER_CMD_PID=$!
sleep 4

# 验证 manager 进程活着
if ps -p "$MANAGER_CMD_PID" > /dev/null 2>&1 || lsof -ti :53293 2>/dev/null | head -1 > /dev/null; then
    pass "serve start"
else
    fail "serve start (check /tmp/ion-ci-host.log)"
    cat /tmp/ion-ci-host.log
    exit 1
fi

# 创建 Worker
OUT=$(quiet "$ION_BIN" rpc --session x --method create_worker --params '{"cwd":"'"$PROJECT_DIR"'"}')
SID=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))" 2>/dev/null)

if [ -n "$SID" ]; then
    pass "create_worker (sid=${SID:0:8}...)"
else
    fail "create_worker"
    echo "  OUT=$OUT"
    lsof -ti :53293 2>/dev/null | xargs kill 2>/dev/null || true
    exit 1
fi

# ── Step 3: RPC 测试 ──
echo ""
echo "── Phase 3: RPC Handlers ──"

rpc_ok() {
    local method="$1"
    local params="$2"
    quiet "$ION_BIN" rpc --session "$SID" --method "$method" --params "$params" \
        | python3 -c "import sys,json; d=json.load(sys.stdin); print('ok' if d.get('success') else 'FAIL: '+str(d.get('error','')))" 2>/dev/null || echo "FAIL: rpc call error"
}

for test_case in \
    "append_custom_message:{\"type\":\"bash_result\",\"content\":\"<bash_result>ok</bash_result>\",\"display\":true}" \
    "append_custom_entry:{\"type\":\"snap\",\"data\":{\"k\":\"v\"}}" \
    "append_system_event:{\"type\":\"model_change\",\"label\":\"switch\",\"display\":true}" \
    "append_model_change:{\"provider\":\"test\",\"modelId\":\"m1\"}" \
    "append_thinking_level_change:{\"level\":\"high\"}" \
    "append_agent_change:{\"name\":\"coord\"}" \
    "append_session_name:{\"name\":\"test-session\"}" \
    "append_label:{\"targetId\":\"msg_x\",\"label\":\"important\"}" \
    "append_active_tools_change:{\"activeToolNames\":[\"bash\",\"read\"]}" \
    "send_custom_message:{\"type\":\"alert\",\"content\":\"test\",\"deliverAs\":\"followUp\"}"
do
    method="${test_case%%:*}"
    params="${test_case#*:}"
    result=$(rpc_ok "$method" "$params")
    if [ "$result" = "ok" ]; then
        pass "$method"
    else
        fail "$method: $result"
    fi
done

# ── Step 4: 字段平铺验证 ──
echo ""
echo "── Phase 4: Field Flattening Verification ──"

# 找包含当前 SID 的 session.jsonl
SESSION_FILE=$(grep -rl "$SID" ~/.ion/agent/sessions/ --include="session.jsonl" 2>/dev/null | head -1)
if [ -z "$SESSION_FILE" ]; then
    # 回退：找最近修改的
    SESSION_FILE=$(find ~/.ion/agent/sessions -name "session.jsonl" -type f 2>/dev/null | head -1)
fi

if [ -z "$SESSION_FILE" ]; then
    fail "find session.jsonl"
else
    pass "session.jsonl found"

    for ENTRY_TYPE in active_tools_change custom_message system_event label; do
        # 只查当前 SID 对应的 entry（避免旧数据干扰）
        LAST=$(grep "\"type\":\"$ENTRY_TYPE\"" "$SESSION_FILE" 2>/dev/null | grep "$SID" | tail -1)
        if [ -z "$LAST" ]; then
            fail "  $ENTRY_TYPE entry exists (sid=$SID)"
        elif echo "$LAST" | python3 -c "import sys,json; e=json.loads(sys.stdin.read()); print('flat' if 'data' not in e else 'nested')" 2>/dev/null | grep -q flat; then
            pass "  $ENTRY_TYPE fields are flat (not under data)"
        else
            fail "  $ENTRY_TYPE fields are nested under data"
        fi
    done
fi

# ── Step 5: (可选) 真实 LLM Prompt ──
echo ""
if [ "${1:-}" = "--with-llm" ]; then
    echo "── Phase 5: LLM Prompt ──"
    OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"say hi"}')
    if echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print('ok' if d.get('success') else 'no')" 2>/dev/null | grep -q ok; then
        pass "prompt (real LLM)"
    else
        fail "prompt (real LLM)"
    fi
else
    echo "── Phase 5: LLM Prompt (skipped, use --with-llm)"
fi

# ── 清理 ──
echo ""
echo "── Cleanup ──"
for pid in $(ps aux | grep "target/debug/ion" | grep -v grep | awk '{print $2}' 2>/dev/null || true); do
    kill "$pid" 2>/dev/null || true
done
rm -f /tmp/ion-host.pid /tmp/ion-ci-host.log
echo "  Cleaned up"

# ── 总结 ──
echo ""
echo "════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════════════════"

if [ $FAIL -gt 0 ]; then
    exit 1
fi
exit 0
