# Hooks 系统 实现规格

> **状态：已完成** — 补丁 1（create_worker 增强）✅ + 补丁 2（hooks 系统）✅ 均已实现并通过测试。
>
> handler 完成度：command ✅ / http ✅ / agent ✅（真能调工具）/ prompt 🔧 stub（需 call_llm）/ mcp_tool 🔧 stub（需 McpManager）
> **验证**: hooks_ci 8 + hooks_agent_ci 4 + hooks_e2e 10 + patch1 5 = 27 测试全过
>
> 本文档是**实现规格**（给写代码的人看），含数据结构、handler 执行引擎、改动清单、bug fix。
>
> - 想看"是什么/怎么用/怎么调"（无代码）→ [HOOKS_GUIDE.md](./HOOKS_GUIDE.md)
> - 想看 CLI 验证用例（Group A-H）→ [HOOKS_CLI_TEST.md](./HOOKS_CLI_TEST.md)

---

## 何时使用这个文档

- 要实现"配置式生命周期触发器"（hooks）——用户不写 Rust/WASM 就能扩展 Agent 行为时
- 要让扩展/hooks 真正能 spawn 一个"带工具 + 带 maxTurns"的子 Agent 时

> 大纲同步是用户用 hooks 搭的**用例**（不是内核能力），看 [HOOKS_GUIDE.md §7](./HOOKS_GUIDE.md)。

**前置阅读**：
- [EXTENSION_SYSTEM.md](./EXTENSION_SYSTEM.md) — WASM 扩展系统（~30 个钩子的底层）
- [HOOK_SYSTEM.md](./HOOK_SYSTEM.md) — 早期 hooks 设计稿（6 事件 + 仅 command，**已被本文档取代**）
- [CONFIG_DIMENSIONS.md](./CONFIG_DIMENSIONS.md) — 存储维度划分（outline.json 属维度③仓库内）

---

## 概览

ION 目前有两个能力短板，导致用户想做"自定义编排"时被迫写 Rust 内置扩展：

1. **扩展 API 的 `create_worker` 字段不全**（缺 agent/task/max_turns/tools），扩展 spawn 的子 Worker 拿不到工具——pi 的 hooks 系统就栽在这个坑里（它的 `agent` handler 名义上能调 agent，实际不传 tools，退化成单轮 LLM）。
2. **没有"配置式钩子"**——用户想"当某事件发生时跑个脚本"必须写 WASM/Rust 扩展，门槛太高。`HOOK_SYSTEM.md` 设计了 `hooks.json` 但标了"暂不开发"。

本文档补齐这两块。补完后，**用户通过一个 `hooks.json` + 几个 shell 脚本就能实现"扫描 MD → 检测大纲不同步 → 调 Agent 更新 → 注入到每个会话"这样的完整编排**，0 行 Rust 代码。

**对齐 pi 的 `extensions/pi-hooks/`**（12 事件 + 5 handler 类型），但修掉 pi 的两个坑：agent handler 真传 tools（依赖补丁 1），mcp_tool handler 不留 no-op。

| 能力 | 入口 | 状态 |
|------|------|------|
| `ExtensionWorkerConfig` 字段补齐（agent/task/max_turns/tools/worktree） | `ExtensionApi::create_worker()` | ✅ 已实现 |
| Hooks 配置系统（`hooks.json` 全局 + 项目级合并） | `~/.ion/hooks.json` + `<project>/.ion/hooks.json` | ✅ 已实现 |
| 12 事件映射 → Extension trait | `HookExtension`（内置 Rust 扩展） | ✅ 已实现 |
| 5 种 handler 执行引擎 | command ✅ / http ✅ / prompt 🔧 / **agent ✅(真传 tools)** / mcp_tool 🔧 | ✅ 已实现（3/5 完整） |

> **大纲同步不是内核能力**——它是用户用上述 hooks 能力搭出的一个用例，见 [HOOKS_GUIDE.md §7](./HOOKS_GUIDE.md)。内核不内化任何 outline/MD 扫描业务逻辑。

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | `ExtensionWorkerConfig` 补 agent/initial_prompt/worktree 字段 | 🔧 | `cargo test extension_worker_config` |
| 1.2 | `create_worker` 实现透传新字段到 Manager | 🔧 | FauxProvider harness：spawn 的子 Worker 带 tools |
| 2.1 | `HooksConfig` 数据结构 + 全局/项目级合并 | 🔧 | `ion hooks validate .ion/hooks.json` |
| 2.2 | `HookExtension` 注册到 ExtensionRegistry | 🔧 | `ion hooks list` |
| 2.3 | command handler（spawn bash + stdin/stdout/退出码） | 🔧 | hooks_ci：扫描脚本注入测试 |
| 2.4 | agent handler（spawn 带 tools 的子 Worker） | 🔧 | hooks_ci：子 Worker 真改文件 |
| 2.5 | 12 事件全部触发 | 🔧 | hooks_ci：每事件至少 1 case |

> 大纲同步用例的验证见 [HOOKS_CLI_TEST.md](../testing/HOOKS_CLI_TEST.md) Group D/G，不属于内核测试。

---

## 第 1 章 背景与对标分析

### 1.1 pi 的 kb（知识库）真相

调研 pi 源码（`/Users/xuyingzhou/Project/temporary/pi-momo-fork/`）发现，"kb"在 pi 里是**两层东西**：

| 层 | 是什么 | 位置 |
|---|---|---|
| 源文档 | 手写的设计/指南 MD，给 LLM 读 | `.opencode/kb/*.md` |
| 同步脚本 | 通过 MCP 协议调 kb-mcp（npm 包 `@dyyz1993/kb-mcp`），把 MD 同步到向量检索库 | `scripts/kb-*.mjs` |
| 向量库服务 | 独立 MCP server（三层搜索：文本/TF-IDF/语义向量），提供 `kb_read/kb_update/kb_search` 工具 | npm 包，mcp.servers 配置接入 |

