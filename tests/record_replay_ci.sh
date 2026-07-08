#!/usr/bin/env bash
# Record/Replay Phase 1 验证 — 录制/回放/管理/安全
# 不调真实 LLM（用 faux 作为录制源）
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
green(){ printf "\033[32m%s\033[0m\n" "$1"; }
red(){ printf "\033[31m%s\033[0m\n" "$1"; }
pass(){ green "✅ PASS: $1"; ((PASS++)); }
fail(){ red "❌ FAIL: $1"; ((FAIL++)); }

echo "── Phase 0: Build ──"
cargo build --bin ion --bin ion-worker 2>&1 | tail -2
ION_BIN="$PROJECT_DIR/target/debug/ion"

TEST_DIR=$(mktemp -d)
cd "$TEST_DIR"
rm -rf ~/.ion/recordings/rr-test-*

echo ""
echo "── Group A: 录制 + 安全 ──"

# A1: 基本录制（faux 作内层，录制器捕获）
ION_FAUX_REPLY="recorded response A1" ION_RECORD=rr-test-a1 timeout 30 "$ION_BIN" --no-session --no-tools "hi" >/tmp/a1.log 2>&1
if [ -f ~/.ion/recordings/rr-test-a1/trace.jsonl ] && grep -q "recorded response A1" ~/.ion/recordings/rr-test-a1/trace.jsonl; then
    pass "A1 录制基本响应"
else
    fail "A1 录制基本响应 (log: $(cat /tmp/a1.log | tail -3))"
fi

# A2: 路径穿越拒绝
OUTPUT=$(timeout 10 "$ION_BIN" --no-session --no-tools --model replay/../../etc/hostname "x" 2>&1 || true)
if echo "$OUTPUT" | grep -qi "invalid recording id"; then
    pass "A2 路径穿越拒绝"
else
    fail "A2 路径穿越拒绝 (output: $(echo "$OUTPUT" | tail -2))"
fi

echo ""
echo "── Group B: 回放 ──"

# B1: 基本回放复现响应
OUTPUT=$(timeout 30 "$ION_BIN" --no-session --no-tools --model replay/rr-test-a1 "replay hi" 2>&1)
if echo "$OUTPUT" | grep -q "recorded response A1"; then
    pass "B1 回放复现录制响应"
else
    fail "B1 回放复现 (output: $(echo "$OUTPUT" | tail -3))"
fi

# B2: 回放不存在 ID 报错
OUTPUT=$(timeout 10 "$ION_BIN" --no-session --no-tools --model replay/nonexistent-xyz "x" 2>&1 || true)
if echo "$OUTPUT" | grep -qi "not found"; then
    pass "B2 回放不存在 ID 报错"
else
    fail "B2 回放不存在 ID (output: $(echo "$OUTPUT" | tail -2))"
fi

# B3: 回放安全提示
OUTPUT=$(timeout 30 "$ION_BIN" --no-session --no-tools --model replay/rr-test-a1 "x" 2>&1)
if echo "$OUTPUT" | grep -qi "Tools will execute\|⚠️"; then
    pass "B3 回放安全提示"
else
    fail "B3 回放安全提示"
fi

echo ""
echo "── Group C: 管理 + 冲突 + 安全 ──"

# C1: ion recordings list
OUTPUT=$(timeout 10 "$ION_BIN" recordings 2>&1)
if echo "$OUTPUT" | grep -q "rr-test-a1"; then
    pass "C1 ion recordings list"
else
    fail "C1 ion recordings list"
fi

# C2: 录制冲突报错（同 ID 无 OVERWRITE）
OUTPUT=$(ION_FAUX_REPLY="x" ION_RECORD=rr-test-a1 timeout 10 "$ION_BIN" --no-session --no-tools "x" 2>&1 || true)
if echo "$OUTPUT" | grep -qi "already exists\|OVERWRITE"; then
    pass "C2 录制冲突报错"
else
    fail "C2 录制冲突报错 (output: $(echo "$OUTPUT" | tail -2))"
fi

# C3: OVERWRITE 覆盖
OUTPUT=$(ION_FAUX_REPLY="overwritten response" ION_RECORD=rr-test-a1 ION_RECORD_OVERWRITE=1 timeout 10 "$ION_BIN" --no-session --no-tools "x" 2>&1)
if grep -q "overwritten response" ~/.ion/recordings/rr-test-a1/trace.jsonl; then
    pass "C3 OVERWRITE 覆盖"
else
    fail "C3 OVERWRITE 覆盖"
fi

# C4: trace 含 request_hash
if grep -q "request_hash" ~/.ion/recordings/rr-test-a1/trace.jsonl; then
    pass "C4 trace 含 request_hash"
else
    fail "C4 trace 含 request_hash"
fi

# C5: 文件权限 0600
PERMS=$(stat -f "%Lp" ~/.ion/recordings/rr-test-a1/trace.jsonl 2>/dev/null || stat -c "%a" ~/.ion/recordings/rr-test-a1/trace.jsonl)
if [ "$PERMS" = "600" ]; then
    pass "C5 trace 文件权限 0600"
else
    fail "C5 trace 文件权限 (got $PERMS, want 600)"
fi

# C6: meta schema_version
if grep -q "schema_version" ~/.ion/recordings/rr-test-a1/meta.json; then
    pass "C6 meta 含 schema_version"
else
    fail "C6 meta 含 schema_version"
fi

echo ""
echo "── 结果 ──"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && green "全部通过" || red "有失败"

# 清理
rm -rf ~/.ion/recordings/rr-test-*
exit $FAIL
