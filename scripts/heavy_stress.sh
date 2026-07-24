#!/usr/bin/env bash
# heavy_stress.sh — 重度并发压力测试（10 角色 × 15min+ 并发）
#
# 一个 serve 常驻，10 个角色同时发复杂任务。
# 每个角色跑 10-15 轮工具调用（读代码→改代码→编译→测试）。
# 采集：成功率、错误、session 状态。
#
# Usage: bash scripts/heavy_stress.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
LOG_DIR="/tmp/ion_heavy"
mkdir -p "$LOG_DIR"

# Create test workspace for each role
setup_workspace() {
    local role_id=$1
    local dir="/tmp/ion_heavy_role${role_id}"
    rm -rf "$dir"
    mkdir -p "$dir/src"
    # Copy some real files for agents to work with
    cp "$PROJECT_DIR/Cargo.toml" "$dir/" 2>/dev/null
    cat > "$dir/src/main.rs" << 'RUST'
fn main() {
    let numbers = vec![1, 2, 3, 4, 5];
    let sum: i32 = numbers.iter().sum();
    println!("Sum: {}", sum);
}
RUST
    cat > "$dir/src/utils.rs" << 'RUST'
pub fn add(a: i32, b: i32) -> i32 { a + b }
pub fn multiply(a: i32, b: i32) -> i32 { a * b }
RUST
    echo "$dir"
}

# Define 10 heavy role tasks (each takes 10+ minutes)
run_role() {
    local role_id=$1
    local role_name=$2
    local task=$3
    local model=${4:-glm-5.2}
    local provider=${5:-zai}
    local log="$LOG_DIR/role${role_id}.log"
    local dir=$(setup_workspace $role_id)

    echo "[$(date +%H:%M:%S)] Role $role_id ($role_name) started" > "$log"

    cd "$dir"
    echo "$task" | timeout 1200 "$ION" \
        --agent developer \
        --provider "$provider" \
        --model "$model" \
        --max-turns 30 \
        >> "$log" 2>&1

    local rc=$?
    local lines=$(wc -l < "$log")
    local errors=$(grep -ci "error\|panic\|fail\|crash" "$log" 2>/dev/null || echo 0)
    local has_response=$(grep -c "tokio\|serde\|fn \|pub \|test\|cargo\|module\|struct" "$log" 2>/dev/null || echo 0)

    echo "[$(date +%H:%M:%S)] Role $role_id FINISHED: rc=$rc lines=$lines errors=$errors responses=$has_response" >> "$log"

    # Record result
    if [ "$has_response" -gt 0 ] && [ "$rc" -eq 0 ]; then
        echo "✅ Role $role_id ($role_name): PASS (lines=$lines responses=$has_response)"
    elif [ "$has_response" -gt 0 ]; then
        echo "⚠️ Role $role_id ($role_name): PARTIAL (rc=$rc responses=$has_response)"
    else
        echo "❌ Role $role_id ($role_name): FAIL (rc=$rc)"
    fi
}

echo "=========================================="
echo "  ION 重度并发压力测试"
echo "=========================================="
echo "  10 角色 × 复杂任务 × 并发"
echo "  预计耗时: 15-20 分钟"
echo "=========================================="
echo ""

# Start serve
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
sleep 1
"$ION" serve > "$LOG_DIR/serve.log" 2>&1 &
sleep 5

# Verify serve is up
HEALTH=$("$ION" rpc --method health --params '{}' 2>/dev/null)
if ! echo "$HEALTH" | grep -q '"ok"'; then
    echo "❌ serve failed to start"
    exit 1
fi
echo "✅ serve running"
echo ""

# ── Launch all 10 roles concurrently ──
echo "Launching 10 roles concurrently..."

# Role 1: Full code review (GLM-5.2, 15+ min)
run_role 1 "代码审查员" \
    "Read src/main.rs and src/utils.rs. Review both files for code quality issues: naming conventions, error handling, missing tests, performance. For each issue found, fix it using the edit tool. After fixing, run: cargo check. Report what you fixed." &
P1=$!

# Role 2: Add new features (GLM-5.2, 15+ min)
run_role 2 "功能开发者" \
    "Read src/utils.rs. Add three new functions: subtract(a,b), divide(a,b) with division by zero check, and power(a,b). Add unit tests for each. Run cargo check after adding. Report the code." &
P2=$!

# Role 3: Refactor (DeepSeek fast, 10+ min)
run_role 3 "重构工程师" \
    "Read src/main.rs. Refactor the main function: extract the sum calculation into a separate function called calculate_sum. Add doc comments. Run cargo check. Report changes." \
    deepseek-v4-flash opencode &
