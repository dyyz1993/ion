#!/usr/bin/env bash
# ci_watchdog_host.sh — 守护 host 进程（被杀后自动重启）
#
# 用法：在 goal_ci_verify.sh 里用它替代直接起 host：
#   bash scripts/ci_watchdog_host.sh &
#   WATCHDOG_PID=$!
#   ... 跑 CI ...
#   kill $WATCHDOG_PID
#
# 原理：watchdog 循环检测 host 是否活着，死了就重启。
# CI 脚本杀 host 后，watchdog 在 2 秒内重启它。

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
HOST_LOG="/tmp/ci_watchdog_host.log"

# 临时启用 global-memory
CONFIG_FILE="$HOME/.ion/config.json"
python3 -c "
import json
with open('$CONFIG_FILE') as f: c = json.load(f)
c.setdefault('extensions', {})['global-memory'] = {'enabled': True}
with open('$CONFIG_FILE', 'w') as f: json.dump(c, f, indent=2)
" 2>/dev/null
rm -f "$HOME/.ion/agent/global-memory.db"* 2>/dev/null
rm -f "$HOME/.ion/agent/extensions/"*.wasm 2>/dev/null

while true; do
    # 检测 host 是否活着
    if ! "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
        # Host 死了——重启
        lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
        rm -f "$HOME/.ion/host.sock" 2>/dev/null
        sleep 1
        ION_FAUX_REPLY="host ready" "$ION_BIN" serve > "$HOST_LOG" 2>&1 &
        echo "[watchdog] Host restarted at $(date)"
        # 等 host 就绪
        for i in $(seq 1 10); do
            sleep 2
            if "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"; then
                echo "[watchdog] Host ready"
                break
            fi
        done
    fi
    sleep 2
done
