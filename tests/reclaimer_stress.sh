#!/usr/bin/env bash
# reclaimer_stress.sh — Context Reclaimer 压力测试
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION="$PROJECT_DIR/target/debug/ion"
FAST_MODEL="deepseek-v4-flash"
FAST_PROVIDER="opencode"
MAX_MODEL="glm-5.2"
MAX_PROVIDER="zai"

TEST_DIR="/tmp/reclaimer_test"
rm -rf "$TEST_DIR"
mkdir -p "$TEST_DIR/src"

echo "============================================"
echo "  Context Reclaimer 压力测试"
echo "============================================"
echo ""

# Generate large files to create heavy context
for i in 1 2 3 4 5; do
    python3 -c "
lines = [f'// File {i} line {j}: pub fn func_{i}_{j}() -> u32 {{ {j} }}' for j in range(200)]
with open('$TEST_DIR/src/file_$i.rs', 'w') as f:
    f.write('\n'.join(lines))
" 2>/dev/null
done
echo "Created 5 files × 200 lines each (~4KB each)"

echo ""
echo "============================================"
echo "  Test 1: Fast model — read ALL files (generate context)"
echo "============================================"

RESULT1=$(cd "$TEST_DIR" && echo "Read src/file_1.rs, then src/file_2.rs, then src/file_3.rs, then src/file_4.rs, then src/file_5.rs. For each file, report the function count." | timeout 120 "$ION" \
    --agent developer \
    --provider "$FAST_PROVIDER" \
    --model "$FAST_MODEL" \
    --max-turns 15 2>&1)

echo "$RESULT1" | grep -v "TRACE\|wasmtime\|cranelift\|BlockLow\|lowering\|emit:\|iter:\|WARN.*wasm" | tail -10

echo ""
echo "--- Reclaimer activity ---"
echo "$RESULT1" | grep "\[reclaimer\]" | head -5

echo ""
echo "============================================"
echo "  Test 2: Max model — read + edit + bash cycle"
echo "============================================"

RESULT2=$(cd "$TEST_DIR" && echo "Read src/file_1.rs. Then edit it to add a new function at the end called test_reclaimer. Then run: wc -l src/file_1.rs. Then read src/file_1.rs again to verify the edit. Report what changed." | timeout 180 "$ION" \
    --agent developer \
    --provider "$MAX_PROVIDER" \
    --model "$MAX_MODEL" \
    --max-turns 15 2>&1)

echo "$RESULT2" | grep -v "TRACE\|wasmtime\|cranelift\|BlockLow\|lowering\|emit:\|iter:\|WARN.*wasm" | tail -10

echo ""
echo "--- Reclaimer activity ---"
echo "$RESULT2" | grep "\[reclaimer\]" | head -5

echo ""
echo "============================================"
echo "  Test 3: Fast model — heavy bash output"
echo "============================================"

RESULT3=$(cd "$TEST_DIR" && echo "Run: cat src/file_1.rs src/file_2.rs src/file_3.rs src/file_4.rs src/file_5.rs | head -500. Then run: grep -c 'func' src/*.rs. Then run: ls -la src/. Report all results." | timeout 120 "$ION" \
    --agent developer \
    --provider "$FAST_PROVIDER" \
    --model "$FAST_MODEL" \
    --max-turns 15 2>&1)

echo "$RESULT3" | grep -v "TRACE\|wasmtime\|cranelift\|BlockLow\|lowering\|emit:\|iter:\|WARN.*wasm" | tail -10

echo ""
echo "--- Reclaimer activity ---"
echo "$RESULT3" | grep "\[reclaimer\]" | head -5

echo ""
echo "============================================"
echo "  Summary"
echo "============================================"
echo ""
echo "Test 1 (fast, read-heavy): $(echo "$RESULT1" | grep -c '\[reclaimer\]') reclaim events"
echo "Test 2 (max, read+edit):   $(echo "$RESULT2" | grep -c '\[reclaimer\]') reclaim events"
echo "Test 3 (fast, bash-heavy): $(echo "$RESULT3" | grep -c '\[reclaimer\]') reclaim events"
echo ""

for label_result in "Test 1:$RESULT1" "Test 2:$RESULT2" "Test 3:$RESULT3"; do
    label="${label_result%%:*}"
    data="${label_result#*:}"
    savings=$(echo "$data" | grep "\[reclaimer\]" | grep -o "[0-9]* → [0-9]* tokens (saved [0-9]*)" | head -1)
    if [ -n "$savings" ]; then
        echo "$label token savings: $savings"
    fi
done

echo ""
echo "Note: If 0 reclaim events, context was under 60% threshold."
echo "Thinking blocks are always stripped (Phase 1) regardless of threshold."
echo "Check with: RUST_LOG=info to see [reclaimer] logs."

echo ""
rm -rf "$TEST_DIR"
echo "Cleanup done ✅"
