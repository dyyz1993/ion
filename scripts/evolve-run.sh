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
        # 用 stdin 喂 prompt（避免 $TASK 含特殊字符被 shell 拆成多个参数，
        # 导致 prompt 末尾混入 --provider/--model）
        echo "$TASK" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer --provider zhipuai --model glm-5.2" 2>&1 | tail -20
    else
        echo "[CI 失败原因] $CI_ERROR
请修复上述 CI 错误。" | "$CONTAINER_BIN" exec -i "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer --provider zhipuai --model glm-5.2" 2>&1 | tail -20
    fi
    echo "  : $(date)"

    # B 跑完后立刻抓 session id（容器内 last_session = host last_session，因为 rw 挂载）
    # 不能等到最后才抓——host 上的 cargo test 会再次污染。
    B_SID=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c 'cat /root/.ion/agent/last_session 2>/dev/null' 2>/dev/null | tr -dc '[:alnum:]_-' | head -c 100)
    echo "  B session: $B_SID"
    # 立刻备份 B 的 session 文件（防止后续 host 操作污染）
    if [ -n "$B_SID" ]; then
        # session 文件路径：~/.ion/agent/sessions/<cwd_hash>--<cwd_basename>--/session.jsonl
        B_SESSION_FILE=$(find "$HOME/.ion/agent/sessions/" -name "session.jsonl" 2>/dev/null | xargs grep -l "\"id\":\"$B_SID\"" 2>/dev/null | head -1)
        if [ -n "$B_SESSION_FILE" ]; then
            cp "$B_SESSION_FILE" "/tmp/evolver_b_session_${B_SID}.jsonl"
            echo "  backed up: /tmp/evolver_b_session_${B_SID}.jsonl"
        fi
    fi

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

# U+FFFD 守门检查：B 偶尔会破坏中文 comment 字符（变 U+FFFD，valid UTF-8 但内容损坏）
# 这种破坏会让后续 developer agent 的 edit 工具 pattern matching 失败。
# 详见 docs/design/EVOLVER_LESSONS_LEARNED.md §11
GARBLED=$(grep -rl $'\xef\xbf\xbd' "$WT_DIR/src/" 2>/dev/null | head -5)
if [ -n "$GARBLED" ]; then
    echo "  ERROR: B's changes contain U+FFFD garbled chars (broken Chinese comments):"
    echo "$GARBLED" | sed 's/^/    /'
    echo "  Rejecting merge. B should preserve existing comments verbatim."
    exit 1
fi

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

#  5.  HTML  (B SID, last_session)
echo ""
echo " Step:  HTML  "
#  B_SID B
if [ -z "$B_SID" ]; then
    B_SID=$(cat ~/.ion/agent/last_session 2>/dev/null || echo "")
fi
REPORT="/tmp/evolver_$(date +%Y%m%d_%H%M%S).html"
if [ -n "$B_SID" ]; then
    "$PROJECT_DIR/target/debug/ion" --export "$REPORT" --session "$B_SID" 2>/dev/null && \
        echo "   : $REPORT" && open "$REPORT" 2>/dev/null
else
    echo "  B_SID  HTML "
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
