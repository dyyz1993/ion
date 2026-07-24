#!/usr/bin/env bash
# evolve_loop.sh — 持续自进化循环
#
# 流程：
#   1. 跑 self_test → 收集结果
#   2. 如果全绿 → 等待新任务（或退出）
#   3. 如果有问题 → A 派 B 修复 → 再测
#   4. 循环
#
# Usage: bash scripts/evolve_loop.sh [max_rounds]
#   max_rounds = 最大循环轮数（默认 5）
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
MAX_ROUNDS="${1:-5}"
LOG_DIR="/tmp/ion_evolve_loop"
mkdir -p "$LOG_DIR"

echo "=========================================="
echo "  ION 持续自进化循环"
echo "=========================================="
echo "  最大轮数: $MAX_ROUNDS"
echo "  日志目录: $LOG_DIR"
echo "=========================================="
echo ""

for round in $(seq 1 "$MAX_ROUNDS"); do
    echo ""
    echo "=========================================="
    echo "  Round $round / $MAX_ROUNDS"
    echo "=========================================="
    
    # ── Step 1: Self-test ──────────────────────────────────
    echo "[Loop] Step 1: Running self_test..."
    bash scripts/self_test.sh 3 > "$LOG_DIR/selftest_round${round}.log" 2>&1
    
    PASSED=$(grep "Passed:" "$LOG_DIR/selftest_round${round}.log" 2>/dev/null | grep -o '[0-9]*' | head -1)
    FAILED=$(grep "Failed:" "$LOG_DIR/selftest_round${round}.log" 2>/dev/null | grep -o '[0-9]*' | head -1)
    
    echo "[Loop] Self-test result: ${PASSED:-0} passed, ${FAILED:-0} failed"
    
    # ── Step 2: Check if all green ────────────────────────
    if [ "${FAILED:-0}" = "0" ]; then
        echo "[Loop] ✅ All scenarios passed! System healthy."
        echo "[Loop] Round $round complete — no fixes needed."
        continue
    fi
    
    # ── Step 3: Found issues → dispatch fix ───────────────
    echo "[Loop] ❌ Issues found. Dispatching fix via evolve_auto..."
    
    # Extract issue description from self_test log
    ISSUE=$(grep "Round.*FAILED" "$LOG_DIR/selftest_round${round}.log" 2>/dev/null | head -1 | sed 's/.*FAILED: //')
    
    if [ -z "$ISSUE" ]; then
        ISSUE="Fix failing self-test scenario"
    fi
    
    echo "[Loop] Issue: $ISSUE"
    echo "[Loop] Dispatching to A→B (DeepSeek fast)..."
    
    NO_PR=1 NO_WATCHDOG=1 MODEL=deepseek-v4-flash PROVIDER=opencode \
    bash scripts/evolve_auto.sh "Fix: $ISSUE" \
        > "$LOG_DIR/fix_round${round}.log" 2>&1
    
    FIX_RESULT=$(grep "Self-Evolution Complete" "$LOG_DIR/fix_round${round}.log" 2>/dev/null)
    
    if [ -n "$FIX_RESULT" ]; then
        echo "[Loop] ✅ Fix applied. Will re-test in next round."
    else
        echo "[Loop] ⚠️ Fix may have failed. Will retry in next round."
    fi
    
    echo "[Loop] Round $round complete."
done

echo ""
echo "=========================================="
echo "  Loop Complete ($MAX_ROUNDS rounds)"
echo "=========================================="
echo "Logs: $LOG_DIR/"
ls -la "$LOG_DIR/"
