# ION 总体架构

> **状态：已完成** — 本文档是 ION 整体系统架构的总览图，以 ASCII 框图形式（对齐 AGENTS.md 风格）呈现分层、三场景、Worker 内部结构、多智能体编排与存储。所有能力点均已在代码中实现，详见各子系统的设计文档。

---

## 0. 阅读这份文档

| 你想看什么 | 跳到 |
|-----------|------|
| 一张图看全貌 | [§1 总体架构大图](#1-总体架构大图) |
| 三种 CLI 跑法（直接执行 / `--host` / `serve`）区别 | [§2 三场景对比](#2-三场景对比) |
| 一个 Worker 子进程内部是怎么转的 | [§3 Worker 内部结构](#3-worker-内部结构) |
| Agent 循环 + 扩展钩子时序 | [§4 Agent 循环与扩展钩子](#4-agent-循环与扩展钩子) |
| Worker 之间怎么通信、多智能体怎么编排 | [§5 通信层与多智能体编排](#5-通信层与多智能体编排) |
| 配置 / 会话 / 快照 / 记忆存哪 | [§6 存储与数据维度](#6-存储与数据维度) |
| 为什么这么设计（关键决策） | [§7 关键设计决策（ADR 简化版）](#7-关键设计决策adr-简化版) |
| 一次工具调用从头到尾怎么走 | [§8 数据流示例](#8-数据流示例一次工具调用的完整链路) |

> **术语约定**：本项目所有可扩展能力统称 **Extension**，禁止使用 "plugin/插件"。内置 Extension 与运行时 WASM Extension 共享同一套 trait 接口（36 钩子 + 27 host functions），唯一区别是"代码住哪"。

---

## 1. 总体架构大图

ION 是一个 AI Agent 编排平台，对齐 pi 的全部能力。整体可以切成 **5 个垂直分层 + 若干横向能力**：

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                              用户接入层 (CLI)                                │
│   ion "任务"          ion --host "任务"          ion serve                   │
│   ion rpc ...         ion subscribe ...         ion history / sessions ...   │
│   ion --agent X       ion --skill / --extension    ion --export HTML         │
└────────────┬──────────────────┬──────────────────────┬──────────────────────┘
             │ (场景 1)          │ (场景 2)              │ (场景 3)
             │ 直接执行          │ 临时 host             │ 常驻 host + socket
             ▼                  ▼                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                            引擎层 (host 引擎 / cmd_run)                      │
│   场景 1: 直接 spawn Agent 进程，跑完即退                                     │
│   场景 2/3 共享同一套 host 引擎:                                             │
│     ┌─ WorkerRegistry (Worker 生命周期 + 内存状态)                            │
│     ├─ 命令循环 (处理 create_session / prompt / rpc ...)                      │
│     ├─ IO Bridge (子进程 stdin/stdout 桥接)                                   │
│     ├─ Event Pump (ExtensionEvent → EventBus broadcast)                       │
│     ├─ stderr 捕获 + exit code + 崩溃恢复 (Dead 保留 + 父通知)                │
│     └─ Unix socket IPC (场景 3: ~/.ion/host.sock)                            │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                │  spawn self (current_exe + --mode rpc)
                                ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                       Worker 层 (子进程, 每个 = 一个 Agent 会话)              │
│  ┌────────────────────────────────────────────────────────────────────────┐  │
│  │ Agent 循环 (内层 LLM 调用 + 外层工具调度 + ~45 个 Extension 钩子)         │  │
│  │   ├─ Provider 抽象 (ion-provider crate)                                │  │
│  │   │    OpenAI / Anthropic / Google / Bedrock / Faux / Record-Replay      │  │
│  │   ├─ 内置工具 (~27: read/write/edit/bash/grep/find/git/spawn_worker...)  │  │
│  │   ├─ Agent 循环保护: Tool Loop Detector / Tool-Use 重试 / GoalSupervisor │  │
│  │   └─ Context 管理: on_context 注入 / Compaction / SoftDelete             │  │
│  └────────────────────────────────────────────────────────────────────────┘  │
│  ┌─ Extension 注册中心 (ExtensionRegistry)                                   │  │
│  │   内置: Bash / Memory / Plan / GoalSupervisor / LSP / Monitor /          │  │
│  │         RulesEngine / Learning / Hooks / Permission / FileSnapshot        │  │
│  │   WASM: 运行时加载 .wasm (todo / stock / 第三方)                          │  │
│  │   接口一致: Extension trait (36 钩子) + 27 host functions + ctx.fs         │  │
│  └─ MCP 客户端 (rmcp 1.x, 方案 C 共享池)                                      │
└───────────────────────────────┬─────────────────────────────────────────────┘
                                │
        ┌───────────────────────┼───────────────────────┐
        ▼                       ▼                       ▼
┌──────────────────┐  ┌────────────────────┐  ┌────────────────────┐
│   能力层          │  │   运行时层 (Runtime) │  │   存储层 (Storage)  │
│                  │  │                    │  │                    │
│ Provider 协议:    │  │ BackendRegistry:   │  │ StorageContext:    │
│ openai-complete   │  │   Local (直接执行)  │  │   ~/.ion/ (5 维)   │
│ anthropic-msg     │  │   Sandbox (权限过滤)│  │   global/agent/    │
│ google-genai      │  │   Remote (SSH)     │  │   project/session/ │
│ bedrock-converse  │  │   AppleContainer   │  │   cwd               │
│ openai-responses  │  │   (VM 隔离)         │  │                    │
│                  │  │ 路由层: 前缀+路径    │  │ SQLite/FTS5:       │
│ MCP bridge:       │  │   命令前缀路由      │  │   global-memory.db │
│   host 持有连接    │  │   路径前缀路由      │  │ JSONL: session     │
│   Worker 代理     │  │                    │  │ Object store:      │
│   call/resource   │  │ 权限引擎:           │  │   file-snapshot     │
│                  │  │   PermissionEngine  │  │                    │
│ 工具执行:         │  │   CommandGuard      │  │ Cache:             │
│   read/write/bash │  │   (白/黑/开放 3 模式)│  │   cargo/target     │
│   git/spawn...    │  │   UiSystem (审批)    │  │   (container volume)│
└──────────────────┘  └────────────────────┘  └────────────────────┘
```

### 1.1 分层职责一句话

| 层 | 职责 | 关键模块 |
|----|------|---------|
| **接入层** | 解析 45+ 参数，决定走哪种执行模式 | `src/bin/ion.rs` |
| **引擎层** | Worker 生命周期 + 事件转发 + IPC | `src/worker_registry.rs` / `worker_rpc.rs` |
| **Worker 层** | 一个 Agent 会话：LLM 循环 + 工具 + 扩展 | `src/agent/` / `src/extension.rs` |
| **能力层** | Provider 协议 / 工具 / MCP / Extension 能力 | `ion-provider/` / `src/mcp/` |
| **运行时层** | 命令实际在哪跑（本地/沙箱/远程/容器）+ 权限 | `src/backend_registry.rs` |
| **存储层** | 统一 5 维路径访问 + SQLite + JSONL + object store | `src/storage_context.rs` |

---

## 2. 三场景对比

ION 的所有 CLI 入口最终归到 **三种执行场景**，场景 1 走直接执行，场景 2 和 3 共享同一套 host 引擎：

| 场景 | CLI | 引擎 | 事件出口 | 同步子任务 | 异步任务 | 退出方式 |
|------|-----|------|---------|-----------|---------|---------|
| **1. 快速执行** | `ion "任务"` | 直接 spawn（无 host） | ❌ | ✅ | ❌ 进程退出子 Worker 被回收 | 跑完即退 |
| **2. 快速编排** | `ion --host "任务"` | host 引擎 | 事件泵 → stdout | ✅ | ✅ host 兜着 | 递归 idle 自动关 |
| **3. 常驻服务** | `ion serve` | host 引擎 + socket | socket → 外部 UI | ✅ | ✅ host 兜着 | 手动 shutdown |

```
              ┌─ 场景 1：直接 spawn 子进程，不经过 host
              │   跑完即退，没有事件转发
              │
    同一套     ├─ 场景 2：临时 host + 事件泵 → stdout
    底层 API  │   递归 idle 自动关
    (spawn、   │
     await、  └─ 场景 3：常驻 host + Unix socket → 外部 UI
    channel)      不自动退，外部可全程接入
```

### 2.1 场景 1：直接执行

```
终端                   进程内
┌──────┐   ┌──────────────────────────┐
│      │   │  cmd_run()               │
│ ion  │──→│  建工具集 + Agent        │
│ "任务"│   │  agent.run(message)      │
│      │   │    ├─ LLM 循环            │
│      │   │    ├─ 调 tool (read/write)│
│      │   │    ├─ spawn_worker(同步)  │
│      │   │    │    └─ spawn 子进程    │
│      │   │    │        await 等完    │
│      │   │    └─ 返回               │
│      │   └─ 进程退出                  │
└──────┘
    ❌ 没有 host，不能异步任务
    ❌ 没有事件转发
    ✅ 同步子任务能用
```

### 2.2 场景 2：临时 host（快速编排）

```
终端                              临时 host
┌──────┐  ┌──────────────────────────────────────────────┐
│      │  │  WorkerRegistry + 命令循环 + 事件泵           │
│ ion  │──│                                              │
│--host│  │  spawn coordinator Worker (子进程)            │
│"任务" │  │    ├─ spawn_worker(dev, 同步)  → await      │
│      │  │    ├─ spawn_worker(dev, 异步)  → agent_end   │
│      │  │    └─ channel_send ← 子 Worker 过程通信      │
│      │  │                                              │
│      │  │  事件泵 → stdout (实时打印 text_delta)        │
│      │  │  ...全部 idle → 清理退出                      │
└──────┘  └──────────────────────────────────────────────┘
    ✅ 有 host，同步异步都行
    ✅ 事件泵 → stdout
    ❌ 没有 socket，外部工具接不了（权限拦截需预配 allow 规则）
```

### 2.3 场景 3：常驻服务（serve）

```
外部 UI / TUI / IDE 插件               常驻 host
┌─────────────────┐   ┌───────────────────────────────────────┐
│        socket    │   │  WorkerRegistry + 命令循环            │
│  Web UI          │   │  Unix socket → ~/.ion/host.sock      │
│  ┌───────────┐   │   │                                       │
│  │进度条     │   │   │  spawn Worker(子进程)                  │
│  │卡片       │◄──│───│  ├─ 同步：spawn → await （UI 可见）   │
│  │步骤状态   │   │   │  ├─ 异步：spawn → agent_end（UI 可见）│
│  │实时日志   │   │   │  ├─ channel_send ← 过程通信          │
│  └───────────┘   │   │  ├─ subscribe → 事件流推给 socket    │
│                  │   │  └─ 一直运行（不自动退）               │
│  ion rpc 命令行  │   │                                       │
│  ┌───────────┐   │   │                                       │
│  │create_   │───│───│  …                                     │
│  │worker     │   │   │                                       │
│  └───────────┘   │   │                                       │
└─────────────────┘   └───────────────────────────────────────┘
    ✅ 有 host，同步异步都行
    ✅ 事件通过 socket 推给外部工具
    ❌ 不自动退出，需要手动 shutdown
```

> **退出条件（场景 2）**：递归 idle 检测——入口 coordinator idle ∧ 它 spawn 的所有子 Worker idle ∧ 子 Worker 的子 Worker idle … 全部 idle → 没有后台进程 → 清理退出。

---

## 3. Worker 内部结构

**Worker = 一个子进程 = 一个 Agent 会话**。host 通过 `current_exe() + --mode rpc` spawn 自身进入 worker 模式（对齐 pi 的 `pi --mode rpc`），通过 JSONL over stdin/stdout 通信。

```
┌─────────────────────────── Worker 子进程 (src/worker_rpc.rs, ~124 个 RPC 命令) ───────────────────────────┐
│                                                                                                           │
│  ┌─ stdin ──→ 命令分发器 (JSONL)                                                                          │
│  │    {"id":"1","method":"prompt","params":{...}}                                                          │
│  │    {"id":"2","method":"rpc","params":{"method":"extension_rpc",...}}                                    │
│  │                                                                                                        │
│  │          ┌─────────────────────────────────────────────────────────────────────────────────┐            │
│  │          │  Agent 主循环 (src/agent/, 内层 + 外层)                                           │            │
│  │          │                                                                                  │            │
│  │          │  while !done:                                                                    │            │
│  │          │    on_input           ← 用户消息进来 (记忆检索 / hook 触发)                       │            │
│  │          │    on_context          ← 组装上下文 (记忆注入 / 规则注入 / 诊断注入 / 快照折叠)    │            │
│  │          │    on_model_select     ← 选模型 (tier 别名 / 扩展可覆盖)                          │            │
│  │          │    ────── LLM 调用 (Provider) ──────                                             │            │
│  │          │       ion-provider::chat_stream(context, options, model)                          │            │
│  │          │         ├─ openai-completions / anthropic-messages / google-generative-ai        │            │
│  │          │         ├─ bedrock-converse-stream / openai-responses                             │            │
│  │          │         └─ faux (Mock) / replay (录放)                                            │            │
│  │          │    on_assistant_message ← LLM 回复 (secret 脱敏 / learning 分析)                  │            │
│  │          │    if 有 tool_calls:                                                              │            │
│  │          │      for tool in tool_calls:                                                      │            │
│  │          │        on_tool_call(前)  ← 权限检查 / hook PreToolUse                              │            │
│  │          │        ── ToolRegistry.dispatch ──                                                │            │
│  │          │           内置工具 (read/write/edit/bash/grep/find/git/...)                       │            │
│  │          │           扩展工具 (extension.register_tool)                                       │            │
│  │          │           MCP 工具 (McpProxyTool bridge → host McpManager)                        │            │
│  │          │           编排工具 (spawn_worker/send/await/channel_send/kill)                    │            │
│  │          │        on_tool_execution_end(后) ← hook PostToolUse / LSP 诊断 / GoalSupervisor    │            │
│  │          │        Tool Loop Detector (防死循环: 3 次 WARN / 5 次 ABORT)                      │            │
│  │          │      done = false (继续循环)                                                      │            │
│  │          │    else:                                                                         │            │
│  │          │      on_gate_check ← GoalSupervisor: 目标完成了吗? 没完成 RetryWith(证据)        │            │
│  │          │      done = true                                                                  │            │
│  │          │  end                                                                              │            │
│  │          │  on_stop ← hook Stop / session 落盘 / auto_title / learning extract               │            │
│  │          └─────────────────────────────────────────────────────────────────────────────────┘            │
│  │                                                                                                        │
│  └── stdout ──→ 事件流 (JSONL)                                                                           │
│       {"type":"response","id":"1","success":true,...}    (RPC 响应)                                       │
│       {"type":"event","event":{"type":"text_delta",...}} (事件, host 转发给 subscriber)                   │
│       {"type":"event","event":{"type":"extension_event",...}} (扩展自定义事件)                            │
│                                                                                                           │
│  ┌─ ExtensionRegistry ──────────────────────────────────────────────────────────────────────────┐          │
│  │  内置扩展 (编译进内核, config.enabled 控制开关):                                                │          │
│  │    Bash / Memory(V0.1+V0.2) / Plan / GoalSupervisor / Monitor / LSP /                         │          │
│  │    RulesEngine / Learning / Hooks / Permission / FileSnapshot / GlobalMemory(单例)             │          │
│  │  WASM 扩展 (运行时 .wasm):                                                                     │          │
│  │    todo / stock / 第三方                                                                       │          │
│  │  共享接口: Extension trait (36 钩子) + 27 host functions + ctx.fs + 4 级数据目录                 │          │
│  └────────────────────────────────────────────────────────────────────────────────────────────────┘          │
│                                                                                                           │
│  ┌─ ExtensionApi (扩展拿到的内核把手, src/worker_api.rs) ─┐                                                  │
│  │  create_worker / channel_send / emit_extension_event    │  (扩展能编排子 Worker + 广播事件)              │
│  │  fs.read_file / fs.write_file / fs.list_dir / fs.glob   │  (统一文件访问, safe_join 防逃逸)             │
│  │  get_flag / set_flag / storage(data_dirs)               │  (运行时 flag + 4 级数据存储)                 │
│  └─────────────────────────────────────────────────────────┘                                                  │
└───────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 3.1 Agent 循环 = 内层 + 外层

| 层 | 干什么 | 触发的钩子 |
|----|--------|-----------|
| **外层循环** | 一轮"用户输入 → Agent 结束"的完整 turn | `on_input` → `on_session_start` → ... → `on_stop` |
| **内层循环** | 一个 turn 内的多次 LLM 调用 + 工具调用 | `on_context` → `on_model_select` → LLM → `on_tool_call` → `on_tool_execution_end` → `on_context` → ... |

### 3.2 Provider 抽象（独立 crate）

`ion-provider` 是独立的 crate，支持 9 种 API 协议，统一成 `chat_stream(context, options, model)` 一个入口：

```
ion-provider crate
├─ registry.rs        ← 模型/Provider 注册（从 ~/.ion/config.json 或 ~/.pi/agent/models.json 加载）
├─ providers/
│   ├─ openai_completions.rs      (SSE + tool_calls)
│   ├─ anthropic_messages.rs      (messages API)
│   ├─ google_generative_ai.rs    (Gemini)
│   ├─ openai_responses.rs        (Responses API)
│   ├─ bedrock_converse.rs        (Bedrock 流式)
│   ├─ faux.rs                    (架构级 LLM Mock, FIFO 队列 + 工厂函数)
│   └─ record_replay.rs           (录制/回放)
└─ transform_messages.rs  ← 不同协议的消息格式互转 + 兼容性检测
```

---

## 4. Agent 循环与扩展钩子

Agent 的每个生命周期点都有钩子，扩展可以挂上去改变行为。下图标注 **钩子触发点 + 谁在用**：

```
用户消息 ──→ on_input ───────────────────────────────────────────────────┐
            (Memory 关键词匹配 / Hooks UserPromptSubmit)                  │
                                                                          ▼
        on_context ────────────────────────────────────────────────┐  组装 messages
        (Memory 注入 / RulesEngine 注入 / LSP 诊断注入 / 快照折叠)  │
                                                                    ▼
        on_model_select ──────────────────────┐  选模型
        (tier_models 解析 / 扩展可 &mut 覆盖)  │
                                               ▼
        ┌─────────────── LLM 调用 (Provider) ──────────────┐
        │  chat_stream(context, options, model)             │
        │  ├─ 失败: retry_async (RetryConfig)               │
        │  └─ 成功: 返回 AssistantMessage                    │
        └────────────────────────┬───────────────────────────┘
                                 ▼
        on_assistant_message ────────────┐  (Secret 脱敏 / Learning 分析)
                                         ▼
                   ┌─── 有 tool_calls ?──┐
                   │                     │
              否   │                     │ 是
                   ▼                     ▼
        on_gate_check              for tool in tool_calls:
        (GoalSupervisor            ├─ on_tool_call (前)
         目标完成否)               │    (Permission 检查 / Hooks PreToolUse)
              │                    │    ├─ Deny → 拒绝 + 注入失败结果
              │                    │    └─ Allow ↓
              │                    ├─ ToolRegistry.dispatch
              │                    │    (内置 / 扩展 / MCP / 编排 工具)
              │                    └─ on_tool_execution_end (后)
              │                         (Hooks PostToolUse / LSP 异步诊断
              │                          / GoalSupervisor drift 检测)
              │                    Tool Loop Detector (同签名 ≥3 WARN / ≥5 ABORT)
              │                    ▼
              │              回到 on_context 继续循环
              ▼
        on_stop ──────────────────────────────────────────────┐
        (Hooks Stop / session 落盘 / auto_title /             │
         Learning extract / Memory consolidation)             │
                                                             ▼
                                                    Agent 结束, Worker idle
```

### 4.1 Extension trait 钩子全景（36 个）

按触发阶段分组，下表列出一部分（全部见 [EXTENSION_SYSTEM.md](./EXTENSION_SYSTEM.md)）：

| 阶段 | 钩子 | 谁在用 |
|------|------|--------|
| 会话 | `on_session_start` / `on_singleton_init` / `on_user_join` / `on_user_leave` / `on_last_user_gone` / `on_singleton_shutdown` | GlobalMemory(单例) / Monitor |
| 输入 | `on_input` / `on_system_prompt` | Memory / RulesEngine |
| 上下文 | `on_context` / `on_pre_compact` / `on_post_compact` | Memory / Compaction / SoftDelete |
| 模型 | `on_model_select(&mut)` | TierModels |
| 工具 | `on_tool_call` / `on_tool_execution_end` / `on_tool_call_failure` | Permission / LSP / GoalSupervisor |
| 交付 | `on_gate_check` | GoalSupervisor |
| 自定义 | `on_extension_rpc` | 扩展私有 RPC 入口 |

---

## 5. 通信层与多智能体编排

### 5.1 Worker 间通信

```
┌─────────────── host 引擎 (WorkerRegistry) ───────────────┐
│                                                          │
│   Worker A          Worker B          Worker C            │
│  ┌────────┐        ┌────────┐        ┌────────┐          │
│  │ coord. │        │ dev-1  │        │ dev-2  │          │
│  └───┬────┘        └───┬────┘        └───┬────┘          │
│      │                  │                 │               │
│      │  send_to_worker(id, msg)  ────────►│  点对点        │
│      │  send_to_session(sid, msg) ───────►│  按会话        │
│      │  channel_send(name, msg) ──► [broadcast] ◄─ 群聊    │
│      │  subscribe(id)            ◄── 事件流                │
│      │                                                    │
└──────┼────────────────────────────────────────────────────┘
       │
       │  spawn_worker(child, wait=true)   → await_worker
       │  spawn_worker(peer, wait=false)   → send → await → kill
       │
       ▼
   新 Worker 子进程
```

| 方式 | 说明 | 用法 |
|------|------|------|
| `send_to_worker(id, msg)` | 点对点（知道对方 ID） | 异步任务 |
| `send_to_session(sid, msg)` | 按会话 ID（没运行会自动启动） | 会话恢复 |
| `channel_send(name, msg)` | 群聊广播（不需知道对方 ID） | 多 Worker 协作 |
| `subscribe(id)` | 订阅 Worker 事件流 | UI / 监控 |

### 5.2 同步 vs 异步子任务

```
同步子任务 (spawn + await)              异步任务 (spawn + agent_end)
───────────────────────────            ──────────────────────────────
Agent: spawn_worker(dev, wait=true)    Agent: spawn_worker(dev, wait=false)
Agent: await_worker(id)                Agent: 继续聊别的
       ────干活────                          ──子 Worker 发消息──
Agent: ← 拿结果                              channel_send 实时收
                                             ──子 Worker agent_end──
                                              host 检测到 → UI 更新
```

### 5.3 多智能体编排（agent.md 驱动，零内核策略）

`ion --host --agent coordinator "做这个"` 拆任务开发，不需要硬编码编排逻辑：

```
┌─ coordinator (host 上) ──────────────────────────────────────────┐
│  读 examples/agents/coordinator.md (agent 定义)                    │
│  ├─ spawn_worker(developer, worktree=true) × N  ──并行            │
│  │     │                                                          │
│  │     ▼                                                          │
│  │  ┌─ developer (独立 worktree 分支) ──────────┐                 │
│  │  │  read/write/edit/bash + cargo test        │                 │
│  │  │  写代码 → 自跑测试 → commit               │                 │
│  │  └───────────────────────────────────────────┘                 │
│  │                                                                │
│  ├─ await_worker(全部 dev 完成)                                    │
│  ├─ spawn_worker(reviewer)  ──审查改动                            │
│  │     ├─ APPROVE → 合并                                          │
│  │     └─ REQUEST_CHANGES → resume_worker(dev) 修复               │
│  ├─ spawn_worker(merger)    ──合并分支 + cleanup                   │
│  └─ spawn_worker(publisher) ──推送 GitHub + 开/关 issue            │
└──────────────────────────────────────────────────────────────────┘

调度策略:
  串行:     wait=true, 一个一个跑
  小批量并行: wait=false + await_worker
  后台同级:  peer 模式
```

**7 个编排工具**（全部验证通过 ✅）：

| 类型 | 工具 | 说明 |
|------|------|------|
| 同步 | `spawn_worker(child, wait=true)` + `resume_worker` | 阻塞等待，用 resume 恢复，**不需 kill** |
| 异步 | `spawn_worker(peer/wait=false)` + `send_to_worker` + `await_worker` + `kill_worker` | 立即返回，**才需要 kill** |
| 通信 | `channel_send` | 群聊广播 |

### 5.4 A→B 自进化架构（host A 编排 container B 改代码）

**铁律：A 只调度，B 改代码。** ZCode/编排者 A **绝不**直接 edit/write 主仓库源码，所有改动通过 container 里的 B（完整 ion 实例）完成。

```
ZCode / coordinator (A)
   │
   ├─ git worktree add (开隔离空间)
   ├─ bash scripts/evolve.sh (启 container + volume cache)
   │
   ├─ echo "任务 spec" | container exec -i B ion --agent developer
   │     │
   │     ▼
   │  ┌─ B = container 里的完整 ion 实例 ──────────────┐
   │  │  有自己的 LLM + 工具 + CI                       │
   │  │  read → edit 加代码 → cargo test → commit      │
   │  └─────────────────────────────────────────────────┘
   │
   ├─ 6 道守门:
   │   ① U+FFFD 守门 (grep 查中文乱码)
   │   ② Cargo.toml 守门 (拒绝加外部依赖)
   │   ③ reviewer agent (SQL/错误处理/边缘 case/测试)
   │   ④ cargo build
   │   ⑤ cargo test --lib
   │   ⑥ cargo clippy (warning 不增)
   │
   └─ 全过 → GitHub PR → merge
```

---

## 6. 存储与数据维度

### 6.1 五维存储（StorageContext 统一访问）

所有扩展通过 `StorageContext` 拿路径，不再自己拼，worktree 透明：

```
~/.ion/                              ← global (跨项目共享)
├── config.json                      ← 全局配置 (provider/model/tier/command_guard)
├── settings.json                    ← 权限规则 (permissions.rules)
├── auth.json                        ← API key
├── models.json                      ← 模型定义 (或读 ~/.pi/agent/models.json)
├── host.sock                        ← 场景 3 Unix socket
└── agent/                           ← agent 维度 (跨 session)
    ├── sessions/                    ← session JSONL + 索引
    │   └── <session_id>/
    │       ├── messages.jsonl       ← 会话消息 (v3 格式)
    │       ├── index.json           ← 实时索引
    │       └── ...
    ├── extensions/                  ← 运行时 WASM 扩展 (.wasm)
    ├── extensions-data/             ← 扩展数据 (4 级目录)
    ├── global-memory.db             ← SQLite + FTS5 (跨项目记忆)
    ├── file-snapshot/               ← 文件快照 object store (去重 + zstd)
    ├── goal-runs/                   ← GoalSupervisor 日志
    ├── active-pipelines.json        ← Monitor 活跃 pipeline
    ├── lsp-metrics.jsonl            ← LSP 执行指标
    └── audit.jsonl                  ← CommandGuard 审计日志

~/.ion/projects/<project_key>/       ← project 维度 (主仓库 + worktree 共享 key)
├── config.json                      ← 项目级配置 (mcp_servers 等)
└── settings.json                    ← 项目级权限规则

<cwd>/.ion/                          ← cwd 维度 (项目内)
├── rules/*.md                       ← 项目规则 (frontmatter glob 匹配)
├── settings.json                    ← 项目内权限
└── extensions/                      ← 项目级 WASM 扩展

<cwd>/                               ← session/cwd 工作区
├── target/                          ← 编译产物 (container volume 持久化)
└── 源码...
```

### 6.2 关键存储组件

| 组件 | 格式 | 位置 | 说明 |
|------|------|------|------|
| Session | JSONL v3 + index.json | `agent/sessions/<sid>/` | 实时索引 + fork/continue/resume + cwd-hash 分组 |
| Session Tree | only-append + leaf 指针 | 同上 | 文件内分支 + tombstone 回滚 |
| File Snapshot | object store + zstd | `agent/file-snapshot/` | 双路快照（工具级 + 目录扫描），content-addressed 去重 |
| Global Memory | SQLite + FTS5 | `agent/global-memory.db` | 跨项目全文检索 |
| Compaction | soft delete + summarize | session JSONL 内 | mark_deleted/summarized/restore |
| Config | JSON (合并) | global + project | 深度合并，project 覆盖 global |

### 6.3 Worktree 隔离与 project_key

```
主仓库 /Users/xuyingzhou/Project/study-rust/ion
   │  git-common-dir = .git
   │
   ├─ worktree A: /Users/.../ion-wt-feature1
   │  git-common-dir = /主仓库/.git  (共享)
   │
   └─ project_key = hash(git-common-dir)  ← 主仓库和所有 worktree 算出同一 key
                                              → file-snapshot / memory / config 共享
```

---

## 7. 关键设计决策（ADR 简化版）

> 以下是 ION 几个关键架构决策，记录"为什么这么做"。

### ADR-1: Worker 用子进程而非线程

- **决策**：每个 Agent 会话 = 一个独立子进程（`current_exe + --mode rpc`）
- **备选**：线程池 / 异步 task
- **理由**：
  - 强隔离：一个 Agent 崩溃不影响其他
  - 崩溃恢复可观测（stderr + exit code + Dead 保留 + 父通知）
  - 对齐 pi 的 `pi --mode rpc` 进程模型
  - worktree/cwd 隔离天然干净
- **代价**：进程间通信用 JSONL over stdin/stdout，比线程慢

### ADR-2: 内置 Extension 与 WASM Extension 共享同一 trait

- **决策**：两类扩展用完全相同的 `Extension` trait + 27 host functions + ctx.fs，唯一区别是代码住哪
- **备选**：内置用 native trait，WASM 用独立 ABI
- **理由**：
  - 扩展开发体验一致（写内置 = 写 WASM）
  - 内置扩展能享受扩展系统的所有钩子
  - WASM 提供安全沙箱 + 热更新
- **代价**：内置扩展也得遵守 Extension trait 的约束

### ADR-3: 三场景共享同一套 host 引擎

- **决策**：场景 1 直接执行；场景 2/3 共享 `WorkerRegistry + 事件转发 + spawn_worker` 底层 API
- **备选**：每个场景独立实现
- **理由**：
  - 一套底层 API，三个入口
  - 场景 2/3 只差"对外暴露方式"（stdout vs socket）和"退出策略"（idle 自动关 vs 常驻）
- **代价**：场景 1 没事件转发能力（但用户要的就是快进快出）

### ADR-4: 多智能体编排零内核策略（agent.md 驱动）

- **决策**：不硬编码任何编排逻辑，全靠 agent.md 定义 + `spawn_worker` 工具
- **备选**：内置 coordinator/team 逻辑
- **理由**：
  - 编排策略是"策略层"，不该进内核
  - agent.md 可热改，零编译
  - `ion-team` 不存在——完全被 `ion --host --agent coordinator` 覆盖
- **代价**：新人需要理解 agent.md 才能用编排

### ADR-5: MCP 方案 C（host 持有连接，Worker 代理）

- **决策**：host 进程持有 MCP 连接，所有 Worker 通过 bridge 代理（`McpProxyTool`）
- **备选**：每个 Worker 自己连 / 全局单例
- **理由**：
  - 进程只 1 份连接（N Worker 不开 N 份 stdio）
  - 连接生命周期跟 host 走，Worker 崩溃不丢连接
  - 权限统一在 host 层管
- **代价**：每次 call 多一跳 bridge

### ADR-6: A→B 铁律（编排者不碰源码）

- **决策**：ZCode/coordinator（A）绝不直接 edit/write 主仓库源码，所有改动通过 container 里的 B（完整 ion 实例）
- **备选**：A 直接改
- **理由**：
  - 守门机制（U+FFFD / reviewer / CI）才能生效——A 改没人审
  - B 有完整隔离环境 + 自跑 CI
  - 跟"ZCode 不碰 ION 源码"同一个原则
- **代价**：多一层 container，慢一些

---

## 8. 数据流示例：一次工具调用的完整链路

以"用户说：读一下 src/lib.rs 然后改成 async"为例（场景 2，host 模式）：

```
① 用户终端
   $ ion --host "读 src/lib.rs 然后改成 async"
        │
        ▼
② CLI 接入层 (src/bin/ion.rs)
   解析参数 → 走场景 2 → 启动临时 host
        │
        ▼
③ host 引擎 (WorkerRegistry)
   spawn Worker 子进程 (current_exe + --mode rpc)
   ├─ 通过 stdin 发 prompt 命令
   └─ 订阅事件流准备打印
        │
        ▼
④ Worker 子进程 (src/worker_rpc.rs)
   ┌─ Agent 循环 ──────────────────────────────────────────┐
   │  Turn 1:                                              │
   │    on_input     → Memory 匹配 (无命中)                 │
   │    on_context   → RulesEngine 注入 *.rs 规则           │
   │    on_model_select → 解析 tier → glm-5.2              │
   │    LLM 调用    → 返回 tool_call: read(src/lib.rs)      │
   │    on_tool_call → Permission Allow                     │
   │    ToolRegistry.dispatch → ReadTool 执行               │
   │       └─ StorageContext 解析路径                       │
   │       └─ 读文件内容                                    │
   │    on_tool_execution_end → LSP 异步诊断                │
   │    (回循环)                                            │
   │                                                       │
   │  Turn 2:                                              │
   │    on_context → 注入上轮 read 结果 + LSP 诊断          │
   │    LLM 调用   → 返回 tool_call: edit(src/lib.rs, ...)  │
   │    on_tool_call → Permission 检查                      │
   │       └─ 未预配 → UiSystem 弹审批                      │
   │       └─ (场景 2 无 socket, 需 settings.json 预配)     │
   │    FileSnapshot 拍 before 快照                         │
   │    EditTool 执行 → 改文件                              │
   │    FileSnapshot 拍 after 快照                          │
   │    (回循环)                                            │
   │                                                       │
   │  Turn 3:                                              │
   │    on_gate_check → GoalSupervisor (无目标, 放行)       │
   │    on_stop → session 落盘 + auto_title                 │
   └───────────────────────────────────────────────────────┘
        │
        │  stdout 事件流 (JSONL)
        │  {"type":"event","event":{"type":"text_delta",...}}
        │  {"type":"event","event":{"type":"file_snapshot",...}}
        ▼
⑤ host 事件泵 → stdout → 终端实时打印
        │
        ▼
⑥ 全部 Worker idle → 递归 idle 检测通过 → 清理退出
```

---

## 9. 源码导航（按层索引）

| 层 | 文件 | 内容 |
|----|------|------|
| 接入 | `src/bin/ion.rs` | 单一 CLI 入口（45+ 参数），`--mode rpc` 进 worker 模式 |
| 引擎 | `src/worker_registry.rs` | Manager 内存状态 + Worker 管理 |
| 引擎 | `src/worker_rpc.rs` | Worker RPC 实现（~124 命令） |
| 引擎 | `src/worker_api.rs` | WorkerHandle + ExtensionApi |
| Agent | `src/agent/` | Agent 循环（内层 + 外层 + 扩展钩子） |
| 能力 | `ion-provider/` | Provider 抽象（独立 crate） |
| 能力 | `src/extension.rs` | WASM 扩展加载器 |
| 能力 | `src/mcp/` | MCP 客户端（rmcp + 方案 C 共享池） |
| 运行时 | `src/backend_registry.rs` | 路由层（命令前缀 + 路径前缀） |
| 存储 | `src/storage_context.rs` | 统一存储路径访问（5 维 + worktree 透明） |
| 存储 | `src/file_snapshot/` | File Snapshot 双路快照 |
| 存储 | `src/global_memory.rs` | 全局记忆库（SQLite + FTS5） |
| 存储 | `src/session_tree.rs` | Session Tree（分支 + leaf 指针） |

更多模块详见 [AGENTS.md §源码导航](../../AGENTS.md)。

---

## 10. 相关文档

| 主题 | 文档 |
|------|------|
| 扩展系统（36 钩子 + 27 host functions） | [EXTENSION_SYSTEM.md](./EXTENSION_SYSTEM.md) |
| Extension Host API（ctx.fs + 4 级数据） | [EXTENSION_HOST_API.md](./EXTENSION_HOST_API.md) |
| Provider 协议 | [PROVIDER_PROTOCOL.md](./PROVIDER_PROTOCOL.md) |
| Team 编排 | [TEAM_ORCHESTRATION.md](./TEAM_ORCHESTRATION.md) |
| 三场景 CLI 完整方案 | [CLI_PLAN.md](./CLI_PLAN.md) |
| 配置与数据维度 | [CONFIG_DIMENSIONS.md](./CONFIG_DIMENSIONS.md) |
| File Snapshot | [FILE_SNAPSHOT.md](./FILE_SNAPSHOT.md) |
| MCP 系统 | [MCP_SYSTEM.md](./MCP_SYSTEM.md) |
| A→B 自进化 | [SELF_EVOLUTION.md](./SELF_EVOLUTION.md) |
| Hooks 系统 | [HOOKS_AND_OUTLINE_SYNC.md](./HOOKS_AND_OUTLINE_SYNC.md) |
| Goal Supervisor | [GOAL_SUPERVISOR.md](./GOAL_SUPERVISOR.md) |
