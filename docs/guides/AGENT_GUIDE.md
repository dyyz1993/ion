# Agent 系统指南

> **状态：已完成** — Agent 配置 + 工具限制 + 多智能体编排全部实现。

---

## 0. 什么是 Agent

Agent 是 ION 的"角色定义"——一个 `.md` 文件，定义了 LLM 的身份、可用工具、行为约束。

```
ion --agent developer "修复 bug"
         ↑
    读 ~/.ion/agent/agents/developer.md → 解析 frontmatter + system prompt
```

每个 Agent 定义：
- **System Prompt**：LLM 的角色和行为指令（Markdown 正文）
- **工具白名单/黑名单**：这个 Agent 能用哪些工具
- **模型配置**：用什么模型、thinking level、最大轮数

---

## 1. Agent .md 格式

```markdown
---
name: developer
description: Implement code per spec
tools:
  - read
  - write
  - edit
  - bash
  - ls
disallowed_tools:
  - spawn_worker
thinking_level: low
max_turns: 50
color: green
---
You are a developer agent. Write code per the specification.

## Rules
1. Always read the file before editing.
2. Commit your work with git.
```

### Frontmatter 字段

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `name` | string | 必填 | Agent 名称（`--agent xxx` 用这个名字） |
| `description` | string | "" | 简短描述（`--list-agents` 显示） |
| `tools` | Vec\<string\> | None（全部可用） | **白名单**：只保留这些工具 |
| `disallowed_tools` | Vec\<string\> | None | **黑名单**：从可用工具里移除 |
| `model` | string | 继承 | 指定模型（如 `glm-5.2`） |
| `provider` | string | 继承 | 指定 provider |
| `thinking_level` | string | 继承 | `low` / `medium` / `high` |
| `max_turns` | u64 | 无限 | 最大对话轮数 |
| `color` | string | 继承 | 终端显示颜色 |
| `system_prompt` | string | "" | 覆盖 system prompt（不用 .md 正文） |
| `workflow` | object | None | Workflow gate 配置 |
| `initial_prompt` | string | None | spawn_worker 时的初始 prompt |

### System Prompt

`.md` 正文部分（frontmatter 之后的 Markdown）就是 System Prompt。LLM 会把它当作角色指令。

---

## 2. 内置 Agent

ION 自带 4 个内置 Agent（不需要 .md 文件）：

| Agent | 工具 | 用途 |
|-------|------|------|
| `build` | 全部 | 默认 Agent（通用编码） |
| `explore` | read, grep, find, ls | 只读探索代码 |
| `plan` | read, grep, find, ls | 规划模式（限制 edit/write/bash） |
| `improver` | 全部 | 通用改进 Agent |

```bash
ion --agent build "修复 bug"        # 默认
ion --agent explore "看看代码结构"   # 只读，不会改代码
ion --agent plan "设计新功能"        # 规划模式
```

---

## 3. 自定义 Agent

### 创建

把 .md 文件放到以下任一位置：

| 路径 | 作用域 |
|------|--------|
| `~/.ion/agent/agents/<name>.md` | 全局（所有项目可用） |
| `<project>/.ion/agents/<name>.md` | 项目级（仅当前项目） |
| `ION_AGENT_DIR` 环境变量指向的目录 | 自定义 |

### 使用

```bash
# 用自定义 agent
ion --agent my-developer "实现功能"

# 用项目级 agent
cd my-project
ion --agent project-reviewer "审查代码"

# 列出所有可用 agent（内置 + 自定义）
ion --list-agents

# spawn_worker 时指定 agent
ion --host --agent coordinator "拆任务"
```

---

## 4. 工具限制系统

### 白名单（tools）

```yaml
tools:
  - read
  - grep
  - find
```

效果：**只保留 read/grep/find，其他工具全部移除**（包括 MCP 工具）。

### 黑名单（disallowed_tools）

```yaml
disallowed_tools:
  - spawn_worker
  - bash
```

效果：移除 spawn_worker 和 bash，其他工具保留。

### 白名单 + 黑名单组合

白名单先执行（只保留列出的），黑名单后执行（从保留的里再移除）。**最终可用工具 = 白名单 ∩ (1 - 黑名单)**。

### 环境变量（只能收紧）

| 环境变量 | 作用 |
|---------|------|
| `ION_ALLOWED_TOOLS` | 与 agent.md 白名单取交集 |
| `ION_DISALLOWED_TOOLS` | 与 agent.md 黑名单取并集 |

环境变量**只能收紧**（减少工具），不能放宽（增加工具）。

### MCP 工具交互

MCP 工具命名格式：`mcp__<server>__<tool>`

| 场景 | 行为 |
|------|------|
| 白名单不含 `mcp__*` | **所有 MCP 工具被移除** |
| 白名单含 `mcp__everything__echo` | 只保留这个 MCP 工具 |
| 黑名单含 `mcp__github__delete_repo` | 只禁这一个 |
| 权限规则 `mcp__*` deny | 所有 MCP 工具被拒绝 |

### CLI flag

```bash
# 全局工具白名单
ion --tools read,grep,find "只读分析"

# 排除工具
ion --exclude-tools bash "不用 bash"
```

---

## 5. 多智能体编排

### spawn_worker（父 Agent 派子 Agent）

```
coordinator agent（父）
  ├─ spawn_worker(child, developer, task1)  → 子 Agent 用 developer.md
  ├─ spawn_worker(child, developer, task2)
  └─ spawn_worker(peer, reviewer)           → 异步审查
```

### spawn_worker 工具限制透传

spawn_worker 时可以通过 `allowed_tools` / `disallowed_tools` 参数进一步限制子 Agent 的工具：