**pi 的 kb 模式 = "MD 文档 → 向量库 → 语义检索"**。重点是检索。

### 1.2 本文档的目标模式（与 pi 不同）

用户的需求不是语义检索，而是**大纲索引同步**：

```
MD 文档（docs/**/*.md）
    ↕  双向同步
outline 大纲（.ion/outline.json）
    ↓  注入
每个会话的 system prompt
```

| 对比项 | pi kb 模式 | 本文档模式 |
|--------|-----------|-----------|
| 核心动作 | MD → 向量库（语义嵌入） | MD ↔ outline（mtime/hash 索引） |
| 检索方式 | kb_search 语义搜索 | 直接读 outline.json |
| 触发更新 | 手动跑 kb-*.mjs 脚本 | hooks 自动触发（SubagentStop/UserPromptSubmit） |
| 存储位置 | kb-mcp 内部（独立服务） | `<project>/.ion/outline.json`（仓库内，可 git 追踪） |
| 注入方式 | LLM 主动调 kb_search 工具 | UserPromptSubmit 钩子自动注入 additionalContext |

**结论：不照搬 pi 的向量库，做"大纲索引同步"。** 但**借鉴 pi 的 hooks 机制**来驱动同步。

### 1.3 pi 的 hooks 真相（实证）

pi 的 hooks 已经**完整实现**（不是设计稿），在 `packages/coding-agent/extensions/pi-hooks/`（~740 行核心）：

- **5 种 handler 类型**：`command` / `http` / `prompt` / `agent` / `mcp_tool`
- **12 个事件点**：SessionStart / Setup / SessionEnd / PreCompact / UserPromptSubmit / PreToolUse / PostToolUse / PostToolUseFailure / PermissionRequest / SubagentStart / SubagentStop / Stop / Notification
- **配置来源**：5 个文件合并（`.claude/settings.json` + `.pi/agent/settings.json` 等）
- **block 语义**：Stop 被 block → reason 作为新 query 注入（`pi.sendMessage(reason, {deliverAs:"followUp"})`）

**pi 的两个坑（ION 要修）**：

| 坑 | pi 的实现 | 后果 | ION 的修法 |
|---|---|---|---|
| `agent` handler 名不副实 | `runAgentHandler` 调 `callLLM` 时**不传 tools** → 退化成单轮 LLM | "可以用工具验证"的注释是假的 | 依赖补丁 1，agent handler 真传 tools + maxTurns |
| `mcp_tool` handler 是 no-op | 直接返回 `{exitCode:0, stdout:""}` | 配了等于没配 | 真接 McpManager.call_tool |

### 1.4 ION `HOOK_SYSTEM.md` 现状

早期设计稿（本文档取代它）：
- 只有 **6 个事件**（SessionStart/UserPromptSubmit/PreToolUse/PostToolUse/Stop/Notification）
- 只有 **1 种 handler**（command）
- 标"状态：暂不开发"，代码零实现

**本文档取两者之长**：实现完整 hooks（事件数对齐 pi 的 12 个，handler 支持 command/http/prompt/**agent(做对)**/mcp_tool），但用 ION 的 Extension trait 和 Rust 生态重写。

---

## 第 2 章 内核补丁 1 — create_worker 能力补齐（前置）✅ 已实现

> **状态：已实现并通过测试** — `ExtensionWorkerConfig` 补 7 字段，`create_worker` 实现补透传，子进程通过 `ION_ALLOWED_TOOLS`/`ION_DISALLOWED_TOOLS`/`ION_MAX_TURNS` 环境变量消费。
> 测试：`tests/patch1_worker_config.rs`（5 passed）+ `tests/manager_integration.rs`（25 passed 未破坏）。

### 2.1 问题

