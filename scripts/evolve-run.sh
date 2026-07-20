#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# evolve-run.sh — A 调 B 改代码 + 完整 CI + 合并 + HTML（生产版）
#
# 在 evolve.sh 之后调用（环境已就绪）。
#
# 用法: bash scripts/evolve-run.sh "任务描述"
# ──────────────────────────────────────────────────────────
set -uo pipefail

TASK="$1"
if [ -z "$TASK" ]; then
    echo "❌ 用法: evolve-run.sh \"任务描述\""
    exit 1
fi

source /tmp/.evolver-state 2>/dev/null
if [ -z "$CONTAINER_NAME" ] || [ -z "$WT_DIR" ]; then
    echo "❌ /tmp/.evolver-state 不存在或缺少 CONTAINER_NAME/WT_DIR"
    echo "   先跑 bash scripts/evolve.sh"
    exit 1
fi

CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
MAX_RETRIES=3

echo "════════════════════════════════════════════════════"
echo "  🧬 A 调 B 改代码（生产版）"
echo "════════════════════════════════════════════════════"
echo "  Container: $CONTAINER_NAME"
echo "  Worktree:  $WT_DIR"
echo "  Task:      $TASK"
echo "════════════════════════════════════════════════════"

# ── 1. 调 B 改代码 + CI（带失败重试）──
CI_PASSED=false
for attempt in $(seq 1 $MAX_RETRIES); do
    echo ""
    echo "── Step $attempt: 调 B 改代码（尝试 $attempt/$MAX_RETRIES）──"
    echo "  开始: $(date)"

    if [ $attempt -eq 1 ]; then
        "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer '$TASK' --provider zhipuai --model glm-5.2" 2>&1 | tail -20
    else
        "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer '上次 CI 失败：$CI_ERROR。请修复。' --provider zhipuai --model glm-5.2" 2>&1 | tail -20
    fi
    echo "  结束: $(date)"

    # ── CI: cargo build + cargo test --lib ──
    echo ""
    echo "  [CI] cargo build..."
    BUILD_OUT=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        'cd /workspace && cargo build --bin ion 2>&1' 2>&1)
    echo "$BUILD_OUT" | tail -3
    if ! echo "$BUILD_OUT" | grep -q "Finished"; then
        CI_ERROR="build failed: $(echo "$BUILD_OUT" | grep 'error' | head -3)"
        echo "  ❌ build 失败"
        continue
    fi
    echo "  ✅ build 通过"

    echo "  [CI] cargo test --lib（全部）..."
    TEST_OUT=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        'cd /workspace && cargo test --lib 2>&1' 2>&1)
    echo "$TEST_OUT" | tail -5
    if ! echo "$TEST_OUT" | grep -qE "test result: ok\."; then
        CI_ERROR="test failed: $(echo "$TEST_OUT" | grep 'FAILED' | head -3)"
        echo "  ❌ test 失败"
        continue
    fi
    TEST_COUNT=$(echo "$TEST_OUT" | grep "test result:" | grep -oE "[0-9]+ passed" | head -1)
    echo "  ✅ test 通过（$TEST_COUNT）"

    CI_PASSED=true
    break
done

if [ "$CI_PASSED" != "true" ]; then
    echo ""
    echo "── ❌ CI 失败（$MAX_RETRIES 次重试都没通过）──"
    echo "  B 的改动在 container：$CONTAINER_NAME"
    exit 1
fi

# ── 2. 只同步 B 改动的文件（不是全量 rsync）──
echo ""
echo "── Step: 同步 B 的改动 ──"
# 在 container 里 git diff 看改了哪些文件
CHANGED_FILES=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
    'cd /workspace && git diff --name-only HEAD 2>/dev/null' 2>&1)
echo "  B 改了这些文件:"
echo "$CHANGED_FILES"

# 只同步改动的文件
for f in $CHANGED_FILES; do
    src="$WT_DIR/$f"
    dst="$PROJECT_DIR/$f"
    if [ -f "$src" ]; then
        mkdir -p "$(dirname "$dst")"
        cp "$src" "$dst"
        echo "  ✅ 同步: $f"
    fi
done

# ── 3. 主仓库验证（B 的改动在主仓库能编译+测试）──
echo ""
echo "── Step: 主仓库验证 ──"
cd "$PROJECT_DIR"
cargo build --bin ion 2>&1 | tail -3
cargo test --lib global_memory 2>&1 | tail -5

# ── 4. git commit ──
echo ""
echo "── Step: git commit ──"
git add -A
git commit -m "evolve: $TASK" 2>&1 | tail -3
git log --oneline -3

# ── 5. 导出 HTML 报告 ──
echo ""
echo "── Step: 导出 HTML 报告 ──"
LAST_SID=$(cat ~/.ion/agent/last_session 2>/dev/null || echo "")
REPORT="/tmp/evolver_$(date +%Y%m%d_%H%M%S).html"
if [ -n "$LAST_SID" ]; then
    "$PROJECT_DIR/target/debug/ion" --export "$REPORT" --session "$LAST_SID" 2>/dev/null && \
        echo "  ✅ 报告: $REPORT" && open "$REPORT" 2>/dev/null
fi

# ── 6. 清理 ──
echo ""
echo "── Step: 清理 ──"
"$CONTAINER_BIN" stop "$CONTAINER_NAME" 2>/dev/null && echo "  ✅ container 已停"
# 删除 worktree 的 target（防磁盘满）
rm -rf "$WT_DIR/target" 2>/dev/null
git worktree remove "$WT_DIR" --force 2>/dev/null && echo "  ✅ worktree 已删"
rm -f /tmp/.evolver-state

echo ""
echo "════════════════════════════════════════════════════"
echo "  ✅ A→B 自进化完成"
echo "  CI: $TEST_COUNT"
echo "  报告: $REPORT"
echo "════════════════════════════════════════════════════"
