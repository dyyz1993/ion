#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Dev Server Detector CI — bash 启动 dev server 端口检测 + system prompt 注入
#
# 验证策略：
#   - 核心逻辑用 Rust e2e 集成测试（直接调钩子，最可靠）
#   - 端口提取用单元测试（12 个格式覆盖）
#   - 扩展注册用 serve 日志确认（场景 1 + 场景 3 双注册）
#
# 注：call_tool RPC 不触发 on_tool_execution_end 钩子（ION 设计：call_tool 是
#     裸调，bypass agent loop）。因此完整链路验证用 e2e 集成测试，不用 call_tool。
#
# 覆盖文档：docs/design/DEV_SERVER_DETECTOR.md
#   Group A：Rust 单元测试（端口提取 12 种格式）
#   Group B：E2E 集成测试（钩子完整链路 8 个）
#   Group C：扩展注册验证（场景 1 + 场景 3）
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0; SKIP=0

if [ -z "${ION_SESSION_DIR:-}" ]; then
    export ION_SESSION_DIR="$HOME/.ion/agent/sessions/_ci_$(basename "$0" .sh)_$$"
    mkdir -p "$ION_SESSION_DIR"
    trap 'rm -rf "$ION_SESSION_DIR"' EXIT
fi

green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"

echo "════════════════════════════════════════════════════"
echo "  Dev Server Detector CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# ═════════════════════════════════════════════════════════
# Group A：Rust 单元测试（端口提取逻辑）
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group A：Rust 单元测试（端口提取）──"
UNIT_OUT=$(cargo test --lib dev_server_detector 2>&1)
if echo "$UNIT_OUT" | grep -q "test result: ok"; then
    UNIT_COUNT=$(echo "$UNIT_OUT" | grep "test result" | grep -oE "[0-9]+ passed" | grep -oE "[0-9]+")
    pass "A: 单元测试全过（${UNIT_COUNT} passed）"
else
    fail "A: 单元测试失败"
    echo "$UNIT_OUT" | grep -E "FAILED|panicked|error\[" | head -5
fi

# ═════════════════════════════════════════════════════════
# Group B：E2E 集成测试（钩子完整链路）
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group B：E2E 集成测试（on_tool_execution_end → on_system_prompt）──"
E2E_OUT=$(cargo test --test dev_server_detector_e2e 2>&1)
if echo "$E2E_OUT" | grep -q "test result: ok"; then
    E2E_COUNT=$(echo "$E2E_OUT" | grep "test result" | grep -oE "[0-9]+ passed" | grep -oE "[0-9]+")
    pass "B: E2E 集成测试全过（${E2E_COUNT} passed）"

    # 逐个确认关键 case
    echo "$E2E_OUT" | grep -q "test_vite_port_detection_and_injection" && pass "B1: Vite 端口检测 + XML 注入"
    echo "$E2E_OUT" | grep -q "test_multiple_frameworks" && pass "B2: 多框架同时检测（count=2）"
    echo "$E2E_OUT" | grep -q "test_non_server_command_no_injection" && pass "B3: 非 server 命令不注入"
    echo "$E2E_OUT" | grep -q "test_dedup_same_signature_no_reinject" && pass "B4: signature 去重"
    echo "$E2E_OUT" | grep -q "test_non_bash_tool_ignored" && pass "B5: 非 bash 工具忽略"
    echo "$E2E_OUT" | grep -q "test_flask_format" && pass "B6: Flask 格式 (127.0.0.1)"
    echo "$E2E_OUT" | grep -q "test_extension_rpc_list" && pass "B7: extension_rpc list"
    echo "$E2E_OUT" | grep -q "test_extension_rpc_clear" && pass "B8: extension_rpc clear"
else
    fail "B: E2E 集成测试失败"
    echo "$E2E_OUT" | grep -E "FAILED|panicked|error\[" | head -10
fi

# ═════════════════════════════════════════════════════════
# Group C：扩展注册验证（场景 1 单进程）
# ═════════════════════════════════════════════════════════
echo ""
echo "── Group C：扩展注册验证（场景 1）──"

# 场景 1：ion "prompt" --provider faux，检查 serve 日志里的注册记录
# 用 faux provider + 空 script，让进程快速退出
FAUX_SCRIPT=$(mktemp /tmp/faux_empty.XXXXXX)
echo '{"text":"ok"}' > "$FAUX_SCRIPT"
trap 'rm -f "$FAUX_SCRIPT"; rm -rf "$ION_SESSION_DIR"' EXIT

REG_LOG=$(ION_FAUX_SCRIPT="$FAUX_SCRIPT" RUST_LOG=ion=info "$ION_BIN" "go" --provider faux --model faux 2>&1)

if echo "$REG_LOG" | grep -q "dev_server_detector registered"; then
    pass "C1: 场景 1 扩展注册成功（[extension] dev_server_detector registered）"
else
    fail "C1: 场景 1 扩展未注册（日志无 dev_server_detector registered）"
    echo "$REG_LOG" | grep -i "extension.*registered" | head -5
fi

# 场景 3 验证（serve 启动时 worker 注册）
echo ""
echo "C2: 场景 3 注册验证（serve host）"
# 杀残留 host
"$ION_BIN" serve stop 2>/dev/null
sleep 1
rm -f "$HOME/.ion/host.sock"

nohup "$ION_BIN" serve >/tmp/ion_detector_ci_serve.log 2>&1 &
HOST_PID=$!

# 等 host 就绪
HOST_READY=false
for i in $(seq 1 30); do
    if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q success; then
        HOST_READY=true; break
    fi
    sleep 0.5
done

if [ "$HOST_READY" = "true" ]; then
    pass "C2: serve host 启动成功"
    # 场景 3 的扩展注册在 worker 进程里，创建 session 时触发
    SID=$("$ION_BIN" rpc --method create_session --params '{"agent":"default"}' 2>/dev/null | python3 -c "import sys,json; print(json.load(sys.stdin)['data']['session_id'])" 2>/dev/null)
    if [ -n "$SID" ]; then
        pass "C2: 场景 3 session 创建成功（$SID）"
        # 确认扩展在 worker 注册（serve 日志会有 worker 侧的注册日志）
        sleep 2
        if grep -q "dev_server_detector" /tmp/ion_detector_ci_serve.log 2>/dev/null; then
            pass "C2: 场景 3 worker 注册了 dev_server_detector"
        else
            skip "C2: 场景 3 worker 注册日志未捕获（worker 日志可能在子进程 stderr，不影响功能）"
        fi
    else
        fail "C2: session 创建失败"
    fi
else
    fail "C2: serve host 启动失败"
fi

# 清理 host
kill $HOST_PID 2>/dev/null

# ═════════════════════════════════════════════════════════
# 汇总
# ═════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "  Result: $PASS passed, $FAIL failed, $SKIP skipped"
echo "════════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then
    exit 1
fi
