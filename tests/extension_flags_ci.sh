#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Extension Flags CI — 运行时 flag 读写验证
#
# 验证：get_flags / set_flag RPC + 缺参数报错 + 类型支持
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
echo "  Extension Flags CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

# 启动 host
SOCK="$HOME/.ion/host.sock"
rm -f "$SOCK" 2>/dev/null

ION_FAUX_REPLY="flag test" $ION_BIN serve >/tmp/ion_flags_host.log 2>&1 &
HOST_PID=$!
sleep 2

if ! kill -0 $HOST_PID 2>/dev/null; then
    fail "F0: host 启动失败"
    cat /tmp/ion_flags_host.log | tail -5
    exit 1
fi
pass "F0: host 启动成功"

CREATE_OUT=$($ION_BIN rpc --method create_session --params '{"agent":"developer"}' 2>&1)
SID=$(echo "$CREATE_OUT" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')

if [ -z "$SID" ]; then
    fail "F0: create_session 失败"
    kill $HOST_PID 2>/dev/null; exit 1
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group F: Flags 系统（运行时 flag 读写）"

# F1: 查询所有扩展的 flag
GETALL_OUT=$($ION_BIN rpc --session "$SID" --method get_flags 2>&1)
if echo "$GETALL_OUT" | grep -qE "flags|success"; then
    pass "F1: get_flags（全部）正常返回"
else
    fail "F1: get_flags 异常"
fi

# F2: 设置 flag
SET_OUT=$($ION_BIN rpc --session "$SID" --method set_flag \
  --params '{"extension":"memory","flag":"debug","value":true}' 2>&1)
if echo "$SET_OUT" | grep -q '"set": true\|"set":true'; then
    pass "F2: set_flag memory.debug=true"
else
    fail "F2: set_flag 失败"
    echo "  输出: $(echo "$SET_OUT" | head -3)"
fi

# F3: 设置后查询
GET_AFTER=$($ION_BIN rpc --session "$SID" --method get_flags \
  --params '{"extension":"memory"}' 2>&1)
if echo "$GET_AFTER" | grep -q "debug"; then
    pass "F3: get_flags 反映了设置的值（debug 可见）"
else
    fail "F3: get_flags 未反映设置"
fi

# F4: 缺参数报错
SET_ERR=$($ION_BIN rpc --session "$SID" --method set_flag \
  --params '{"flag":"debug","value":true}' 2>&1)
if echo "$SET_ERR" | grep -qi "error\|missing"; then
    pass "F4: set_flag 缺参数正确报错"
else
    fail "F4: set_flag 缺参数未报错"
fi

# F5: 查不存在的扩展
GET_GHOST=$($ION_BIN rpc --session "$SID" --method get_flags \
  --params '{"extension":"nonexistent"}' 2>&1)
if echo "$GET_GHOST" | grep -qE "flags|success"; then
    pass "F5: get_flags 不存在的扩展返回空（不崩溃）"
else
    fail "F5: get_flags 不存在的扩展异常"
fi

# F6: 设置 number 类型
SET_NUM=$($ION_BIN rpc --session "$SID" --method set_flag \
  --params '{"extension":"memory","flag":"limit","value":42}' 2>&1)
if echo "$SET_NUM" | grep -q '"set": true\|"set":true'; then
    pass "F6: set_flag number 类型正常"
else
    fail "F6: set_flag number 类型失败"
fi

# F7: 设置 string 类型
SET_STR=$($ION_BIN rpc --session "$SID" --method set_flag \
  --params '{"extension":"memory","flag":"mode","value":"strict"}' 2>&1)
if echo "$SET_STR" | grep -q '"set": true\|"set":true'; then
    pass "F7: set_flag string 类型正常"
else
    fail "F7: set_flag string 类型失败"
fi

# F8: 查询验证多种类型
GET_MULTI=$($ION_BIN rpc --session "$SID" --method get_flags \
  --params '{"extension":"memory"}' 2>&1)
if echo "$GET_MULTI" | grep -q "limit" && echo "$GET_MULTI" | grep -q "mode"; then
    pass "F8: get_flags 多种类型 flag 值都可见"
else
    fail "F8: get_flags 多类型值缺失"
fi

# 清理
kill $HOST_PID 2>/dev/null
wait $HOST_PID 2>/dev/null
rm -f "$SOCK"

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
