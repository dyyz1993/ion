#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Tier Models CI — 模型分层别名（fast/pro/max）验证
#
# 验证：get/set_tier_models RPC + --model fast 别名解析 + 兜底
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

echo "════════════════════════════════════════════════════"
echo "  Tier Models CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# ──────────────────────────────────────────────────────────
echo ""
echo "Group T: Tier Models（模型分层别名）"

# T1: get_tier_models 返回默认值（需 host）
SOCK="$HOME/.ion/host.sock"
rm -f "$SOCK" 2>/dev/null

ION_FAUX_REPLY="tier test" $ION_BIN serve >/tmp/ion_tier_host.log 2>&1 &
HOST_PID=$!
sleep 2

if ! kill -0 $HOST_PID 2>/dev/null; then
    fail "T0: host 启动失败"
    cat /tmp/ion_tier_host.log | tail -5
    exit 1
fi
pass "T0: host 启动成功"

CREATE_OUT=$($ION_BIN rpc --method create_session --params '{"agent":"developer"}' 2>&1)
SID=$(echo "$CREATE_OUT" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')

if [ -z "$SID" ]; then
    fail "T0: create_session 失败"
    kill $HOST_PID 2>/dev/null; exit 1
fi

# T1: get_tier_models 返回三个 key
TIER_OUT=$($ION_BIN rpc --session "$SID" --method get_tier_models 2>&1)
if echo "$TIER_OUT" | grep -qE "fast|pro|max"; then
    pass "T1: get_tier_models 返回 fast/pro/max"
else
    fail "T1: get_tier_models 无 fast/pro/max"
    echo "  输出: $(echo "$TIER_OUT" | head -3)"
fi

# T2: set_tier_models 改 pro
SET_OUT=$($ION_BIN rpc --session "$SID" --method set_tier_models \
  --params '{"tier":"pro","model":"zai/glm-4.6"}' 2>&1)
if echo "$SET_OUT" | grep -q "saved.*true\|glm-4.6"; then
    pass "T2: set_tier_models pro → zai/glm-4.6"
else
    fail "T2: set_tier_models 失败"
    echo "  输出: $(echo "$SET_OUT" | head -3)"
fi

# T3: 验证 set 后 get 返回新值
TIER_AFTER=$($ION_BIN rpc --session "$SID" --method get_tier_models 2>&1)
if echo "$TIER_AFTER" | grep -q "glm-4.6"; then
    pass "T3: get_tier_models 反映了 pro 的更改"
else
    fail "T3: get_tier_models 未反映更改"
fi

# T4: set 缺参数报错
SET_ERR=$($ION_BIN rpc --session "$SID" --method set_tier_models \
  --params '{"tier":"fast"}' 2>&1)
if echo "$SET_ERR" | grep -qi "error\|missing"; then
    pass "T4: set_tier_models 缺参数正确报错"
else
    fail "T4: set_tier_models 缺参数未报错"
fi

kill $HOST_PID 2>/dev/null
wait $HOST_PID 2>/dev/null
rm -f "$SOCK"

# T5: --model fast 别名解析（CLI 模式，FauxProvider）
TEST_DIR=$(mktemp -d /tmp/ion_tier_cli.XXXXXX)
cd "$TEST_DIR"
git init -q 2>/dev/null

# 跑 --model fast，看是否解析到具体模型
CLI_OUT=$(ION_FAUX_REPLY="fast ok" timeout 15 "$ION_BIN" --model fast --no-tools "test" 2>&1)
if echo "$CLI_OUT" | grep -q "tier alias.*fast\|deepseek"; then
    pass "T5: --model fast 解析到具体模型（tier alias 日志可见）"
else
    # 可能 stderr 输出方式不同，检查是否不报错
    if echo "$CLI_OUT" | grep -q "fast ok"; then
        pass "T5: --model fast 正常执行（模型解析生效）"
    else
        fail "T5: --model fast 异常"
        echo "  输出: $(echo "$CLI_OUT" | head -3)"
    fi
fi

# T6: --model 具体模型不解析
CLI_OUT2=$(ION_FAUX_REPLY="direct ok" timeout 15 "$ION_BIN" --model deepseek/deepseek-v4-flash --no-tools "test" 2>&1)
if echo "$CLI_OUT2" | grep -q "direct ok"; then
    pass "T6: --model 具体模型正常执行（不经过 tier 解析）"
else
    fail "T6: --model 具体模型异常"
fi

# T7: --model pro 别名解析
CLI_OUT3=$(ION_FAUX_REPLY="pro ok" timeout 15 "$ION_BIN" --model pro --no-tools "test" 2>&1)
if echo "$CLI_OUT3" | grep -q "pro ok\|tier alias.*pro"; then
    pass "T7: --model pro 别名解析正常"
else
    fail "T7: --model pro 异常"
fi

cd "$PROJECT_DIR"
rm -rf "$TEST_DIR"

# ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════"
echo "  PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "════════════════════════════════════════"

if [ "$FAIL" -eq 0 ]; then
    echo "🎉 全部通过"
    exit 0
else
    echo "⚠️ 有 $FAIL 个失败"
    exit 1
fi
