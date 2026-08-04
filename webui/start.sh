#!/bin/bash
# webui/start.sh — one-shot launcher for the lyric-remixer web UI.
#
#   1. ensure `ion serve` is running (start it in the background if not)
#   2. ensure node + the `ws` dependency are available
#   3. start the gateway and print the URL
#
# Usage:
#   bash webui/start.sh [--port 8787] [--ion-bin ./target/debug/ion]
set -euo pipefail

PORT=8787
ION_BIN="${ION_BIN:-ion}"
for ((i=1; i<=$#; i++)); do
  case "${!i}" in
    --port) ((i++)); PORT="${!i}" ;;
    --ion-bin) ((i++)); ION_BIN="${!i}" ;;
  esac
done

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WEBUI_DIR="$ROOT/webui"
SOCK="$HOME/.ion/host.sock"

green() { printf "\033[32m%s\033[0m\n" "$1"; }
yellow() { printf "\033[33m%s\033[0m\n" "$1"; }
red() { printf "\033[31m%s\033[0m\n" "$1"; }

# --- 1. ensure ion serve --------------------------------------------------
if [ ! -S "$SOCK" ]; then
  echo "$(yellow 'ion serve 未运行，正在后台启动…')"
  if ! command -v ion >/dev/null 2>&1 && [ ! -x "$ION_BIN" ]; then
    # fall back to a locally built binary
    if [ -x "$ROOT/target/debug/ion" ]; then
      ION_BIN="$ROOT/target/debug/ion"
    else
      echo "$(red '找不到 ion 二进制。请先 cargo build --bin ion，或设置 ION_BIN')"
      exit 1
    fi
  fi
  # Start serve in the background. Logs go to a temp file.
  SERVE_LOG="$(mktemp -t ion_serve_XXXX.log)"
  nohup "$ION_BIN" serve > "$SERVE_LOG" 2>&1 &
  SERVE_PID=$!
  echo "  serve PID=$SERVE_PID, log=$SERVE_LOG"
  # wait for the socket to appear (max ~20s)
  for _ in $(seq 1 40); do
    [ -S "$SOCK" ] && break
    sleep 0.5
  done
  if [ ! -S "$SOCK" ]; then
    echo "$(red 'ion serve 启动超时，日志：')"
    tail -20 "$SERVE_LOG" || true
    exit 1
  fi
else
  green "ion serve 已运行 (socket: $SOCK)"
fi

# --- 2. ensure node + deps ------------------------------------------------
if ! command -v node >/dev/null 2>&1; then
  echo "$(red '未找到 node，请先安装 Node.js (>= 18)')"
  exit 1
fi
NODE_VER="$(node -v | sed 's/v//' | cut -d. -f1)"
if [ "$NODE_VER" -lt 18 ]; then
  echo "$(red "node 版本过低 ($NODE_VER)，需要 >= 18")"
  exit 1
fi

# ensure ws is installed (gateway.mjs requires it)
if [ ! -d "$WEBUI_DIR/node_modules/ws" ]; then
  echo "$(yellow '安装网关依赖 ws …')"
  ( cd "$WEBUI_DIR" && npm install --silent --no-audit --no-fund >/dev/null 2>&1 ) || {
    echo "$(red 'npm install 失败，请手动在 webui/ 下运行 npm install')"
    exit 1
  }
fi

# --- 3. start gateway -----------------------------------------------------
echo ""
echo "启动网关 (port $PORT)…"
green "→  http://localhost:$PORT"
echo "    停止网关: Ctrl-C  (ion serve 仍后台运行)"
echo ""
exec node "$WEBUI_DIR/gateway.mjs" --port "$PORT"
