#!/usr/bin/env bash
# ci_host_helper.sh — 通用 host 管理 helper（支持并发隔离）
#
# 用法：在 CI 脚本开头加：
#   source "$(dirname "$0")/ci_host_helper.sh"
#   ensure_host   # 检测已有 host 就复用，没有就起一个新的
#
# 并发隔离：如果调用方没设 ION_HOST_SOCKET，自动用基于 PID 的独立 socket
# （/tmp/ion_ci_<PID>.sock），让多个 CI 脚本能并发跑而不互相 kill host。
# 调用方也可以显式设 ION_HOST_SOCKET 来控制（比如一组 CI 共享一个 host）。
#
# 串行复用模式：设 ION_HOST_SOCKET 为固定路径（如 ~/.ion/host.sock），
# 多个 CI 复用同一个 host（适合串行跑）。

ION_BIN="${ION_BIN:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/ion}"

# ── 自动隔离 socket（如果调用方没设）──
if [ -z "${ION_HOST_SOCKET:-}" ]; then
    export ION_HOST_SOCKET="/tmp/ion_ci_$$.sock"
fi

# 检测 host 是否在跑（用当前 ION_HOST_SOCKET）
host_is_running() {
    "$ION_BIN" rpc --method list_sessions 2>/dev/null | grep -q "sessions"
}

# 确保 host 在跑：有就复用，没有就起
ensure_host() {
    if host_is_running; then
        echo "  (reusing existing host at $ION_HOST_SOCKET)"
        return 0
    fi
    # 没有在跑——清残留 socket 文件 + 起新的
    rm -f "$ION_HOST_SOCKET" 2>/dev/null || true
    sleep 0.5
    ION_FAUX_REPLY="${ION_FAUX_REPLY:-ci host ready}" \
        "$ION_BIN" serve > "/tmp/ci_host_$$.log" 2>&1 &
    export CI_HOST_PID=$!
    # 等待就绪
    for i in $(seq 1 15); do
        sleep 1
        if host_is_running; then
            echo "  (started new host PID=$CI_HOST_PID at $ION_HOST_SOCKET)"
            return 0
        fi
    done
    echo "  ⚠️ Host not ready after 15s (socket=$ION_HOST_SOCKET)"
    echo "  📋 host log tail:"
    tail -5 "/tmp/ci_host_$$.log" 2>/dev/null | sed 's/^/     /'
    return 1
}

# CI 结束时清理（如果是自己起的 host）
cleanup_host() {
    if [ -n "$CI_HOST_PID" ]; then
        kill "$CI_HOST_PID" 2>/dev/null || true
        wait "$CI_HOST_PID" 2>/dev/null || true
    fi
    rm -f "$ION_HOST_SOCKET" 2>/dev/null || true
}

trap cleanup_host EXIT
