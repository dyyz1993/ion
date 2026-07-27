#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Goal Supervisor CI — B1 核心验证
#
# 验证内容：
#   Group A: 单元测试（24 个，覆盖数据结构/checks/guards/logging）
#   Group B: goal_set tool 注册可见
#   Group C: config 启用/禁用
#
# 后续 B2/B3 会加 FauxProvider 端到端闭环验证（on_gate_check 触发）。
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

echo "════════════════════════════════════════════════════"
echo "  Goal Supervisor CI — $(date)"
echo "════════════════════════════════════════════════════"

# ── Build ──
echo ""
echo "Build: cargo build --bin ion"
cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# ──────────────────────────────────────────────────────────
echo ""
echo "Group A: 单元测试（数据结构 + checks + guards + logging）"
# ──────────────────────────────────────────────────────────

UNIT_OUT=$(cargo test --lib goal_supervisor_extension 2>&1)
if echo "$UNIT_OUT" | grep -q "test result: ok"; then
    COUNT=$(echo "$UNIT_OUT" | grep -oE "[0-9]+ passed" | head -1)
    pass "A1: goal_supervisor 单元测试全过（$COUNT）"
else
    fail "A1: goal_supervisor 单元测试有失败"
    echo "$UNIT_OUT" | tail -10
fi

# ── A2: 数据结构测试 ──
if echo "$UNIT_OUT" | grep -q "test_config_defaults"; then
    pass "A2: config_defaults 测试存在"
else
    fail "A2: config_defaults 测试缺失"
fi

# ── A3: checks 测试 ──
CHECK_TESTS=$(echo "$UNIT_OUT" | grep -cE "test_check_(exit_code|grep_empty|file_exists)")
if [ "$CHECK_TESTS" -ge 6 ]; then
    pass "A3: check 执行测试覆盖（$CHECK_TESTS/6）"
else
    fail "A3: check 执行测试不足（$CHECK_TESTS/6）"
fi

# ── A4: guards 测试 ──
GUARD_TESTS=$(echo "$UNIT_OUT" | grep -cE "test_guard_(max_iterations|max_duration|max_cost|repetitive|none)")
if [ "$GUARD_TESTS" -ge 5 ]; then
    pass "A4: guards 防线测试覆盖（$GUARD_TESTS/5）"
else
    fail "A4: guards 防线测试不足（$GUARD_TESTS/5）"
fi

# ── A5: similarity 测试 ──
SIM_TESTS=$(echo "$UNIT_OUT" | grep -cE "test_calculate_similarity")
if [ "$SIM_TESTS" -ge 3 ]; then
    pass "A5: 重复检测相似度测试覆盖（$SIM_TESTS/3）"
else
    fail "A5: 相似度测试不足（$SIM_TESTS/3）"
fi

# ── A6: logging 测试 ──
if echo "$UNIT_OUT" | grep -q "test_log_iteration_writes_jsonl"; then
    pass "A6: 日志落盘测试存在"
else
    fail "A6: 日志落盘测试缺失"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group B: goal_set tool 注册可见"
# ──────────────────────────────────────────────────────────

# B1: tool 在 ToolRegistry 里注册（通过源码 grep 验证）
if grep -q "GoalSetTool" src/worker_rpc.rs; then
    pass "B1: GoalSetTool 在 worker_rpc.rs 注册"
else
    fail "B1: GoalSetTool 未注册"
fi

# B2: tool 的 name() 返回 "goal_set"
if grep -q '"goal_set"' src/goal_supervisor_extension.rs; then
    pass "B2: tool name 是 goal_set"
else
    fail "B2: tool name 不正确"
fi

# B3: SharedGoalState 在 tool 和 extension 间共享
if grep -q "shared_goal.clone()" src/worker_rpc.rs; then
    pass "B3: tool 和 extension 共享 SharedGoalState"
else
    fail "B3: 共享状态未建立"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group C: Extension 注册 + config"
# ──────────────────────────────────────────────────────────

# C1: GoalSupervisorExtension 在 worker_rpc 注册
if grep -q "GoalSupervisorExtension::new" src/worker_rpc.rs; then
    pass "C1: GoalSupervisorExtension 注册存在"
else
    fail "C1: Extension 未注册"
fi

# C2: config 驱动（is_extension_enabled("goal-supervisor")）
if grep -q 'is_extension_enabled("goal-supervisor")' src/worker_rpc.rs; then
    pass "C2: config goal-supervisor 开关接入"
else
    fail "C2: config 开关未接入"
fi

# C3: on_gate_check 钩子实现（核心闭环入口）
if grep -q "fn on_gate_check" src/goal_supervisor_extension.rs; then
    pass "C3: on_gate_check 钩子已实现"
else
    fail "C3: on_gate_check 钩子缺失"
fi

# C4: GateDecision::RetryWith 用于强制继续
if grep -q "GateDecision::RetryWith" src/goal_supervisor_extension.rs; then
    pass "C4: RetryWith 强制继续逻辑存在"
else
    fail "C4: RetryWith 逻辑缺失"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group D: 日志 schema 完整性（静态检查）"
# ──────────────────────────────────────────────────────────

# D1: iterations.jsonl 写入逻辑
if grep -q "iterations.jsonl" src/goal_supervisor_extension.rs; then
    pass "D1: iterations.jsonl 日志写入"
else
    fail "D1: iterations.jsonl 缺失"
fi

# D2: final-report.json 写入逻辑
if grep -q "final-report.json" src/goal_supervisor_extension.rs; then
    pass "D2: final-report.json 写入"
else
    fail "D2: final-report.json 缺失"
fi

# D3: 证据收集（evidence）
if grep -q "artifact_path" src/goal_supervisor_extension.rs; then
    pass "D3: evidence 收集（artifact_path）"
else
    fail "D3: evidence 收集缺失"
fi

# D4: U+FFFD 检查（守门）— grep -c 返回 0 时 exit 1，用 || true 兜底
UFFFD=$(grep -c $'\xef\xbf\xbd' src/goal_supervisor_extension.rs 2>/dev/null || true)
UFFFD=$(echo "$UFFFD" | head -1 | tr -d '[:space:]')
if [ "$UFFFD" = "0" ] || [ -z "$UFFFD" ]; then
    pass "D4: 0 个 U+FFFD（无乱码）"
else
    fail "D4: $UFFFD 个 U+FFFD（有乱码）"
fi

# ──────────────────────────────────────────────────────────
# 总结
# ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════"
echo "  Goal Supervisor CI Summary"
echo "════════════════════════════════════════════════════"
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo "  Skipped: $SKIP"
echo "════════════════════════════════════════════════════"

if [ $FAIL -gt 0 ]; then
    exit 1
fi
exit 0
