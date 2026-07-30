# MCP 用法指南

> **状态：已完成** — Phase 1-4 全部实现，37 个 CI 检查通过。

---

## 0. 快速开始

### 配置 MCP Server

在 `~/.ion/config.json` 里添加 MCP server 配置：

```json
{
  "mcp_servers": {
    "everything": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-everything"]
    }
  }
}
```

或者项目级配置 `~/.ion/projects/<project-key>/config.json`。

启动 `ion serve` 后，ION 自动连接所有配置的 MCP server，发现工具并通过 `mcp__<server>__<tool>` 命名暴露给 LLM。

### 两种传输方式

| 方式 | 配置 | 适用场景 |
|------|------|---------|
| **Stdio** | `"command": "npx", "args": [...]` | 本地 MCP server（子进程） |
| **HTTP** | `"url": "https://mcp.example.com/sse"` | 远程 MCP server（HTTP SSE） |

---

## 1. CLI 命令速查

所有 MCP 管理通过 `ion rpc`（需要 `ion serve` 在跑）：

### 查询

```bash
# 列出所有 MCP server + 连接状态
ion rpc --method get_mcp_servers

# 列出所有 MCP 工具（含 mcp__server__tool 名称）
ion rpc --method mcp_list_tools
```

**响应示例（get_mcp_servers）：**
```json
{
  "success": true,
  "data": {
    "servers": [
      {"name": "everything", "status": "connected", "tools_count": 13},
      {"name": "myserver", "status": "disconnected"}
    ]
  }
}
```

**响应示例（mcp_list_tools）：**
```json
{
  "success": true,
  "data": {
    "tools": [
      {"name": "mcp__everything__echo", "description": "Echoes input"},
      {"name": "mcp__everything__get_sum", "description": "Adds two numbers"}
    ]
  }
}
```

### 管理

```bash
# 开关 MCP server（不需要重启 ion serve）
ion rpc --method mcp_toggle_server \
  --params '{"name": "everything", "enabled": false}'

# 重启 MCP server（连接断了时用）
ion rpc --method mcp_restart_server \
  --params '{"name": "everything"}'

# 热更新配置（改了 config.json 后不用重启 worker）
ion rpc --method mcp_reload
```

### Resources & Prompts

```bash
# 列出 MCP server 提供的 resources
ion rpc --method list_resources \
  --params '{"server": "everything"}'

# 列出 MCP server 提供的 prompts
ion rpc --method list_prompts \
  --params '{"server": "everything"}'

# 读取 resource
ion rpc --method read_resource \
  --params '{"server": "everything", "uri": "test://static/resource/1"}'
```

---

## 2. LLM 怎么使用 MCP 工具

ION 自动把 MCP 工具注册到 LLM 的工具列表里。LLM 看到的工具名是 `mcp__<server>__<original_tool_name>`：

```
LLM 可用工具：
  read, write, bash, edit, grep, find, ls  ← 内置工具
  mcp__everything__echo                     ← MCP 工具
  mcp__everything__get_sum                  ← MCP 工具
  mcp__github__create_issue                 ← MCP 工具
  goal_set, goal_refine, goal_diagnose     ← Goal Supervisor
```

LLM 调用 MCP 工具跟调用内置工具一样——不需要特殊语法。

---

## 3. Agent 工具限制 + MCP

### 白名单模式

Agent `.md` 里配 `tools:` 白名单时，**只保留列出的工具**，MCP 工具如果不列也会被移除：

```yaml
---
name: readonly-agent
tools:
  - read
  - grep
  - find
---
```

这个 agent 只能用 read/grep/find，**所有 MCP 工具被移除**。

### 白名单 + MCP

如果要保留 MCP 工具，需要显式列出：

```yaml
---
name: mcp-agent
tools:
  - read
  - mcp__everything__echo
  - mcp__everything__get_sum
---
```

### 黑名单模式

用 `disallowed_tools:` 禁用特定工具：

```yaml
---
name: safe-agent
disallowed_tools:
  - mcp__github__delete_repo
---
```

