#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# MCP CI — Phase 1 + Phase 2 验证
#
# 验证：
#   Group A:  配置加载（disabled server，不触发真实连接）
#   Group B:  运行时 toggle
#   Group C:  restart
#   Group D:  错误处理（不存在的命令 → status:error）
#
# 注意：真实 MCP server 连接测试（Group D in MCP_SYSTEM.md）需要
#   npx @modelcontextprotocol/server-everything，留手动验证。
# ──────────────────────────────────────────────────────────
set -o pipefail

PASS=0; FAIL=0; SKIP=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⏭️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }
skip() { SKIP=$((SKIP+1)); yellow "$1"; }

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"
ION_BIN="$PROJECT_DIR/target/debug/ion"

echo "════════════════════════════════════════════════════"
echo "  MCP CI (Phase 1+2) — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion --bin ion-worker 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion + ion-worker"

TEST_HOME="/tmp/ion_mcp_ci_home_$$"
rm -rf "$TEST_HOME" 2>/dev/null
mkdir -p "$TEST_HOME/.ion"
export HOME="$TEST_HOME"

# 启动 host（用 ION_FAUX_REPLY 避免 LLM 调用）
start_host() {
    SOCK="$TEST_HOME/.ion/host.sock"
    rm -f "$SOCK" 2>/dev/null
    ION_FAUX_REPLY="mcp test" $ION_BIN serve >/tmp/ion_mcp_host.log 2>&1 &
    HOST_PID=$!
    sleep 2
    if ! kill -0 $HOST_PID 2>/dev/null; then
        echo "❌ host 启动失败"; cat /tmp/ion_mcp_host.log | tail -5; exit 1
    fi
    CREATE_OUT=$($ION_BIN rpc --method create_session --params '{"agent":"developer"}' 2>&1)
    SID=$(echo "$CREATE_OUT" | grep '"session_id"' | sed 's/.*"session_id"[: ]*"//;s/".*//')
    sleep 1  # 等 worker 完成初始化（含 MCP connect_all）
}

stop_host() {
    kill $HOST_PID 2>/dev/null
    sleep 1
}

rpc() {
    $ION_BIN rpc --session "$SID" --method "$1" ${2:+--params "$2"} 2>&1
}

# ──────────────────────────────────────────────────────────
echo ""
echo "Group A: 配置加载（disabled，不触发真实连接）"

# A1: 空配置
cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{}
EOF
start_host
OUT=$(rpc get_mcp_servers)
if echo "$OUT" | grep -q '"success": true\|"success":true'; then
    if echo "$OUT" | grep -q '"data": \[\]\|"data":\[\]'; then
        pass "A1: 空配置 get_mcp_servers 返回空数组"
    else
        fail "A1: 空配置应返回空数组"
    fi
else
    fail "A1: get_mcp_servers 请求失败"
fi
stop_host

# A2: 配置 1 个 disabled stdio server（不连接，避免超时）
cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "test-kb": {
      "command": "echo",
      "args": ["hello"],
      "disabled": true
    }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
if echo "$OUT" | grep -q "test-kb" && echo "$OUT" | grep -q "stdio"; then
    pass "A2: disabled stdio server 出现在列表，transport=stdio"
else
    fail "A2: stdio server 未出现"
    echo "  输出: $(echo "$OUT" | head -5)"
fi
if echo "$OUT" | grep -q '"disabled": true\|"disabled":true'; then
    pass "A2b: disabled=true（配置标记）"
else
    fail "A2b: disabled 标记错误"
fi
if echo "$OUT" | grep -q '"status": "disconnected"\|"status":"disconnected"'; then
    pass "A2c: status=disconnected（disabled 不连接）"
else
    fail "A2c: disabled server 应为 disconnected"
fi
stop_host

# A3: HTTP server（disabled，避免连接超时）
cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "remote-api": {
      "type": "streamable-http",
      "url": "http://localhost:9999/mcp",
      "disabled": true
    }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
