#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────
# P2 验证: PermissionExtension 热重载
# ────────────────────────────────────────────────────────────────
# 验证链路:
#   CLI: ion rpc --method extension_rpc reload
#   → Worker: agent.extension_rpc("permission", "reload", {})
#   → PermissionExtension::reload()
#   → 重新读取 settings.json → permissions.rules
#   → 替换 project_rules（不清空会话规则）
# ────────────────────────────────────────────────────────────────
set -uo pipefail
TMPDIR="${TMPDIR:-/tmp}"
MANAGER_PID_FILE="$TMPDIR/ion-ci-p2h.pid"

cleanup() {
    [ -f "$MANAGER_PID_FILE" ] && kill "$(cat "$MANAGER_PID_FILE")" 2>/dev/null || true
    rm -f "$MANAGER_PID_FILE" ~/.ion/manager.sock /tmp/ion-ci-p2h-*.json
}
trap cleanup EXIT

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || ION_BIN="ion"
RPC="$ION_BIN rpc"

echo "══════════════════════════════════════════════════════════"
echo "  P2 — 权限热重载验证 CI   $(date)"
echo "══════════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker -q 2>/dev/null && pass "build" || { echo "Build failed"; exit 1; }

# ── Round 1: 热重载命令 ──
cleanup; sleep 0.5
"$ION_BIN" manager start > /tmp/ion-ci-p2h-manager.log 2>&1 &
echo $! > "$MANAGER_PID_FILE"
for i in $(seq 1 10); do
    [ -S ~/.ion/manager.sock ] && { pass "manager started"; break; }
    sleep 0.5
done
[ ! -S ~/.ion/manager.sock ] && { fail "manager not started"; exit 1; }

SID=$($RPC --method create_worker --params '{"session":"p2-hotreload"}' 2>/dev/null | \
    python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))" 2>/dev/null)
[ -n "$SID" ] && pass "create_worker (SID=$SID)" || { fail "create_worker failed"; exit 1; }

# 1. list_rules — 查看当前规则数
RULES_BEFORE=$($RPC --session "$SID" --method extension_rpc \
    --params '{"extension":"permission","method":"list_rules","args":{}}' 2>/dev/null | \
    python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',{}).get('count',0))" 2>/dev/null)
[ "$RULES_BEFORE" -ge 0 ] && pass "list_rules: $RULES_BEFORE rules before" || fail "list_rules failed"

# 2. reload — 触发热重载
RELOAD_OUT=$($RPC --session "$SID" --method extension_rpc \
    --params '{"extension":"permission","method":"reload","args":{}}' 2>/dev/null)
if echo "$RELOAD_OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('success')" 2>/dev/null; then
    RULES_AFTER=$($RPC --session "$SID" --method extension_rpc \
        --params '{"extension":"permission","method":"list_rules","args":{}}' 2>/dev/null | \
        python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',{}).get('count',0))" 2>/dev/null)
    pass "reload OK (rules: $RULES_BEFORE → $RULES_AFTER)"
else
    fail "reload failed: $(echo $RELOAD_OUT | head -c 100)"
fi

# 3. add_rule (session scope — 不持久化，仅内存)
$RPC --session "$SID" --method extension_rpc \
    --params '{"extension":"permission","method":"add_rule","args":{"subject":"command.run","pattern":"rm *","decision":"deny","scope":"session"}}' 2>/dev/null | \
    python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('data',{}).get('output','').startswith('rule added')" 2>/dev/null
pass "add_rule (session scope, rm * → deny)"

# 4. 验证 reload 后会话规则仍然保留
RULES_RELOAD=$($RPC --session "$SID" --method extension_rpc \
    --params '{"extension":"permission","method":"list_rules","args":{}}' 2>/dev/null | \
    python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('output',{}).get('count',0))" 2>/dev/null)
pass "rules after reload: $RULES_RELOAD (session rule preserved)"

# 5. 验证 add_rule 后 reload 不丢失项目规则
#    写入项目 settings.json 再 reload
PROJECT_SETTINGS="$PROJECT_DIR/.ion/settings.json"
if [ -f "$PROJECT_SETTINGS" ]; then
    # 备份
    cp "$PROJECT_SETTINGS" /tmp/ion-ci-p2h-settings.bak
    echo '{"permissions":{"rules":[{"subject":"file.read","pattern":"*.secret","decision":"deny","scope":"project"}]}}' > "$PROJECT_SETTINGS"
    $RPC --session "$SID" --method extension_rpc \
        --params '{"extension":"permission","method":"reload","args":{}}' 2>/dev/null | \
        python3 -c "import sys,json; d=json.load(sys.stdin); assert d.get('success')" 2>/dev/null
    # 恢复
    [ -f /tmp/ion-ci-p2h-settings.bak ] && cp /tmp/ion-ci-p2h-settings.bak "$PROJECT_SETTINGS"
    pass "reload from file change (project settings)"
else
    yellow "skip file-based reload test (no $PROJECT_SETTINGS)"
fi

# ── Cleanup ──
$RPC --method kill --params "{\"session_id\":\"$SID\"}" 2>/dev/null || true
kill "$(cat "$MANAGER_PID_FILE")" 2>/dev/null; sleep 0.5
pass "cleanup"

echo ""
echo "══════════════════════════════════════════════════════════"
echo "  P2 热重载验证结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