### 权限规则控制

除了 agent 配置，还可以用权限规则控制 MCP 工具的访问：

```json
// settings.json
{
  "permissions": {
    "rules": [
      {"pattern": {"tool": "mcp__github__*"}, "decision": "deny"},
      {"pattern": {"tool": "mcp__everything__echo"}, "decision": "allow"}
    ]
  }
}
```

---

## 4. 进程模型（方案 C 共享池）

```
ion serve (host)
  └─ McpManager（host 端，持有所有 MCP 连接）
       ├─ mcp-server-everything (子进程)
       └─ mcp-server-github (子进程)

  └─ Worker 1 ← bridge 代理 → McpManager
  └─ Worker 2 ← bridge 代理 → McpManager
  └─ Worker 3 ← bridge 代理 → McpManager
```

**关键设计**：MCP 连接只在 host 端维护一份（方案 C），所有 Worker 通过 bridge 代理调用。避免每个 Worker 各连一份 MCP server。

---

## 5. 自动重连

MCP server 断线时自动重连（指数退避）：

```
断线 → 1s 重试 → 2s → 4s → 8s → 16s → 30s（封顶）
  ↓
连接恢复 → 正常工作
  ↓
3 次失败 → emit mcp_connection_change 事件 → UI 显示断线
```

手动重连：`ion rpc --method mcp_restart_server --params '{"name":"xxx"}'`

---

## 6. 事件

| 事件 | 触发时机 | subscribe 可见 |
|------|---------|---------------|
| `mcp_connection_change` | server 连接/断线 | ✅ |
| `tool_execution_start` | MCP 工具开始执行 | ✅ |
| `tool_execution_end` | MCP 工具执行完成 | ✅ |

```bash
ion subscribe  # 实时看到 MCP 连接变化
```

---

## 7. 常见配置示例

### GitHub MCP

```json
{
  "mcp_servers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": {"GITHUB_TOKEN": "ghp_xxx"}
    }
  }
}
```

### 文件系统 MCP

```json
{
  "mcp_servers": {
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"]
    }
  }
}
```

### HTTP 远程 MCP

```json
{
  "mcp_servers": {
    "remote-api": {
      "url": "https://mcp.example.com/sse",
      "headers": {"Authorization": "Bearer xxx"}
    }
  }
}
```

---

## 8. CLI 测试

### 快速验证 MCP 连接

```bash
# 1. 起 serve
ion serve &

# 2. 检查 server 状态
ion rpc --method get_mcp_servers | jq '.data.servers'

# 3. 检查工具列表
ion rpc --method mcp_list_tools | jq '.data.tools[].name'

# 4. 列出 resources
ion rpc --method list_resources --params '{"server":"everything"}' | jq '.data'

# 5. 调用工具（通过 agent prompt 让 LLM 调）
echo "use mcp__everything__echo to say hello" | ion -p
```

### 自动化 CI

```bash
# MCP CI（37 个检查，覆盖配置/toggle/restart/错误/真实连接/共享池/场景1/权限/resources）
bash tests/mcp_ci.sh
```

---

## 9. 故障排查

| 症状 | 排查 |
|------|------|
| MCP server 显示 disconnected | `ion rpc --method mcp_restart_server --params '{"name":"xxx"}'` |
| LLM 看不到 MCP 工具 | `ion rpc --method mcp_list_tools` 确认工具是否注册 |
| Agent 配了 tools 白名单后 MCP 消失 | 白名单需显式列出 `mcp__server__tool` |
| 改了 config.json 但没生效 | `ion rpc --method mcp_reload` |
| 连接反复断开 | 检查 MCP server 进程是否稳定，看 `ion serve` 日志 |

---

## 参考

- [MCP 系统设计文档](../design/MCP_SYSTEM.md)（1701 行，Phase 1-4 完整设计）
- [MCP CI 测试](../../tests/mcp_ci.sh)（37 个检查）
- [权限系统](../design/PERMISSION_SYSTEM.md)（MCP 工具权限控制）
