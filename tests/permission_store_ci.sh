#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：Stored-Decision 权限记忆 CLI 端到端验证
# ──────────────────────────────────────────────────────────
# 覆盖 docs/design/PERMISSION_STORE.md Group A：
#   A1 store_decision（模拟用户选 always allow → 决策被存储）
#   A2 list_stored（列出已存储决策）
#   A3 自动放行（同一操作不再问）
#   A4 remove_stored（删除单条存储决策）
#   A5 clear_stored（清空所有存储决策）
#   A6 Config 规则不被 clear/remove 误伤（source 隔离）
#   A7 session scope stored（仅当前会话）
#   A8 extension_rpc 等价路径（store_decision/list_stored/remove_stored/clear_stored）
#
# 用法:
#   bash tests/permission_store_ci.sh
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

# 静默运行：过滤掉 zcode 注入的无关 stderr
quiet() { "$@" 2>/dev/null | grep -v "setValueForKey\|valueForKey\|_encode\|_decode" || true; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

# ── 隔离测试目录（避免污染真实项目 settings.json）──
TEST_PROJECT="/tmp/ion-perm-store-test-$$"
rm -rf "$TEST_PROJECT"
mkdir -p "$TEST_PROJECT/.ion"

echo "════════════════════════════════════════════════════"
echo "  ION Stored-Decision Permission CI Test"
echo "  $(date)"
echo "  test project: $TEST_PROJECT"
echo "════════════════════════════════════════════════════"

# ── Phase 0: Build ──
echo ""
echo "── Phase 0: Build ──"
if cargo build --bin ion --bin ion-worker 2>/dev/null; then
    pass "cargo build ion/ion-worker"
else
    fail "cargo build ion/ion-worker"
    exit 1
fi

if [ -x "$PROJECT_DIR/target/debug/ion" ]; then
    ION_BIN="$PROJECT_DIR/target/debug/ion"
else
    ION_BIN="ion"
fi
echo "  使用 binary: $ION_BIN"

# ── Phase 1: 启动 Manager + Worker（cwd 指向隔离测试目录）──
echo ""
echo "── Phase 1: Manager & Worker ──"

# 清理残留
lsof -ti :53293 2>/dev/null | xargs kill -9 2>/dev/null || true
for pid in $(ps aux | grep "target/debug/ion" | grep -v grep | awk '{print $2}' 2>/dev/null || true); do
    kill -9 "$pid" 2>/dev/null || true
done
sleep 1
rm -f /Users/xuyingzhou/.ion/host.sock

cargo run --bin ion -- serve start > /tmp/ion-perm-store-host.log 2>&1 &
MANAGER_CMD_PID=$!
sleep 4

if ps -p "$MANAGER_CMD_PID" > /dev/null 2>&1 || lsof -ti :53293 2>/dev/null | head -1 > /dev/null; then
    pass "serve start"
else
    fail "serve start"
    exit 1
fi

# 创建 Worker（cwd = 隔离测试目录，这样 settings.json 写到这里）
OUT=$(quiet "$ION_BIN" rpc --session x --method create_worker --params '{"cwd":"'"$TEST_PROJECT"'"}')
SID=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))" 2>/dev/null)

if [ -n "$SID" ]; then
    pass "create_worker (sid=${SID:0:8}...)"
else
    fail "create_worker"
    cat /tmp/ion-perm-store-host.log | tail -20
    exit 1
fi

# 辅助：rpc 调用并提取 success 字段（递归解包 data 嵌套）
rpc_success() {
    quiet "$ION_BIN" rpc --session "$SID" --method "$1" --params "$2" \
        | python3 -c "
import sys,json
d=json.load(sys.stdin)
# 递归解包 data 直到找到 success 或到底
while isinstance(d,dict) and 'success' not in d and 'data' in d:
    d=d['data']
print('1' if (isinstance(d,dict) and d.get('success')) else '0')
" 2>/dev/null || echo "0"
}

