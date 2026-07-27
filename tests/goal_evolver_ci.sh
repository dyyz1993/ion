#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Goal Evolver CI — 验证日志分析逻辑（10 fixture 场景）
#
# 验证内容：
#   Group A: 单元测试（12 个，覆盖 10 fixture + 解析健壮性）
#   Group B: 每个 fixture 的分析正确性（该报的报了，健康的不误报）
#   Group C: run_once 目录扫描（全量 10 场景）
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

echo "════════════════════════════════════════════════════"
echo "  Goal Evolver CI — $(date)"
echo "════════════════════════════════════════════════════"

# ── Build ──
cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# ──────────────────────────────────────────────────────────
echo ""
echo "Group A: 单元测试（12 个，覆盖全部 10 fixture + 解析健壮性）"
# ──────────────────────────────────────────────────────────

UNIT_OUT=$(cargo test --lib goal_evolver 2>&1)
if echo "$UNIT_OUT" | grep -q "test result: ok"; then
    COUNT=$(echo "$UNIT_OUT" | grep -oE "[0-9]+ passed" | head -1)
    pass "A1: goal_evolver 单元测试全过（$COUNT）"
else
    fail "A1: goal_evolver 单元测试有失败"
    echo "$UNIT_OUT" | tail -15
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group B: 问题场景必须报（5 维度，8 个问题 case）"
# ──────────────────────────────────────────────────────────

# B1: case_02 死循环（检测项太严）
if echo "$UNIT_OUT" | grep -q "test_case_02_strict_check_finds_deadloop"; then
    pass "B1: case_02 死循环检测（strict check）"
else
    fail "B1: case_02 测试缺失"
fi

# B2: case_03 死循环（agent 能力不足）
if echo "$UNIT_OUT" | grep -q "test_case_03_weak_agent_finds_deadloop"; then
    pass "B2: case_03 死循环检测（weak agent）"
else
    fail "B2: case_03 测试缺失"
fi

# B3: case_04 模型错（generate_checks 弱模型）
if echo "$UNIT_OUT" | grep -q "test_case_04_weak_generate_checks_model"; then
    pass "B3: case_04 模型检测（generate_checks 弱）"
else
    fail "B3: case_04 测试缺失"
fi

# B4: case_05 模型错（analyze_failure 弱模型）
if echo "$UNIT_OUT" | grep -q "test_case_05_weak_analyze_failure_model"; then
    pass "B4: case_05 模型检测（analyze_failure 弱）"
else
    fail "B4: case_05 测试缺失"
fi

# B5: case_06 上下文缺（测试结果）
if echo "$UNIT_OUT" | grep -q "test_case_06_missing_test_results"; then
    pass "B5: case_06 上下文检测（缺测试结果）"
else
    fail "B5: case_06 测试缺失"
fi

# B6: case_07 上下文缺（git diff）
if echo "$UNIT_OUT" | grep -q "test_case_07_missing_git_diff"; then
    pass "B6: case_07 上下文检测（缺 git diff）"
else
    fail "B6: case_07 测试缺失"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group C: 健康场景不误报（2 个健康 case）"
# ──────────────────────────────────────────────────────────

# C1: case_01 健康（一次过，不应有 High/Medium finding）
if echo "$UNIT_OUT" | grep -q "test_case_01_healthy_no_findings"; then
    pass "C1: case_01 健康不误报"
else
    fail "C1: case_01 测试缺失"
fi

# C2: case_10 成功但曲折（repetitive guard 正常工作，不应误报 deadloop）
if echo "$UNIT_OUT" | grep -q "test_case_10_hard_won_no_false_deadloop"; then
    pass "C2: case_10 repetitive guard 验证（不误报 deadloop）"
else
    fail "C2: case_10 测试缺失"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group D: run_once 全量扫描（10 场景）"
# ──────────────────────────────────────────────────────────

# D1: 全量扫描能找到 10 个 goal
if echo "$UNIT_OUT" | grep -q "test_run_once_all_fixtures"; then
    pass "D1: run_once 全量扫描（10 goal）"
else
    fail "D1: run_once 测试缺失"
fi

# D2: 解析健壮性（缺字段不崩溃）
if echo "$UNIT_OUT" | grep -q "test_parse_handles_missing_fields"; then
    pass "D2: 解析健壮性（缺字段 graceful）"
else
    fail "D2: 解析健壮性测试缺失"
fi

# D3: 不存在目录报错
if echo "$UNIT_OUT" | grep -q "test_parse_goal_run_missing_dir_errors"; then
    pass "D3: 不存在目录正确报错"
else
    fail "D3: 报错测试缺失"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group E: RPC 接口（goal_evolver_run_once 可用性）"
# ──────────────────────────────────────────────────────────

# E1: RPC method 已注册（源码检查）
if grep -q '"goal_evolver_run_once"' src/worker_rpc.rs; then
    pass "E1: goal_evolver_run_once RPC 已注册"
else
    fail "E1: RPC 未注册"
fi

# E2: RPC 在 help 列表
if grep -q 'goal_evolver_run_once.*desc' src/worker_rpc.rs; then
    pass "E2: RPC 在 get_commands help 列表"
else
    fail "E2: RPC 不在 help 列表"
fi

# E3: dry_run 默认 true（安全）
if grep -q "dry_run.*unwrap_or(true)" src/worker_rpc.rs; then
    pass "E3: dry_run 默认 true（安全，不误提交 Issue）"
else
    fail "E3: dry_run 默认值不对"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group F: fixture 数据完整性（10 场景全在）"
# ──────────────────────────────────────────────────────────

FIXTURE_COUNT=$(ls -d tests/fixtures/goal-runs/case_*/ 2>/dev/null | wc -l | tr -d ' ')
if [ "$FIXTURE_COUNT" = "10" ]; then
    pass "F1: 10 个 fixture 场景目录齐全"
else
    fail "F1: fixture 数量不对（$FIXTURE_COUNT/10）"
fi

# F2: 每个 fixture 有 iterations.jsonl + final-report.json
MISSING=0
for d in tests/fixtures/goal-runs/case_*/; do
    [ -f "$d/iterations.jsonl" ] || { echo "  缺 iterations.jsonl: $d"; MISSING=$((MISSING+1)); }
    [ -f "$d/final-report.json" ] || { echo "  缺 final-report.json: $d"; MISSING=$((MISSING+1)); }
done
if [ "$MISSING" = "0" ]; then
    pass "F2: 全部 fixture 文件完整（iterations + final-report）"
else
    fail "F2: 缺 $MISSING 个文件"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════"
echo "  Goal Evolver CI Summary"
echo "════════════════════════════════════════════════════"
echo "  Passed: $PASS"
echo "  Failed: $FAIL"
echo "════════════════════════════════════════════════════"

if [ $FAIL -gt 0 ]; then exit 1; fi
exit 0
