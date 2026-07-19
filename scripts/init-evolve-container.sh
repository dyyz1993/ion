#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# init-evolve-container.sh — 初始化自我进化的 Apple Container 环境
#
# 功能：
#   1. 创建 Apple Container（Rust 编译环境）
#   2. 挂载 worktree 目录到 /workspace
#   3. 复制 ion binary 到 container
#   4. 验证 cargo/rustc 可用
#   5. 输出 container name 供后续使用
#
# 用法：
#   bash scripts/init-evolve-container.sh <worktree_dir>
#
# 输出：
#   CONTAINER_NAME=ion-evolve-<timestamp>（写到 stdout 最后一行）
#
# 环境要求：
#   - Apple Container 已安装（/usr/local/bin/container）
#   - worktree 目录存在（git worktree add 创建的）
#   - host 上有 target/release/ion 或 target/debug/ion
#
# 链接：
#   - 设计文档：docs/design/SELF_EVOLUTION.md
#   - AGENTS.md 环境配置章节
# ──────────────────────────────────────────────────────────
set -euo pipefail

CONTAINER_BIN="${CONTAINER_BIN:-/usr/local/bin/container}"

# ── 参数 ──
WORKTREE_DIR="${1:?用法: init-evolve-container.sh <worktree_dir>}"
if [ ! -d "$WORKTREE_DIR" ]; then
    echo "❌ worktree 目录不存在: $WORKTREE_DIR" >&2
    exit 1
fi

# ── 生成 container name ──
TIMESTAMP=$(date +%Y%m%d-%H%M%S)
CONTAINER_NAME="ion-evolve-${TIMESTAMP}"

# ── 找 ion binary ──
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

ION_BIN=""
for candidate in \
    "$PROJECT_DIR/target/release/ion" \
    "$PROJECT_DIR/target/debug/ion"; do
    if [ -x "$candidate" ]; then
        ION_BIN="$candidate"
        break
    fi
done

if [ -z "$ION_BIN" ]; then
    echo "⚠️ ion binary 未找到，container 里需要手动编译" >&2
    echo "  请先在 host 上跑：cargo build --release" >&2
fi

# ── Docker 镜像选择 ──
# Rust 项目需要 Rust 编译环境。
# 优先用本地构建的 ion-evolve-rust 镜像（从 alpine + rustup 构建，~300MB）。
# 如果不存在，尝试拉 rust:latest（~1GB，需要好网络）或从 Dockerfile.evolve 构建。
IMAGE="${EVOLVE_IMAGE:-ion-evolve-rust:latest}"

echo "════════════════════════════════════════════════════"
echo "  ION 自我进化 Container 初始化"
echo "════════════════════════════════════════════════════"
echo "  worktree:   $WORKTREE_DIR"
echo "  container:  $CONTAINER_NAME"
echo "  image:      $IMAGE"
echo "  ion binary: ${ION_BIN:-(无，需在 container 内编译)}"
echo "════════════════════════════════════════════════════"

# ── Step 1: 检查 container 系统 ──
echo ""
echo "── Step 1: 检查 Apple Container ──"
if ! "$CONTAINER_BIN" system status >/dev/null 2>&1; then
    echo "  启动 container 系统..."
    "$CONTAINER_BIN" system start
    sleep 3
fi
echo "  ✅ container 系统就绪"

# ── Step 1b: 检查镜像是否存在，不存在则从 Dockerfile 构建 ──
if ! "$CONTAINER_BIN" image list 2>/dev/null | grep -q "$IMAGE"; then
    echo "  镜像 $IMAGE 不存在，从 Dockerfile.evolve 构建..."
    "$CONTAINER_BIN" build \
        -f "$SCRIPT_DIR/Dockerfile.evolve" \
        -t "$IMAGE" \
        --memory 4G --cpus 4 \
        "$SCRIPT_DIR" 2>&1 || {
        echo "❌ 镜像构建失败" >&2
        exit 1
    }
    echo "  ✅ 镜像构建成功"
fi

# ── Step 2: 创建 container ──
echo ""
echo "── Step 2: 创建 container ──"

# 先检查是否已存在同名 container
if "$CONTAINER_BIN" inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
    echo "  container 已存在，先删除..."
    "$CONTAINER_BIN" stop "$CONTAINER_NAME" >/dev/null 2>&1 || true
fi

# 找 ion-provider 目录（Cargo.toml 里用 path = "../ion-provider"）
# worktree 的 --show-toplevel 返回 worktree 路径（如 /tmp/xxx），
# 需要从 git common dir 找主仓库位置。
GIT_COMMON_DIR=$(cd "$WORKTREE_DIR" && git rev-parse --git-common-dir 2>/dev/null || echo "")
# git common dir 格式：/Users/xxx/ion/.git → 主仓库是 /Users/xxx/ion
if [ -n "$GIT_COMMON_DIR" ]; then
    MAIN_REPO=$(cd "$GIT_COMMON_DIR/.." && pwd)
else
    MAIN_REPO="$WORKTREE_DIR"
fi
HOST_PARENT=$(dirname "$MAIN_REPO")
ION_PROVIDER_DIR="$HOST_PARENT/ion-provider"

CONTAINER_CMD=(
    "$CONTAINER_BIN" run
    --name "$CONTAINER_NAME"
    --detach
    --rm
    --network default
    -v "${WORKTREE_DIR}:/workspace"
    -w /workspace
    --memory "${EVOLVE_MEMORY:-4G}"
    --cpus "${EVOLVE_CPUS:-4}"
)