# 辅助：rpc 调用并提取最内层 payload（递归解包 data 嵌套）
# 顶层 RPC 响应结构：{success, data: {success, data: {actual_payload}}}
# extension_rpc 响应结构：{success, data: {method, output: {actual_payload}}}
rpc_payload() {
    quiet "$ION_BIN" rpc --session "$SID" --method "$1" --params "$2" \
        | python3 -c "
import sys,json
d=json.load(sys.stdin)
# 递归解包 data
for _ in range(5):
    if isinstance(d,dict) and 'data' in d:
        d=d['data']
    elif isinstance(d,dict) and 'output' in d:
        d=d['output']
    else:
        break
print(json.dumps(d))
" 2>/dev/null || echo "{}"
}

# 旧别名（兼容）
rpc_data() { rpc_payload "$1" "$2"; }

SETTINGS="$TEST_PROJECT/.ion/settings.json"

# ─────────────────────────────────────────────────────────
# Group A：stored-decision 基本流程
# ─────────────────────────────────────────────────────────
echo ""
echo "── Group A: Stored-Decision 基本流程 ──"

# A1: store_decision（allow, project scope）→ 决策被存储
OUT=$(rpc_data "permission_store_decision" \
  '{"subject":"command.run","pattern":"git status","decision":"allow","scope":"project"}')
STORED_ID=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); d=d.get('data',d); msg=d.get('message',''); print(msg.split('(')[-1].rstrip(')'))" 2>/dev/null)

if [ -n "$STORED_ID" ] && [[ "$STORED_ID" == perm_stored_* ]]; then
    pass "A1 store_decision (id=$STORED_ID)"
else
    fail "A1 store_decision (OUT=$OUT)"
fi

# 验证 settings.json 里出现 rule 且 source=stored
if [ -f "$SETTINGS" ] && grep -q '"source": "stored"' "$SETTINGS"; then
    pass "A1b settings.json 含 source=stored 规则"
else
    fail "A1b settings.json 缺 source=stored（文件: $SETTINGS）"
    [ -f "$SETTINGS" ] && cat "$SETTINGS"
fi

# A2: list_stored → 应有 1 条
OUT=$(rpc_data "permission_list_stored" '{}')
COUNT=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null)
if [ "$COUNT" = "1" ]; then
    pass "A2 list_stored (count=1)"
else
    fail "A2 list_stored (count=$COUNT, OUT=$OUT)"
fi

# A3: 同一操作自动放行 — call_tool bash "git status" 应 success
# （stored allow 规则命中 → before_tool_call 放行）
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool \
  --params '{"tool":"bash","args":{"command":"git status"}}')
if echo "$OUT" | grep -q '"success": true\|"success":true'; then
    pass "A3 自动放行: git status (stored allow 生效)"
else
    fail "A3 自动放行: git status (OUT=$OUT)"
fi

# A4: remove_stored → 删除单条，验证返回 removed
OUT=$(rpc_data "permission_remove_stored" "{\"id\":\"$STORED_ID\"}")
if echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); d=d.get('data',d); exit(0 if d.get('removed') else 1)" 2>/dev/null; then
    pass "A4 remove_stored (removed=$STORED_ID)"
else
    fail "A4 remove_stored (OUT=$OUT)"
fi

# 验证 list_stored 变空
OUT=$(rpc_data "permission_list_stored" '{}')
COUNT=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null)
if [ "$COUNT" = "0" ]; then
    pass "A4b remove 后 list_stored count=0"
else
    fail "A4b remove 后仍剩 count=$COUNT"
fi

# A4c: remove_stored 不存在的 id → 应失败（success=false）
SF=$(rpc_success "permission_remove_stored" '{"id":"perm_stored_nonexistent"}')
if [ "$SF" = "0" ]; then
    pass "A4c remove_stored 不存在 id → 报错"
else
    fail "A4c remove_stored 不存在 id 应报错却成功"
fi