P3=$!

# Role 4: Test writer (DeepSeek fast, 10+ min)
run_role 4 "测试工程师" \
    "Read src/utils.rs. Write comprehensive tests for add and multiply functions in a new file src/utils_test.rs. Include edge cases: negative numbers, zero, large numbers. Run cargo check." \
    deepseek-v4-flash opencode &
P4=$!

# Role 5: Documentation (GLM-5.2, 10+ min)
run_role 5 "文档撰写者" \
    "Read src/main.rs and src/utils.rs. Create a README.md with: project description, usage examples, function reference, build instructions. Write comprehensive documentation." &
P5=$!

# Role 6: Bug hunter (GLM-5.2, 15+ min)
run_role 6 "Bug猎手" \
    "Read src/main.rs and src/utils.rs carefully. Look for potential bugs: integer overflow, unused variables, missing error handling. For each bug found, write a comment explaining it. Create a file BUGS.md listing all findings." &
P6=$!

# Role 7: Performance analyst (DeepSeek fast, 10+ min)
run_role 7 "性能分析师" \
    "Read src/main.rs. Analyze performance: memory usage, time complexity, potential bottlenecks. Write optimization suggestions in a file PERF.md. Suggest at least 3 improvements." \
    deepseek-v4-flash opencode &
P7=$!

# Role 8: Security audit (GLM-5.2, 10+ min)
run_role 8 "安全审计员" \
    "Read src/main.rs and src/utils.rs. Check for security issues: input validation, unsafe operations, resource leaks. Create SECURITY.md with findings and recommendations." &
P8=$!

# Role 9: API designer (GLM-5.2, 15+ min)
run_role 9 "API设计师" \
    "Read src/utils.rs. Design a public API module: create src/api.rs that re-exports utils functions with better names and adds convenience methods. Add doc comments. Run cargo check." &
P9=$!

# Role 10: CI/CD setup (DeepSeek fast, 10+ min)
run_role 10 "CICD工程师" \
    "Read Cargo.toml. Create a GitHub Actions workflow file .github/workflows/ci.yml that: builds the project, runs cargo test, runs cargo clippy, runs cargo fmt --check. Use Rust stable toolchain." \
    deepseek-v4-flash opencode &
P10=$!

echo ""
echo "All 10 roles launched. Waiting for completion..."
echo "Monitor: tail -f $LOG_DIR/role*.log"
echo ""

# Wait for all (max 20 minutes)
WAIT_START=$(date +%s)
for pid in $P1 $P2 $P3 $P4 $P5 $P6 $P7 $P8 $P9 $P10; do
    wait $pid 2>/dev/null
done
WAIT_END=$(date +%s)
TOTAL_TIME=$((WAIT_END - WAIT_START))

echo ""
echo "=========================================="
echo "  重度并发压力测试结果"
echo "=========================================="
echo "  总耗时: ${TOTAL_TIME}s ($(( TOTAL_TIME / 60 ))min $(( TOTAL_TIME % 60 ))s)"
echo ""

# Collect results
PASS_COUNT=0
PARTIAL_COUNT=0
FAIL_COUNT=0
for i in $(seq 1 10); do
    LOG="$LOG_DIR/role${i}.log"
    LINES=$(wc -l < "$LOG" 2>/dev/null || echo 0)
    RESPONSES=$(grep -c "tokio\|serde\|fn \|pub \|test\|cargo\|module\|struct\|Read\|Write\|Edit" "$LOG" 2>/dev/null || echo 0)
    ERRORS=$(grep -ci "error\|panic\|fail\|crash" "$LOG" 2>/dev/null || echo 0)

    if [ "$RESPONSES" -gt 5 ]; then
        STATUS="✅ PASS"
        PASS_COUNT=$((PASS_COUNT + 1))
    elif [ "$RESPONSES" -gt 0 ]; then
        STATUS="⚠️ PARTIAL"
        PARTIAL_COUNT=$((PARTIAL_COUNT + 1))
    else
        STATUS="❌ FAIL"
        FAIL_COUNT=$((FAIL_COUNT + 1))
    fi

    echo "  Role $i: $STATUS (lines=$LINES responses=$RESPONSES errors=$ERRORS)"
done

echo ""
echo "  通过: $PASS_COUNT / 10"
echo "  部分: $PARTIAL_COUNT / 10"
echo "  失败: $FAIL_COUNT / 10"
echo ""
echo "  Logs: $LOG_DIR/"
echo "=========================================="

# Cleanup serve
lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
