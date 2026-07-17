#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Hooks Handler CI — 命令行验证 5 种 handler 的执行 + 可观测性
#
# 用 ion serve + subscribe 观察 hook_handler_executed 事件。
# 每次 run_hook_test 独立起 host，用精确 PID + socket 清理保证稳定。
#
# 对齐 AGENTS.md「命令行可验证原则」。
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0; SKIP=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
SOCK="$HOME/.ion/host.sock"

echo "════════════════════════════════════════════════════"
echo "  Hooks Handler CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

TESTDIR=$(mktemp -d)
mkdir -p "$TESTDIR/.ion"
cleanup() {
    # 精确 PID 清理（不 pkill）
    for pidfile in /tmp/ion_hh_pid_*.txt; do
        [ -f "$pidfile" ] && kill "$(cat "$pidfile")" 2>/dev/null
    done
    lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null
    rm -f "$SOCK"; rm -rf "$TESTDIR" /tmp/ion_hh_pid_*.txt
}
trap cleanup EXIT

# 彻底清理：kill 旧 host + 等 socket 消失
kill_host_and_wait() {
    lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null
    for i in $(seq 1 8); do [ ! -S "$SOCK" ] && break; sleep 1; done
    rm -f "$SOCK"
}

# 辅助：起 host + subscribe 抓事件 + 触发 prompt
# 用法：run_hook_test <hooks_json> <prompt_text>
run_hook_test() {
    local hooks_json="$1"; local prompt_text="$2"
    echo "$hooks_json" > "$TESTDIR/.ion/hooks.json"

    kill_host_and_wait
    sleep 2  # 等 worker 子进程完全退出

    ION_FAUX_REPLY='ok' "$ION_BIN" serve >/tmp/ion_hh_host.log 2>&1 &
    local hpid=$!
    echo "$hpid" > /tmp/ion_hh_pid_current.txt
    for i in $(seq 1 10); do [ -S "$SOCK" ] && break; sleep 1; done

    local sid=$("$ION_BIN" rpc --method create_session --params "{\"agent\":\"build\",\"cwd\":\"$TESTDIR\"}" 2>&1 | grep -o 'sess_[a-z0-9]*' | head -1)

    timeout 10 "$ION_BIN" subscribe --session "$sid" >/tmp/ion_hh_evt.log 2>&1 &
    local spid=$!; sleep 1

    "$ION_BIN" rpc --session "$sid" --method prompt --params "{\"text\":\"$prompt_text\"}" >/dev/null 2>&1
    sleep 4
    kill $spid 2>/dev/null; wait $spid 2>/dev/null
    kill $hpid 2>/dev/null; wait $hpid 2>/dev/null
}

# ════════════════════════════════════════════════════════
echo ""
echo "── Group A：command handler ──"

run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"command","command":"echo conv","timeout":5}]}}' "test command"

if grep -q "hook_handler_executed" /tmp/ion_hh_evt.log && grep -q '"command"' /tmp/ion_hh_evt.log; then
    pass "A1: command handler 执行（subscribe 收到 hook_handler_executed + handler_type=command）"
else
    fail "A1: command handler 未触发"
    echo "    事件: $(cat /tmp/ion_hh_evt.log | grep -v subscribed | head -3)"
fi

# ════════════════════════════════════════════════════════
echo ""
echo "── Group B：http handler 安全校验 ──"

run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"http","url":"http://example.com/hook","timeout":5}]}}' "test http"

if grep -q '"http"' /tmp/ion_hh_evt.log; then
    pass "B1: http handler 触发（handler_type=http）"
else
    fail "B1: http handler 未触发"
fi
if grep -q '"block": true' /tmp/ion_hh_evt.log || grep -q '"block":true' /tmp/ion_hh_evt.log; then
    pass "B2: http 非 HTTPS URL → block"
else
    fail "B2: http 非 HTTPS 应 block"
fi

run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"http","url":"https://localhost:8080/hook","timeout":5}]}}' "test localhost"

if grep -q '"block": true' /tmp/ion_hh_evt.log || grep -q '"block":true' /tmp/ion_hh_evt.log; then
    pass "B3: http localhost → block（私网拒绝）"
else
    fail "B3: http localhost 应 block"
fi

# ════════════════════════════════════════════════════════
echo ""
echo "── Group C：prompt handler ──"

run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"prompt","prompt":"return ok","timeout":15}]}}' "test prompt"

if grep -q '"prompt"' /tmp/ion_hh_evt.log; then
    pass "C1: prompt handler 触发（handler_type=prompt）"
else
    fail "C1: prompt handler 未触发"
    echo "    事件: $(cat /tmp/ion_hh_evt.log | grep -v subscribed | head -3)"
fi

# ════════════════════════════════════════════════════════
echo ""
echo "── Group D：mcp_tool handler ──"

if ! command -v mcp-server-everything &>/dev/null; then
    skip "mcp-server-everything 不可用，跳过 Group D"
else
    run_hook_test '{"version":1,"hooks":{"UserPromptSubmit":[{"type":"mcp_tool","server":"everything","tool":"echo","input":{"message":"hook-mcp-test"},"timeout":15}]}}' "test mcp"

    if grep -q '"mcp_tool"' /tmp/ion_hh_evt.log; then
        pass "D1: mcp_tool handler 触发（handler_type=mcp_tool）"
    else
        fail "D1: mcp_tool handler 未触发"
        echo "    事件: $(cat /tmp/ion_hh_evt.log | grep -v subscribed | head -3)"
    fi
    if grep -q '"block": false' /tmp/ion_hh_evt.log || grep -q '"block":false' /tmp/ion_hh_evt.log; then
        pass "D2: mcp_tool handler 调 echo 成功（block=false）"
    else
        fail "D2: mcp_tool handler 执行结果异常"
    fi
fi

# ════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "════════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then echo "❌ hooks_handler_ci 有失败"; exit 1; fi
echo "✅ hooks_handler_ci 全部通过"
exit 0
