#!/usr/bin/env bash
# evolve_tests.sh — 批量补单测（B 自动给每个 0 测试文件加测试）
#
# 找所有 0 测试的 .rs 文件 → 让 B 逐个补测试 → cargo test 验证 → commit
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"

# 找 0 测试的大文件（>100 行）
echo "=== 扫描需要补测试的文件 ==="
TASKS=()
for f in $(find src -name "*.rs" -not -path "*/tests/*" -not -path "*/bin/*"); do
    LINES=$(wc -l < "$f")
    TESTS=$(grep -c "#\[test\]\|#\[tokio::test\]" "$f" 2>/dev/null)
    if [ "$LINES" -gt 100 ] && [ "$TESTS" -eq 0 ]; then
        TASKS+=("$f")
        echo "  $f ($LINES lines, 0 tests)"
    fi
done

NUM=${#TASKS[@]}
echo ""
echo "=========================================="
echo "  Batch Test Generation"
echo "  Files: $NUM"
echo "  Model: $MODEL"
echo "=========================================="
echo ""

SUCCESS=0
FAIL=0

for f in "${TASKS[@]}"; do
    echo "──────────────────────────────────────────"
    echo "Processing: $f"
    echo "──────────────────────────────────────────"

    # B 补测试
    RESULT=$(echo "Task: Add unit tests to $f.

Read the file first. Add a #[cfg(test)] mod tests block at the END of the file.

Rules:
1. Only test PURE functions (struct construction, serialization, enum Display, helper utilities)
2. Do NOT test async functions or functions needing tokio/network/subprocess
3. Write 5-10 tests minimum
4. ALL comments in ENGLISH
5. Use edit tool to append at end of file
6. Do NOT modify existing code
7. After editing, verify: cargo test --lib $(basename $f .rs) 2>&1 | tail -3

If the file has very few testable functions (mostly async/trait impls), add at least
3 tests for whatever is testable (struct construction, default values, Display traits)." \
        | timeout 180 ./target/debug/ion --agent build --model "$MODEL" --provider "$PROVIDER" 2>&1 | tail -5)

    echo "$RESULT"

    # 验证编译
    BUILD=$(cargo build --bin ion 2>&1 | tail -1)
    if echo "$BUILD" | grep -q "Finished"; then
        # 跑测试
        MODULE=$(basename "$f" .rs)
        TEST_OUT=$(cargo test --lib "$MODULE" 2>&1)
        if echo "$TEST_OUT" | grep -q "test result: ok"; then
            PASSED=$(echo "$TEST_OUT" | grep -oE "[0-9]+ passed" | head -1)
            echo "  ✅ $MODULE tests: $PASSED"
            git add "$f"
            git commit -m "test($MODULE): add unit tests (was 0)" 2>&1 | head -1
            SUCCESS=$((SUCCESS + 1))
        else
            echo "  ❌ $MODULE tests failed"
            git checkout -- "$f" 2>/dev/null
            FAIL=$((FAIL + 1))
        fi
    else
        echo "  ❌ Build failed, reverting"
        git checkout -- "$f" 2>/dev/null
        FAIL=$((FAIL + 1))
    fi
    echo ""
done

# Push
git push origin master 2>&1 | tail -2

echo ""
echo "=========================================="
echo "  Batch Complete"
echo "=========================================="
echo "  Success: $SUCCESS / $((SUCCESS + FAIL))"
echo "=========================================="