```bash
# 让子 Agent 只有 read（比 developer.md 的白名单更严）
spawn_worker(child, developer, "审查代码", allowed_tools=["read"])
```

### Agent 切换（switch_agent）

Worker 运行中可以切换 Agent（改变 system prompt + 工具集）：

```bash
ion rpc --session xxx --method switch_agent --params '{"agent": "reviewer"}'
```

切换时重新应用 tools/disallowed_tools 限制。

---

## 6. CLI 命令

| 命令 | 作用 |
|------|------|
| `ion --agent <name> "prompt"` | 用指定 Agent 执行 |
| `ion --list-agents` | 列出所有可用 Agent |
| `ion --agent <name> --print "prompt"` | 单次执行（场景1） |
| `ion --host --agent <name> "prompt"` | 用 host 编排（场景2） |
| `ion serve` + `ion rpc --method switch_agent` | 运行中切换 Agent |
| `ION_AGENT_DIR=/path/to/agents ion --agent xxx` | 自定义 Agent 目录 |

---

## 7. Agent 模板库

`examples/agents/` 提供了 22 个现成的 Agent 模板：

| Agent | 用途 | 关键工具限制 |
|-------|------|------------|
| coordinator | 拆任务 + spawn_worker | 有 spawn_worker，无 edit/write/bash |
| developer | 写代码 | 有 edit/write/bash，无 spawn_worker |
| reviewer | 审查代码 | 有 read，无 edit/write |
| merger | 合并分支 | 有 bash(git)，无 edit/write |
| publisher | 推送 GitHub | 有 bash(gh/git) |
| orchestrator | 全流程编排 | 有 spawn_worker |
| evolver | A→B 自进化 | 只有 bash |
| goal-evolver | Goal 日志分析 + 提 Issue | read/ls/grep/find/bash |
| goal-diagnostician | 目标诊断 | read/ls/grep/find/bash |
| wf | Workflow 执行 | 有 edit/bash/spawn_worker |
| ci_runner_coordinator | CI 并行编排 | 有 spawn_worker |
| ci_runner_worker | CI 执行 | 有 bash/cargo |

---

## 8. CLI 测试

### 快速验证 Agent 工具限制

```bash
# 1. 创建只读 agent
mkdir -p ~/.ion/agent/agents
cat > ~/.ion/agent/agents/readonly-test.md << 'EOF'
---
name: readonly-test
tools: [read, grep, find]
---
You are readonly.
EOF

# 2. 起 serve
ion serve &

# 3. 用这个 agent 创建 session
SID=$(ion rpc --method create_session --params '{"agent":"readonly-test"}' | jq -r '.data.session_id')

# 4. 验证 read 可用
ion rpc --session "$SID" --method call_tool --params '{"tool":"read","args":{"file_path":"/etc/hostname"}}'

# 5. 验证 write 不可用（应该报 "tool not found"）
ion rpc --session "$SID" --method call_tool --params '{"tool":"write","args":{}}'

# 6. 验证 bash 不可用
ion rpc --session "$SID" --method call_tool --params '{"tool":"bash","args":{}}'
```

### CI 测试

```bash
# MCP + Agent 工具限制 CI（27 个检查）
bash tests/mcp_agent_tools_ci.sh

# 多智能体编排 CI
bash tests/scenario2_ci.sh

# Team 编排 E2E
bash tests/team_e2e.sh
```

---

## 9. 架构

### Agent 加载流程

```
ion --agent developer "prompt"
  ↓
1. find_agent("developer")
   ├─ 查 ~/.ion/agent/agents/developer.md
   ├─ 查 <cwd>/.ion/agents/developer.md
   └─ 查内置 agent (build/explore/plan/improver)
  ↓
2. 解析 frontmatter → AgentConfig
  ↓
3. build_tools() → 注册所有内置工具
  ↓
4. 应用工具限制：
   ├─ AgentConfig.tools → restrict_tools(白名单)
   ├─ AgentConfig.disallowed_tools → remove(黑名单)
   └─ ION_ALLOWED_TOOLS / ION_DISALLOWED_TOOLS → 环境变量收紧
  ↓
5. 构建 system prompt（.md 正文 + 环境信息注入）
  ↓
6. 创建 Agent 实例 → 运行
```

### Agent 在不同场景下的行为

| 场景 | Agent 加载 | 工具来源 |
|------|----------|---------|
| 场景1（`ion -p`） | `--agent` 参数 → build_tools | 内置工具 + MCP 直连 |
| 场景2（`ion --host`） | coordinator agent → spawn_worker | Worker 内 build_tools |
| 场景3（`ion serve`） | `create_session(agent=xxx)` | Worker 内 build_tools |

---

## 10. 故障排查

| 症状 | 排查 |
|------|------|
| `--agent xxx` 找不到 | 检查 `~/.ion/agent/agents/xxx.md` 或 `--list-agents` |
| Agent 工具比预期少 | 检查 frontmatter 的 `tools:` 白名单 |
| MCP 工具在 Agent 下不可用 | 白名单需要显式列 `mcp__server__tool` |
| switch_agent 后工具没变 | 确认 worker_rpc 正确解析了新 agent 的 tools |
| spawn_worker 的子 Agent 工具太多 | 用 `allowed_tools` 参数收紧 |

---

## 参考

- [Team 编排设计](../design/TEAM_ORCHESTRATION.md) — 多智能体编排
- [权限系统](../design/PERMISSION_SYSTEM.md) — 工具权限控制
- [MCP 用法指南](./MCP_USAGE.md) — MCP 工具 + Agent 交互
- [Agent 模板库](../../examples/agents/) — 22 个现成 Agent
- `src/agent_config.rs` — Agent 配置解析代码
- `src/bin/ion.rs` → `build_tools()` — 工具注册 + 过滤
