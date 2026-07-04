# Shell Hook 系统 (TRAE 兼容)

> **状态：暂不开发** — 本文档为设计规划，尚未实现。
>
> 相关：AGENTS.md 路线图 P6 条目。

---

设计目标：支持用户通过 `hooks.json` 配置文件，在 Agent 生命周期的关键节点执行自定义 Shell 命令，实现对 Agent 行为的拦截、校验、注入和验收。对齐 TRAE IDE 的 Hook 协议（stdin/stdout JSON + 退出码语义），以便用户脚本可以直接复用。

## 配置格式

```json
{
  "version": 1,
  "hooks": {
    "SessionStart": [
      {
        "hooks": [
          {
            "command": "bash ./scripts/setup_env.sh",
            "timeout": 30
          }
        ]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "RunCommand|Write",
        "hooks": [
          {
            "command": "python3 ./validate_command.py",
            "timeout": 10
          }
        ]
      }
    ],
    "Stop": [
      {
        "loop_limit": 3,
        "hooks": [
          {
            "command": "python3 ./check_tests.py",
            "timeout": 60
          }
        ]
      }
    ]
  }
}
```

### 字段说明

| 层级 | 字段 | 类型 | 必填 | 描述 |
|------|------|------|------|------|
| 顶层 | `version` | number | 否 | Schema 版本，默认 `1`，当前仅支持 `1` |
| 顶层 | `hooks` | object | 是 | 事件名到 Hook 组列表的映射 |
| 事件层 | `<EventName>` | array | 是 | 该事件下的 Hook 组列表 |
| Hook 组 | `matcher` | string | 否 | 正则表达式匹配工具名（仅 PreToolUse / PostToolUse 有效）。`*` 或空表示匹配所有 |
| Hook 组 | `loop_limit` | number | 否 | Stop 事件循环阻断上限（默认 5） |
| Hook 组 | `hooks` | array | 是 | 该组下要执行的 Hook 列表 |
| Hook 定义 | `type` | string | 否 | 默认为 `"command"`，当前仅支持此类型 |
| Hook 定义 | `command` | string | 是 | 要执行的 Shell 命令 |
| Hook 定义 | `timeout` | number | 否 | 超时秒数（默认 30） |

## 事件映射

利用已有的 `Extension` trait，用 `HookExtension` 实现 6 种事件到 Shell 命令的桥接。

| Extension 方法 | TRAE 事件 | 行为 |
|---|---|---|
| `on_session_start` | `SessionStart` | 注入上下文 / 写 `ION_ENV_FILE` 环境变量文件 |
| `on_input` | `UserPromptSubmit` | 拦截请求 (`decision: "block"`) / 附加 context |
| `before_tool_call` | `PreToolUse` | 校验参数 / `permissionDecision: allow/deny/ask` / `updatedInput` 覆盖参数 |
| `after_tool_call` | `PostToolUse` | 校验结果 / `decision: "block"` 阻断后续 |
| `on_agent_end` | `Stop` | 验收检查 / `decision: "block"` + `reason` 作为新 Query |
| (异步非阻塞) | `Notification` | spawn-and-forget，不阻塞主流程 |

## 通信协议

```
stdin  → JSON 上下文
stdout ← JSON 控制指令（或纯文本）

退出码 0  = 正常，解析 stdout
退出码 2  = 阻断性错误，stderr 内容作为错误信息
其他退出码 = 非阻断性错误，忽略 stdout/stderr
```

### stdin 通用字段

所有事件共用字段：

```json
{
  "session_id": "string",
  "cwd": "/path/to/workspace",
  "hook_event_name": "PreToolUse",
  "workspace_roots": ["/path/to/workspace"]
}
```

### stdout 通用控制字段

```json
{
  "continue": true,
  "stopReason": "string"
}
```

| 字段 | 类型 | 默认值 | 描述 |
|------|------|--------|------|
| `continue` | boolean | `true` | Agent 是否继续执行。设为 `false` 时停止 |
| `stopReason` | string | — | 停止时展示给用户的原因 |

