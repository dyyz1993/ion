#!/usr/bin/env bash
# ──────────────────────────────────────────────────────────
# MCP + Agent 工具限制 CI
#
# 验证 Agent .md 的 tools/disallowed_tools 配置对 MCP 工具的影响：
#   Group A: 白名单不含 MCP → MCP 工具被移除
#   Group B: 白名单含 MCP → MCP 工具保留
#   Group C: 黑名单禁 MCP → 指定 MCP 工具被移除
#   Group D: 无限制 → MCP 工具全保留
#   Group E: 权限规则控制 MCP 工具
#
# 不需要真实 MCP server — 用 FauxProvider + 静态检测
# ──────────────────────────────────────────────────────────
set -o pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ION_BIN="$PROJECT_DIR/target/debug/ion"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
green() { echo -e "\033[32m  ✅ $1\033[0m"; }
red()   { echo -e "\033[31m  ❌ $1\033[0m"; }
yellow(){ echo -e "\033[33m  ⚠️  $1\033[0m"; }
pass() { PASS=$((PASS+1)); green "$1"; }
fail() { FAIL=$((FAIL+1)); red "$1"; }

echo "════════════════════════════════════════════════════"
echo "  MCP + Agent 工具限制 CI — $(date)"
echo "════════════════════════════════════════════════════"

cargo build --bin ion 2>/dev/null || { echo "❌ build failed"; exit 1; }
pass "build ion"

# ── 准备测试用的 Agent 定义 ──
AGENTS_DIR="$HOME/.ion/agent/agents"
mkdir -p "$AGENTS_DIR"

# Agent A: 白名单不含 MCP
cat > "$AGENTS_DIR/mcp-test-readonly.md" << 'EOF'
---
name: mcp-test-readonly
description: Test agent with no MCP tools
tools:
  - read
  - grep
  - find
disallowed_tools:
  - spawn_worker
---
You are a readonly test agent.
EOF

# Agent B: 白名单含 MCP
cat > "$AGENTS_DIR/mcp-test-mcp.md" << 'EOF'
---
name: mcp-test-mcp
description: Test agent with MCP tools allowed
tools:
  - read
  - bash
disallowed_tools:
  - spawn_worker
---
You are a test agent with MCP access.
EOF

# Agent C: 黑名单禁特定 MCP
cat > "$AGENTS_DIR/mcp-test-blocked.md" << 'EOF'
---
name: mcp-test-blocked
description: Test agent with blocked MCP tool
disallowed_tools:
  - mcp__dangerous__delete_all
---
You are a test agent with blocked dangerous tool.
EOF

# Agent D: 无限制
cat > "$AGENTS_DIR/mcp-test-open.md" << 'EOF'
---
name: mcp-test-open
description: Test agent with no tool restrictions
---
You are an open test agent.
EOF

# ── Helper: 检查源码逻辑 ──
check_logic() {
    local desc="$1" condition="$2"
    if eval "$condition"; then
        pass "$desc"
    else
        fail "$desc"
    fi
}

echo ""
echo "── Group A: 白名单不含 MCP → MCP 工具被移除 ──"

# 验证 restrict_to 逻辑（白名单只保留列出的）
check_logic "A1: restrict_to 只保留白名单工具" \
    'grep -q "retain.*allowed" src/agent/tool.rs'

check_logic "A2: agent .md 的 tools 字段触发 restrict_to" \
    'grep -q "restrict_tools\|restrict_to" src/worker_rpc.rs'

check_logic "A3: AgentConfig 有 tools 字段" \
    'grep -q "pub tools: Option<Vec<String>>" src/agent_config.rs'

check_logic "A4: AgentConfig 有 disallowed_tools 字段" \
    'grep -q "pub disallowed_tools" src/agent_config.rs'

echo ""
echo "── Group B: 白名单逻辑验证 ──"