# ── A5: clear_stored ──
# 先存 2 条，再 clear，验证清空
rpc_data "permission_store_decision" \
  '{"subject":"command.run","pattern":"npm install","decision":"allow","scope":"project"}' > /dev/null
rpc_data "permission_store_decision" \
  '{"subject":"command.run","pattern":"npm test","decision":"allow","scope":"project"}' > /dev/null
OUT=$(rpc_data "permission_list_stored" '{}')
COUNT=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null)
if [ "$COUNT" = "2" ]; then
    :
else
    fail "A5 预备：存 2 条后 count 应=2 实际=$COUNT"
fi

OUT=$(rpc_data "permission_clear_stored" '{}')
REMOVED=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('removed',0))" 2>/dev/null)
if [ "$REMOVED" = "2" ]; then
    pass "A5 clear_stored (removed=2)"
else
    fail "A5 clear_stored (removed=$REMOVED, OUT=$OUT)"
fi

# 验证清空后 count=0
OUT=$(rpc_data "permission_list_stored" '{}')
COUNT=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null)
if [ "$COUNT" = "0" ]; then
    pass "A5b clear 后 list_stored count=0"
else
    fail "A5b clear 后仍剩 count=$COUNT"
fi

# ── A6: source 隔离 — Config 规则不被 clear/remove 误伤 ──
echo ""
echo "── A6: source 隔离（Config 规则不受影响）──"

# 用 add_rule（source=Config）加一条 project 规则
rpc_data "extension_rpc" \
  '{"extension":"permission","method":"add_rule","args":{"subject":"command.run","pattern":"echo config-rule","decision":"allow","scope":"project"}}' > /dev/null
# 用 store_decision（source=Stored）加一条
rpc_data "permission_store_decision" \
  '{"subject":"command.run","pattern":"echo stored-rule","decision":"allow","scope":"project"}' > /dev/null

# list_rules 应有 2 条（1 config + 1 stored）
OUT=$(rpc_data "extension_rpc" '{"extension":"permission","method":"list_rules"}')
TOTAL=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('count',0))" 2>/dev/null || echo "0")
if [ "$TOTAL" = "2" ]; then
    pass "A6a add_rule(Config) + store_decision(Stored) = 2 条"
else
    fail "A6a 总规则数=$TOTAL 应=2"
fi

# clear_stored → 只清 Stored，Config 保留
rpc_data "permission_clear_stored" '{}' > /dev/null
OUT=$(rpc_data "extension_rpc" '{"extension":"permission","method":"list_rules"}')
TOTAL=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('count',0))" 2>/dev/null || echo "0")
if [ "$TOTAL" = "1" ]; then
    pass "A6b clear_stored 后 Config 规则保留（剩 1 条）"
else
    fail "A6b clear_stored 后剩 $TOTAL 条 应=1（Config 应保留）"
fi

# 剩下的那条 source 应为 config
SOURCE=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); rules=d.get('rules',[]); print(rules[0].get('source','') if rules else '')" 2>/dev/null)
if [ "$SOURCE" = "config" ]; then
    pass "A6c 剩余规则 source=config（未被误删）"
else
    fail "A6c 剩余规则 source=$SOURCE 应=config"
fi

# ── A7: session scope stored ──
echo ""
echo "── A7: session scope stored ──"

rpc_data "permission_store_decision" \
  '{"subject":"command.run","pattern":"ls -la","decision":"allow","scope":"session"}' > /dev/null
OUT=$(rpc_data "permission_list_stored" '{}')
COUNT=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('count',0))" 2>/dev/null)
# 注意：A6b 后 list_rules 剩 1 条 config（project 级），这里 list_stored 只看 Stored
# session stored 算 1 条
if [ "$COUNT" = "1" ]; then
    pass "A7 session scope stored (count=1)"
else
    fail "A7 session scope stored (count=$COUNT)"
fi

