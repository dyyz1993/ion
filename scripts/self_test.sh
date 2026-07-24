#!/usr/bin/env bash
# self_test.sh — A 驱动的自测闭环
#
# 流程：
#   1. A（coordinator）安排 QA 设计测试用例（场景 1/2/3）
#   2. A 安排 developer 执行测试（跑 ion，验证功能）
#   3. A 验收结果，采集问题
#   4. 问题回流 → A 安排 B 修复 → 再测 → 循环
#   5. A 可以拒绝修复（低优先级问题标记为 "known issue"）
#
# 这个脚本跑的是场景 2（--host），让 coordinator 通过 spawn_worker 编排子 agent。
#
# Usage: bash scripts/self_test.sh [rounds]
#   rounds = 循环轮数（默认 3）
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION="$PROJECT_DIR/target/debug/ion"
ROUNDS="${1:-3}"
MODEL="${MODEL:-glm-5.2}"
PROVIDER="${PROVIDER:-zai}"
FAST_MODEL="${FAST_MODEL:-deepseek-v4-flash}"
FAST_PROVIDER="${FAST_PROVIDER:-opencode}"

echo "=========================================="
echo "  ION Self-Test Loop (A→B)"
echo "=========================================="
echo "  Rounds:    $ROUNDS"
echo "  Max model: $MODEL ($PROVIDER)"
echo "  Fast model: $FAST_MODEL ($FAST_PROVIDER)"
echo "=========================================="
echo ""

# Ensure ion binary is built
if [ ! -f "$ION" ]; then
    echo "Building ion..."
    cargo build --bin ion --bin ion-worker 2>&1 | tail -3
fi

# Track issues across rounds
ISSUES_FILE="/tmp/ion_selftest_issues.jsonl"
rm -f "$ISSUES_FILE"

