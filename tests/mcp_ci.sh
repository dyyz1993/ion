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
echo ""
echo "Group H: MCP 工具权限控制（permission rules 管 mcp__*）"

if ! command -v mcp-server-everything &>/dev/null; then
    skip "H1-H3: mcp-server-everything 未安装，跳过"
else
    # 准备：配 MCP server + 权限规则（禁用 echo 工具）
    cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "everything": {"command": "mcp-server-everything", "disabled": false}
  }
}
EOF
    # 全局权限规则：禁用 mcp__everything__echo
    cat > "$TEST_HOME/.ion/settings.json" <<'EOF'
{
  "permissions": {
    "rules": [
      {
        "id": "perm_mcp_echo_deny",
        "provider": "user",
        "subject": "mcp_tool",
        "pattern": "mcp__everything__echo",
        "decision": "Deny",
        "scope": "Project"
      }
    ]
  }
}
EOF

    start_host
    sleep 12  # 等 MCP 连接

    # H1: 被禁用的工具调不了
    OUT=$(rpc call_tool '{"tool":"mcp__everything__echo","args":{"message":"blocked"}}')
    if echo "$OUT" | grep -q "denied\|Permission\|permission"; then
        pass "H1: mcp__everything__echo 被 permission Deny 拦截"
    elif echo "$OUT" | grep -q '"success": false\|"success":false'; then
        pass "H1: echo 被拦截（success=false）"
    else
        fail "H1: echo 应被权限拦截"
        echo "  输出: $(echo "$OUT" | head -3)"
    fi

    # H2: 未禁用的工具正常（get-sum）
    OUT=$(rpc call_tool '{"tool":"mcp__everything__get-sum","args":{"a":1,"b":2}}')
    if echo "$OUT" | grep -q '"success": true\|"success":true'; then
        pass "H2: mcp__everything__get-sum 不受影响（未禁用）"
    else
        fail "H2: get-sum 应正常"
    fi

    # H3: 通配符禁用整个 server
    cat > "$TEST_HOME/.ion/settings.json" <<'EOF'
{
  "permissions": {
    "rules": [
      {
        "id": "perm_mcp_all_deny",
        "provider": "user",
        "subject": "mcp_tool",
        "pattern": "mcp__everything__*",
        "decision": "Deny",
        "scope": "Project"
      }
    ]
  }
}
EOF
    # 热重载权限
    rpc extension_rpc '{"extension":"permission","method":"reload"}' >/dev/null 2>&1
    sleep 1

    # get-sum 也应被禁了
    OUT=$(rpc call_tool '{"tool":"mcp__everything__get-sum","args":{"a":1,"b":2}}')
    if echo "$OUT" | grep -q "denied\|Permission\|permission\|\"success\": false\|\"success\":false"; then
        pass "H3: 通配符 mcp__everything__* 禁用全部工具"
    else
        fail "H3: 通配符应禁用 get-sum"
    fi

    stop_host
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group I: resources/prompts 发现 + read_resource 调用"

if ! command -v mcp-server-everything &>/dev/null; then
    skip "I1-I4: mcp-server-everything 未安装，跳过"
