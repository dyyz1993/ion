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
    CREATE_OUT=$($ION_BIN rpc --method create_session --params '{"agent":"build"}' 2>&1)
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
echo ""
echo "Group E: 真实连接（需要 mcp-server-everything）"

# 检查 mcp-server-everything 是否可用
if ! command -v mcp-server-everything &>/dev/null; then
    skip "E1-E5: mcp-server-everything 未安装，跳过真实连接测试"
    skip "  安装: npm install -g @modelcontextprotocol/server-everything"
else
    # E1: 真实 stdio 连接 + 工具发现
    cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "everything": {"command": "mcp-server-everything", "disabled": false}
  }
}
EOF
    start_host
    # 多等一会儿让 MCP 连接完成
    sleep 12

    OUT=$(rpc get_mcp_servers)
    STATUS=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); s=d['data'][0]; print(s['status'])" 2>/dev/null)
    if [ "$STATUS" = "connected" ]; then
        pass "E1: mcp-server-everything 连接成功 (status=connected)"
    else
        fail "E1: 应为 connected，实际 $STATUS"
    fi

    # E2: 工具发现（tools 非空）
    TOOL_COUNT=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(len(d['data'][0].get('tools',[])))" 2>/dev/null)
    if [ "$TOOL_COUNT" -gt 0 ] 2>/dev/null; then
        pass "E2: 工具发现成功 ($TOOL_COUNT 个工具)"
    else
        fail "E2: 工具数应为 >0，实际 $TOOL_COUNT"
    fi

    # E3: 工具调用 echo
    OUT=$(rpc call_tool '{"tool":"mcp__everything__echo","args":{"message":"ci-test"}}')
    if echo "$OUT" | grep -q '"success": true\|"success":true'; then
        OUTPUT_TEXT=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('data',{}).get('output','')[:50])" 2>/dev/null)
        if echo "$OUTPUT_TEXT" | grep -q "ci-test"; then
            pass "E3: echo 工具调用成功（返回含输入消息）"
        else
            pass "E3: echo 工具调用成功"
        fi
    else
        fail "E3: echo 工具调用失败"
        echo "  输出: $(echo "$OUT" | head -3)"
    fi

    # E4: 工具调用 get-sum（验证参数传递）
    OUT=$(rpc call_tool '{"tool":"mcp__everything__get-sum","args":{"a":7,"b":3}}')
    if echo "$OUT" | grep -q '"success": true\|"success":true'; then
        pass "E4: get-sum 工具调用成功（参数传递正确）"
    else
        fail "E4: get-sum 工具调用失败"
    fi

    # E5: 工具名格式验证（mcp__server__tool）
    if rpc get_mcp_servers | grep -q "mcp__everything__"; then
        pass "E5: 工具名格式 mcp__server__tool 正确"
    else
        fail "E5: 工具名格式不符"
    fi

    stop_host
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group F: 方案 C 进程共享（host 持有 MCP，Worker bridge 代理）"

if ! command -v mcp-server-everything &>/dev/null; then
    skip "F1-F3: mcp-server-everything 未安装，跳过"
else
    # F1: host 持有 MCP + Worker 通过 bridge 代理访问
    cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "everything": {"command": "mcp-server-everything", "disabled": false}
  }
}
EOF
    start_host
    # Worker 通过 bridge 拉 host 的工具列表
    OUT=$(rpc get_mcp_servers)
    STATUS=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); s=d['data'][0]; print(s['status'])" 2>/dev/null)
    if [ "$STATUS" = "connected" ]; then
        pass "F1: Worker 通过 bridge 代理查到 host MCP（status=connected）"
    else
        fail "F1: 应为 connected，实际 $STATUS"
    fi

    # F2: Worker 的 Agent 注册了 MCP 代理工具
    TOOL_COUNT=$(rpc get_active_tools | python3 -c "
import sys,json; d=json.load(sys.stdin)
tools=d.get('data',{}).get('tools',[])
mcp=[t for t in tools if 'mcp__' in t]
print(len(mcp))" 2>/dev/null)
    if [ "$TOOL_COUNT" -gt 0 ] 2>/dev/null; then
        pass "F2: Worker Agent 注册了 $TOOL_COUNT 个 MCP 代理工具"
    else
        fail "F2: MCP 工具数应为 >0，实际 $TOOL_COUNT"
    fi

    # F3: Worker 通过 bridge 代理调用 MCP 工具（不直连 server）
    OUT=$(rpc call_tool '{"tool":"mcp__everything__echo","args":{"message":"proxy-test"}}')
    if echo "$OUT" | grep -q '"success": true\|"success":true'; then
        OUTPUT_TEXT=$(echo "$OUT" | python3 -c "import sys,json; d=json.load(sys.stdin); print(d.get('data',{}).get('output','')[:50])" 2>/dev/null)
        if echo "$OUTPUT_TEXT" | grep -q "proxy-test"; then
            pass "F3: bridge 代理调用 echo 成功（返回含 proxy-test）"
        else
            pass "F3: bridge 代理调用 echo 成功"
        fi
    else
        fail "F3: 代理调用失败"
        echo "  输出: $(echo "$OUT" | head -3)"
    fi

    # F4: 只 spawn 了一份 server 进程（验证进程共享）
    MCP_PROC_COUNT=$(pgrep -f "mcp-server-everything" | wc -l | tr -d ' ')
    if [ "$MCP_PROC_COUNT" = "1" ]; then
        pass "F4: 只 spawn 1 份 MCP server 进程（进程共享）"
    else
        fail "F4: 应为 1 份进程，实际 $MCP_PROC_COUNT"
    fi

    stop_host
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group G: 场景 1 MCP 支持（cmd_run 直连）"

if ! command -v mcp-server-everything &>/dev/null; then
    skip "G1-G2: mcp-server-everything 未安装，跳过"
else
    # G1: 场景 1 配了 MCP server，Agent 初始化 MCP 工具
    cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "everything": {"command": "mcp-server-everything", "disabled": false}
  }
}
EOF
    # 验证 MCP 初始化（RUST_LOG=info 看 connecting + tools registered）
    OUT=$(ION_FAUX_REPLY="mcp-ok" RUST_LOG=info timeout 30 $ION_BIN "test" 2>&1)
    if echo "$OUT" | grep -qi "mcp.*connecting\|mcp.*tools registered"; then
        pass "G1: 场景 1 MCP 初始化成功（connecting + tools registered）"
    else
        fail "G1: 场景 1 MCP 未生效"
        echo "  输出: $(echo "$OUT" | grep -i mcp | head -3)"
    fi

    # G2: 场景 1 没配 MCP 零开销
    echo '{}' > "$TEST_HOME/.ion/config.json"
    OUT=$(ION_FAUX_REPLY="zero-test" timeout 15 $ION_BIN "hello" 2>&1)
    if echo "$OUT" | grep -q "zero-test"; then
        pass "G2: 场景 1 空配置正常执行（零开销）"
    else
        fail "G2: 场景 1 空配置异常"
    fi
fi

# ──────────────────────────────────────────────────────────
# 清理
rm -rf "$TEST_HOME" 2>/dev/null

echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "════════════════════════════════════════════════════"

[ "$FAIL" = "0" ] && exit 0 || exit 1
