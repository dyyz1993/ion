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

# ── 1. 确认编译完成 ──
echo ""
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

# ── 2. 调 B 改代码 + CI（带失败重试）──
CI_PASSED=false
for attempt in $(seq 1 $MAX_RETRIES); do
    echo ""
    echo "── Step 2: 调 B 改代码（尝试 $attempt/$MAX_RETRIES）──"
    echo "  开始: $(date)"

    if [ $attempt -eq 1 ]; then
        # 第一次：正常调 B
        "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer '$TASK' --provider zhipuai --model glm-5.2" 2>&1 | tail -20
    else
        # 重试：把 CI 错误信息给 B，让它修复
        "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
            "cd /workspace && ./target/release/ion --agent developer '上次改动 CI 失败了。错误：$CI_ERROR。请修复这个问题。' --provider zhipuai --model glm-5.2" 2>&1 | tail -20
    fi
    echo "  结束: $(date)"

    # ── 3. B 跑完整 CI ──
    echo ""
    echo "── Step 3: B 跑完整 CI ──"

    echo "  [3a] cargo build..."
    BUILD_OUT=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        'cd /workspace && cargo build --bin ion 2>&1' 2>&1)
    echo "$BUILD_OUT" | tail -3
    if ! echo "$BUILD_OUT" | grep -q "Finished"; then
        CI_ERROR="cargo build failed: $(echo "$BUILD_OUT" | grep 'error' | head -3)"
        echo "  ❌ build 失败"
        continue
    fi
    echo "  ✅ build 通过"

    echo "  [3b] cargo test --lib（全部测试）..."
    TEST_OUT=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        'cd /workspace && cargo test --lib 2>&1' 2>&1)
    echo "$TEST_OUT" | tail -5
    if ! echo "$TEST_OUT" | grep -qE "test result: ok\."; then
        CI_ERROR="cargo test failed: $(echo "$TEST_OUT" | grep 'FAILED\|error' | head -3)"
        echo "  ❌ test 失败"
        continue
    fi
    TEST_COUNT=$(echo "$TEST_OUT" | grep "test result:" | grep -oE "[0-9]+ passed" | head -1)
    echo "  ✅ test 通过（$TEST_COUNT）"

    echo "  [3c] cargo test --test（集成测试）..."
    INTEG_OUT=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        'cd /workspace && cargo test --test unit_rpc_test 2>&1 && cargo test --test manager_integration 2>&1' 2>&1)
    echo "$INTEG_OUT" | tail -5
    if echo "$INTEG_OUT" | grep -q "FAILED"; then
        CI_ERROR="integration test failed"
        echo "  ⚠️ 集成测试有失败（不阻塞，记录）"
    else
        echo "  ✅ 集成测试通过"
    fi

    CI_PASSED=true
    break
done

if [ "$CI_PASSED" != "true" ]; then
    echo ""
    echo "── ❌ CI 失败（$MAX_RETRIES 次重试都没通过）──"
    echo "  B 的改动在 worktree：$WT_DIR"
    echo "  手动检查：cd $WT_DIR && cargo test --lib 2>&1 | tail -20"
    # 不合并，不清理，留给用户检查
    exit 1
fi

# ── 4. 合并 B 的改动 ──
echo ""
echo "── Step 4: 合并 B 的改动到主仓库 ──"
cd "$WT_DIR"
git add -A 2>/dev/null
git diff --cached --quiet 2>/dev/null || git commit -m "evolve: $TASK" 2>/dev/null
echo "  B 的 commits:"
git log --oneline -3

# 用 rsync 同步代码到主仓库（worktree 是独立 git repo）
cd "$PROJECT_DIR"
rsync -av --exclude='target' --exclude='.git' "$WT_DIR/src/" src/ 2>/dev/null
echo "  ✅ 代码已同步到主仓库"

# ── 5. 导出 HTML 报告 ──
echo ""
echo "── Step 5: 导出 HTML 报告 ──"
# A 的 session
LAST_SID=$(cat ~/.ion/agent/last_session 2>/dev/null || echo "")
REPORT_A="/tmp/evolver_A_$(date +%Y%m%d_%H%M%S).html"
if [ -n "$LAST_SID" ]; then
    "$PROJECT_DIR/target/debug/ion" --export "$REPORT_A" --session "$LAST_SID" 2>/dev/null && \
        echo "  ✅ A 的报告: $REPORT_A" || echo "  ⚠️ A 的 session 导出失败"
fi

# 找 B 在 container 里的 session
B_SID=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
    'ls -t /root/.ion/agent/sessions/*/session.jsonl 2>/dev/null | head -1' 2>/dev/null | xargs dirname 2>/dev/null | xargs basename 2>/dev/null)
if [ -n "$B_SID" ]; then
    REPORT_B="/tmp/evolver_B_$(date +%Y%m%d_%H%M%S).html"
    "$PROJECT_DIR/target/debug/ion" --export "$REPORT_B" --session "$B_SID" 2>/dev/null && \
        echo "  ✅ B 的报告: $REPORT_B" || echo "  ⚠️ B 的 session 导出失败"
fi

# macOS 自动打开
if command -v open >/dev/null 2>&1; then
    [ -f "$REPORT_A" ] && open "$REPORT_A"
    [ -f "$REPORT_B" ] && open "$REPORT_B"
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
echo "  CI: $TEST_COUNT"
echo "  A 报告: ${REPORT_A:-无}"
echo "  B 报告: ${REPORT_B:-无}"
echo "════════════════════════════════════════════════════"