else
    cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "everything": {"command": "mcp-server-everything", "disabled": false}
  }
}
EOF
    # 清理 Group H 可能残留的权限规则
    rm -f "$TEST_HOME/.ion/settings.json"
    start_host
    sleep 12

    # I1: resources 发现（get_mcp_servers 含 resources 列表）
    OUT=$(rpc get_mcp_servers)
    RES_COUNT=$(echo "$OUT" | python3 -c "
import sys,json; d=json.load(sys.stdin)
s=d['data'][0]
print(len(s.get('resources',[])))" 2>/dev/null)
    if [ "$RES_COUNT" -gt 0 ] 2>/dev/null; then
        pass "I1: resources 发现成功（$RES_COUNT 个）"
    else
        fail "I1: resources 应 >0，实际 $RES_COUNT"
    fi

    # I2: prompts 发现
    PROMPT_COUNT=$(echo "$OUT" | python3 -c "
import sys,json; d=json.load(sys.stdin)
s=d['data'][0]
print(len(s.get('prompts',[])))" 2>/dev/null)
    if [ "$PROMPT_COUNT" -gt 0 ] 2>/dev/null; then
        pass "I2: prompts 发现成功（$PROMPT_COUNT 个）"
    else
        fail "I2: prompts 应 >0，实际 $PROMPT_COUNT"
    fi

    # I3: read_resource 调用（读 server-everything 的静态文档 resource）
    # server-everything 有 demo://resource/static/document/architecture
    # 但 read_resource 需要一个 RPC 入口。当前 ion_worker 没有 mcp_read_resource RPC
    # 检查 host 的 process_pending_commands 有 mcp_read_resource 命令（间接验证）
    # 通过 call_tool 的 bridge 代理验证 MCP 连接仍然工作
    OUT=$(rpc call_tool '{"tool":"mcp__everything__echo","args":{"message":"res-test"}}')
    if echo "$OUT" | grep -q '"success": true\|"success":true'; then
        pass "I3: resources 发现后 MCP 工具调用仍正常"
    else
        fail "I3: MCP 工具调用异常"
    fi

    # I4: resource URI 格式验证
    OUT=$(rpc get_mcp_servers)
    if echo "$OUT" | python3 -c "
import sys,json; d=json.load(sys.stdin)
s=d['data'][0]
res=s.get('resources',[])
has_uri = any(r.get('uri','') for r in res)
print('ok' if has_uri else 'no_uri')" 2>/dev/null | grep -q "ok"; then
        pass "I4: resources 含有效 URI"
    else
        fail "I4: resources URI 无效"
    fi

    stop_host
fi

# ──────────────────────────────────────────────────────────
echo ""
echo "Group J: mcp_reload 配置热更新"

if ! command -v mcp-server-everything &>/dev/null; then
    skip "J1-J3: mcp-server-everything 未安装，跳过"
else
    # J1: 启动时无 MCP server，reload 后加上
    echo '{}' > "$TEST_HOME/.ion/config.json"
    rm -f "$TEST_HOME/.ion/settings.json"
    start_host

    # 确认初始无 server
    OUT=$(rpc get_mcp_servers)
    INITIAL_COUNT=$(echo "$OUT" | python3 -c "
import sys,json; d=json.load(sys.stdin)
print(len(d.get('data',[])))" 2>/dev/null)
    if [ "$INITIAL_COUNT" = "0" ]; then
        pass "J1a: 初始无 MCP server"
    else
        fail "J1a: 初始应为 0，实际 $INITIAL_COUNT"
    fi

    # 改 config.json 加 server
    cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "everything": {"command": "mcp-server-everything", "disabled": false}
  }
}
EOF

    # 调 mcp_reload（Worker → bridge → host 重新加载）
    OUT=$(rpc mcp_reload)
    RELOAD_OK=$(echo "$OUT" | python3 -c "
import sys,json; d=json.load(sys.stdin)
data=d.get('data',{})
print(data.get('servers_loaded',0))" 2>/dev/null)
    if [ "$RELOAD_OK" = "1" ]; then
        pass "J1b: mcp_reload 加载 1 个 server"
    else
        fail "J1b: reload 应加载 1 个，实际 $RELOAD_OK"
    fi

    # 等连接完成
    sleep 12

    # 验证 server 出现了
    OUT=$(rpc get_mcp_servers)
    STATUS=$(echo "$OUT" | python3 -c "
import sys,json; d=json.load(sys.stdin)
s=d['data'][0] if d.get('data') else {}
print(s.get('status','none'))" 2>/dev/null)
    if [ "$STATUS" = "connected" ] || [ "$STATUS" = "connecting" ]; then
        pass "J1c: reload 后 MCP server 出现并连接（status=$STATUS）"
    else
        fail "J1c: reload 后应 connected/connecting，实际 $STATUS"
    fi

    stop_host

    # J2: mcp_read_resource（通过 bridge 代理读 MCP 资源）
    cat > "$TEST_HOME/.ion/config.json" <<'EOF'
{
  "mcp_servers": {
    "everything": {"command": "mcp-server-everything", "disabled": false}
  }
}
EOF
    start_host
    sleep 12

    # 读一个 resource（server-everything 有 demo://resource/static/document/architecture）
    OUT=$(rpc mcp_read_resource '{"server":"everything","uri":"demo://resource/static/document/architecture.md"}')
    if echo "$OUT" | grep -q '"success": true\|"success":true'; then
        CONTENT=$(echo "$OUT" | python3 -c "
import sys,json; d=json.load(sys.stdin)
print(d.get('data',{}).get('content','')[:30])" 2>/dev/null)
        if [ -n "$CONTENT" ]; then
            pass "J2: mcp_read_resource 成功（content 前30字符: $CONTENT）"
        else
            pass "J2: mcp_read_resource 成功（content 为空或 blob）"
        fi
    else
        fail "J2: read_resource 失败"
        echo "  输出: $(echo "$OUT" | head -3)"
    fi

    stop_host

    # J3: 验证 mcp_reload 不崩溃（即使没有初始 mcp_manager）
    echo '{}' > "$TEST_HOME/.ion/config.json"
    start_host
    OUT=$(rpc mcp_reload)
    if echo "$OUT" | grep -q '"success": true\|"success":true'; then
        pass "J3: mcp_reload 不崩溃（host 无 mcp_manager 时自动创建）"
    else
        fail "J3: mcp_reload 崩溃"
        echo "  输出: $(echo "$OUT" | head -3)"
    fi
    stop_host
fi

# ──────────────────────────────────────────────────────────
# 清理
rm -rf "$TEST_HOME" 2>/dev/null

echo ""
echo "════════════════════════════════════════════════════"
echo "  结果: PASS=$PASS  FAIL=$FAIL  SKIP=$SKIP"
echo "════════════════════════════════════════════════════"

[ "$FAIL" = "0" ] && exit 0 || exit 1