## 各事件协议详情

### SessionStart

**触发时机**：创建 Session 后、发起第一个对话之前。

**stdin**：
```json
{
  "session_id": "...",
  "hook_event_name": "SessionStart",
  "source": "startup"
}
```

**stdout — 纯文本**：直接输出，作为附加上下文提供给模型。

**stdout — JSON**：
```json
{
  "hookSpecificOutput": {
    "hookEventName": "SessionStart",
    "additionalContext": "文本内容"
  }
}
```

**环境变量注入**：通过向 `$ION_ENV_FILE` 文件写入键值对，可以为后续 Hook 和 RunCommand 工具注入环境变量。支持三种格式：

```bash
# Bash 格式
export NODE_ENV=production
export PATH="/usr/local/bin"

# Dotenv 格式
NODE_ENV=production
MY_VAR="hello world"
```

**退出码 2 的行为**：不影响会话流程（非阻断）。

---

### UserPromptSubmit

**触发时机**：用户发送消息后、智能体开始处理前。

**stdin**：
```json
{
  "session_id": "...",
  "hook_event_name": "UserPromptSubmit",
  "prompt": "用户输入的 Prompt"
}
```

**stdout — 纯文本**：直接输出，作为附加上下文提供给模型。

**stdout — JSON**：
```json
{
  "decision": "block",
  "reason": "该请求不被允许的原因",
  "hookSpecificOutput": {
    "hookEventName": "UserPromptSubmit",
    "additionalContext": "附加给模型的上下文"
  }
}
```

| 字段 | 类型 | 描述 |
|------|------|------|
| `decision` | string | 仅支持 `"block"`，设置后禁止智能体处理该 Prompt。留空则允许 |
| `reason` | string | 当 `decision` 为 `"block"` 时，展示给用户的错误信息 |
| `additionalContext` | string | 附加给模型的上下文文本 |

**退出码 2 的行为**：等价于 `"decision": "block"`，直接禁止处理。

---

### PreToolUse

**触发时机**：智能体发起工具调用后、实际执行前。

**matcher**：通过 `matcher` 字段配置正则表达式匹配工具名。

**stdin**：
```json
{
  "session_id": "...",
  "hook_event_name": "PreToolUse",
  "tool_use_id": "toolcall-id-string",
  "tool_name": "RunCommand",
  "llm_tool_name": "RunCommand",
  "tool_input": { ... }
}
```