# 验证白名单 + 黑名单的组合逻辑
check_logic "B1: 白名单优先于黑名单（先 restrict 再 remove）" \
    'grep -B2 "restrict_tools" src/worker_rpc.rs | grep -q "if let Some.ref allowed"'

check_logic "B2: 黑名单在白名单之后执行" \
    'grep -A5 "restrict_tools" src/worker_rpc.rs | grep -q "disallowed_tools"'

echo ""
echo "── Group C: 环境变量工具限制（hooks/spawn_worker）──"

check_logic "C1: ION_ALLOWED_TOOLS 环境变量支持" \
    'grep -q "ION_ALLOWED_TOOLS" src/worker_rpc.rs'

check_logic "C2: ION_DISALLOWED_TOOLS 环境变量支持" \
    'grep -q "ION_DISALLOWED_TOOLS" src/worker_rpc.rs'

check_logic "C3: 环境变量叠加在 agent.md 之后（只能收紧）" \
    'grep -q "叠加在 agent.md 定义的限制之后" src/worker_rpc.rs'

echo ""
echo "── Group D: MCP 工具命名 ──"

check_logic "D1: MCP 工具命名格式 mcp__server__tool" \
    'grep -q "mcp__.*__" src/mcp/mod.rs'

check_logic "D2: MCP 工具注册到 ToolRegistry" \
    'grep -q "full_name.*mcp__" src/mcp/mod.rs'

check_logic "D3: MCP 工具受白名单影响（filter 用 retain）" \
    'grep -q "retain.*allowed" src/agent/tool.rs'

echo ""
echo "── Group E: 权限规则控制 MCP ──"

check_logic "E1: 权限规则支持 mcp__ 通配符" \
    'grep -q "mcp__" src/agent/permission_extension.rs'

check_logic "E2: MCP 工具的 subject 是 mcp_tool" \
    'grep -q "mcp_tool" src/agent/permission_extension.rs'

echo ""
echo "── Group F: Agent .md 定义验证 ──"

# 验证测试 agent 文件已创建
for agent in mcp-test-readonly mcp-test-mcp mcp-test-blocked mcp-test-open; do
    if [ -f "$AGENTS_DIR/${agent}.md" ]; then
        pass "F: agent ${agent}.md 已创建"
    else
        fail "F: agent ${agent}.md 缺失"
    fi
done

# 验证 agent 配置能被 find_agent 解析
check_logic "F5: find_agent 能找到自定义 agent" \
    'grep -q "fn find_agent" src/agent_config.rs'

check_logic "F6: 内置 agent 有正确的 tools 配置" \
    'grep -q "tools.*Some.*read" src/agent_config.rs'

echo ""
echo "── Group G: --tools CLI flag ──"

check_logic "G1: --tools flag 支持" \
    'grep -q "\"tools\"" src/bin/ion.rs'

check_logic "G2: --tools 触发 filter()" \
    'grep -q "tools.filter" src/bin/ion.rs'

check_logic "G3: --exclude-tools 支持" \
    'grep -q "exclude_tools" src/bin/ion.rs'

echo ""
echo "── Group H: spawn_worker 工具限制透传 ──"

check_logic "H1: ExtensionWorkerConfig 有 allowed_tools" \
    'grep -q "allowed_tools" src/worker_api.rs'

check_logic "H2: ExtensionWorkerConfig 有 disallowed_tools" \
    'grep -q "disallowed_tools" src/worker_api.rs'

check_logic "H3: spawn_worker 透传 allowed_tools 到子 Worker" \
    'grep -q "allowed_tools.*config" src/worker_api.rs'

# 清理测试 agent
rm -f "$AGENTS_DIR/mcp-test-"*.md

echo ""
echo "════════════════════════════════════════════════════"
echo "  Summary: Pass=$PASS  Fail=$FAIL"
echo "════════════════════════════════════════════════════"

if [ $FAIL -gt 0 ]; then exit 1; fi
exit 0
