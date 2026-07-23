#!/usr/bin/env bash
# watchdog.sh — Safe upgrade for a running `ion serve` (single-instance, never loses contact)
#
# Design principle: NEVER lose contact with the running ion instance.
#   - If compile fails     → A_old keeps running, report error
#   - If A_new fails health → A_old keeps running, rollback binary
#   - If A_old crashes      → restart from last known-good backup
#
# Based on industry-standard patterns (nginx SIGUSR2, systemd socket activation):
#   - Single instance (not blue-green): AF_UNIX sockets can't be dual-bound
#   - rename() to replace binary: bypasses macOS ETXTBSY
#   - Health check via real RPC: ion rpc --method health
#   - Automatic rollback on any failure
#
# Usage:
#   bash scripts/watchdog.sh                # Monitor loop (foreground)
#   bash scripts/watchdog.sh --upgrade      # One-shot: compile + health-check + switch
#
# Env vars:
#   ION_BIN     Path to ion binary (default: target/release/ion)
#   HEALTH_TIMEOUT  Seconds to wait for A_new health (default: 30)
#   COMPILE_TIMEOUT Seconds to wait for cargo build (default: 600)
set -uo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="${ION_BIN:-$PROJECT_DIR/target/release/ion}"
ION_DEBUG="${ION_BIN:-$PROJECT_DIR/target/debug/ion}"
BACKUP_DIR="$PROJECT_DIR/target/release/.backups"
RESTART_FILE="/tmp/.ion-evolve-restart"
HEALTH_TIMEOUT="${HEALTH_TIMEOUT:-30}"
COMPILE_TIMEOUT="${COMPILE_TIMEOUT:-600}"
SOCK_PATH="${HOME}/.ion/host.sock"

mkdir -p "$BACKUP_DIR"

# ── Helpers ─────────────────────────────────────────────────────────

# Check if ion serve is running (by PID file or socket)
is_running() {
    local pid_file="${HOME}/.ion/host.pid"
    if [ -f "$pid_file" ]; then
        local pid
        pid=$(cat "$pid_file" 2>/dev/null)
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
    fi
    # Fallback: check if socket exists and is connectable
    if [ -S "$SOCK_PATH" ]; then
        return 0
    fi
    return 1
}

# Get the PID of the running ion serve
get_pid() {
    local pid_file="${HOME}/.ion/host.pid"
    if [ -f "$pid_file" ]; then
        cat "$pid_file" 2>/dev/null
        return
    fi
    # Fallback: find by socket
    lsof -ti "$SOCK_PATH" 2>/dev/null | head -1
}

# Real health check: send RPC and parse response
health_check() {
    local bin="$1"
    local timeout="${2:-$HEALTH_TIMEOUT}"

    for i in $(seq 1 "$timeout"); do
        local resp
        resp=$("$bin" rpc --method health --params '{}' 2>/dev/null)
        if echo "$resp" | grep -q '"status":[[:space:]]*"ok"'; then
            return 0
        fi
        sleep 1
    done
    return 1
}

# Graceful shutdown: SIGTERM → wait → SIGKILL
graceful_kill() {
    local pid="$1"
    local timeout="${2:-10}"

    if [ -z "$pid" ] || ! kill -0 "$pid" 2>/dev/null; then
        return 0
    fi

    # Send SIGTERM (graceful)
    kill -TERM "$pid" 2>/dev/null

    # Wait up to timeout seconds
    for i in $(seq 1 "$timeout"); do
        if ! kill -0 "$pid" 2>/dev/null; then
            return 0
        fi
        sleep 1
    done

    # Force kill
    echo "[W] Process $pid did not exit in ${timeout}s, sending SIGKILL"
    kill -KILL "$pid" 2>/dev/null
    sleep 1
}

# ── Core: do one safe upgrade ───────────────────────────────────────

