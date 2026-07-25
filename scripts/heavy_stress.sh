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
    mkdir -p "$dir/src" "$dir/src/tests" "$dir/docs" "$dir/.github/workflows"
    # Copy some real files for agents to work with
    cp "$PROJECT_DIR/Cargo.toml" "$dir/" 2>/dev/null

    # Initialize git repo (agents need git for commits/diffs)
    cd "$dir"
    git init -q 2>/dev/null
    git config user.email "test@test.com" 2>/dev/null
    git config user.name "Test Agent" 2>/dev/null
    git add -A 2>/dev/null
    git commit -q -m "initial" 2>/dev/null
    cd "$PROJECT_DIR"
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
    "You are a senior code reviewer. Your job is NOT done until you have checked every single file thoroughly. Step 1: Read src/main.rs. List ALL issues. Step 2: Read src/utils.rs. List ALL issues. Step 3: Read src/models.rs. List ALL issues. Step 4: Read src/database.rs. List ALL issues. Step 5: Read src/project_lib.rs (this is a large file, read it fully). List ALL issues. Step 6: Fix every issue you found, one by one. After each fix, run cargo check. Step 7: Write REVIEW.md with full report. DO NOT stop early. DO NOT summarize without reading every file first. If you think you are done, re-read each file to verify your fixes." &
P1=$!

# Role 2: Build complete feature module (GLM-5.2, 15+ min)
run_role 2 "功能开发者" \
    "You are building a production-grade user management system. DO NOT rush. Step 1: Read src/models.rs and src/database.rs to understand existing types. Step 2: Create src/user_service.rs — implement UserService with: new(), create_user (with email validation), get_user (with not-found error), update_user (partial updates), delete_user, list_users (with pagination). Each method must return Result<T, UserServiceError>. Step 3: Create src/user_service_error.rs with a proper error enum (NotFound, AlreadyExists, InvalidEmail, InvalidId, DatabaseError). Step 4: Create src/user_service_tests.rs with 15+ tests. Step 5: Run cargo check. Fix ALL errors. Step 6: Create FEATURES.md documenting the API." &
P2=$!

# Role 3: Full refactor + migration (DeepSeek, 15+ min)
run_role 3 "重构工程师" \
    "You are doing a complete codebase refactor. DO NOT stop after one file. Process: 1) Read src/main.rs — refactor: extract constants, add error handling, add docs. Run cargo check. 2) Read src/utils.rs — refactor: use Result types, add docs, improve naming. Run cargo check. 3) Read src/models.rs — refactor: add Display trait, add validation methods. Run cargo check. 4) Read src/database.rs — refactor: add error handling, add iter methods. Run cargo check. 5) Read src/project_lib.rs — this is large, read it in full, refactor what you can. Run cargo check. 6) Write REFACTOR.md listing every change made. DO NOT skip any file." \
    deepseek-v4-flash opencode &
P3=$!

# Role 4: Comprehensive test suite (DeepSeek, 15+ min)
run_role 4 "测试工程师" \
    "You must create an exhaustive test suite. DO NOT write fewer than 30 tests total. Step 1: Read src/utils.rs. Write src/tests/utils_test.rs with 10 tests: add normal, add overflow, add negative, multiply normal, multiply overflow, multiply by zero, multiply negative, edge cases. Step 2: Read src/models.rs. Write src/tests/models_test.rs with 10 tests: User creation, Product creation, Order creation, field validation, serialization. Step 3: Read src/database.rs. Write src/tests/database_test.rs with 10 tests: add_user, add_product, get_user, get_product, delete, concurrent access, large dataset. Step 4: Run cargo check after EACH test file. Step 5: Write TESTS.md with coverage report." \
    deepseek-v4-flash opencode &
P4=$!

# Role 5: Full project documentation (GLM-5.2, 15+ min)
run_role 5 "文档撰写者" \
    "You must create comprehensive documentation. DO NOT write docs without reading the source first. Step 1: Read src/main.rs, src/utils.rs, src/models.rs, src/database.rs, src/project_lib.rs one by one. Step 2: Write README.md — project overview, installation, usage, examples. Step 3: Write docs/API.md — document EVERY public function with signature, description, parameters, return value, example. Step 4: Write docs/ARCHITECTURE.md — module diagram, data flow, dependency graph. Step 5: Write docs/CONTRIBUTING.md — code style, testing requirements, PR process. Step 6: Write docs/CHANGELOG.md — current features. DO NOT summarize — be detailed." &