**stdout**：
```json
{
  "hookSpecificOutput": {
    "hookEventName": "PreToolUse",
    "permissionDecision": "allow",
    "permissionDecisionReason": "决策原因说明",
    "updatedInput": { ... },
    "additionalContext": "附加给模型的上下文"
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `permissionDecision` | string | `allow` / `deny` / `ask`。多个 Hook 并行时优先级：`deny` > `ask` > `allow` |
| `permissionDecisionReason` | string | 决策原因 |
| `updatedInput` | object | 修改后的工具输入参数，**整体覆盖**原参数（非合并） |
| `additionalContext` | string | 附加给模型的上下文 |

**退出码 2 的行为**：等价于 `"permissionDecision": "deny"`，拒绝执行。

---

### PostToolUse

**触发时机**：工具调用实际执行完成后。

**matcher**：通过 `matcher` 字段配置正则表达式匹配工具名。

**stdin**：
```json
{
  "session_id": "...",
  "hook_event_name": "PostToolUse",
  "tool_use_id": "toolcall-id-string",
  "tool_name": "RunCommand",
  "llm_tool_name": "RunCommand",
  "tool_input": { ... },
  "tool_response": { ... }
}
```

**stdout**：
```json
{
  "decision": "block",
  "reason": "阻断原因",
  "hookSpecificOutput": {
    "hookEventName": "PostToolUse",
    "additionalContext": "附加给模型的上下文"
  }
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `decision` | string | 仅支持 `"block"`。设置后会向模型传递阻断信息（工具已执行无法撤销）。留空则继续 |
| `reason` | string | 阻断原因 |
| `additionalContext` | string | 附加给模型的上下文 |

**退出码 2 的行为**：将 `stderr` 传递给模型的上下文。

---

### Stop

**触发时机**：智能体完成输出、准备结束当前查询时。

**stdin**：
```json
{
  "session_id": "...",
  "hook_event_name": "Stop",
  "stop_hook_active": false,
  "loop_count": 0,
  "last_assistant_message": "大语言模型最终输出的文本内容"
}
```

| 字段 | 类型 | 描述 |
|------|------|------|
| `stop_hook_active` | boolean | 当前查询是否已经被 Stop Hook 至少阻断过一次 |
| `loop_count` | number | 当前查询被阻断次数，从 0 开始。`loop_count >= loop_limit` 时跳过该 HookGroup |
| `last_assistant_message` | string | LLM 最终输出的文本内容 |

**stdout**：
```json
{
  "decision": "block",
  "reason": "请继续检查测试是否通过"
}
```

| 字段 | 类型 | 说明 |
|------|------|------|
| `decision` | string | 仅支持 `"block"`。设置后阻断停止，`reason` 作为新 Query。留空则允许停止 |
| `reason` | string | 阻断原因，作为新的用户请求让智能体继续执行 |

**退出码 2 的行为**：等价于 `"decision": "block"`，阻断停止。

**决策控制流**：
```
智能体准备停止
    │
    ▼
检查 loop_count >= loop_limit? ── 是 ──► 跳过 Hook，允许停止
    │
   否
    │
    ▼
执行 Stop Hook
    │
    ├── 退出码 0 + decision 为空 ────► 允许停止
    ├── 退出码 0 + decision="block" ──► 阻断，reason 作为新 Query
    ├── 退出码 2 ────────────────────► 阻断，stderr 作为新 Query
    └── 其他退出码 ──────────────────► 忽略，允许停止
```

---

### Notification

**触发时机**：工具调用等待用户确认，或智能体完成任务时。**异步执行，不阻塞主流程**。

**matcher**：基于 `notification_type` 匹配，而非工具名。

**stdin**：
```json
{
  "session_id": "...",
  "hook_event_name": "Notification",
  "notification_type": "idle_prompt",
  "message": "智能体已完成任务",
  "tool_use_id": "toolu_xxx"
}
```

| 字段 | 类型 | 描述 |
|------|------|------|
| `notification_type` | string | 通知类别：`idle_prompt` / `permission_prompt` / `document_review` / `ask_user_question` / `browser_interaction` |
| `message` | string | 通知正文 |
| `tool_use_id` | string? | 关联的工具调用 ID（仅工具调用相关通知携带） |

**stdout**：忽略。

**退出码**：任意退出码均视为非阻断性结果，不影响主流程。

## 工具名列表（PreToolUse / PostToolUse matcher）

| 分类 | 工具名称 | 描述 |
|------|----------|------|
| 文件读取 | `Read` | 读取文件内容 |
| 文件写入 | `Write` | 写入文件 |
| 文件编辑 | `Edit` | 单次查找并替换文件内容 |
| 搜索 | `Glob` | 基于文件路径模式匹配搜索 |
| | `Grep` | 基于正则表达式内容搜索 |
| | `LS` | 列出目录下的文件与子目录 |
| 终端 | `RunCommand` | 执行终端命令 |
| 网络 | `WebSearch` | 网络搜索 |
| | `WebFetch` | 获取网页内容 |
| 交互 | `AskUserQuestion` | 向用户提问 |
| Skill | `Skill` | 加载 Skill |
| MCP | `mcp__<serverName>__<toolName>` | MCP 工具，可用 `mcp__.*` 匹配所有 |

## 数据结构设计（规划，未实现）

```rust
/// ──────────────────────────────────────────────────────────────
/// HooksConfig — hooks.json 的 Rust 映射
/// 支持全局 (~/.ion/hooks.json) 和项目级 (<project>/.ion/hooks.json) 合并
/// ──────────────────────────────────────────────────────────────

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct HooksConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    pub hooks: HashMap<String, Vec<HookGroup>>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct HookGroup {
    #[serde(default)]
    pub matcher: Option<String>,       // 正则，匹配 Pre/PostToolUse 的工具名
    #[serde(default)]
    pub loop_limit: Option<u32>,       // Stop 事件的循环阻断上限（默认 5）
    pub hooks: Vec<HookDef>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct HookDef {
    #[serde(default = "default_hook_type")]
    pub r#type: String,                // 当前仅支持 "command"
    pub command: String,
    #[serde(default)]
    pub timeout: Option<u32>,          // 秒，默认 30
}
```

## HookExtension 设计（规划，未实现）

```rust
/// ──────────────────────────────────────────────────────────────
/// HookExtension — 实现 Extension trait
/// 在每个钩子点检查 hooks.json 配置，执行匹配的 Shell 命令
/// ──────────────────────────────────────────────────────────────

pub struct HookExtension {
    config: HooksConfig,
    work_dir: PathBuf,                          // 命令工作目录
    loop_counts: Arc<Mutex<HashMap<String, u32>>>,  // Stop 循环计数（per session）
    env_file: Option<PathBuf>,                  // ION_ENV_FILE 路径
}

impl HookExtension {
    /// 加载 hooks.json（全局 + 项目级合并）
    pub fn load(project_dir: Option<PathBuf>) -> Self { .. }

    /// 运行一组 Hook 定义，返回 HookResult
    async fn run_hooks(
        hooks: &[HookDef],
        stdin: serde_json::Value,
    ) -> HookResult { .. }
}

enum HookResult {
    /// 退出码 0，stdout 解析成功
    Continue { output: HookOutput },
    /// 退出码 2，阻断性错误
    Block { reason: String },
    /// 其他退出码，忽略
    Ignore,
}

enum HookOutput {
    /// JSON 格式（结构化控制）
    Json(serde_json::Value),
    /// 纯文本（仅 SessionStart / UserPromptSubmit 支持）
    Text(String),
}
```

## 配置文件存储路径

```
~/.ion/hooks.json              ← 全局 Hook（对本机所有工作区生效）
<project>/.ion/hooks.json      ← 项目级 Hook（仅当前项目生效）
```

多个配置文件共存时**合并执行**（对齐 TRAE 行为）。

## 与现有系统的集成

```
Agent Loop → ExtensionRegistry
               ├── WASM Extension (已有)
               ├── JSON Extension (已有)
               ├── Plan Extension (已有)
               └── HookExtension ◄── 新增
                      │
              ┌───────▼────────┐
              │  HookRunner    │
              │  spawn cmd     │
              │  stdin/stdout  │
              │  退出码协议     │
              └────────────────┘
```

**集成关系**：

| 现有组件 | 集成方式 |
|----------|----------|
| `PermissionEngine` | PreToolUse 的 `"deny"` 决策直接映射到 `PermissionResult::Deny` |
| `CommandGuard` | 与 Hook 独立。Hook 做策略/业务层拦截，Guard 做命令层安全拦截，两者串联不冲突 |
| `Runtime` trait | Hook 命令的执行也走 Runtime，本地/沙箱模式兼容 |
| `ION_ENV_FILE` | SessionStart 生成的临时环境变量文件（类似 TRAE_ENV_FILE），后续 Hook 和 RunCommand 工具自动读取 |

## 工作量估算

| 模块 | 行数 |
|------|------|
| 数据结构定义 | ~50 |
| HookRunner（spawn + stdin/stdout + 退出码协议） | ~120 |
| HookExtension（6 个事件映射到 Extension trait） | ~150 |
| env_file 支持（ION_ENV_FILE 生成/注入/清理） | ~40 |
| 配置加载与多文件合并 | ~60 |
| Agent 集成（init 时加载并注册 HookExtension） | ~30 |
| 测试 | ~100 |
| **总计** | **~550** |
