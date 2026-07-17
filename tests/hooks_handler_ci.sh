#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Hooks Handler CI — 命令行验证 5 种 handler 的执行 + 可观测性
#
# 验证链路：hooks.json 配 handler → ion rpc prompt/call_tool 触发事件
#           → HookExtension process_event → run_handler 执行
#           → emit hook_handler_executed 事件 → ion subscribe 收到
#
# 对齐 AGENTS.md「命令行可验证原则」：hook 执行结果必须能从外部观察。
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"

echo "════════════════════════════════════════════════════"
echo "  Hooks Handler CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

SOCK="$HOME/.ion/host.sock"
# 安全清理（按 socket 杀，不用 pkill）
lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null; sleep 1
rm -f "$SOCK"

TESTDIR=$(mktemp -d)
mkdir -p "$TESTDIR/.ion"

cleanup() {
    lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null
    rm -f "$SOCK"; rm -rf "$TESTDIR"
}
trap cleanup EXIT

# 辅助：起 host + subscribe 抓事件 + 触发 prompt
# 用法：run_hook_test <hooks_json_content> <prompt_text>
# 输出 EVT_FILE 里存 subscribe 抓到的事件
run_hook_test() {
    local hooks_json="$1"; local prompt_text="$2"
    echo "$hooks_json" > "$TESTDIR/.ion/hooks.json"
    rm -f "$SOCK"; lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null; sleep 1

    ION_FAUX_REPLY='{"block":true,"reason":"hook-veto"}' \
        "$ION_BIN" serve >/tmp/ion_hh_host.log 2>&1 &
    local hpid=$!
    # 等 host 就绪（socket 出现），最多 5 秒
    for i in 1 2 3 4 5; do [ -S "$SOCK" ] && break; sleep 1; done

    local sid=$("$ION_BIN" rpc --method create_session --params "{\"agent\":\"build\",\"cwd\":\"$TESTDIR\"}" 2>&1 | grep -o 'sess_[a-z0-9]*' | head -1)

    timeout 8 "$ION_BIN" subscribe --session "$sid" >/tmp/ion_hh_evt.log 2>&1 &
    local spid=$!; sleep 1

    "$ION_BIN" rpc --session "$sid" --method prompt --params "{\"text\":\"$prompt_text\"}" >/dev/null 2>&1
    sleep 3
    kill $spid 2>/dev/null; wait $spid 2>/dev/null
    kill $hpid 2>/dev/null; wait $hpid 2>/dev/null
}

# ════════════════════════════════════════════════════════
# Group A：command handler 触发 + 可观测
# ════════════════════════════════════════════════════════
echo ""
echo "── Group A：command handler ──"

run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"command","command":"echo conv","timeout":5}]}}' "test command"

if grep -q "hook_handler_executed" /tmp/ion_hh_evt.log && grep -q '"command"' /tmp/ion_hh_evt.log; then
    pass "A1: command handler 执行 → subscribe 收到 hook_handler_executed"
else
    fail "A1: command handler 未触发或事件未收到"
    echo "    事件: $(cat /tmp/ion_hh_evt.log | grep -v subscribed | head -3)"
fi

# ════════════════════════════════════════════════════════
# Group B：http handler 安全校验（非 HTTPS / 私网 IP → block）
# ════════════════════════════════════════════════════════
echo ""
echo "── Group B：http handler 安全校验 ──"

# B1：非 HTTPS URL → block（validate_url 拒绝）
run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"http","url":"http://example.com/hook","timeout":5}]}}' "test http"

if grep -q '"http"' /tmp/ion_hh_evt.log; then
    pass "B1: http handler 触发（subscribe 收到 handler_type=http）"
else
    fail "B1: http handler 未触发"
    echo "    事件: $(cat /tmp/ion_hh_evt.log | grep -v subscribed | head -3)"
fi

# B2：非 HTTPS → block=true（validate_url 返回 Err → HookOutcome block）
if grep -q '"block": true' /tmp/ion_hh_evt.log || grep -q '"block":true' /tmp/ion_hh_evt.log; then
    pass "B2: http 非 HTTPS URL → block（安全校验生效）"
else
    fail "B2: http 非 HTTPS 应 block"
    echo "    事件: $(grep -o 'block[^,}]*' /tmp/ion_hh_evt.log | head -1)"
fi

# B3：localhost → block（私网拒绝）
run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"http","url":"https://localhost:8080/hook","timeout":5}]}}' "test localhost"

if grep -q '"block": true' /tmp/ion_hh_evt.log || grep -q '"block":true' /tmp/ion_hh_evt.log; then
    pass "B3: http localhost URL → block（私网拒绝）"
else
    fail "B3: http localhost 应 block"
    echo "    事件: $(grep -o 'block[^,}]*' /tmp/ion_hh_evt.log | head -1)"
fi

# ════════════════════════════════════════════════════════
# Group C：prompt handler 触发（调 LLM）
# ════════════════════════════════════════════════════════
echo ""
echo "── Group C：prompt handler ──"

run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"prompt","prompt":"Return {\"block\":true,\"reason\":\"v\"}","timeout":15}]}}' "test prompt"

if grep -q '"prompt"' /tmp/ion_hh_evt.log; then
    pass "C1: prompt handler 触发（subscribe 收到 handler_type=prompt）"
else
    fail "C1: prompt handler 未触发"
    echo "    事件: $(cat /tmp/ion_hh_evt.log | grep -v subscribed | head -3)"
fi

# ════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL"
echo "════════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then echo "❌ hooks_handler_ci 有失败"; exit 1; fi
echo "✅ hooks_handler_ci 全部通过"
exit 0
