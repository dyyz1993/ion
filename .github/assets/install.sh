#!/usr/bin/env bash
# ion 一键安装脚本（解压自 release tarball 后运行）
# 用法：
#   tar -xzf ion-*.tar.gz -C /tmp && /tmp/install.sh
#   或直接：bash install.sh [install_dir]
#
# 默认装到 ~/.local/bin（若不在 PATH 会提示），root 用户装到 /usr/local/bin。
set -euo pipefail

INSTALL_DIR="${1:-}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ -z "$INSTALL_DIR" ]; then
    if [ "$(id -u)" = "0" ]; then
        INSTALL_DIR="/usr/local/bin"
    else
        INSTALL_DIR="$HOME/.local/bin"
    fi
fi

mkdir -p "$INSTALL_DIR"

SRC="$SCRIPT_DIR/ion"
if [ ! -f "$SRC" ]; then
    echo "ERROR: $SRC not found. 请先 tar -xzf 解压 release 包，再运行解压目录里的 install.sh" >&2
    exit 1
fi

cp -f "$SRC" "$INSTALL_DIR/ion"
chmod +x "$INSTALL_DIR/ion"

echo "✅ ion 已安装到 $INSTALL_DIR/ion"

# PATH 检查
case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo "⚠️  $INSTALL_DIR 不在 PATH 里。请加到 ~/.bashrc / ~/.zshrc："
        echo "    export PATH=\"$INSTALL_DIR:\$PATH\""
        ;;
esac

# 冒烟
if "$INSTALL_DIR/ion" --version >/dev/null 2>&1; then
    echo "✅ 冒烟通过：$("$INSTALL_DIR/ion" --version 2>&1 | head -1)"
else
    echo "⚠️  ion --version 退出非零（可能正常，ion 没有 version 子命令）。尝试 --help："
    "$INSTALL_DIR/ion" --help 2>&1 | head -3 || true
fi
