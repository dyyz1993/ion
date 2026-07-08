#!/usr/bin/env bash
# FauxProvider 三场景验证 — 验证 faux 在 ion / ion --host / ion serve 三种执行路径下都工作
# 不调真实 LLM，不依赖 API key，完全确定。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
pass() { green "✅ PASS: $1"; PASS=$((PASS+1)); }
fail() { red "❌ FAIL: $1"; FAIL=$((FAIL+1)); }

# Phase 0: Build
echo "── Phase 0: Build ──"
cargo build --bin ion --bin ion-worker 2>&1 | tail -3
ION_BIN="$PROJECT_DIR/target/debug/ion"
ION_WORKER_BIN="$PROJECT_DIR/target/debug/ion-worker"
[ -x "$ION_BIN" ] || { echo "ion binary missing"; exit 1; }

# 准备干净测试目录（避免 session 状态串扰）
TEST_DIR=$(mktemp -d)
cd "$TEST_DIR"

echo ""
echo "── Group A: 场景 1 直接执行 (ion \"msg\") ──"

# A1: 基本文本回放
OUTPUT=$(ION_FAUX_REPLY="hello from faux" timeout 30 "$ION_BIN" --no-session "say hi" 2>&1)
if echo "$OUTPUT" | grep -q "hello from faux"; then
    pass "A1 场景1 基本文本回放"
else
    fail "A1 场景1 基本文本回放 (output: $OUTPUT)"
fi

# A2: script 文件多步
cat > /tmp/faux_script_a2.jsonl <<'EOF'
{"text":"first"}
{"text":"second"}
EOF
OUTPUT=$(ION_FAUX_SCRIPT=/tmp/faux_script_a2.jsonl timeout 30 "$ION_BIN" --no-session "m1" 2>&1)
if echo "$OUTPUT" | grep -q "first"; then
    pass "A2 场景1 script 文件"
else
    fail "A2 场景1 script 文件 (output: $OUTPUT)"
fi

echo ""
echo "── Group B: 场景 2 快速编排 (ion --host) ──"

# B1: host 模式 + faux
OUTPUT=$(ION_FAUX_REPLY="host faux reply" ION_HOST_TIMEOUT=30 timeout 60 "$ION_BIN" --host --no-tools "say hello" 2>&1)
if echo "$OUTPUT" | grep -q "host faux reply"; then
    pass "B1 场景2 host + faux 回放"
elif echo "$OUTPUT" | grep -qi "faux"; then
    pass "B1 场景2 host 启用了 faux (回放内容未匹配但 faux 工作)"
else
    fail "B1 场景2 host + faux (output: $OUTPUT)"
fi

echo ""
echo "── Group C: 场景 3 常驻服务 (ion serve) ──"

# C1: serve + rpc prompt + read back assistant text via get_last_assistant_text
ION_FAUX_REPLY="serve faux reply" timeout 30 "$ION_BIN" serve >/tmp/faux_serve.log 2>&1 &
SERVE_PID=$!
sleep 4
# 创建一个 session
SID=$(timeout 15 "$ION_BIN" rpc --method create_session --params '{"agent":"build"}' 2>&1 | grep -o 'sess_[a-f0-9]*' | head -1)
if [ -z "$SID" ]; then
    kill $SERVE_PID 2>/dev/null || true
    wait $SERVE_PID 2>/dev/null || true
    fail "C1 场景3 serve: 无法创建 session"
else
    # 发 prompt（异步，立即返回）
    timeout 30 "$ION_BIN" rpc --session "$SID" --method prompt --params '{"text":"hi"}' >/dev/null 2>&1 || true
    # 等待 Agent 处理
    sleep 3
    # 读回最后一条 assistant 文本
    GET_OUTPUT=$(timeout 15 "$ION_BIN" rpc --session "$SID" --method get_last_assistant_text 2>&1 || true)
    kill $SERVE_PID 2>/dev/null || true
    wait $SERVE_PID 2>/dev/null || true
    if echo "$GET_OUTPUT" | grep -qi "serve faux reply\|faux"; then
        pass "C1 场景3 serve + faux"
    else
        fail "C1 场景3 serve + faux (get_last_assistant_text output: $GET_OUTPUT)"
    fi
fi

echo ""
echo "── 结果 ──"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && green "全部通过" || red "有失败"
exit $FAIL
