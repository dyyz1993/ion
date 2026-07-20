#!/usr/bin/env bash
# 
# evolve-run.sh  A  B  +  CI +  + HTML
#
#  evolve.sh 
#
# : bash scripts/evolve-run.sh ""
# 
set -uo pipefail

TASK="$1"
if [ -z "$TASK" ]; then
    echo " : evolve-run.sh \"\""
    exit 1
fi

source /tmp/.evolver-state 2>/dev/null
if [ -z "$CONTAINER_NAME" ] || [ -z "$WT_DIR" ]; then
    echo " /tmp/.evolver-state  CONTAINER_NAME/WT_DIR"
    echo "    bash scripts/evolve.sh"
    exit 1
fi

CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MAX_RETRIES=3

echo ""
echo "   A  B "
echo ""
echo "  Container: $CONTAINER_NAME"
echo "  Worktree:  $WT_DIR"
echo "  Task:      $TASK"
echo ""

#  1.  B  + CI
CI_PASSED=false
for attempt in $(seq 1 $MAX_RETRIES); do
    echo ""
    echo " Step $attempt:  B  $attempt/$MAX_RETRIES"
    echo "  : $(date)"

    if [ $attempt -eq 1 ]; then
        "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer '$TASK' --provider zhipuai --model glm-5.2" 2>&1 | tail -20
    else
        "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer ' CI $CI_ERROR' --provider zhipuai --model glm-5.2" 2>&1 | tail -20
    fi
    echo "  : $(date)"

    #  CI: cargo build + cargo test --lib 
    echo ""
    echo "  [CI] cargo build..."
    BUILD_OUT=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        'cd /workspace && cargo build --bin ion 2>&1' 2>&1)
    echo "$BUILD_OUT" | tail -3
    if ! echo "$BUILD_OUT" | grep -q "Finished"; then
        CI_ERROR="build failed: $(echo "$BUILD_OUT" | grep 'error' | head -3)"
        echo "   build "
        continue
    fi
    echo "   build "

    echo "  [CI] cargo test --lib..."
    TEST_OUT=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        'cd /workspace && cargo test --lib 2>&1' 2>&1)
    echo "$TEST_OUT" | tail -5
    if ! echo "$TEST_OUT" | grep -qE "test result: ok\."; then
        CI_ERROR="test failed: $(echo "$TEST_OUT" | grep 'FAILED' | head -3)"
        echo "   test "
        continue
    fi
    TEST_COUNT=$(echo "$TEST_OUT" | grep "test result:" | grep -oE "[0-9]+ passed" | head -1)
    echo "   test $TEST_COUNT"

    CI_PASSED=true
    break
done

if [ "$CI_PASSED" != "true" ]; then
    echo ""
    echo "  CI $MAX_RETRIES "
    echo "  B  container$CONTAINER_NAME"
    exit 1
fi

#  2. Sync B's changes (use diff between container init and now)
echo ""
echo " Step: sync B changes"
# Find changed .rs files by comparing worktree with project dir
# (git diff doesn't work in container's standalone repo)
CHANGED_FILES=""
for f in $(find "$WT_DIR/src" -name "*.rs" 2>/dev/null); do
    rel="${f#$WT_DIR/}"
    proj_file="$PROJECT_DIR/$rel"
    if [ ! -f "$proj_file" ] || ! diff -q "$f" "$proj_file" >/dev/null 2>&1; then
        CHANGED_FILES="$CHANGED_FILES $rel"
    fi
done
echo "  Changed files: $CHANGED_FILES"

for f in $CHANGED_FILES; do
    src="$WT_DIR/$f"
    dst="$PROJECT_DIR/$f"
    if [ -f "$src" ]; then
        mkdir -p "$(dirname "$dst")"
        cp "$src" "$dst"
        echo "  synced: $f"
    fi
done

#  3. B +
echo ""
echo " Step:  "
cd "$PROJECT_DIR"
cargo build --bin ion 2>&1 | tail -3
cargo test --lib global_memory 2>&1 | tail -5

#  4. git commit 
echo ""
echo " Step: git commit "
git add -A
git commit -m "evolve: $TASK" 2>&1 | tail -3
git log --oneline -3

#  5.  HTML  
echo ""
echo " Step:  HTML  "
LAST_SID=$(cat ~/.ion/agent/last_session 2>/dev/null || echo "")
REPORT="/tmp/evolver_$(date +%Y%m%d_%H%M%S).html"
if [ -n "$LAST_SID" ]; then
    "$PROJECT_DIR/target/debug/ion" --export "$REPORT" --session "$LAST_SID" 2>/dev/null && \
        echo "   : $REPORT" && open "$REPORT" 2>/dev/null
fi

#  6.  
echo ""
echo " Step:  "
"$CONTAINER_BIN" stop "$CONTAINER_NAME" 2>/dev/null && echo "   container "
#  worktree  target
rm -rf "$WT_DIR/target" 2>/dev/null
git worktree remove "$WT_DIR" --force 2>/dev/null && echo "   worktree "
rm -f /tmp/.evolver-state

echo ""
echo ""
echo "   AB "
echo "  CI: $TEST_COUNT"
echo "  : $REPORT"
echo ""
