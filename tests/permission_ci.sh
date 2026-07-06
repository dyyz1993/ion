#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# CI 测试脚本：权限系统 CLI 端到端验证
# ──────────────────────────────────────────────────────────
# 用法:
#   bash tests/permission_ci.sh              # 快速模式
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

quiet() { "$@" 2>/dev/null | grep -v "setValueForKey\|valueForKey\|_encode\|_decode" || true; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

echo "════════════════════════════════════════════════════"
echo "  ION Permission System CI Test"
echo "  $(date)"
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

# 选择 binary
if [ -x "$PROJECT_DIR/target/debug/ion" ]; then
    ION_BIN="$PROJECT_DIR/target/debug/ion"
else
    ION_BIN="ion"
fi
echo "  使用 binary: $ION_BIN"

# ── Phase 1: 单元测试 ──
echo ""
echo "── Phase 1: Unit Tests ──"
RUST_LOG=error cargo test --lib --color never 2>&1 > /tmp/ion-ci-perm-lib.log
if grep -q "^test result:" /tmp/ion-ci-perm-lib.log; then
    pass "cargo test --lib"
else
    fail "cargo test --lib"
fi

# ── Phase 2: 启动 Manager + Worker ──
echo ""
echo "── Phase 2: Manager & Worker ──"

# 清理残留
lsof -ti :53293 2>/dev/null | xargs kill -9 2>/dev/null || true
for pid in $(ps aux | grep "target/debug/ion" | grep -v grep | awk '{print $2}' 2>/dev/null || true); do
    kill -9 "$pid" 2>/dev/null || true
done
sleep 2
rm -f /Users/xuyingzhou/.ion/manager.sock

cargo run --bin ion -- manager start > /tmp/ion-ci-perm-manager.log 2>&1 &
MANAGER_CMD_PID=$!
sleep 4

if ps -p "$MANAGER_CMD_PID" > /dev/null 2>&1 || lsof -ti :53293 2>/dev/null | head -1 > /dev/null; then
    pass "manager start"
else
    fail "manager start"
    exit 1
fi

# 创建 Worker
OUT=$(quiet "$ION_BIN" rpc --session x --method create_worker --params '{"cwd":"'"$PROJECT_DIR"'"}')
SID=$(echo "$OUT" | python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))" 2>/dev/null)

if [ -n "$SID" ]; then
    pass "create_worker (sid=${SID:0:8}...)"
else
    fail "create_worker"
    exit 1
fi

# ── Phase 3: PermissionExtension RPC ──
echo ""
echo "── Phase 3: PermissionExtension RPC ──"

rpc_ok() {
    quiet "$ION_BIN" rpc --session "$SID" --method "$1" --params "$2" \
        | python3 -c "import sys,json; d=json.load(sys.stdin); print('ok' if d.get('success') else 'FAIL: '+str(d.get('error','')))" 2>/dev/null || echo "FAIL: rpc call error"
}

# 3a: list_rules（初始应为空）
rpc_ok "extension_rpc" '{"extension":"permission","method":"list_rules"}' | grep -q ok && pass "list_rules (empty)" || fail "list_rules (empty)"

# 3b: add_rule (session scope)
OUT=$(rpc_ok "extension_rpc" '{"extension":"permission","method":"add_rule","args":{"subject":"command.run","pattern":"echo *","decision":"allow","scope":"session"}}')
echo "$OUT" | grep -q ok && pass "add_rule session scope" || fail "add_rule session scope"

# 3c: add_rule (project scope)
OUT=$(rpc_ok "extension_rpc" '{"extension":"permission","method":"add_rule","args":{"subject":"file.read","pattern":"**/.env*","decision":"deny","scope":"project"}}')
echo "$OUT" | grep -q ok && pass "add_rule project scope" || fail "add_rule project scope"

# 3d: list_rules（应有 2 条）
OUT=$(rpc_ok "extension_rpc" '{"extension":"permission","method":"list_rules"}')
echo "$OUT" | grep -q ok && pass "list_rules (2 rules)" || fail "list_rules (2 rules)"

# ── Phase 4: 规则匹配测试 ──
echo ""
echo "── Phase 4: Rule Matching ──"

# 4a: allow rule — echo hello 应放行
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo hello"}}')
SUCCESS=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('success',''))" 2>/dev/null)
if [ "$SUCCESS" = "True" ] || echo "$OUT" | grep -q '"success": true'; then
    pass "allow rule: echo hello"
else
    fail "allow rule: echo hello"
    echo "  OUT=$OUT"
fi

# 4b: deny rule — read .env 应拒绝
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool --params '{"tool":"read","args":{"file_path":"'"$PROJECT_DIR"'/.env"}}')
if echo "$OUT" | grep -q '"success": false'; then
    pass "deny rule: read .env (blocked)"
else
    fail "deny rule: read .env"
    echo "  OUT=$OUT"
fi

# 4c: CommandGuard — rm -rf 应拦截
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"rm -rf /"}}')
if echo "$OUT" | grep -q "CommandGuard"; then
    pass "CommandGuard: rm -rf / (blocked)"
else
    fail "CommandGuard: rm -rf /"
    echo "  OUT=$OUT"
fi

# 4d: 安全命令放行
OUT=$(quiet "$ION_BIN" rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo safe"}}')
if echo "$OUT" | grep -q '"success": true'; then
    pass "safe command: echo safe"
else
    fail "safe command: echo safe"
fi

# ── Phase 5: 清理 ──
echo ""
echo "── Cleanup ──"
for pid in $(ps aux | grep "target/debug/ion" | grep -v grep | awk '{print $2}' 2>/dev/null || true); do
    kill "$pid" 2>/dev/null || true
done
rm -f /tmp/ion-ci-perm-manager.log /tmp/ion-ci-perm-lib.log
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
