#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# evolve.sh — A 驱动 B 的环境初始化脚本
#
# 把 worktree + container + 编译 ion 打包成一个脚本，
# A 只需要调一个 bash 命令就能完成前 3 步。
#
# 用法: bash scripts/evolve.sh
# 输出: 写 /tmp/.evolver-state（WT_DIR + CONTAINER_NAME + BUILD_STATUS）
# ──────────────────────────────────────────────────────────
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"
IMAGE="${EVOLVE_IMAGE:-ion-evolve-rust:latest}"
ION_DIR="${HOME}/.ion"

echo "════════════════════════════════════════════════════"
echo "  🧬 A→B 环境初始化"
echo "════════════════════════════════════════════════════"

# ── 1. 开 worktree ──
WT_DIR=$(mktemp -d /tmp/ion-evolve-XXXXXX)
cd "$PROJECT_DIR"
# 确保从 master 最新代码创建 worktree
git checkout master 2>/dev/null || true
git worktree add "$WT_DIR" -b "evolve/$(date +%Y%m%d-%H%M%S)" master 2>&1 | tail -2
echo "  ✅ worktree: $WT_DIR (from master)"

# 验证 worktree 代码能编译（host 上快速检查）
echo "  验证 worktree 代码..."
(cd "$WT_DIR" && cargo check --bin ion 2>&1 | tail -2) || echo "  ⚠️ worktree 代码 cargo check 有警告/错误"

# 把 worktree 的 .git 从文件（链接）转成独立 repo（container 里 gitdir 路径无效）
if [ -f "$WT_DIR/.git" ]; then
    rm -f "$WT_DIR/.git"
    (cd "$WT_DIR" && git init -q && git config user.email 'ion-evolver@example.com' && git config user.name 'ION Evolver' && git add -A && git commit -q -m 'container init' 2>/dev/null)
fi

# ── 2. 启动 container ──
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
CONTAINER_NAME="ion-evolve-${TIMESTAMP}"

# 找 ion-provider
GIT_COMMON_DIR=$(cd "$PROJECT_DIR" && git rev-parse --git-common-dir 2>/dev/null || echo "")
HOST_PARENT=""
if [ -n "$GIT_COMMON_DIR" ]; then
    MAIN_REPO=$(cd "$GIT_COMMON_DIR/.." 2>/dev/null && pwd)
    HOST_PARENT=$(dirname "$MAIN_REPO")
fi
ION_PROVIDER_DIR="$HOST_PARENT/ion-provider"

CONTAINER_CMD=(
    "$CONTAINER_BIN" run
    --name "$CONTAINER_NAME"
    --detach --rm
    --network default
    -v "${WT_DIR}:/workspace"
    -w /workspace
    --memory 4G --cpus 4
)

[ -d "$ION_PROVIDER_DIR" ] && CONTAINER_CMD+=("-v" "${ION_PROVIDER_DIR}:/ion-provider")
[ -d "$ION_DIR" ] && CONTAINER_CMD+=("-v" "${ION_DIR}:/root/.ion:ro")

CONTAINER_CMD+=("$IMAGE" sh -lc "sleep infinity")

"${CONTAINER_CMD[@]}" 2>&1 | tail -2
echo "  ✅ container: $CONTAINER_NAME"

# 修复 Cargo.toml 路径
if [ -d "$ION_PROVIDER_DIR" ]; then
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        "cd /workspace && sed -i 's|path = \"../ion-provider\"|path = \"/ion-provider\"|' Cargo.toml" 2>/dev/null
fi

# ── 3. 前台编译 ion（等完成再返回，A 不用轮询）──
echo "  ⏳ 编译 ion（6-15 分钟，等完成）..."
"$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
    'source $HOME/.cargo/env && cd /workspace && cargo build --release --bin ion 2>&1 | tail -5 && touch /tmp/ion-build-done'
BUILD_EXIT=$?

if [ $BUILD_EXIT -eq 0 ]; then
    echo "  ✅ ion 编译成功"
    # 验证 binary
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c 'cd /workspace && ./target/release/ion --version' 2>&1 | head -1
else
    echo "  ❌ ion 编译失败"
    cat > /tmp/.evolver-state << EOF
WT_DIR=$WT_DIR
CONTAINER_NAME=$CONTAINER_NAME
PROJECT_DIR=$PROJECT_DIR
BUILD_STATUS=FAILED
EOF
    exit 1
fi

# ── 输出状态文件 ──
cat > /tmp/.evolver-state << EOF
WT_DIR=$WT_DIR
CONTAINER_NAME=$CONTAINER_NAME
PROJECT_DIR=$PROJECT_DIR
BUILD_STATUS=OK
EOF

echo ""
echo "════════════════════════════════════════════════════"
echo "  ✅ 环境就绪"
echo "════════════════════════════════════════════════════"
echo "  Container: $CONTAINER_NAME"
echo "  Worktree:  $WT_DIR"
echo ""
echo "  后续命令："
echo "    # 检查编译状态"
echo "    container exec $CONTAINER_NAME test -f /tmp/ion-build-done && echo DONE || echo BUILDING"
echo ""
echo "    # 调 B 改代码"
echo "    container exec $CONTAINER_NAME sh -c 'cd /workspace && ./target/release/ion --agent developer \"任务\"'"
echo ""
echo "    # B 跑 CI"
echo "    container exec $CONTAINER_NAME sh -c 'cd /workspace && cargo test --lib'"
echo ""
echo "    # 清理"
echo "    container stop $CONTAINER_NAME && git worktree remove $WT_DIR --force"
echo "════════════════════════════════════════════════════"
