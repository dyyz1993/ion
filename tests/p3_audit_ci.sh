#!/usr/bin/env bash
# ────────────────────────────────────────────────────────────────
# P3 验证: 审计日志 — CommandGuard 决策持久化
# ────────────────────────────────────────────────────────────────
set -uo pipefail
TMPDIR="${TMPDIR:-/tmp}"
MANAGER_PID_FILE="$TMPDIR/ion-ci-p3a.pid"

cleanup() {
    [ -f "$MANAGER_PID_FILE" ] && kill "$(cat "$MANAGER_PID_FILE")" 2>/dev/null || true
    rm -f "$MANAGER_PID_FILE" ~/.ion/manager.sock
}
trap cleanup EXIT

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"
[ -x "$ION_BIN" ] || ION_BIN="ion"
RPC="$ION_BIN rpc"

rm -f ~/.ion/agent/audit.jsonl
echo "══════════════════════════════════════════════════════════"
echo "  P3 — 审计日志验证 CI   $(date)"
echo "══════════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker -q 2>/dev/null && pass "build" || { echo "Build failed"; exit 1; }

cleanup; sleep 0.5
"$ION_BIN" manager start > /tmp/ion-ci-p3a-manager.log 2>&1 &
echo $! > "$MANAGER_PID_FILE"
for i in $(seq 1 10); do
    [ -S ~/.ion/manager.sock ] && { pass "manager started"; break; }
    sleep 0.5
done
[ ! -S ~/.ion/manager.sock ] && { fail "manager not started"; exit 1; }

SID=$($RPC --method create_worker --params '{"session":"p3-audit"}' 2>/dev/null | \
    python3 -c "import sys,json; print(json.load(sys.stdin).get('data',{}).get('sessionId',''))" 2>/dev/null)
[ -n "$SID" ] && pass "create_worker (SID=$SID)" || { fail "create_worker failed"; exit 1; }

# 1. 执行一个 Allow 命令 → 审计日志应有 allow 记录
$RPC --session "$SID" --method call_tool --params '{"tool":"bash","args":{"command":"echo allow-cmd"}}' > /dev/null 2>&1
sleep 0.5
if grep '"decision":"allow"' ~/.ion/agent/audit.jsonl 2>/dev/null | grep -q "allow-cmd"; then
    pass "audit: allow command logged"
else
    fail "audit: allow command NOT found in log"
fi

# 2. 随机日志格式验证
AUDIT_LINES=$(wc -l < ~/.ion/agent/audit.jsonl 2>/dev/null || echo 0)
if [ "$AUDIT_LINES" -ge 1 ]; then
    pass "audit: $AUDIT_LINES log entries written"
else
    fail "audit: empty log file"
fi

# 3. 验证 JSONL 格式可解析
if python3 -c "
import json
with open('$HOME/.ion/agent/audit.jsonl') as f:
    for line in f:
        entry = json.loads(line.strip())
        assert 'timestamp' in entry
        assert 'command' in entry
        assert 'decision' in entry
assert entry['timestamp'].endswith('Z')
print('Valid JSONL format')
"; then
    pass "audit: valid JSONL format"
else
    fail "audit: invalid JSONL format"
fi

# Cleanup
$RPC --method kill --params "{\"session_id\":\"$SID\"}" 2>/dev/null || true
kill "$(cat "$MANAGER_PID_FILE")" 2>/dev/null; sleep 0.5
pass "cleanup"

echo ""
echo "══════════════════════════════════════════════════════════"
echo "  P3 审计日志验证结果: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════════"
[ "$FAIL" -eq 0 ] && exit 0 || exit 1
