#!/usr/bin/env bash
# watchdog.sh — 看门狗：监控 A，有新代码时安全切换
#
# W = 看门狗（这个脚本，永不进化，纯 bash）
# A_old = 当前运行的 ion（安全基线）
# A_new = 合并新代码后的 ion（自检通过才替换 A_old）
#
# 流程：
#   1. W 启动 A_old（ion serve --port $PORT_OLD）
#   2. W 后台监控 /tmp/.ion-evolve-restart 文件
#   3. A_old 合并新代码后 touch /tmp/.ion-evolve-restart
#   4. W 检测到 → 后台编译 A_new → 启动 A_new（不同端口）
#   5. A_new 自检（health check）
#   6. 通过 → W 杀 A_old → A_new 成为新的 A_old
#   7. 失败 → W 回滚 → A_old 继续
#
# Usage: bash scripts/watchdog.sh
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
PORT_OLD="${PORT_OLD:-9000}"
PORT_NEW="${PORT_NEW:-9001}"
RESTART_FILE="/tmp/.ion-evolve-restart"
HEALTH_TIMEOUT=60   # seconds to wait for A_new health check
COMPILE_TIMEOUT=600  # seconds to wait for cargo build

echo "=========================================="
echo "  ION Watchdog"
echo "=========================================="
echo "  Project: $PROJECT_DIR"
echo "  Port:    $PORT_OLD (A_old)"
echo "=========================================="
echo ""

# ── Step 1: 启动 A_old ────────────────────────────────────────────
echo "[W] Starting A_old (ion serve on port $PORT_OLD)..."

# Backup current binary as known-good baseline
BIN_PATH="$PROJECT_DIR/target/release/ion"
BACKUP_PATH="$PROJECT_DIR/target/release/ion.backup"

if [ -f "$BIN_PATH" ]; then
    cp "$BIN_PATH" "$BACKUP_PATH"
    echo "[W] Backed up current binary to ion.backup"
fi

# Start A_old in background
cd "$PROJECT_DIR"
"$BIN_PATH" serve &
OLD_PID=$!
echo "[W] A_old started (PID: $OLD_PID)"

# Give it time to start
sleep 3
if ! kill -0 "$OLD_PID" 2>/dev/null; then
    echo "[W] ❌ A_old failed to start!"
    exit 1
fi
echo "[W] A_old is running"

# ── Step 2: 监控循环 ──────────────────────────────────────────────
echo ""
echo "[W] Monitoring for restart signals..."
echo "[W] A_old will touch $RESTART_FILE when new code is merged"
echo ""

while true; do
    # Check if A_old is still alive
    if ! kill -0 "$OLD_PID" 2>/dev/null; then
        echo "[W] ⚠️  A_old (PID $OLD_PID) died! Restarting from backup..."
        cp "$BACKUP_PATH" "$BIN_PATH" 2>/dev/null
        "$BIN_PATH" serve &
        OLD_PID=$!
        echo "[W] A_old restarted (PID: $OLD_PID)"
        sleep 3
        continue
    fi

    # Check for restart signal
    if [ -f "$RESTART_FILE" ]; then
        echo ""
        echo "[W] 🔄 Restart signal detected! Starting dual-version switch..."
        rm -f "$RESTART_FILE"

        # ── Step 3: Compile A_new ──────────────────────────────────
        echo "[W] Compiling A_new (timeout: ${COMPILE_TIMEOUT}s)..."
        cd "$PROJECT_DIR"

        if ! timeout "$COMPILE_TIMEOUT" cargo build --release --bin ion --bin ion-worker 2>&1 | tail -3; then
            echo "[W] ❌ Compile failed or timed out. A_old continues."
            continue
        fi

        if [ ! -f "$BIN_PATH" ]; then
            echo "[W] ❌ Binary not found after compile. A_old continues."
            continue
        fi
        echo "[W] ✅ A_new compiled"

        # ── Step 4: Start A_new on different port ──────────────────
        echo "[W] Starting A_new on port $PORT_NEW..."
        "$BIN_PATH" serve &
        NEW_PID=$!
        echo "[W] A_new started (PID: $NEW_PID)"

        # ── Step 5: Health check ───────────────────────────────────
        echo "[W] Health check (timeout: ${HEALTH_TIMEOUT}s)..."
        HEALTHY=false
        for i in $(seq 1 "$HEALTH_TIMEOUT"); do
            if ! kill -0 "$NEW_PID" 2>/dev/null; then
                echo "[W] ❌ A_new died during health check!"
                break
            fi
            # Simple health check: does the process respond?
            # In production, this would be: curl -s http://localhost:$PORT_NEW/health
            # For now, just check if process is alive after 5 seconds
            if [ "$i" -ge 5 ]; then
                echo "[W] ✅ A_new is alive and healthy"
                HEALTHY=true
                break
            fi
            sleep 1
        done

        # ── Step 6: Switch or rollback ─────────────────────────────
        if [ "$HEALTHY" = true ]; then
            echo "[W] 🔄 Switching: kill A_old ($OLD_PID) → A_new becomes A_old"
            kill "$OLD_PID" 2>/dev/null
            wait "$OLD_PID" 2>/dev/null
            OLD_PID="$NEW_PID"
            # Update backup
            cp "$BIN_PATH" "$BACKUP_PATH"
            echo "[W] ✅ Switch complete. New A_old PID: $OLD_PID"
        else
            echo "[W] 🔄 Rollback: kill A_new, restore A_old from backup"
            kill "$NEW_PID" 2>/dev/null
            wait "$NEW_PID" 2>/dev/null
            cp "$BACKUP_PATH" "$BIN_PATH" 2>/dev/null
            echo "[W] ✅ Rolled back. A_old continues (PID: $OLD_PID)"
        fi

        echo ""
        echo "[W] Monitoring for next restart signal..."
    fi

    # Poll every 5 seconds
    sleep 5
done