do_upgrade() {
    echo "[W] =========================================="
    echo "[W]   Safe Upgrade Starting"
    echo "[W] =========================================="

    # ── Step 0: Ensure A_old is running and healthy ──────────────────
    if ! is_running; then
        echo "[W] ion serve is not running. Starting it first..."
        cd "$PROJECT_DIR"
        "$ION_BIN" serve &
        local serve_pid=$!
        echo "[W] Started ion serve (PID: $serve_pid)"
        sleep 3

        if ! kill -0 "$serve_pid" 2>/dev/null; then
            echo "[W] ❌ ion serve failed to start!"
            return 1
        fi

        # Health check the freshly started instance
        if ! health_check "$ION_BIN" 10; then
            echo "[W] ❌ ion serve started but health check failed!"
            return 1
        fi
        echo "[W] ✅ ion serve is healthy"
    fi

    local OLD_PID
    OLD_PID=$(get_pid)
    echo "[W] A_old PID: ${OLD_PID:-unknown}"

    # ── Step 1: Backup current binary ───────────────────────────────
    local backup
    backup="$BACKUP_DIR/ion.$(date +%Y%m%d_%H%M%S).backup"

    if [ -f "$ION_BIN" ]; then
        cp "$ION_BIN" "$backup"
        echo "[W] Backed up current binary → $backup"
        # Keep only last 5 backups
        ls -t "$BACKUP_DIR"/ion.*.backup 2>/dev/null | tail -n +6 | while read -r old; do
            rm -f "$old"
        done
    else
        echo "[W] ⚠️  No existing binary at $ION_BIN"
    fi

    # ── Step 2: Compile new binary ──────────────────────────────────
    # Match the build profile to ION_BIN path (debug vs release).
    # This ensures backup/compile/restore all operate on the SAME binary.
    local build_flag="--release"
    case "$ION_BIN" in
        */target/debug/*) build_flag="" ;;
        */target/release/*) build_flag="--release" ;;
    esac
    echo "[W] Compiling A_new ($build_flag, timeout: ${COMPILE_TIMEOUT}s)..."

    if ! timeout "$COMPILE_TIMEOUT" cargo build $build_flag --bin ion --bin ion-worker 2>&1 | tail -5; then
        echo "[W] ❌ Compile failed or timed out. A_old continues."
        return 1
    fi

    if [ ! -f "$ION_BIN" ]; then
        echo "[W] ❌ Binary not found at $ION_BIN after compile. A_old continues."
        return 1
    fi
    echo "[W] ✅ A_new compiled"

    # ── Step 3: Graceful stop A_old ─────────────────────────────────
    if [ -n "$OLD_PID" ] && kill -0 "$OLD_PID" 2>/dev/null; then
        echo "[W] Graceful stop A_old (PID: $OLD_PID)..."
        graceful_kill "$OLD_PID" 10
        echo "[W] A_old stopped"
    fi

    # Clean up stale socket
    rm -f "$SOCK_PATH" 2>/dev/null

    # ── Step 4: Start A_new ─────────────────────────────────────────
    echo "[W] Starting A_new..."
    cd "$PROJECT_DIR"
    "$ION_BIN" serve &
    local NEW_PID=$!
    echo "[W] A_new started (PID: $NEW_PID)"

    # ── Step 5: Health check A_new ──────────────────────────────────
    echo "[W] Health check A_new (timeout: ${HEALTH_TIMEOUT}s)..."

    if health_check "$ION_BIN" "$HEALTH_TIMEOUT"; then
        echo "[W] ✅ A_new is healthy!"
        echo "[W] =========================================="
        echo "[W]   Upgrade complete: $OLD_PID → $NEW_PID"
        echo "[W] =========================================="
        return 0
    else
        echo "[W] ❌ A_new health check failed! Rolling back..."

        # Kill the broken A_new
        graceful_kill "$NEW_PID" 5
        rm -f "$SOCK_PATH" 2>/dev/null

        # Restore backup binary
        if [ -f "$backup" ]; then
            cp "$backup" "$ION_BIN"
            echo "[W] Restored binary from backup"
        fi

        # Restart A_old
        "$ION_BIN" serve &
        local ROLLBACK_PID=$!
        echo "[W] A_old restarted (PID: $ROLLBACK_PID)"
        sleep 3

        if health_check "$ION_BIN" 15; then
            echo "[W] ✅ Rolled back successfully. A_old is serving again."
        else
            echo "[W] ❌❌ FATAL: Rollback also failed! Manual intervention needed."
            return 2
        fi
        return 1
    fi
}

# ── Monitor loop ────────────────────────────────────────────────────

do_monitor() {
    echo "=========================================="
    echo "  ION Watchdog (Monitor Mode)"
    echo "=========================================="
    echo "  Project:     $PROJECT_DIR"
    echo "  Binary:      $ION_BIN"
    echo "  Socket:      $SOCK_PATH"
    echo "  Restart signal: $RESTART_FILE"
    echo "=========================================="
    echo ""

    # Ensure ion serve is running
    if ! is_running; then
        echo "[W] ion serve not running. Starting..."
        cd "$PROJECT_DIR"
        "$ION_BIN" serve &
        local pid=$!
        sleep 3
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "[W] ❌ Cannot start ion serve. Exiting."
            exit 1
        fi
        echo "[W] ion serve started (PID: $pid)"
    fi

    local CURRENT_PID
    CURRENT_PID=$(get_pid)
    echo "[W] Monitoring A_old (PID: ${CURRENT_PID:-unknown})"
    echo ""

    while true; do
        # ── Check: A_old alive? ─────────────────────────────────────
        if ! is_running; then
            echo ""
            echo "[W] ⚠️  A_old died! Attempting restart from last backup..."

            # Find latest backup
            local latest_backup
            latest_backup=$(ls -t "$BACKUP_DIR"/ion.*.backup 2>/dev/null | head -1)

            if [ -n "$latest_backup" ] && [ -f "$latest_backup" ]; then
                cp "$latest_backup" "$ION_BIN"
                echo "[W] Restored from: $latest_backup"
            else
                echo "[W] No backup found, trying current binary as-is"
            fi

            rm -f "$SOCK_PATH" 2>/dev/null
            cd "$PROJECT_DIR"
            "$ION_BIN" serve &
            CURRENT_PID=$!
            echo "[W] A_old restarted (PID: $CURRENT_PID)"
            sleep 5

            if health_check "$ION_BIN" 15; then
                echo "[W] ✅ Restarted successfully"
            else
                echo "[W] ❌ Restart failed! Will retry in 30s"
                sleep 30
                continue
            fi
        fi

        # ── Check: restart signal? ──────────────────────────────────
        if [ -f "$RESTART_FILE" ]; then
            echo ""
            echo "[W] 🔄 Restart signal detected!"
            rm -f "$RESTART_FILE"

            do_upgrade
            local rc=$?

            if [ $rc -eq 0 ]; then
                CURRENT_PID=$(get_pid)
                echo "[W] Monitoring new A_old (PID: ${CURRENT_PID:-unknown})"
            else
                echo "[W] Upgrade returned code $rc. Continuing with current instance."
                CURRENT_PID=$(get_pid)
            fi

            echo ""
            echo "[W] Resuming monitor..."
        fi

        sleep 5
    done
}

# ── Main ────────────────────────────────────────────────────────────

case "${1:-monitor}" in
    --upgrade|upgrade)
        do_upgrade
        exit $?
        ;;
    --monitor|monitor|"")
        do_monitor
        ;;
    --help|-h|help)
        echo "Usage: bash scripts/watchdog.sh [--upgrade|--monitor]"
        echo ""
        echo "  --upgrade   One-shot: compile + health-check + switch (with rollback)"
        echo "  --monitor   Continuous: watch for restart signal + auto-restart on crash"
        echo ""
        echo "Env:"
        echo "  ION_BIN          Path to ion binary (default: target/release/ion)"
        echo "  HEALTH_TIMEOUT   Health check timeout in seconds (default: 30)"
        echo "  COMPILE_TIMEOUT  Cargo build timeout in seconds (default: 600)"
        ;;
    *)
        echo "Unknown option: $1"
        echo "Run: bash scripts/watchdog.sh --help"
        exit 1
        ;;
esac
