#!/usr/bin/env bash
# ci_host_helper.sh — 通用 host 管理 helper
#
# 用法：在 CI 脚本开头加：
#   source "$(dirname "$0")/ci_host_helper.sh"
#   ensure_host   # 检测已有 host 就复用，没有就起一个新的
#
# 原理：避免 CI 之间互相杀 host。如果 goal_ci_verify.sh 起了全局 host，
# 各 CI 复用它而不是杀掉重启。

ION_BIN="${ION_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/ion}"

# 检测 host 是否在跑
host_is_running() {
    "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"
}

# 确保 host 在跑：有就复用，没有就起
ensure_host() {
    if host_is_running; then
        echo "  (reusing existing host)"
        return 0
    fi
    # 没有在跑——杀残留 + 起新的
    lsof -ti "$HOME/.ion/host.sock" 2>/dev/null | xargs kill 2>/dev/null || true
    sleep 1
    ION_FAUX_REPLY="${ION_FAUX_REPLY:-ci host ready}" \
        "$ION_BIN" serve > /tmp/ci_host_$$.log 2>&1 &
    export CI_HOST_PID=$!
    # 等待就绪
    for i in $(seq 1 10); do
        sleep 2
        if host_is_running; then
            echo "  (started new host PID=$CI_HOST_PID)"
            return 0
        fi
    done
    echo "  ⚠️ Host not ready after 20s"
    return 1
}

# CI 结束时清理（如果是自己起的 host）
cleanup_host() {
    if [ -n "$CI_HOST_PID" ]; then
        kill "$CI_HOST_PID" 2>/dev/null || true
    fi
}

trap cleanup_host EXIT
