#!/usr/bin/env bash
#
# regress_after_merge.sh — A→B 回归验证：合并 ion-worker 后，让 B 在 container 跑全套 spawn-worker CI
#
# 设计原则（AGENTS.md A→B 铁律）：
#   - A（host）只调度，不改代码
#   - B（container 里的 ion）执行所有 CI 脚本，独立验证合并未引入回归
#   - B 跑出来的结果是第三方独立证据，不依赖 ZCode 主仓库的测试结果
#
# 用法：
#   bash scripts/regress_after_merge.sh
#
# 产出：
#   /tmp/regress_after_merge.log — 完整日志
#   退出码 0 = 全部 CI 通过；非 0 = 有失败

set -uo pipefail
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

LOG=/tmp/regress_after_merge.log
echo "=== A→B 回归验证 $(date) ===" | tee "$LOG"

# ─── 复用 evolve.sh 启动 container + 编译 ───
echo "" | tee -a "$LOG"
echo "Phase 1: 启动 container + 编译 ion (复用 evolve.sh)" | tee -a "$LOG"
ION_TOOL_TIMEOUT=1800 bash scripts/evolve.sh >> "$LOG" 2>&1
if [ ! -f /tmp/.evolver-state ]; then
    echo "❌ evolve.sh 启动失败" | tee -a "$LOG"
    exit 1
fi
source /tmp/.evolver-state
CONTAINER_NAME="${CONTAINER_NAME:-}"
WT_DIR="${WT_DIR:-}"
if [ -z "$CONTAINER_NAME" ] || [ ! -d "$WT_DIR" ]; then
    echo "❌ container 或 worktree 未就绪" | tee -a "$LOG"
    exit 1
fi
echo "✅ container=$CONTAINER_NAME worktree=$WT_DIR" | tee -a "$LOG"

# 把当前主仓库的合并 commit 同步到 worktree（evolve.sh 创建 worktree 时可能用的是旧 HEAD）
cd "$PROJECT_DIR"
LATEST_COMMIT=$(git rev-parse HEAD)
echo "" | tee -a "$LOG"
echo "同步最新 commit 到 worktree: $LATEST_COMMIT" | tee -a "$LOG"
cd "$WT_DIR"
# worktree 是独立 git init 的（见 evolve.sh），直接 rsync 主仓库 src/tests/scripts/Cargo.toml
for item in src tests scripts Cargo.toml Cargo.lock AGENTS.md docs; do
    rm -rf "$WT_DIR/$item"
    cp -R "$PROJECT_DIR/$item" "$WT_DIR/"
done
cd "$PROJECT_DIR"

# ─── Phase 2: B 在 container 里编译 ───
echo "" | tee -a "$LOG"
echo "Phase 2: B 在 container 里编译 ion (release)" | tee -a "$LOG"
# 注意：不能在 container exec 后面接管道——管道会吃掉 cargo 的退出码
# 改用临时文件存日志 + 单独检查退出码
BUILD_LOG=$(mktemp)
container exec "$CONTAINER_NAME" sh -c \
    'source $HOME/.cargo/env && cd /workspace && cargo build --release --bin ion' > "$BUILD_LOG" 2>&1
BUILD_EXIT=$?
tail -5 "$BUILD_LOG" >> "$LOG"
rm -f "$BUILD_LOG"
if [ $BUILD_EXIT -ne 0 ]; then
    echo "❌ B 编译失败 (exit=$BUILD_EXIT)" | tee -a "$LOG"
    container stop "$CONTAINER_NAME" >/dev/null 2>&1
    exit 1
fi
echo "✅ B 编译通过" | tee -a "$LOG"
# 验证二进制真的存在
if ! container exec "$CONTAINER_NAME" sh -c 'test -x /workspace/target/release/ion'; then
    echo "❌ B 编译后找不到 /workspace/target/release/ion" | tee -a "$LOG"
    container stop "$CONTAINER_NAME" >/dev/null 2>&1
    exit 1
fi