P5=$!

# Role 6: Deep bug analysis + fixes (GLM-5.2, 15+ min)
run_role 6 "Bug猎手" \
    "You are a meticulous bug hunter. DO NOT stop after finding 2-3 bugs. Step 1: Read src/main.rs. List every potential bug: integer overflow, panic risks, logic errors. Step 2: Read src/utils.rs. Same analysis. Step 3: Read src/models.rs. Same analysis. Step 4: Read src/database.rs. Same analysis: HashMap unbounded growth, missing error handling. Step 5: Read src/project_lib.rs. Deep analysis of this large file. Step 6: For EACH bug found, write a reproduction test in src/bug_tests.rs. Step 7: Fix each bug. Run cargo check after each fix. Step 8: Write BUGS.md with full report including severity ratings." &
P6=$!

# Role 7: Performance optimization (DeepSeek, 15+ min)
run_role 7 "性能分析师" \
    "You are a performance engineer. DO NOT write suggestions without measuring. Step 1: Read src/main.rs. Analyze: time complexity, space complexity, allocation count. Step 2: Read src/utils.rs. Same analysis. Step 3: Read src/models.rs. Analyze struct sizes, alignment. Step 4: Read src/database.rs. Analyze HashMap vs BTreeMap, growth strategy. Step 5: Read src/project_lib.rs. This is large — identify hot paths. Step 6: Write src/perf_benchmarks.rs with criterion-like benchmarks for each function. Step 7: Implement the top 3 optimizations. Run cargo check. Step 8: Write PERF.md with before/after analysis." \
    deepseek-v4-flash opencode &
P7=$!

# Role 8: Security audit + hardening (GLM-5.2, 15+ min)
run_role 8 "安全审计员" \
    "You are a security auditor. DO NOT stop after surface-level checks. Step 1: Read src/main.rs. Check: input validation, output encoding, panic on attacker input. Step 2: Read src/utils.rs. Check: integer overflow as security issue, denial of service. Step 3: Read src/models.rs. Check: data leakage in Display/Debug, PII handling. Step 4: Read src/database.rs. Check: resource exhaustion, race conditions. Step 5: Read src/project_lib.rs. Deep security review. Step 6: For each finding, write the fix and apply it. Step 7: Create src/security_tests.rs with tests proving the fixes work. Step 8: Write SECURITY.md with full CVE-style report." &
P8=$!

# Role 9: API design + trait implementation (GLM-5.2, 15+ min)
run_role 9 "API设计师" \
    "You are designing a clean public API layer. DO NOT rush the design. Step 1: Read src/models.rs, src/database.rs, src/utils.rs to understand existing types. Step 2: Create src/traits.rs — define Repository<T>, Validatable, Serializable, Displayable traits. Step 3: Create src/api.rs — implement ApiClient with builder pattern: new(), with_database(), with_auth(), create_user(), query_products(), etc. Step 4: Create src/api_error.rs — comprehensive error types with thiserror. Step 5: Implement all traits for User, Product, Order. Step 6: Create src/api_tests.rs with 10+ integration tests. Step 7: Run cargo check after EACH new file. Step 8: Write API_DESIGN.md." &
P9=$!

# Role 10: CI/CD + DevOps setup (DeepSeek, 15+ min)
run_role 10 "CICD工程师" \
    "You are setting up complete CI/CD from scratch. DO NOT create just one file. Step 1: Read Cargo.toml to understand the project. Step 2: Create .github/workflows/ci.yml — matrix build (stable + beta), cargo build, cargo test (per module), cargo clippy -D warnings, cargo fmt --check, cargo audit. Step 3: Create .github/workflows/release.yml — tagged release, cross-compile, GitHub release creation. Step 4: Create Dockerfile — multi-stage build, minimal final image. Step 5: Create docker-compose.yml — app + redis. Step 6: Create scripts/ci_local.sh — runs all CI steps locally. Step 7: Create .gitignore. Step 8: Create Makefile with common commands. Step 9: Run cargo check to verify. DO NOT skip any file." \
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
