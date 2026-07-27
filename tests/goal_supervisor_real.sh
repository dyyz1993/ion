#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# Goal Supervisor Real LLM E2E — ION_E2E=1 触发
#
# 用真实 GLM-5.2 跑一个 goal（不带 checks，验证 B2 LLM 自动生成检测项）。
# 参考 hooks_agent_real.sh 模式。
#
# Usage: ION_E2E=1 bash tests/goal_supervisor_real.sh
# ──────────────────────────────────────────────────────────
set -o pipefail

if [ "${ION_E2E:-0}" != "1" ]; then
    echo "⏭️  Skipping goal_supervisor_real (set ION_E2E=1 to run)"
    exit 0
fi

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
cd "$PROJECT_DIR"

echo "════════════════════════════════════════════════════"
echo "  Goal Supervisor Real LLM E2E — $(date)"
echo "════════════════════════════════════════════════════"

# Build
cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# Temp workdir (separate from main repo to avoid polluting it)
WD=$(mktemp -d /tmp/goal_real_XXXXXX)
cd "$WD"
git init -q
git config user.email "goal-test@test.com"
git config user.name "Goal Test"

# Create a minimal Rust project so cargo checks work
mkdir -p src
cat > Cargo.toml << 'CARGO'
[package]
name = "goal-test"
version = "0.1.0"
edition = "2021"
[lib]
path = "src/lib.rs"
CARGO
echo "// Goal test project" > src/lib.rs
git add -A && git commit -q -m "init"

# Clean previous goal-runs for this session
rm -rf "$HOME/.ion/agent/goal-runs"

echo ""
echo "Running real LLM goal (GLM-5.2, ~2-5 min)..."

# Run with real provider. The prompt asks agent to add a function + use goal_set.
# Agent should: call goal_set (no checks → B2 auto-generates) → write code → Stop → gate checks
OUTPUT=$(ION_HOST_TIMEOUT=300 timeout 360 "$ION_BIN" --host \
    "Add a 'pub fn double(n: i32) -> i32' function to src/lib.rs. \
     First call goal_set with this objective (no checks — let the system auto-generate). \
     Then implement it. The supervisor will verify completion." \
    --workdir "$WD" 2>&1)

echo "$OUTPUT" | tail -10

# ── Assertions (loose — LLM is non-deterministic) ──
echo ""
echo "── Assertions ──"

# R1: goal-runs log exists (gate triggered)
ITERLOG=$(find "$HOME/.ion/agent/goal-runs" -name "iterations.jsonl" 2>/dev/null | head -1)
if [ -n "$ITERLOG" ] && [ -s "$ITERLOG" ]; then
    ITER_COUNT=$(wc -l < "$ITERLOG" | tr -d ' ')
    pass "R1: iterations.jsonl exists with $ITER_COUNT iterations"
else
    fail "R1: no iterations.jsonl found (gate did not trigger)"
    ITER_COUNT=0
fi

# R2: final-report exists
REPORT=$(find "$HOME/.ion/agent/goal-runs" -name "final-report.json" 2>/dev/null | head -1)
if [ -n "$REPORT" ]; then
    FSTATUS=$(python3 -c "import json; print(json.load(open('$REPORT')).get('final_status','?'))" 2>/dev/null)
    pass "R2: final-report exists (status=$FSTATUS)"
else
    fail "R2: no final-report"
    FSTATUS="unknown"
fi

# R3: iteration count reasonable (1-15, no infinite loop)
if [ "$ITER_COUNT" -ge 1 ] && [ "$ITER_COUNT" -le 15 ]; then
    pass "R3: iteration count reasonable ($ITER_COUNT)"
else
    fail "R3: iteration count out of range ($ITER_COUNT)"
fi

# R4: if complete, verify the function was actually added
if [ "$FSTATUS" = "complete" ]; then
    if grep -q "pub fn double" "$WD/src/lib.rs" 2>/dev/null; then
        pass "R4: target function added (pub fn double)"
    else
        fail "R4: function not found despite complete status"
    fi
else
    echo "  ⚠️  R4: goal did not complete ($FSTATUS) — function check skipped"
fi

# R5: U+FFFD check (no garbled chars from real LLM)
if ! grep -rq $'\xef\xbf\xbd' "$WD/src/" 2>/dev/null; then
    pass "R5: no U+FFFD garbled chars"
else
    fail "R5: U+FFFD found in source (LLM garbled UTF-8)"
fi

# Cleanup
cd "$PROJECT_DIR"
rm -rf "$WD"

echo ""
echo "════════════════════════════════════════════════════"
echo "  Goal Supervisor Real LLM E2E Summary"
echo "  Passed: $PASS  Failed: $FAIL"
echo "════════════════════════════════════════════════════"

if [ $FAIL -gt 0 ]; then exit 1; fi
exit 0