# session stored 规则的 scope 字段应为 session
SCOPE=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); d=d.get('data',d); rules=d.get('rules',[]); print(rules[0].get('scope','') if rules else '')" 2>/dev/null)
if [ "$SCOPE" = "session" ]; then
    pass "A7b session stored scope=session"
else
    fail "A7b session stored scope=$SCOPE 应=session"
fi

# ── A8: extension_rpc 等价路径 ──
echo ""
echo "── A8: extension_rpc 等价路径（与顶层 RPC 一致）──"

# 用 extension_rpc store_decision
OUT=$(rpc_data "extension_rpc" \
  '{"extension":"permission","method":"store_decision","args":{"subject":"file.read","pattern":"**/test-data/*","decision":"deny","scope":"project"}}')
if echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); d=d.get('data',d); exit(0 if 'stored' in d.get('message','') else 1)" 2>/dev/null; then
    pass "A8a extension_rpc store_decision 等价"
else
    fail "A8a extension_rpc store_decision (OUT=$OUT)"
fi

# 用 extension_rpc list_stored → 应包含刚加的
OUT=$(rpc_data "extension_rpc" '{"extension":"permission","method":"list_stored"}')
EXT_COUNT=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); d=d.get('data',d); print(d.get('count',0))" 2>/dev/null || echo "0")
if [ "$EXT_COUNT" -ge "1" ] 2>/dev/null; then
    pass "A8b extension_rpc list_stored 等价 (count=$EXT_COUNT)"
else
    fail "A8b extension_rpc list_stored (count=$EXT_COUNT)"
fi

# 用 extension_rpc clear_stored
OUT=$(rpc_data "extension_rpc" '{"extension":"permission","method":"clear_stored"}')
EXT_REMOVED=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); d=d.get('data',d); print(d.get('removed',0))" 2>/dev/null || echo "0")
if [ "$EXT_REMOVED" -ge "1" ] 2>/dev/null; then
    pass "A8c extension_rpc clear_stored 等价 (removed=$EXT_REMOVED)"
else
    fail "A8c extension_rpc clear_stored (removed=$EXT_REMOVED, OUT=$OUT)"
fi

# ── A9: 错误处理 ──
echo ""
echo "── A9: 错误处理 ──"

# store_decision 缺 decision → 应失败
SF=$(rpc_success "permission_store_decision" \
  '{"subject":"command.run","pattern":"x","scope":"project"}')
# decision 默认 allow，所以这个实际会成功。改成非法 decision 值
SF=$(rpc_success "permission_store_decision" \
  '{"subject":"command.run","pattern":"x","decision":"maybe","scope":"project"}')
if [ "$SF" = "0" ]; then
    pass "A9a store_decision 非法 decision → 报错"
else
    fail "A9a store_decision 非法 decision 应报错"
fi

# store_decision 非法 scope → 应失败
SF=$(rpc_success "permission_store_decision" \
  '{"subject":"command.run","pattern":"x","decision":"allow","scope":"global"}')
if [ "$SF" = "0" ]; then
    pass "A9b store_decision 非法 scope → 报错"
else
    fail "A9b store_decision 非法 scope 应报错"
fi

# remove_stored 缺 id → 应失败
SF=$(rpc_success "permission_remove_stored" '{}')
if [ "$SF" = "0" ]; then
    pass "A9c remove_stored 缺 id → 报错"
else
    fail "A9c remove_stored 缺 id 应报错"
fi

# ── Cleanup ──
echo ""
echo "── Cleanup ──"
for pid in $(ps aux | grep "target/debug/ion" | grep -v grep | awk '{print $2}' 2>/dev/null || true); do
    kill "$pid" 2>/dev/null || true
done
rm -rf "$TEST_PROJECT" /tmp/ion-perm-store-host.log
echo "  Cleaned up (含测试目录 $TEST_PROJECT)"

# ── 总结 ──
echo ""
echo "════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "════════════════════════════════════════════════════"

if [ $FAIL -gt 0 ]; then
    exit 1
fi
exit 0
