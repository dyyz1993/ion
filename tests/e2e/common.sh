#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# E2E 测试公共库 — 所有 group_*.sh source 这个文件
# ──────────────────────────────────────────────────────────

# 初始化（每个 group 脚本开头调一次）
e2e_init() {
    PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    cd "$PROJECT_DIR"
    ION_BIN="$PROJECT_DIR/target/debug/ion"

    PASS=0; FAIL=0; SKIP=0
    TEST_HOME="/tmp/ion_e2e_${GROUP}_$$"
    rm -rf "$TEST_HOME" 2>/dev/null
    mkdir -p "$TEST_HOME/.ion/agent"
    # HOME 必须在 ion 执行前设置
    export HOME="$TEST_HOME"
    # 同时设 ION_SESSION_DIR 确保 session 存到测试目录
    export ION_AGENT_DIR="$TEST_HOME/.ion/agent"
    export _OLD_HOME="${HOME_BACKUP:-$HOME}"

    echo ""
    echo "── Group $GROUP ─────────────────────────────────"
    echo "  TEST_HOME=$TEST_HOME"
}

pass() { PASS=$((PASS+1)); echo -e "  \033[32m✅ $1\033[0m"; }
fail() { FAIL=$((FAIL+1)); echo -e "  \033[31m❌ $1\033[0m"; }
skip() { SKIP=$((SKIP+1)); echo -e "  \033[33m⏭️  $1\033[0m"; }

# 启动 host（返回 SID 到全局变量 E2E_SID）
start_host() {
    export HOME="$TEST_HOME"
    rm -f "$TEST_HOME/.ion/host.sock"
    ION_FAUX_REPLY="${1:-e2e test}" "$ION_BIN" serve >/tmp/ion_e2e_host.log 2>&1 &
    E2E_HOST_PID=$!
    sleep 2
    if ! kill -0 $E2E_HOST_PID 2>/dev/null; then
        echo "  ❌ host 启动失败"; cat /tmp/ion_e2e_host.log | tail -3
        return 1
    fi
    CREATE=$("$ION_BIN" rpc --method create_session --params '{"agent":"build"}' 2>&1)
    E2E_SID=$(echo "$CREATE" | grep -o 'sess_[a-f0-9]*' | head -1)
    sleep 1
}

stop_host() {
    kill "$E2E_HOST_PID" 2>/dev/null
    sleep 1
}

rpc() {
    "$ION_BIN" rpc --session "$E2E_SID" --method "$1" ${2:+--params "$2"} 2>&1
}

# 收尾（每个 group 脚本结尾调）
e2e_done() {
    kill "$E2E_HOST_PID" 2>/dev/null
    rm -rf "$TEST_HOME" 2>/dev/null
    echo ""
    echo "  Group $GROUP: PASS=$PASS FAIL=$FAIL SKIP=$SKIP"
    echo "─────────────────────────────────────────────────"
}