# 额外挂载 ion-provider（Cargo.toml 用 path = "../ion-provider" 引用）
if [ -d "$ION_PROVIDER_DIR" ]; then
    CONTAINER_CMD+=("-v" "${ION_PROVIDER_DIR}:/ion-provider")
fi

CONTAINER_CMD+=("$IMAGE")
CONTAINER_CMD+=(sh -lc "sleep infinity")

echo "  命令: ${CONTAINER_CMD[*]}"
"${CONTAINER_CMD[@]}"
echo "  ✅ container 创建成功"

# ── Step 2b: 修复 Cargo.toml 的 ion-provider 路径 ──
# worktree 里 Cargo.toml 写的是 path = "../ion-provider"
# container 里需要改成 path = "/ion-provider"
if [ -d "$ION_PROVIDER_DIR" ]; then
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        "cd /workspace && sed -i 's|path = \"../ion-provider\"|path = \"/ion-provider\"|' Cargo.toml" 2>/dev/null
    echo "  ✅ Cargo.toml 路径已修复（/ion-provider）"
fi

# ── Step 3: 等待 container 就绪 ──
echo ""
echo "── Step 3: 等待 container 就绪 ──"
for i in $(seq 1 10); do
    if "$CONTAINER_BIN" exec "$CONTAINER_NAME" echo "ok" >/dev/null 2>&1; then
        echo "  ✅ container 就绪"
        break
    fi
    echo "  等待... ($i/10)"
    sleep 2
done

# ── Step 4: 验证 Rust 环境 ──
echo ""
echo "── Step 4: 验证 Rust 编译环境 ──"
RUSTC_VER=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" rustc --version 2>&1 || echo "未知")
CARGO_VER=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" cargo --version 2>&1 || echo "未知")
GIT_VER=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" git --version 2>&1 || echo "未知")
echo "  rustc: $RUSTC_VER"
echo "  cargo: $CARGO_VER"
echo "  git:   $GIT_VER"

if echo "$RUSTC_VER" | grep -q "未知"; then
    echo "  ❌ Rust 未安装！尝试安装..."
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && source \$HOME/.cargo/env && rustc --version"
fi

# ── Step 5: 复制 ion binary（如果有）──
echo ""
echo "── Step 5: 复制 ion binary ──"
if [ -n "$ION_BIN" ]; then
    # 通过 worktree 挂载间接复制——ion binary 在 project dir 里，worktree 里可能没有
    # 用 container exec + cp 从 host 挂载的路径复制
    HOST_PROJECT_ROOT=$(cd "$WORKTREE_DIR" && git rev-parse --git-common-dir 2>/dev/null | xargs dirname || echo "")
    if [ -n "$HOST_PROJECT_ROOT" ] && [ -f "$HOST_PROJECT_ROOT/target/release/ion" ]; then
        echo "  从 host 复制 ion binary..."
        cp "$HOST_PROJECT_ROOT/target/release/ion" "$WORKTREE_DIR/target/release/ion" 2>/dev/null || \
        cp "$HOST_PROJECT_ROOT/target/debug/ion" "$WORKTREE_DIR/target/debug/ion" 2>/dev/null || true
        echo "  ✅ ion binary 已复制到 worktree"
    else
        echo "  ⚠️ ion binary 未复制（将在 container 内编译）"
    fi
else
    echo "  ⚠️ host 上没有 ion binary（将在 container 内 cargo build）"
fi

# ── Step 6: 安装额外工具 ──
echo ""
echo "── Step 6: 安装额外工具 ──"
# gh CLI（开 PR 用）—— rust 镜像可能没有
GH_OK=$("$CONTAINER_BIN" exec "$CONTAINER_NAME" which gh 2>/dev/null || echo "")
if [ -z "$GH_OK" ]; then
    echo "  安装 gh CLI..."
    "$CONTAINER_BIN" exec "$CONTAINER_NAME" sh -c \
        "curl -fsSL https://cli.github.com/packages/githubcli-archive-keyring.gpg | dd of=/usr/share/keyrings/githubcli-archive-keyring.gpg 2>/dev/null && \
         echo 'deb [arch=$(dpkg --print-architecture) signed-by=/usr/share/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main' > /etc/apt/sources.list.d/github-cli.list && \
         apt-get update -qq && apt-get install -y -qq gh 2>/dev/null" 2>/dev/null && \
    echo "  ✅ gh CLI 已安装" || echo "  ⚠️ gh CLI 安装失败（PR 步骤需在 host 执行）"
else
    echo "  ✅ gh CLI 已存在"
fi

# ── 输出 ──
echo ""
echo "════════════════════════════════════════════════════"
echo "  ✅ Container 环境初始化完成"
echo "════════════════════════════════════════════════════"
echo ""
echo "  Container:  $CONTAINER_NAME"
echo "  Worktree:   $WORKTREE_DIR"
echo "  Rust:       $RUSTC_VER"
echo ""
echo "  后续命令："
echo "    $CONTAINER_BIN exec $CONTAINER_NAME sh -c 'cd /workspace && cargo build'"
echo "    $CONTAINER_BIN exec $CONTAINER_NAME sh -c 'cd /workspace && cargo test --lib'"
echo "    $CONTAINER_BIN exec $CONTAINER_NAME sh -c 'cd /workspace && ion --host --agent developer \"...\"'"
echo "    $CONTAINER_BIN stop $CONTAINER_NAME"
echo ""
echo "CONTAINER_NAME=$CONTAINER_NAME"
