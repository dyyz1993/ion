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

    # Create more files for deeper tasks
    cat > "$dir/src/models.rs" << 'RUST'
pub struct User { pub id: u32, pub name: String, pub email: String }
pub struct Product { pub id: u32, pub name: String, pub price: f64 }
pub struct Order { pub id: u32, pub user_id: u32, pub product_id: u32, pub quantity: u32 }
RUST

    cat > "$dir/src/database.rs" << 'RUST'
use std::collections::HashMap;
pub struct Database { users: HashMap<u32, String>, products: HashMap<u32, f64> }
impl Database {
    pub fn new() -> Self { Self { users: HashMap::new(), products: HashMap::new() } }
    pub fn add_user(&mut self, id: u32, name: String) { self.users.insert(id, name); }
    pub fn add_product(&mut self, id: u32, price: f64) { self.products.insert(id, price); }
}
RUST

    # Copy real project files for deeper analysis
    cp "$PROJECT_DIR/src/lib.rs" "$dir/src/project_lib.rs" 2>/dev/null
    cp "$PROJECT_DIR/src/agent/agent_loop.rs" "$dir/src/agent_loop_ref.rs" 2>/dev/null
    cp "$PROJECT_DIR/Cargo.toml" "$dir/Cargo.toml" 2>/dev/null

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

# Role 1: Full code review + fix + test (GLM-5.2, 15+ min)
run_role 1 "代码审查员" \
    "You are doing a comprehensive code review. Read ALL these files: src/main.rs, src/utils.rs, src/models.rs, src/database.rs, src/project_lib.rs. For EACH file: 1) Identify at least 3 code quality issues (naming, error handling, missing docs, performance). 2) Fix each issue using the edit tool. 3) After ALL fixes, run cargo check. 4) Write a REVIEW.md summarizing all changes. Be thorough — do not skip any file." &
P1=$!

# Role 2: Build complete feature module (GLM-5.2, 15+ min)
run_role 2 "功能开发者" \
    "Build a complete user management module. Read src/models.rs and src/database.rs first. Then create src/user_service.rs with: UserService struct, create_user, get_user, update_user, delete_user, list_users methods. Add input validation for each. Create src/user_service_tests.rs with at least 10 test cases covering normal + edge cases. Run cargo check. Fix any errors." &
P2=$!

# Role 3: Full refactor + migration (DeepSeek, 15+ min)
run_role 3 "重构工程师" \
    "Read src/main.rs, src/utils.rs, src/models.rs, src/database.rs. Refactor ALL files: 1) Extract magic numbers into constants. 2) Add Result return types where errors can occur. 3) Add doc comments to every public function. 4) Rename any unclear variable names. 5) Run cargo check after EACH file. 6) Create REFACTOR.md documenting all changes." \
    deepseek-v4-flash opencode &
P3=$!

# Role 4: Comprehensive test suite (DeepSeek, 15+ min)
run_role 4 "测试工程师" \
    "Read src/utils.rs, src/models.rs, src/database.rs. Create a comprehensive test suite: src/tests/utils_test.rs (10+ tests for utils), src/tests/models_test.rs (8+ tests for models), src/tests/database_test.rs (10+ tests for database CRUD). Cover: normal cases, edge cases (empty, negative, overflow), error cases. Run cargo check. Document test coverage in TESTS.md." \
    deepseek-v4-flash opencode &
P4=$!

# Role 5: Full project documentation (GLM-5.2, 15+ min)
run_role 5 "文档撰写者" \
    "Read ALL source files: src/main.rs, src/utils.rs, src/models.rs, src/database.rs, src/project_lib.rs. Create comprehensive documentation: README.md (project overview, getting started, architecture), docs/API.md (every public function with examples), docs/ARCHITECTURE.md (module relationships, data flow), docs/CONTRIBUTING.md (coding standards, PR process). Be detailed — read every file before writing." &
P5=$!

# Role 6: Deep bug analysis + fixes (GLM-5.2, 15+ min)
run_role 6 "Bug猎手" \
    "Read src/main.rs, src/utils.rs, src/models.rs, src/database.rs, src/project_lib.rs. Perform deep analysis: 1) Integer overflow risks (add/multiply with i32). 2) Memory issues (HashMap growth, cloning). 3) Thread safety (if used concurrently). 4) Logic errors. 5) Missing input validation. For EACH bug found: write a test that reproduces it, then fix it. Create BUGS.md with full report. Run cargo check." &
P6=$!

# Role 7: Performance optimization (DeepSeek, 15+ min)
run_role 7 "性能分析师" \
    "Read src/main.rs, src/utils.rs, src/models.rs, src/database.rs. Analyze performance deeply: 1) Time complexity of each function. 2) Memory allocations. 3) Clone vs borrow. 4) HashMap vs Vec for small datasets. Write src/perf_bench.rs with benchmark code. Create PERF.md with: current analysis, 5+ optimization suggestions, estimated impact. Implement the top 2 optimizations. Run cargo check." \
    deepseek-v4-flash opencode &
P7=$!

# Role 8: Security audit + hardening (GLM-5.2, 15+ min)
run_role 8 "安全审计员" \
    "Read src/main.rs, src/utils.rs, src/models.rs, src/database.rs. Perform thorough security audit: 1) Input validation gaps. 2) Injection risks. 3) Unsafe code. 4) Resource exhaustion (unbounded HashMap). 5) Integer overflow as security issue. 6) Error message information leakage. For each finding: rate severity (Critical/High/Medium/Low), write a fix, apply it. Create SECURITY.md with full report. Run cargo check." &
P8=$!

# Role 9: API design + trait implementation (GLM-5.2, 15+ min)
run_role 9 "API设计师" \
    "Read src/models.rs, src/database.rs, src/utils.rs. Design a clean public API: 1) Create src/traits.rs with traits: Repository, Validatable, Serializable. 2) Implement traits for User, Product, Order, Database. 3) Create src/api.rs with builder pattern for queries. 4) Add doc comments with examples. 5) Run cargo check. 6) Create API_CHANGES.md documenting the design decisions." &
P9=$!

# Role 10: CI/CD + DevOps setup (DeepSeek, 15+ min)
run_role 10 "CICD工程师" \
    "Read Cargo.toml and ALL source files. Create complete CI/CD: .github/workflows/ci.yml (build, test, clippy, fmt, security audit), .github/workflows/release.yml (tagged release), Dockerfile (multi-stage build), docker-compose.yml, .gitignore updates, scripts/test.sh (local CI runner), scripts/lint.sh. Make ci.yml run tests for each module separately. Run cargo check to validate project compiles." \
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