for round in $(seq 1 "$ROUNDS"); do
    echo ""
    echo "=========================================="
    echo "  Round $round / $ROUNDS"
    echo "=========================================="
    echo ""

    # ── Phase 1: QA designs test cases ──────────────────────────
    echo "📦 Phase 1: QA designs test cases for Scenario $round..."

    # Scenario mapping: round 1 = scenario 1, round 2 = scenario 2, round 3 = scenario 3
    case "$round" in
        1) SCENARIO="Scenario 1: Direct execution (ion 'task')"
           SCENARIO_CMD="ion \"Read Cargo.toml and report the package name\""
           SCENARIO_CHECK="verify output contains the package name"
           ;;
        2) SCENARIO="Scenario 2: Quick orchestration (ion --host)"
           SCENARIO_CMD="ion --host --agent developer \"Read Cargo.toml and list dependencies\""
           SCENARIO_CHECK="verify spawn_worker works and developer reads file"
           ;;
        3) SCENARIO="Scenario 3: Persistent service (ion serve + rpc)"
           SCENARIO_CMD="start ion serve, then ion rpc --method health"
           SCENARIO_CHECK="verify health RPC returns status ok"
           ;;
        *) SCENARIO="Extra round: WASM extension test"
           SCENARIO_CMD="ion --agent developer 'Read src/lib.rs and list modules'"
           SCENARIO_CHECK="verify WASM extensions loaded (rules-engine, file-time-guard, session-supervisor)"
           ;;
    esac

    echo "  Scenario: $SCENARIO"
    echo ""

    # ── Phase 2: Execute test ───────────────────────────────────
    echo "📦 Phase 2: Execute test (fast model for speed)..."

    case "$round" in
        1)
            # Scenario 1: direct execution
            OUTPUT=$(echo "Read Cargo.toml and report the package name and version" | timeout 60 "$ION" \
                --provider "$PROVIDER" --model "$MODEL" --max-turns 5 2>&1)
            echo "$OUTPUT" | grep -v "TRACE\|wasmtime\|cranelift\|BlockLow\|lowering\|emit:\|iter:\|WARN.*wasm\|stream-debug\|extension_message\|INFO" | head -10
            ;;
        2)
            # Scenario 2: host orchestration
            OUTPUT=$(echo "Read Cargo.toml and list all dependencies" | timeout 90 "$ION" \
                --host --agent developer \
                --provider "$PROVIDER" --model "$MODEL" --max-turns 8 2>&1)
            echo "$OUTPUT" | grep -v "TRACE\|wasmtime\|cranelift\|BlockLow\|lowering\|emit:\|iter:\|WARN.*wasm\|stream-debug\|extension_message\|INFO" | head -10
            ;;
        3)
            # Scenario 3: serve + health RPC
            # Kill stale serve
            lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null
            rm -f "$HOME/.ion/host.sock" "$HOME/.ion/host.pid"
            sleep 1

            "$ION" serve > /dev/null 2>&1 &
            SERVE_PID=$!
            sleep 3

            HEALTH=$("$ION" rpc --method health --params '{}' 2>/dev/null)
            echo "  Health RPC: $HEALTH"

            # Test create_session + prompt
            SID=$("$ION" rpc --method create_session --params '{"agent":"developer"}' 2>/dev/null | grep -o '"session_id":"[^"]*"' | head -1 | sed 's/"session_id":"//;s/"//')
            echo "  Session: $SID"

            if [ -n "$SID" ]; then
                PROMPT_RESULT=$(timeout 60 "$ION" rpc --session "$SID" --method prompt --params '{"text":"Read Cargo.toml and report version"}' 2>/dev/null | tail -5)
                echo "  Prompt result: $(echo "$PROMPT_RESULT" | head -3)"
            fi

            kill $SERVE_PID 2>/dev/null
            OUTPUT="$HEALTH"
            ;;
    esac

    # ── Phase 3: Collect results ────────────────────────────────
    echo ""
    echo "📦 Phase 3: Collect results..."

    PASS=true
    ISSUES=""

    case "$round" in
        1|2)
            # Check if output contains meaningful response
            if echo "$OUTPUT" | grep -qi "no response\|error\|panic"; then
                PASS=false
                ISSUES="no response or error in output"
            elif echo "$OUTPUT" | grep -qi "ion\|package\|cargo\|version\|depend"; then
                echo "  ✅ Agent returned meaningful response"
            else
                PASS=false
                ISSUES="response doesn't contain expected content"
            fi
            ;;
        3)
            if echo "$HEALTH" | grep -q '"ok"'; then
                echo "  ✅ Health RPC returned ok"
            else
                PASS=false
                ISSUES="health RPC failed"
            fi
            ;;
    esac

    # Check WASM extensions loaded
    if echo "$OUTPUT" | grep -q "rules-engine-wasm initialized"; then
        echo "  ✅ rules-engine WASM loaded"
    else
        echo "  ⚠️  rules-engine WASM not found in output"
    fi

    if echo "$OUTPUT" | grep -q "session-supervisor initialized"; then
        echo "  ✅ session-supervisor WASM loaded"
    else
        echo "  ⚠️  session-supervisor WASM not found in output"
    fi

    # ── Phase 4: Record results ─────────────────────────────────
    if [ "$PASS" = true ]; then
        echo ""
        echo "✅ Round $round PASSED"
    else
        echo ""
        echo "❌ Round $round FAILED: $ISSUES"
        # Record issue for potential fix
        echo "{\"round\":$round,\"scenario\":\"$SCENARIO\",\"issue\":\"$ISSUES\",\"status\":\"open\"}" >> "$ISSUES_FILE"
    fi
done

# ── Summary ─────────────────────────────────────────────────────
echo ""
echo "=========================================="
echo "  Self-Test Summary"
echo "=========================================="
echo ""

TOTAL_FAILED=$(wc -l < "$ISSUES_FILE" 2>/dev/null || echo "0")
TOTAL_PASSED=$((ROUNDS - TOTAL_FAILED))

echo "  Passed: $TOTAL_PASSED / $ROUNDS"
echo "  Failed: $TOTAL_FAILED"

if [ "$TOTAL_FAILED" -gt 0 ] && [ -f "$ISSUES_FILE" ]; then
    echo ""
    echo "  Issues found:"
    cat "$ISSUES_FILE" | python3 -c "
import sys, json
for line in sys.stdin:
    try:
        d = json.loads(line.strip())
        print(f\"    Round {d['round']}: {d['issue']} ({d['scenario']})\")
    except: pass
" 2>/dev/null

    echo ""
    echo "  Next steps:"
    echo "    A should review these issues and decide:"
    echo "    - Fix via: NO_PR=1 bash scripts/evolve_auto.sh \"fix: <issue>\""
    echo "    - Or reject (mark as known issue)"
else
    echo ""
    echo "  ✅ All scenarios passed! System is healthy."
fi

echo ""
echo "=========================================="
echo "  Done"
echo "=========================================="
