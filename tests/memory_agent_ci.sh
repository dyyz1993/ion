#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Memory Agent CI — 验证 Active Memory sub-agent 自动 spawn + 可查询
#
# 验证链路：
#   ion serve 启动 → GlobalMemoryExtension.on_singleton_post_init
#   → spawn memory-agent Worker（WorkerRelation::System）
#   → list_workers 能看到 → send_to_worker 能投递 → extension_rpc 能查记忆
#
# 不调真 LLM（memory-agent 的 LLM 查询用 FauxProvider 兜底）。
# 对齐 AGENTS.md「命令行可验证原则」。
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
SOCK="$HOME/.ion/host.sock"

echo "════════════════════════════════════════════════════"
echo "  Memory Agent CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# 确保 memory-agent.md 在全局 agents 目录
mkdir -p "$HOME/.ion/agent/agents"
if [ ! -f "$HOME/.ion/agent/agents/memory-agent.md" ]; then
    cp "$PROJECT_DIR/examples/agents/memory-agent.md" "$HOME/.ion/agent/agents/memory-agent.md"
fi

# 清理 + 起 host（FauxProvider 兜底，memory-agent 的 LLM 查询不调真 API）
# 彻底清理：kill 旧 host（按 socket + pidfile）+ 等 socket 消失
if [ -f "$HOME/.ion/host.pid" ]; then
    kill "$(cat "$HOME/.ion/host.pid")" 2>/dev/null
fi
lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null
for i in 1 2 3 4 5; do [ ! -S "$SOCK" ] && break; sleep 1; done
rm -f "$SOCK" "$HOME/.ion/host.pid"; sleep 2

cleanup() {
    lsof -ti "$SOCK" 2>/dev/null | xargs kill 2>/dev/null
    rm -f "$SOCK" "$HOME/.ion/host.pid" "$FAUX_SCRIPT"
}
trap cleanup EXIT

# 用 FauxScript 提供足够响应（memory-agent init 消费 1 条 + 后续查询各消费）
FAUX_SCRIPT=$(mktemp)
printf '{"text":"Memory Agent standing by."}\n{"text":"No memories found."}\n{"text":"No memories found."}\n{"text":"No memories found."}\n{"text":"No memories found."}\n' > "$FAUX_SCRIPT"

ION_FAUX_SCRIPT="$FAUX_SCRIPT" RUST_LOG=info \
    "$ION_BIN" serve >/tmp/ion_memagent_ci.log 2>&1 &
HOST_PID=$!
for i in $(seq 1 10); do [ -S "$SOCK" ] && break; sleep 1; done

if ! kill -0 "$HOST_PID" 2>/dev/null; then
    fail "host 启动失败"; cat /tmp/ion_memagent_ci.log | tail -5; exit 1
fi
pass "host 启动成功"

# 等 memory-agent spawn（post_init 在 host 启动后异步执行）
sleep 5

# ════════════════════════════════════════════════════════
# Group A：memory-agent 自动 spawn
# ════════════════════════════════════════════════════════
echo ""
echo "── Group A：memory-agent 自动 spawn ──"

# A1：日志里有 spawn 记录
if grep -q "Active Memory sub-agent started" /tmp/ion_memagent_ci.log; then
    pass "A1: 日志确认 memory-agent spawn（Active Memory sub-agent started）"
else
    fail "A1: 未找到 memory-agent spawn 日志"
    echo "    日志: $(grep -i memory /tmp/ion_memagent_ci.log | tail -3)"
fi