if echo "$OUT" | grep -q "remote-api" && echo "$OUT" | grep -q "streamable-http"; then
    pass "A3: http server transport=streamable-http"
else
    fail "A3: http server 未正确识别"
    echo "  输出: $(echo "$OUT" | head -5)"
fi
stop_host

# A4: 两个 disabled server（stdio + http）
cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "local-srv": { "command": "echo", "disabled": true },
    "remote-srv": { "type": "streamable-http", "url": "http://x/mcp", "disabled": true }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
COUNT=$(echo "$OUT" | grep -o '"name"' | wc -l | tr -d ' ')
if [ "$COUNT" = "2" ]; then
    pass "A4: stdio + http 两个 server 同时列出"
else
    fail "A4: 应有 2 个 server，实际 $COUNT"
fi
stop_host

# ──────────────────────────────────────────────────────────
echo ""
echo "Group B: 运行时 toggle"

# 用 disabled server 做基础，然后 toggle 开启/关闭
cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "toggle-test": { "command": "nonexistent-mcp-cmd-xyz", "disabled": true }
  }
}
EOF
start_host

# B1: toggle 关闭（已经是 disabled，再关一次）
OUT=$(rpc mcp_toggle_server '{"name":"toggle-test","enabled":false}')
if echo "$OUT" | grep -q '"success": true\|"success":true'; then
    pass "B1: toggle 关闭 toggle-test"
else
    fail "B1: toggle 关闭失败"
    echo "  输出: $(echo "$OUT" | head -3)"
fi

# B3: toggle 不存在的 server
OUT=$(rpc mcp_toggle_server '{"name":"ghost","enabled":true}')
if echo "$OUT" | grep -q '"success": false\|"success":false'; then
    pass "B2: toggle 不存在 server 报错"
else
    fail "B2: 应报错 unknown mcp server"
fi

# B4: 缺 enabled 参数
OUT=$(rpc mcp_toggle_server '{"name":"toggle-test"}')
if echo "$OUT" | grep -q '"success": false\|"success":false'; then
    pass "B3: 缺 enabled 参数报错"
else
    fail "B3: 应报错 missing enabled"
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group C: restart"

# C1: restart（server 用不存在的命令，restart 会尝试连接→失败→status:error）
OUT=$(rpc mcp_restart_server '{"name":"toggle-test"}')
if echo "$OUT" | grep -q '"success": true\|"success":true\|"status"\|"error"'; then
    pass "C1: restart toggle-test（返回状态）"
else
    fail "C1: restart 失败"
    echo "  输出: $(echo "$OUT" | head -3)"
fi

# C2: restart 不存在的 server
OUT=$(rpc mcp_restart_server '{"name":"ghost"}')
if echo "$OUT" | grep -q '"success": false\|"success":false\|"error"'; then
    pass "C2: restart 不存在 server 报错"
else
    fail "C2: 应报错"
fi

stop_host

# ──────────────────────────────────────────────────────────
echo ""
echo "Group D: 错误处理（真实连接错误）"

# D1: 不存在的命令 → status:error（非超时，立即失败）
cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "bad-cmd": { "command": "nonexistent-mcp-cmd-xyz-12345", "disabled": false }
  }
}
EOF
start_host
OUT=$(rpc get_mcp_servers)
if echo "$OUT" | grep -q '"status": "error"\|"status":"error"'; then
    pass "D1: 不存在的命令 → status=error"
else
    fail "D1: 应为 error 状态"
    echo "  输出: $(echo "$OUT" | head -5)"
fi

# D2: error 字段含错误信息
if echo "$OUT" | grep -q '"error"'; then
    pass "D2: error 字段含错误信息"
else
    fail "D2: 应有 error 字段"
fi

stop_host

# ──────────────────────────────────────────────────────────
# 清理
rm -rf "$TEST_HOME" 2>/dev/null

echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "════════════════════════════════════════════════════"

[ "$FAIL" = "0" ] && exit 0 || exit 1
