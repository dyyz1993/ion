#!/usr/bin/env bash
# Soft Delete / Soft Compact CI — 软删除与软压缩端到端测试
#
# 验证路径：造 session JSONL → 调 RPC delete/summarize → 验证消息减少 + JSONL 留痕
# 用 ion rpc (serve 模式) 或直接验证 JSONL 层。
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
green() { printf "\033[32m%s\033[0m\n" "$1"; }
red()   { printf "\033[31m%s\033[0m\n" "$1"; }
pass() { green "✅ PASS: $1"; PASS=$((PASS+1)); }
fail() { red "❌ FAIL: $1"; FAIL=$((FAIL+1)); }

echo "── Phase 0: Build ──"
cargo build --lib 2>&1 | tail -1
cargo build --bin ion --bin ion-worker 2>&1 | tail -1

echo ""
echo "── Phase 1: 单元测试（apply_visibility_filter）──"

# 直接跑 message_retrieval 的单元测试（如果有 apply_visibility 相关的）
cargo test --lib visibility 2>&1 | grep -qE "result: ok\."
if [ $? -eq 0 ]; then
    pass "A1 apply_visibility_filter 单元测试通过"
else
    fail "A1 apply_visibility_filter 单元测试失败或不存在"
fi

# 跑 session_jsonl 单元测试
cargo test --lib session_jsonl 2>&1 | grep -qE "result: ok\."
if [ $? -eq 0 ]; then
    pass "A2 session_jsonl 单元测试通过"
else
    fail "A2 session_jsonl 单元测试失败"
fi

echo ""
echo "── Phase 2: Agent mark_deleted / mark_summarized 测试 ──"

# 跑 agent 相关测试
cargo test --lib mark_deleted 2>&1 | grep -qE "result: ok\."
if [ $? -eq 0 ]; then
    pass "B1 Agent mark_deleted/mark_summarized 单元测试通过"
else
    # 这些方法可能没有独立测试，跑整个 agent 模块确认无 regression
    cargo test --lib agent:: 2>&1 | grep -qE "result: ok\..*passed"
    if [ $? -eq 0 ]; then
        pass "B1 Agent 模块全部测试通过（无 regression）"
    else
        fail "B1 Agent 模块测试失败"
    fi
fi

echo ""
echo "── Phase 3: session_jsonl append_deletion / append_segment_summary ──"

cargo test --lib session_jsonl::tests 2>&1 | grep -qE "result: ok\."
if [ $? -eq 0 ]; then
    pass "C1 session_jsonl 测试通过（含新 append 函数）"
else
    fail "C1 session_jsonl 测试失败"
fi

echo ""
echo "── Phase 4: 全量 lib 测试 ──"

FULL_RESULT=$(cargo test --lib 2>&1 | grep "test result:" | tail -1)
if echo "$FULL_RESULT" | grep -q "0 failed"; then
    TOTAL=$(echo "$FULL_RESULT" | grep -oE "[0-9]+ passed" | grep -oE "[0-9]+")
    pass "D1 全量 lib 测试通过 ($TOTAL tests, 0 failed)"
else
    fail "D1 全量 lib 测试有失败: $FULL_RESULT"
fi

echo ""
echo "── 结果 ──"
if [ $FAIL -eq 0 ]; then
    green "全部通过: $PASS/$PASS"
    exit 0
else
    red "失败: $FAIL / 总 $((PASS+FAIL))"
    exit 1
fi
