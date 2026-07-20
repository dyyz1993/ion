#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# evolve-run.sh — A 调 B 改代码 + CI + 合并 + 清理（完整闭环）
#
# 在 evolve.sh 之后调用（环境已就绪）。
# A 只需要调一个 bash 命令：bash scripts/evolve-run.sh "任务描述"
#
# 用法: bash scripts/evolve-run.sh "给 global_memory 加 fn xxx() 方法"
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

echo "════════════════════════════════════════════════════"
echo "  🧬 A 调 B 改代码"
echo "════════════════════════════════════════════════════"
echo "  Container: $CONTAINER_NAME"
echo "  Worktree:  $WT_DIR"
echo "  Task:      $TASK"
echo "════════════════════════════════════════════════════"
echo ""

# ── 1. 确认编译完成 ──
echo "── Step 1: 确认 ion 编译完成 ──"
if ! "$CONTAINER_BIN" exec "$CONTAINER_NAME" test -f /tmp/ion-build-done 2>/dev/null; then
    echo "  ⏳ 等待编译完成..."
    for i in $(seq 1 60); do
        sleep 30
        if "$CONTAINER_BIN" exec "$CONTAINER_NAME" test -f /tmp/ion-build-done 2>/dev/null; then
            echo "  ✅ 编译完成（等待 $((i*30))s）"
            break
        fi
        echo "  ... ($((i*30))s)"
    done
else
    echo "  ✅ 已编译完成"
fi

# ── 2. 调 B 改代码 ──
echo ""
echo "── Step 2: 调 B（container 里的 ION developer agent）改代码 ──"
echo "  开始: $(date)"
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
    "cd /workspace && ./target/release/ion --agent developer '$TASK' --provider zhipuai --model glm-5.2" 2>&1 | tail -20
echo "  结束: $(date)"

# ── 3. B 跑 CI ──
echo ""
echo "── Step 3: B 跑 cargo test ──"
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
    'cd /workspace && cargo test --lib 2>&1' | tail -10
CI_EXIT=${PIPESTATUS[0]}

if [ "$CI_EXIT" -ne 0 ]; then
    echo "  ❌ CI 失败（exit $CI_EXIT）"
    echo "  B 的改动在 worktree：$WT_DIR"
    echo "  检查：cd $WT_DIR && cargo test --lib 2>&1 | tail -20"
else
    echo "  ✅ CI 通过"
fi

# ── 4. 合并 B 的改动到 master ──
echo ""
echo "── Step 4: 合并 B 的改动 ──"
cd "$WT_DIR"
git add -A 2>/dev/null
git diff --cached --quiet 2>/dev/null || git commit -m "evolve: $TASK" 2>/dev/null
echo "  B 的 commits:"
git log --oneline -3

# 把 worktree 的改动复制到主仓库（worktree 是独立 git repo，不能直接 merge）
cd "$PROJECT_DIR"
# 找出 worktree 相对于 init 的改动文件
rsync -av --exclude='target' --exclude='.git' "$WT_DIR/src/" "$PROJECT_DIR/src/" 2>/dev/null
echo "  ✅ 代码已同步到主仓库"

# ── 5. 导出 HTML 报告 ──
echo ""
echo "── Step 5: 导出 HTML 报告 ──"
LAST_SID=$(cat ~/.ion/agent/last_session 2>/dev/null || echo "")
REPORT="/tmp/evolver_$(date +%Y%m%d_%H%M%S).html"
if [ -n "$LAST_SID" ] && [ -x "$PROJECT_DIR/target/debug/ion" ]; then
    "$PROJECT_DIR/target/debug/ion" --export "$REPORT" --session "$LAST_SID" 2>/dev/null && \
        echo "  ✅ 报告: $REPORT" && open "$REPORT" 2>/dev/null
else
    echo "  ⚠️ 跳过导出（找不到 session 或 ion binary）"
fi

# ── 6. 清理 ──
echo ""
echo "── Step 6: 清理 ──"
"$CONTAINER_BIN" stop "$CONTAINER_NAME" 2>/dev/null && echo "  ✅ container 已停"
git worktree remove "$WT_DIR" --force 2>/dev/null && echo "  ✅ worktree 已删"
rm -f /tmp/.evolver-state

echo ""
echo "════════════════════════════════════════════════════"
echo "  ✅ A→B 自进化完成"
echo "════════════════════════════════════════════════════"
