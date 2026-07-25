#!/usr/bin/env bash
# evolve_native_loop.sh — Host 原生自进化闭环（不走 container）
#
# 流程：
#   1. self_test + user_experience → 采集问题
#   2. 如果有问题 → coordinator --host 派 developer 修
#   3. 修完 → 再测
#   4. 全绿 → 发版（可选）
#   5. 循环
#
# 所有用 host 上的 ion binary，不需要 container。
# coordinator 通过 spawn_worker 派 developer 子 agent。
#
# Usage: bash scripts/evolve_native_loop.sh [max_rounds]
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
MAX_ROUNDS="${1:-5}"
LOG_DIR="/tmp/ion_evolve_native"
mkdir -p "$LOG_DIR"

echo "=========================================="
echo "  ION Host 原生自进化闭环"
echo "=========================================="
echo "  最大轮数: $MAX_ROUNDS"
echo "  模式: host 原生（不走 container）"
echo "  日志: $LOG_DIR"
echo "=========================================="

for round in $(seq 1 "$MAX_ROUNDS"); do
    echo ""
    echo "=========================================="
    echo "  Round $round / $MAX_ROUNDS"
    echo "=========================================="

    # ── Step 1: 自测 ──
    echo "[Round $round] Step 1: 自测（self_test + user_experience）..."

    # self_test
    bash scripts/self_test.sh 3 > "$LOG_DIR/selftest_r${round}.log" 2>&1
    ST_PASS=$(grep "Passed:" "$LOG_DIR/selftest_r${round}.log" 2>/dev/null | grep -o '[0-9]*' | head -1)
    ST_FAIL=$(grep "Failed:" "$LOG_DIR/selftest_r${round}.log" 2>/dev/null | grep -o '[0-9]*' | head -1)
    echo "  self_test: ${ST_PASS:-0}/3 passed, ${ST_FAIL:-0} failed"

    # user_experience
    bash scripts/user_experience.sh > "$LOG_DIR/ux_r${round}.log" 2>&1
    UX_PASS=$(grep "通过:" "$LOG_DIR/ux_r${round}.log" 2>/dev/null | grep -o '[0-9]*' | head -1)
    UX_FAIL=$(grep "失败:" "$LOG_DIR/ux_r${round}.log" 2>/dev/null | grep -o '[0-9]*' | head -1)
    echo "  user_experience: ${UX_PASS:-0}/10 passed, ${UX_FAIL:-0} failed"

    # 边界测试
    bash tests/edge_cases.sh > "$LOG_DIR/edge_r${round}.log" 2>&1
    EDGE_PASS=$(grep "通过:" "$LOG_DIR/edge_r${round}.log" 2>/dev/null | grep -o '[0-9]*' | head -1)
    EDGE_FAIL=$(grep "失败:" "$LOG_DIR/edge_r${round}.log" 2>/dev/null | grep -o '[0-9]*' | head -1)
    echo "  edge_cases: ${EDGE_PASS:-0}/10 passed, ${EDGE_FAIL:-0} failed"

    TOTAL_FAIL=$(( ${ST_FAIL:-0} + ${UX_FAIL:-0} + ${EDGE_FAIL:-0} ))

    # ── Step 2: 判断 ──
    if [ "$TOTAL_FAIL" = "0" ]; then
        echo ""
        echo "[Round $round] ✅ 全绿！系统健康。"

        # 全量 lib 测试也跑一下
        LIB_PASS=$(cargo test --lib 2>&1 | grep -o '[0-9]* passed' | head -1)
        echo "  lib tests: $LIB_PASS"

        echo "[Round $round] 没有发现问题。等待下一轮或结束。"
        continue
    fi

    # ── Step 3: 收集问题 ──
    echo ""
    echo "[Round $round] ❌ 发现 $TOTAL_FAIL 个问题。开始修复..."

    # 收集所有问题
    ISSUES=""
    [ "${ST_FAIL:-0}" -gt 0 ] && ISSUES="$ISSUES self_test有${ST_FAIL}个失败"
    [ "${UX_FAIL:-0}" -gt 0 ] && ISSUES="$ISSUES user_experience有${UX_FAIL}个失败"
    [ "${EDGE_FAIL:-0}" -gt 0 ] && ISSUES="$ISSUES edge_cases有${EDGE_FAIL}个失败"

    # 从日志提取具体问题
    FAIL_DETAILS=""
    grep -h "FAIL\|issue" "$LOG_DIR/selftest_r${round}.log" "$LOG_DIR/ux_r${round}.log" "$LOG_DIR/edge_r${round}.log" 2>/dev/null | head -5 | while read line; do
        FAIL_DETAILS="$FAIL_DETAILS $line"
    done

    echo "  问题: $ISSUES"
    echo "  详情: $FAIL_DETAILS"

    # ── Step 4: 用 coordinator --host 派 developer 修复 ──
    echo ""
    echo "[Round $round] Step 4: coordinator 编排修复..."

    FIX_TASK="You are fixing bugs in the ION project. The following tests failed:

$ISSUES
$FAIL_DETAILS

Read the failing test logs in $LOG_DIR/ to understand the errors.
Use spawn_worker to create a developer child worker to fix each issue.
The developer should:
1. Read the relevant source files
2. Fix the bug
3. Run cargo test to verify

After all fixes, run: cargo test --lib to confirm all tests pass."

    echo "$FIX_TASK" | timeout 600 "$ION" \
        --host --agent coordinator \
        --provider zai --model glm-5.2 \
        --max-turns 100 \
        > "$LOG_DIR/fix_r${round}.log" 2>&1

    FIX_RESULT=$(tail -5 "$LOG_DIR/fix_r${round}.log" 2>/dev/null | sed 's/\x1b\[[0-9;]*m//g')
    echo "  修复结果: $FIX_RESULT"

    # ── Step 5: 再测 ──
    echo ""
    echo "[Round $round] Step 5: 修复后重测..."

    cargo test --lib 2>&1 | tail -3 > "$LOG_DIR/retest_r${round}.log"
    RETEST_PASS=$(grep -o '[0-9]* passed' "$LOG_DIR/retest_r${round}.log" | head -1)
    echo "  lib tests after fix: $RETEST_PASS"

    echo "[Round $round] 完成。"
done

echo ""
echo "=========================================="
echo "  自进化循环完成 ($MAX_ROUNDS 轮)"
echo "=========================================="
echo "日志: $LOG_DIR/"
ls -la "$LOG_DIR/" 2>/dev/null | tail -10
echo "=========================================="