扩展 API 的 `ExtensionWorkerConfig`（[src/worker_api.rs:17](../../src/worker_api.rs#L17)）只有 5 个字段：

```rust
// 现状（缺字段）
pub struct ExtensionWorkerConfig {
    pub session: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub channels: Option<Vec<String>,
    pub parent: Option<String>,
}
```

而 Manager 端的 `WorkerCreateConfig`（[src/worker_registry.rs:2027](../../src/worker_registry.rs#L2027)）**已经有** `agent` / `initial_prompt` / `worktree` / `relation` / `report_channel` 等字段。问题在于：

1. `ExtensionWorkerConfig` 没有这些字段 → 扩展拿不到入口去配
2. `ExtensionApi::create_worker` 实现（[src/worker_api.rs:171](../../src/worker_api.rs#L171)）只透传了 5 个字段到 Manager → Manager 已有的能力被堵死

**对比 LLM 工具**：LLM 的 `spawn_worker` 工具（[src/agent/tool.rs:995](../../src/agent/tool.rs#L995)）走的是另一条链路（`Runtime::spawn_worker` + `SpawnWorkerRequest`），**已经支持 agent/task/worktree**。所以 LLM 能 spawn 带工具的子 Worker，扩展却不能——这是不对等的。

### 2.2 改动

**文件**：[src/worker_api.rs](../../src/worker_api.rs)

```rust
/// 扩展创建子 Worker 的配置（补齐后）。
/// 与 Manager 端 WorkerCreateConfig 对齐，字段透传。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ExtensionWorkerConfig {
    pub session: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub channels: Option<Vec<String>>,
    pub parent: Option<String>,

    // ── 新增字段（补丁 1）──
    /// Agent 角色（对应 .ion/agents/<name>.md）。必填如果要让子 Worker 有工具循环。
    pub agent: Option<String>,
    /// 子 Worker 的初始 prompt（任务描述）。
    pub initial_prompt: Option<String>,
    /// Worktree 隔离配置。Some → 在独立 git worktree 里跑。
    pub worktree: Option<WorktreeConfig>,
    /// 与创建者的关系。默认 Child。
    pub relation: Option<String>,  // "child" | "peer"
    /// 允许的工具白名单（None = 继承全部）。
    pub allowed_tools: Option<Vec<String>>,
    /// 禁用的工具黑名单。
    pub disallowed_tools: Option<Vec<String>>,
    /// 最大 turn 数（None = 继承 host 默认，通常是无限）。
    pub max_turns: Option<u32>,
}
```

**`create_worker` 实现补透传**（[src/worker_api.rs:171](../../src/worker_api.rs#L171)）：

```rust
pub async fn create_worker(&self, config: ExtensionWorkerConfig) -> Result<WorkerHandle, String> {
    if let Some(ref bridge) = self.bridge {
        // 把所有字段透传给 Manager 的 create_worker 命令
        let params = serde_json::json!({
            "session": config.session,
            "model": config.model,
            "provider": config.provider,
            "channels": config.channels,
            "parent": config.parent,
            // ── 新增透传 ──
            "agent": config.agent,
            "initial_prompt": config.initial_prompt,
            "worktree": config.worktree,
            "relation": config.relation,
            "allowed_tools": config.allowed_tools,
            "disallowed_tools": config.disallowed_tools,
            "max_turns": config.max_turns,
        });
        let resp = bridge.send_command("create_worker", params).await?;
        // ...（其余不变）
    }
}
```

**Manager 端不用改**——`WorkerCreateConfig` 用 `serde_json::from_value` 反序列化，新字段自动消费。但需要确认 Manager 端把 `allowed_tools`/`disallowed_tools`/`max_turns` 传给子 Worker 进程（通过环境变量或 RPC 初始化参数）。

### 2.3 子 Worker 进程消费新字段

子 Worker 进程（`ion-worker`）启动时需要接收 `allowed_tools` / `disallowed_tools` / `max_turns`，在 Agent 循环里生效：

- `allowed_tools` → ToolRegistry 过滤（白名单）
- `disallowed_tools` → ToolRegistry 过滤（黑名单，对齐已有逻辑，AGENTS.md 提到"disallowed_tools 黑名单生效"已修）
- `max_turns` → Agent 循环退出条件（对齐 `--max-turns` flag）

### 2.4 意义

这个补丁是**第 3 章 hooks 的 agent handler 能"真调工具"的前提**。不补，agent handler 就会重蹈 pi 覆辙——调 `callLLM` 不传 tools，退化成单轮 LLM。

### 2.5 改动文件清单

| 文件 | 改动 | 行数 |
|------|------|------|
| `src/worker_api.rs` | `ExtensionWorkerConfig` 补字段 + `create_worker` 补透传 | ~30 |
| `src/worker_registry.rs` | `WorkerCreateConfig` 补 `allowed_tools`/`disallowed_tools`/`max_turns`（如果缺）+ spawn 时传给子进程 | ~40 |
| `src/bin/ion_worker.rs` | 子 Worker 启动时接收并应用新字段 | ~50 |
| `tests/` | harness 测试 | ~30 |
| **小计** | | **~150** |

---

## 第 3 章 内核补丁 2 — Hooks 系统（核心，~700 行）

### 3.1 配置文件

**路径**（对齐 pi 的多文件合并模式，也对齐 ION 的 CONFIG_DIMENSIONS）：

```
~/.ion/hooks.json              ← 全局 hooks（对本机所有项目生效，维度①）
<project>/.ion/hooks.json      ← 项目级 hooks（仅当前项目，维度③，可 git 追踪）
```

多个文件**合并执行**（同事件名的 hooks 拼接，不是覆盖）。

**任一文件设 `"disableAllHooks": true` → 全局禁用**（紧急逃生阀，对齐 pi config-loader.ts:65）。

### 3.2 配置格式

**文件**：`src/hooks/mod.rs`（新增模块）

```rust
use std::collections::HashMap;

/// hooks.json 的 Rust 映射
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct HooksConfig {
    #[serde(default)]
    pub version: u32,  // 默认 1
    #[serde(default)]
    pub disable_all_hooks: bool,  // 紧急逃生阀
    /// 事件名 → Hook 组列表
    #[serde(default)]
    pub hooks: HashMap<String, Vec<HookEntry>>,
}

/// 一个事件下的一条配置：可以是单个 handler，也可以是带 matcher 的 group
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
pub enum HookEntry {
    /// 单个 handler（简写形式）
    Handler(HookHandler),
    /// 带 matcher 的 group（PreToolUse/PostToolUse 按 matcher 过滤工具名）
    Group(HookGroup),
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HookGroup {
    /// 正则匹配工具名（仅 PreToolUse/PostToolUse 有效）。None 或 "*" = 全匹配
    #[serde(default)]
    pub matcher: Option<String>,
    /// Stop 事件的循环阻断上限（默认 5）
    #[serde(default)]
    pub loop_limit: Option<u32>,
    pub hooks: Vec<HookHandler>,
}

/// 单个 handler 的定义
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct HookHandler {
    #[serde(rename = "type")]
    pub handler_type: HandlerType,
    /// command 类型必填
    pub command: Option<String>,
    /// http 类型必填
    pub url: Option<String>,
    /// prompt / agent 类型必填
    pub prompt: Option<String>,
    /// mcp_tool 类型必填
    pub server: Option<String>,
    pub tool: Option<String>,

    // ── 通用可选字段 ──
    pub input: Option<serde_json::Value>,
    pub model: Option<String>,
    pub timeout: Option<u32>,            // 秒，默认 30
    pub if_clause: Option<String>,       // 条件表达式（对齐 pi if-parser）
    pub r#async: Option<bool>,           // 仅 PreToolUse 生效，后台跑不阻塞
    pub async_rewake: Option<bool>,      // async + exit 2 时把 reason 作为 nextTurn 注入
    pub once: Option<bool>,              // 只触发一次
    pub status_message: Option<String>,  // 执行时给 UI 的状态文案
    pub allowed_tools: Option<Vec<String>>,    // agent 类型：子 Worker 工具白名单
    pub max_turns: Option<u32>,               // agent 类型：子 Worker 最大 turn 数
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandlerType {
    Command,
    Http,
    Prompt,
    Agent,
    McpTool,
}
```

**配置示例**（完整可用）：

```json
{
  "version": 1,
  "hooks": {
    "UserPromptSubmit": [
      {
        "type": "command",
        "command": "bash .ion/scripts/inject_outline.sh",
        "timeout": 5
      }
    ],
    "SubagentStop": [
      {
        "type": "agent",
        "prompt": "检查 docs/ 下 MD 与 .ion/outline.json 的同步状态，不同步就更新大纲",
        "model": "fast",
        "max_turns": 100,
        "allowed_tools": ["read", "write", "edit", "bash", "grep", "find"]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "bash|write|edit",
        "hooks": [
          {
            "type": "command",
            "command": "bash .ion/scripts/freeze_guard.sh",
            "timeout": 5
          }
        ]
      }
    ]
  }
}
```

### 3.3 12 个事件映射

对齐 pi 的 12 个事件，映射到 ION 的 Extension trait 方法：

| pi hook 事件名 | ION Extension trait 方法 | stdin 关键字段 | block 能力 |
|---|---|---|---|
| `SessionStart` | `on_session_start(&SessionContext)` | session_id, reason, cwd | ❌（非阻断） |
| `Setup` | `on_session_start`（reason=="startup" 时额外触发） | 同上 | ❌ |
| `SessionEnd` | `on_session_shutdown(&SessionContext)` | session_id | ❌ |
| `PreCompact` | `on_session_before_compact(&mut Vec<Message>)` | session_id, message_count | ❌ |
| `UserPromptSubmit` | `on_input(&mut InputContext)` | session_id, prompt | ✅ block（decision="block"） |
| `PreToolUse` | `before_tool_call(&ToolCall)` | tool_name, tool_input, tool_use_id | ✅ deny/ask/allow + 改参数 |
| `PostToolUse` | `after_tool_call(&ToolCall, &ToolResult)` | tool_name, tool_input, tool_response | ✅ block（通知 LLM） |
| `PostToolUseFailure` | `after_tool_call`（result.is_error 时） | 同上 + error | ✅ block |
| `PermissionRequest` | `on_permission_request(&str, &Value)` | tool, args | ✅ deny |
| `SubagentStart` | `on_agent_start(&AgentContext)` | agent_type, session_id | ❌ |
| `SubagentStop` | `on_agent_end(&AgentContext)` | session_id, last_message, loop_count | ✅ block（reason 作为新 query） |
| `Stop` | `on_turn_end(&TurnContext)` | session_id, last_assistant_message | ✅ block（reason 作为新 query） |
| `Notification` | `on_message_end(...)`（异步派生） | notification_type, message | ❌（异步非阻断） |

**block 后的语义**（对齐 pi）：
- `Stop`/`SubagentStop` 被 block → reason 通过 `inject_follow_up` 重新注入对话，让模型继续
- `UserPromptSubmit` 被 block → 返回错误消息给用户，不进入 agent 处理
- `PreToolUse` exit 2 → deny；exit 3 → 弹出 UI 确认（走 PermissionEngine）
- `loop_count >= loop_limit` → 跳过该 HookGroup（防死循环）

### 3.4 Handler 执行引擎

**文件**：`src/hooks/handler_runner.rs`（新增）

入口函数：

```rust
/// 执行单个 handler，返回结构化结果
pub async fn run_handler(
    handler: &HookHandler,
    stdin_data: serde_json::Value,
    ctx: &HookExecContext,
) -> HookResult {
    match handler.handler_type {
        HandlerType::Command => run_command(handler, stdin_data, ctx).await,
        HandlerType::Http    => run_http(handler, stdin_data, ctx).await,
        HandlerType::Prompt  => run_prompt(handler, stdin_data, ctx).await,
        HandlerType::Agent   => run_agent(handler, stdin_data, ctx).await,  // ⭐ 依赖补丁 1
        HandlerType::McpTool => run_mcp_tool(handler, stdin_data, ctx).await,
    }
}

pub enum HookResult {
    /// 退出码 0，stdout 解析成功
    Continue { output: HookOutput },
    /// 退出码 2，阻断性错误
    Block { reason: String },
    /// 退出码 3（仅 PreToolUse），要求用户确认
    Ask { reason: String },
    /// 其他退出码，忽略
    Ignore,
}

pub enum HookOutput {
    /// stdout 是 JSON（结构化控制：additionalContext/permissionDecision/updatedInput...）
    Json(serde_json::Value),
    /// stdout 是纯文本（仅 SessionStart/UserPromptSubmit 当作 additionalContext）
    Text(String),
}
```

#### 3.4.1 command handler

```rust
/// spawn bash -c <command>，stdin 写 JSON，按退出码 0/2/3 解释
async fn run_command(handler: &HookHandler, stdin_data: Value, ctx: &HookExecContext) -> HookResult {
    let cmd = handler.command.as_ref().ok_or("command handler missing command")?;
    let timeout = Duration::from_secs(handler.timeout.unwrap_or(30) as u64);

    // 变量替换（对齐 pi handler-runner.ts:35-41）
    let cmd = cmd
        .replace("$CLAUDE_PROJECT_DIR", &ctx.project_dir)
        .replace("$TOOL", &ctx.tool_name.unwrap_or_default())
        .replace("$BASH_COMMAND", &ctx.bash_command.unwrap_or_default());

    let mut child = tokio::process::Command::new("bash")
        .arg("-c").arg(&cmd)
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
        .env("ION_HOOK_EVENT", &ctx.event_name)
        .env("CLAUDE_PROJECT_DIR", &ctx.project_dir)  // 兼容 pi 脚本
        .spawn()?;

    // stdin 写 JSON
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(&serde_json::to_vec(&stdin_data)?).await?;
    }

    // 超时等待
    let output = tokio::time::timeout(timeout, child.wait_with_output()).await??;

    interpret_exit_code(&output, handler)  // 0→Continue, 2→Block, 3→Ask, else→Ignore
}
```

**退出码协议**（Claude Code 兼容）：

| 退出码 | 含义 | stdout 处理 |
|--------|------|------------|
| 0 | 正常 | 尝试 JSON.parse，失败当纯文本 |
| 2 | 阻断 | reason 取 stderr 或 JSON.permissionDecisionReason |
| 3 | 请求确认（仅 PreToolUse） | 走 PermissionEngine UI 确认 |
| 其他 | 非阻断错误 | 忽略 stdout/stderr |

#### 3.4.2 http handler

```rust
/// POST stdin_data 到 url，强制 HTTPS + 拒私网 IP（对齐 pi runHttpHandler）
async fn run_http(handler: &HookHandler, stdin_data: Value, ctx: &HookExecContext) -> HookResult {
    let url = handler.url.as_ref().ok_or("http handler missing url")?;
    // 安全校验：必须 https://，拒绝 127.0.0.1/10.*/172.16.*/192.168.*
    validate_url(url)?;

    let resp = ctx.http_client
        .post(url)
        .json(&stdin_data)
        .headers(handler.headers.clone().unwrap_or_default())
        .timeout(Duration::from_secs(handler.timeout.unwrap_or(30) as u64))
        .send().await?;

    match resp.status().as_u16() {
        200 => HookResult::Continue { output: parse_resp_body(&resp).await },
        403 => HookResult::Block { reason: resp.text().await.unwrap_or_default() },
        _   => HookResult::Ignore,
    }
}
```

#### 3.4.3 prompt handler

```rust
/// 调 callLLM 单轮判断（无工具，对齐 pi runPromptHandler）
/// 适合"这段输入是否合规"这类简单判断
async fn run_prompt(handler: &HookHandler, stdin_data: Value, ctx: &HookExecContext) -> HookResult {
    let prompt = handler.prompt.as_ref().ok_or("prompt handler missing prompt")?;
    let system = format!("{}\n\n---\nHook context:\n{}", prompt, serde_json::to_string_pretty(&stdin_data)?);

    let result = ctx.call_llm(CallLLMRequest {
        system_prompt: Some(system),
        messages: vec![Message::user("Respond with JSON: {\"block\": false} or {\"block\": true, \"reason\": \"...\"}")],
        model: handler.model.clone(),
        max_tokens: Some(1024),  // 不传 tools
        ..Default::default()
    }).await?;

    parse_block_decision(&result.text)  // 解析 LLM 返回的 JSON 决策
}
```

#### 3.4.4 agent handler ⭐（修 pi 的坑）

```rust
/// spawn 一个带 tools + maxTurns 的子 Worker（依赖补丁 1）
/// 适合"检查 + 更新"这类需要多轮工具调用的重任务
async fn run_agent(handler: &HookHandler, stdin_data: Value, ctx: &HookExecContext) -> HookResult {
    let prompt = handler.prompt.as_ref().ok_or("agent handler missing prompt")?;

    // ⭐ 关键：用补丁 1 补齐的 ExtensionWorkerConfig，真传 agent + allowed_tools + max_turns
    let config = ExtensionWorkerConfig {
        agent: handler.agent.clone().or(Some("default".into())),
        initial_prompt: Some(format!("{}\n\n---\nHook context:\n{}", prompt, serde_json::to_string_pretty(&stdin_data)?)),
        model: handler.model.clone(),
        allowed_tools: handler.allowed_tools.clone(),
        max_turns: handler.max_turns.or(Some(50)),
        relation: Some("child".into()),
        ..Default::default()
    };

    let worker = ctx.extension_api.create_worker(config).await?;

    // 等子 Worker 跑完（或超时）
    let result = tokio::time::timeout(
        Duration::from_secs(handler.timeout.unwrap_or(300) as u64),
        worker.wait_for_completion(),
    ).await;

    match result {
        Ok(Ok(output)) => {
            // 解析子 Worker 的最终输出，判断 block 与否
            parse_block_decision(&output.text)
        }
        Ok(Err(e)) => HookResult::Ignore,  // 子 Worker 失败不阻断主流程
        Err(_) => {
            let _ = worker.kill().await;
            HookResult::Ignore  // 超时不阻断
        }
    }
}
```

**与 pi 的对比**：

| | pi 的 agent handler | ION 的 agent handler |
|---|---|---|
| 调用方式 | `callLLM({systemPrompt, messages, maxTurns:50, maxTokens:4096})` | `create_worker({agent, allowed_tools, max_turns})` |
| 传 tools？ | ❌ 不传 | ✅ 传 `allowed_tools` |
| 有工具循环？ | ❌ 退化成单轮 | ✅ 真工具循环 |
| 能改文件？ | ❌ 不能 | ✅ 能（带 write/edit/bash） |

#### 3.4.5 mcp_tool handler

```rust
/// 调 MCP server 的工具（修 pi 的 no-op）
async fn run_mcp_tool(handler: &HookHandler, stdin_data: Value, ctx: &HookExecContext) -> HookResult {
    let server = handler.server.as_ref().ok_or("mcp_tool handler missing server")?;
    let tool = handler.tool.as_ref().ok_or("mcp_tool handler missing tool")?;

    // 合并 input 配置 + stdin_data
    let mut args = handler.input.clone().unwrap_or(serde_json::json!({}));
    if let Some(obj) = args.as_object_mut() {
        if let Some(stdin_obj) = stdin_data.as_object() {
            obj.extend(stdin_obj.clone());
        }
    }

    let result = ctx.mcp_manager.call_tool(server, tool, args).await?;
    // 解析返回，判断 block
    parse_mcp_result(&result)
}
```

### 3.5 HookExtension（内置 Rust 扩展）

**文件**：`src/hooks/extension.rs`（新增）

实现 `Extension` trait，在 12 个钩子点查配置 + 执行匹配的 handler。

```rust
pub struct HookExtension {
    config: Arc<RwLock<HooksConfig>>,           // hooks.json（全局+项目级合并）
    config_signature: Arc<Mutex<String>>,        // 配置变更检测（对齐 pi getConfigSignature）
    loop_counts: Arc<Mutex<HashMap<String, u32>>>, // Stop 循环计数（per session）
    project_dir: PathBuf,
}

#[async_trait]
impl Extension for HookExtension {
    fn name(&self) -> &str { "hooks" }

    // ── Session lifecycle ──
    async fn on_session_start(&self, ctx: &SessionContext) -> AgentResult<()> {
        self.process_event("SessionStart", build_session_stdin(ctx)).await?;
        if ctx.reason == "startup" {
            self.process_event("Setup", build_session_stdin(ctx)).await?;
        }
        Ok(())
    }
    async fn on_session_shutdown(&self, ctx: &SessionContext) -> AgentResult<()> {
        self.process_event("SessionEnd", build_session_stdin(ctx)).await
    }
    async fn on_session_before_compact(&self, msgs: &mut Vec<Message>) -> AgentResult<()> {
        let stdin = serde_json::json!({"message_count": msgs.len()});
        self.process_event("PreCompact", stdin).await
    }

    // ── Input ──
    async fn on_input(&self, ctx: &mut InputContext) -> AgentResult<()> {
        let stdin = serde_json::json!({"prompt": ctx.text});
        match self.process_event("UserPromptSubmit", stdin).await? {
            HookOutcome::Block { reason } => {
                ctx.handled = true;  // 吞掉输入
                ctx.text = format!("[blocked by hook] {}", reason);
            }
            HookOutcome::Continue { additional_context: Some(text) } => {
                ctx.text = format!("{}\n\n---\n{}", ctx.text, text);  // 注入附加上下文
            }
            _ => {}
        }
        Ok(())
    }

    // ── Tool ──
    async fn before_tool_call(&self, call: &ToolCall) -> AgentResult<()> {
        let stdin = serde_json::json!({"tool_name": call.name, "tool_input": call.arguments});
        match self.process_event_filtered("PreToolUse", &call.name, stdin).await? {
            HookOutcome::Block { reason } => {
                return Err(AgentError::Permission(format!("tool blocked by hook: {}", reason)));
            }
            HookOutcome::Ask { reason } => {
                // 走 PermissionEngine UI 确认（对齐 pi index.ts:402-455）
                self.request_permission(&call.name, &reason).await?;
            }
            _ => {}
        }
        Ok(())
    }
    async fn after_tool_call(&self, call: &ToolCall, result: &ToolResult) -> AgentResult<()> {
        let event = if result.is_error { "PostToolUseFailure" } else { "PostToolUse" };
        let stdin = serde_json::json!({
            "tool_name": call.name,
            "tool_input": call.arguments,
            "tool_response": result.content,
        });
        self.process_event_filtered(event, &call.name, stdin).await?;
        Ok(())
    }

    // ── Agent / Turn ──
    async fn on_agent_start(&self, ctx: &AgentContext) -> AgentResult<()> {
        self.process_event("SubagentStart", build_agent_stdin(ctx)).await
    }
    async fn on_agent_end(&self, ctx: &AgentContext) -> AgentResult<()> {
        let stdin = build_agent_stdin(ctx);
        match self.process_event("SubagentStop", stdin).await? {
            HookOutcome::Block { reason } => {
                // ⭐ 对齐 pi：reason 作为新 query 注入，让 subagent 继续
                self.inject_follow_up(Message::user(reason))?;
            }
            _ => {}
        }
        Ok(())
    }
    async fn on_turn_end(&self, ctx: &TurnContext) -> AgentResult<()> {
        let stdin = serde_json::json!({"last_assistant_message": ctx.last_message});
        match self.process_event("Stop", stdin).await? {
            HookOutcome::Block { reason } => {
                self.inject_follow_up(Message::user(reason))?;  // 阻断停止，reason 作为新 query
            }
            _ => {}
        }
        Ok(())
    }

    // ── System prompt（注入 additionalContext 的另一条路）──
    async fn on_system_prompt(&self, prompt: &mut String) -> AgentResult<()> {
        // SessionStart/UserPromptSubmit 的 additionalContext 累积注入
        if let Some(extra) = self.drain_pending_context() {
            prompt.push_str(&format!("\n\n---\n{}", extra));
        }
        Ok(())
    }
}
```

`process_event` 核心逻辑：

```rust
async fn process_event(&self, event: &str, stdin: Value) -> AgentResult<HookOutcome> {
    let config = self.config.read().await;
    if config.disable_all_hooks { return Ok(HookOutcome::Pass); }

    // 配置变更检测（对齐 pi：签名变了就 reload）
    self.check_config_reload().await;

    let entries = config.hooks.get(event).cloned().unwrap_or_default();
    let mut outcome = HookOutcome::Pass;

    for entry in entries {
        let handlers = match entry {
            HookEntry::Handler(h) => vec![h],
            HookEntry::Group(g) => g.hooks,
        };
        for handler in handlers {
            // once 去重 / if 条件过滤
            if !self.should_run(&handler)? { continue; }

            let result = run_handler(&handler, stdin.clone(), &self.exec_ctx()).await;
            outcome = outcome.merge(result_to_outcome(result));
            if outcome.is_terminal() { break; }  // block 后停止后续 handler
        }
    }
    Ok(outcome)
}
```

### 3.6 注册

**文件**：[src/bin/ion.rs](../../src/bin/ion.rs)（启动时）

```rust
// 启动时加载 hooks.json 并注册 HookExtension
let hook_config = HookExtension::load(
    project_dir.as_deref(),  // 项目级 .ion/hooks.json
).unwrap_or_default();
if !hook_config.is_empty() {
    ext_reg.register(Box::new(hook_config));
    tracing::info!("[hooks] loaded {} events", hook_config.event_count());
}
```

**热重载策略**：`process_event` 每次**动态读 hooks.json，不缓存**（见 3.5 的 `load_fresh`）。这是"改完即生效"的根本——不用文件监听，不用签名检测，每次触发读最新文件。hooks.json 通常几十行，读+解析微秒级，相比 handler spawn 的几十 ms 可忽略。

### 3.7 stdin 协议（Claude Code 兼容）

**文件**：`src/hooks/stdin_builder.rs`（新增）

所有事件 stdin 的通用字段：

```json
{
  "session_id": "string",
  "cwd": "/path/to/workspace",
  "hook_event_name": "PreToolUse",
  "workspace_roots": ["/path/to/workspace"]
}
```

各事件附加字段（对齐 pi stdin-builder.ts）：

| 事件 | 附加字段 |
|------|---------|
| PreToolUse / PostToolUse | `tool_use_id`, `tool_name`, `llm_tool_name`, `tool_input`, `tool_response`(PostToolUse) |
| UserPromptSubmit | `prompt` |
| Stop / SubagentStop | `stop_hook_active`, `loop_count`, `last_assistant_message` |
| Notification | `notification_type`, `message`, `tool_use_id` |

### 3.8 改动文件清单

| 文件 | 内容 | 行数 |
|------|------|------|
| `src/hooks/mod.rs` | HooksConfig / HookEntry / HookGroup / HookHandler 数据结构 | ~100 |
| `src/hooks/config_loader.rs` | 全局+项目级合并加载 + 签名检测 | ~80 |
| `src/hooks/handler_runner.rs` | 5 种 handler 执行 + interpret_exit_code | ~250 |
| `src/hooks/stdin_builder.rs` | 各事件 stdin 组装 | ~60 |
| `src/hooks/extension.rs` | HookExtension（12 个 trait 方法实现） | ~180 |
| `src/hooks/matcher.rs` | matcher 正则 + if 条件解析 | ~50 |
| `src/bin/ion.rs` | 启动注册 HookExtension | ~15 |
| **小计** | | **~735** |

---

## 第 4 章 hooks 在三种场景下的行为（基础设施属性）

> ⚠️ 本章只讲 **hooks 系统本身**在各场景的基础设施行为。
> **大纲同步是用户用 hooks 搭的用例**，其"后台更新/多会话去重/消息通知"等业务逻辑不属于内核，见 [HOOKS_GUIDE.md §7](./HOOKS_GUIDE.md)。

### 4.1 三场景行为

| 场景 | hooks 触发 | agent handler 的子 Worker | 说明 |
|------|-----------|--------------------------|------|
| 场景 1（直接执行） | ✅ 正常触发 | 能 spawn，但进程退出子 Worker 被干掉 | hooks 是被动的，场景 1 照常触发，只是没 host 兜底 |
| 场景 2（--host） | ✅ 正常触发 | host 兜着，临时存活期间能跑 | 适合需要子 Worker 的 hooks |
| 场景 3（serve） | ✅ 正常触发 | 常驻 host，最适合 | 同上 |

**关键**：hooks 系统本身**不区分场景**——它在所有场景都正常加载和触发。场景差异只影响 `agent` handler spawn 的子 Worker 生命周期（场景 1 进程退出就没了），这是 WorkerRegistry 已有的行为，hooks 不额外处理。

### 4.2 内核不内化任何业务逻辑

hooks 系统只提供**机制**，不提供**策略**：

| 内核提供（机制） | 用户自行决定（策略） |
|----------------|---------------------|
| 12 个事件触发点 | 在哪些事件挂 handler |
| 5 种 handler 执行引擎 | 用 command 还是 agent |
| create_worker（带 tools/maxTurns） | spawn 什么 agent、干什么活 |
| emit_extension_event | 发什么事件、data 里放什么 |
| ION_ALLOWED_TOOLS 环境变量 | 限制哪些工具 |
| async + asyncRewake | 要不要后台跑 |

**内核不包含**：outline 格式定义、MD 扫描逻辑、mtime/hash 对比、"outline_synced" 事件、outline-syncer agent、任何特定业务的状态机。这些都是用户用 hooks.json + 脚本搭的。
    }
  }
}
```


---

> **第 5-7 章已拆分**：
> - 参考用例（MD↔Outline 大纲同步）→ [HOOKS_GUIDE.md §7](./HOOKS_GUIDE.md)
> - 完整 CLI 子命令族 + 接口规格 → [HOOKS_GUIDE.md §5](./HOOKS_GUIDE.md) + [HOOKS_CLI_TEST.md](./HOOKS_CLI_TEST.md)
> - CLI 测试 Group A-H → [HOOKS_CLI_TEST.md](./HOOKS_CLI_TEST.md)

---
```bash
ion --host "test"  # 场景 2
# 同步中时 Ctrl+C
ps aux | grep ion-worker  # 无僵尸进程
```
- ✅ 子 Worker 被 cleanup

---

## 第 8 章 后续工作

| # | 待办 | 优先级 | 说明 |
|---|------|--------|------|
| 1 | 定时器/cron | P2 | 当前不做，靠事件触发近似。若需求出现，加 `schedule_cron` 内核能力（属"如果两个无关扩展都想做，就该进内核"） |
| 2 | hooks 热更新 RPC | P2 | 当前靠文件签名检测自动 reload，补一个 `hooks_reload` RPC 主动触发 |
| 3 | 跨项目大纲聚合 | P3 | 把各项目的 outline.json 聚合到 GlobalMemory，跨项目检索 |
| 4 | hooks 审计日志 | P3 | 记录每次 hook 执行（event/handler/exit_code/duration）到 audit.jsonl |
| 5 | 条件表达式增强 | P3 | pi 的 if-parser 只支持 ToolName(glob)，扩展到通用表达式 |
| 6 | hooks 依赖管理 | P4 | handler 间声明依赖（A 必须在 B 之后跑） |

---

## 第 9 章 与现有文档的关系

### 9.1 取代 HOOK_SYSTEM.md

[HOOK_SYSTEM.md](./HOOK_SYSTEM.md) 是早期设计稿（6 事件 + 仅 command），**已被本文档取代**。HOOK_SYSTEM.md 顶部状态改为"已被 HOOKS_AND_OUTLINE_SYNC.md 取代"，内容保留作历史查阅。

### 9.2 与 EXTENSION_SYSTEM.md 的关系

[EXTENSION_SYSTEM.md](./EXTENSION_SYSTEM.md) 描述 WASM 扩展系统（底层钩子 trait）。本文档的 HookExtension 是 Extension trait 的一个**内置 Rust 实现**，不是 WASM。两者关系：

```
Extension trait（EXTENSION_SYSTEM.md 定义，~30 个钩子）
    ├── WASM 扩展（用户写的 .wasm）
    ├── 内置扩展（Memory / Bash / FileSnapshot / ...）
    └── HookExtension（本文档，读 hooks.json + 跑 handler）⭐ 新增
```

### 9.3 与 CONFIG_DIMENSIONS.md 的关系

| 文件 | 维度 | 说明 |
|------|------|------|
| `~/.ion/hooks.json` | ① 全局 | 天然全局 |
| `<project>/.ion/hooks.json` | ③ 仓库内 | 可 git 追踪，团队共享 |
| `<project>/.ion/outline.json` | ③ 仓库内 | 大纲数据，可 git 追踪 |
| `<project>/.ion/scripts/*.sh` | ③ 仓库内 | hook 脚本，可 git 追踪 |

详见 [CONFIG_DIMENSIONS.md](./CONFIG_DIMENSIONS.md)。

### 9.4 与 AGENTS.md 路线图的关系

AGENTS.md 路线图 P6 "Shell Hook 系统 (TRAE 兼容)" 从"暂不开发"改为"开发中"，指向本文档。

---

## 关键 bug fix 记录

> 实现过程中踩过的坑写这里，避免回退。

### Bug 1：agent handler 不传 tools（pi 的坑，ION 避开了）

**pi 的实现**：`runAgentHandler` 调 `callLLM` 时不传 tools → 退化成单轮 LLM，"可以用工具验证"的注释是假的。

**ION 的避法**：agent handler 走 `Runtime::spawn_worker`（补丁 1 补齐字段），真传 `allowed_tools` + `max_turns`，spawn 的子 Worker 有完整工具循环。✅ 已验证（hooks_agent_ci E1-E4）。

### Bug 2：Stop block 死循环（loop_count 防护）

**问题**：Stop 被 block → reason 作为新 query → agent 继续跑 → 又 Stop → 又 block → 死循环。

**修复**：`loop_count >= loop_limit`（默认 5）时跳过 HookGroup，强制允许停止（对齐 pi index.ts:208-222）。per-session 计数。

### Bug 3：agent handler 跨进程递归 spawn（hook_depth 防护）⭐ 实际遇到

**问题**：Stop 事件配 agent handler → 入口 Worker 跑完触发 Stop → agent handler spawn 子 Worker → 子 Worker 跑完也触发 Stop → 又 spawn → **无限递归（实测 spawn 了 16 个 Worker）**。

**根因**：`loop_limit` 是 per-session 的，但每个子 Worker 是独立进程（独立 session_id），loop_count 不跨进程共享，防不住。

**修复**：`ION_HOOK_DEPTH` 跨进程递归深度传递：
- `SpawnWorkerRequest.hook_depth` 字段 → `WorkerCreateConfig.hook_depth` → Manager spawn 时设子进程 `ION_HOOK_DEPTH`
- `run_agent` 读当前 depth，spawn 时设 `hook_depth = current_depth + 1`
- `HookExtension.process_event` 读 `ION_HOOK_DEPTH`，**agent handler 且 depth >= 2 时跳过**
- 入口 Worker（普通 spawn_worker 工具）不设 hook_depth → 子进程没有此变量 → depth=0 → agent handler 正常

**效果**：入口 Worker(depth=0) → 子 Worker(depth=1) → 子子 Worker(depth=2) 跳过。三层截断。

**验证**：hooks_agent_ci E4「没有死循环（worker 数 <= 3）」✅

### Bug 4：入口 Worker agent handler 被误跳过

**问题**：初版 hook_depth 实现里，Manager 无条件给所有子进程 `ION_HOOK_DEPTH = parent_depth + 1`。入口 Worker 也是 Manager spawn 的 → depth=1 → 但阈值设 >0 → 入口 Worker 的 agent handler 被跳过。

**修复**：改成只有 `WorkerCreateConfig.hook_depth` 被显式设了才传环境变量。入口 Worker（普通 spawn_worker）不设 → 子进程没有此变量 → depth=0 → agent handler 正常。阈值也从 >0 改成 >=2。