# ─── Phase 2.5: 装 bash + coreutils（CI 脚本依赖）───
# alpine image 默认只有 sh/ash，CI 脚本 shebang 是 #!/usr/bin/env bash
echo "" | tee -a "$LOG"
echo "Phase 2.5: 装 bash + coreutils（CI 脚本依赖）" | tee -a "$LOG"
container exec "$CONTAINER_NAME" sh -c \
    "apk add --no-cache bash coreutils 2>&1 | tail -3" >> "$LOG" 2>&1
echo "✅ bash + coreutils 就绪" | tee -a "$LOG"

# ─── Phase 3: B 跑全套 spawn-worker 相关 CI ───
echo "" | tee -a "$LOG"
echo "Phase 3: B 跑 spawn-worker 相关 CI 脚本" | tee -a "$LOG"

# 关键 CI 脚本清单（覆盖所有 spawn worker 的链路）
CI_SCRIPTS=(
    # 基础 RPC + worker spawn
    "tests/faux_scenarios_ci.sh"        # 三场景 faux（直接/host/serve）
    "tests/extensions_ci.sh"            # WASM 扩展（host + worker）
    "tests/extension_flags_ci.sh"       # extension flags RPC
    "tests/extension_fs_ci.sh"          # ctx.fs 路径
    # 场景 2/3 spawn worker 主链路
    "tests/abort_ci.sh"                 # 工具执行中 abort + 进程清理
    "tests/soft_interrupt_ci.sh"        # interrupt 中断
    "tests/realtime_stitch_ci.sh"       # subscribe 实时事件
    # 高级功能（依赖 spawn worker）
    "tests/mcp_ci.sh"                   # MCP 共享池
    "tests/memory_agent_ci.sh"          # memory-agent 自动 spawn
    "tests/hooks_agent_ci.sh"           # agent handler spawn 子 Worker
    "tests/hooks_handler_ci.sh"         # command/http/prompt handler
    "tests/file_snapshot_ci.sh"         # file snapshot 审批
    "tests/session_hook_ci.sh"          # session switch 事件
    "tests/skill_tool_ci.sh"            # skill fork 模式 spawn 子 Worker
    "tests/streaming_throughput_ci.sh"  # 大 tool_call 流式
    "tests/export_ci.sh"                # export 工具面板
)

PASS=0
FAIL=0
FAILED_SCRIPTS=()
for script in "${CI_SCRIPTS[@]}"; do
    name=$(basename "$script")
    echo "" | tee -a "$LOG"
    echo "── 跑 $name ──" | tee -a "$LOG"
    # 在 container 里跑（B 视角）
    if container exec "$CONTAINER_NAME" sh -c \
        "cd /workspace && bash $script" >> "$LOG" 2>&1; then
        echo "✅ $name 通过" | tee -a "$LOG"
        PASS=$((PASS+1))
    else
        echo "❌ $name 失败" | tee -a "$LOG"
        FAIL=$((FAIL+1))
        FAILED_SCRIPTS+=("$name")
    fi
done

# ─── Phase 4: 汇总 + 清理 ───
echo "" | tee -a "$LOG"
echo "════════════════════════════════════════════════════" | tee -a "$LOG"
echo "  A→B 回归汇总: PASS=$PASS  FAIL=$FAIL  TOTAL=${#CI_SCRIPTS[@]}" | tee -a "$LOG"
if [ $FAIL -gt 0 ]; then
    echo "  失败脚本: ${FAILED_SCRIPTS[*]}" | tee -a "$LOG"
fi
echo "════════════════════════════════════════════════════" | tee -a "$LOG"

# 清理 container + worktree
container stop "$CONTAINER_NAME" >/dev/null 2>&1
cd "$PROJECT_DIR"
git worktree remove "$WT_DIR" --force 2>/dev/null
git worktree prune

if [ $FAIL -gt 0 ]; then
    exit 1
fi
echo "🎉 全部 CI 通过" | tee -a "$LOG"
exit 0