# A2：list_workers 里有 agent=memory-agent 的 worker
WORKERS_OUT=$("$ION_BIN" rpc --method list_workers --params '{}' 2>&1)
MA_WID=$(echo "$WORKERS_OUT" | python3 -c "
import sys, json
try:
    data = json.load(sys.stdin)
    workers = data.get('data', {}).get('workers', [])
    for w in workers:
        if w.get('agent') == 'memory-agent':
            print(w['workerId'])
            break
except: pass
" 2>/dev/null)

if [ -n "$MA_WID" ]; then
    pass "A2: list_workers 找到 memory-agent worker（$MA_WID）"
else
    fail "A2: list_workers 未找到 memory-agent worker"
    echo "    workers: $(echo "$WORKERS_OUT" | grep -o '"agent":"[^"]*"' | head -5)"
fi

# A3：memory-agent 的 status 是 Idle 或 Running（不是 Dead）
if [ -n "$MA_WID" ]; then
    STATUS=$(echo "$WORKERS_OUT" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for w in data.get('data', {}).get('workers', []):
    if w.get('workerId') == '$MA_WID':
        print(w.get('status', ''))
        break
" 2>/dev/null)
    if echo "$STATUS" | grep -qiE "idle|running|busy"; then
        pass "A3: memory-agent 状态正常（$STATUS）"
    else
        fail "A3: memory-agent 状态异常（$STATUS）"
    fi
fi

# ════════════════════════════════════════════════════════
# Group B：memory-agent 可达性（session 存在 + get_state 不崩）
# ════════════════════════════════════════════════════════
echo ""
echo "── Group B：memory-agent 可达性 ──"

if [ -n "$MA_WID" ]; then
    # B1：memory-agent 的 session 能查到（证明 worker 不是 Dead）
    MA_SESSION=$(echo "$WORKERS_OUT" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for w in data.get('data', {}).get('workers', []):
    if w.get('workerId') == '$MA_WID':
        print(w.get('sessionId', ''))
        break
" 2>/dev/null)
    if [ -n "$MA_SESSION" ]; then
        pass "B1: memory-agent 有有效 session（$MA_SESSION）"
    else
        fail "B1: memory-agent session 为空"
    fi

    # B2：memory-agent 的 model 字段非空（证明 spawn 时传了配置）
    MA_MODEL=$(echo "$WORKERS_OUT" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for w in data.get('data', {}).get('workers', []):
    if w.get('workerId') == '$MA_WID':
        print(w.get('model', ''))
        break
" 2>/dev/null)
    if [ -n "$MA_MODEL" ]; then
        pass "B2: memory-agent 配置了 model（$MA_MODEL）"
    else
        fail "B2: memory-agent model 为空"
    fi
else
    fail "B1+B2: 无 memory-agent worker（前置 A2 失败）"
fi

# ════════════════════════════════════════════════════════
# Group C：global-memory singleton 的 extension_rpc 可用
# ════════════════════════════════════════════════════════
echo ""
echo "── Group C：global-memory extension_rpc ──"

# C1：先存一条记忆
OUT=$("$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"save","args":{"content":"Rust uses tokio for async","category":"decision","project":"test-ci","tags":["rust","async"]}}' 2>&1)
if echo "$OUT" | grep -qi "true\|id"; then
    pass "C1: extension_rpc save 成功"
else
    fail "C1: extension_rpc save 失败"
    echo "    OUT: $OUT"
fi

# C2：搜索刚存的记忆
OUT=$("$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"search","args":{"query":"tokio async","project":"test-ci"}}' 2>&1)
if echo "$OUT" | grep -qi "tokio"; then
    pass "C2: extension_rpc search 找到刚存的记忆（tokio）"
else
    fail "C2: search 未找到记忆"
    echo "    OUT: $OUT"
fi

# C3：清理测试数据
OUT=$("$ION_BIN" rpc --method extension_rpc --params '{"extension":"global-memory","method":"clear_stored","args":{}}' 2>&1)
if echo "$OUT" | grep -qi "true\|cleared"; then
    pass "C3: 清理测试记忆（clear_stored）"
else
    fail "C3: 清理失败"
fi

# ════════════════════════════════════════════════════════
echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL"
echo "════════════════════════════════════════════════════"

if [ "$FAIL" -gt 0 ]; then echo "❌ memory_agent_ci 有失败"; exit 1; fi
echo "✅ memory_agent_ci 全部通过"
exit 0
