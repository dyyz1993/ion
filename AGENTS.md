# ION — AI Agent Orchestration Platform

## Architecture Overview

```
                    ┌─────────────────────────────────────┐
                    │         对外接口层                     │
                    │  CLI  │  HTTP  │  WebSocket  │  RPC  │
                    └───────────────┬─────────────────────┘
                                    │
                    ┌───────────────▼─────────────────────┐
                    │         Manager (守护进程)            │
                    │                                     │
                    │  ┌──────────┐  ┌──────────────────┐ │
                    │  │ Worker   │  │ Channel Router    │ │
                    │  │ Registry │  │ channel→subscribers│ │
                    │  └────┬─────┘  └──────────────────┘ │
                    │       │                              │
                    │  ┌────▼──────────────────────────┐  │
                    │  │  Worker Pool                    │  │
                    │  │  ┌──────┐ ┌──────┐ ┌──────┐   │  │
                    │  │  │ Wkr A│ │ Wkr B│ │ Wkr C│   │  │
                    │  │  │child │ │peer  │ │parent│   │  │
                    │  │  └──────┘ └──────┘ └──────┘   │  │
                    │  └───────────────────────────────┘  │
                    └─────────────────────────────────────┘
```

### 核心概念

| 概念 | 说明 |
|------|------|
| **Worker** | 一个 Agent 实例 = 一个子进程 (`ion worker --mode rpc`) |
| **Manager** | 全局守护进程，管理 Worker 生命周期 + 消息路由 |
| **Channel** | Worker 间通信的命名通道 |
| **Project** | Worker 所属的项目，用于隔离和统计 |
| **Session** | Worker 的对话历史，存为 JSONL 文件 |
| **WorkerHandle** | 远程 Worker 的本地代理，全部方法通过 RPC 透传 |

---

## RPC 协议

### 传输层

- **模式**: JSONL over stdin/stdout
- **编码**: UTF-8
- **行分隔**: `\n` (LF)
- **协议版本**: 1

### 请求格式

```json
{"id":"req_1","method":"prompt","params":{"text":"hello"}}
{"id":"req_2","method":"set_model","params":{"provider":"openai","modelId":"gpt-4o"}}
```

### 响应格式

```json
{"id":"req_1","type":"success","result":{"output":"Hello!"}}
{"id":"req_2","type":"success","result":null}
{"id":"req_1","type":"error","error":{"message":"Something went wrong","code":"-32000"}}
```

### 事件推送（无需请求，主动推送）

```json
{"type":"event","event":{"type":"text_delta","delta":"正在分析..."}}
{"type":"child_event","worker_id":"wkr_b","event":{"type":"text_delta","delta":"子任务进度"}}
{"type":"channel_msg","channel":"review","from":"wkr_c","msg":{"text":"审查完成"}}
```

---

## WorkerHandle — 所有 55+ 方法

### Prompting（7）

| 方法 | RPC | 说明 |
|------|-----|------|
| `prompt(text)` | `prompt` | 发送用户消息 |
| `steer(msg)` | `steer` | 高优先级注入消息 |
| `follow_up(msg)` | `follow_up` | 低优先级后续消息 |
| `continue()` | `continue` | 继续上次会话 |
| `abort()` | `abort` | 中止当前执行 |
| `new_session(parent?)` | `new_session` | 创建新会话 |
| `get_state()` | `get_state` | 获取会话状态 |

### Model（7）

| 方法 | RPC | 说明 |
|------|-----|------|
| `set_model(provider, model)` | `set_model` | 切换模型 |
| `cycle_model()` | `cycle_model` | 循环下一个模型 |
| `get_available_models()` | `get_available_models` | 列出可用模型 |
| `get_tier_models()` | `get_tier_models` | 获取 tier 别名 |
| `set_tier_models(models)` | `set_tier_models` | 设置 tier 别名 |
| `set_thinking_level(level)` | `set_thinking_level` | 设置思考级别 |
| `cycle_thinking_level()` | `cycle_thinking_level` | 循环思考级别 |

### Session（14）

| 方法 | RPC | 说明 |
|------|-----|------|
| `get_session_stats()` | `get_session_stats` | 会话统计 |
| `export_html(path?)` | `export_html` | 导出 HTML |
| `switch_session(path)` | `switch_session` | 切换会话 |
| `fork(entry_id, opts?)` | `fork` | 分叉会话 |
| `navigate_tree(target)` | `navigate_tree` | 树导航 |
| `delete_entries(ids)` | `delete_entries` | 删除条目 |
| `summarize_entries(ids)` | `summarize_entries` | 摘要条目 |
| `clone()` | `clone` | 克隆会话 |
| `set_session_name(name)` | `set_session_name` | 命名会话 |
| `set_cwd(cwd)` | `set_cwd` | 设置工作目录 |
| `get_messages()` | `get_messages` | 获取消息列表 |
| `get_full_messages(opts?)` | `get_full_messages` | 获取完整消息 |
| `get_tree()` | `get_tree` | 获取会话树 |
| `get_last_assistant_text()` | `get_last_assistant_text` | 最后助手回复 |

### Tools & Resources（10）

| 方法 | RPC | 说明 |
|------|-----|------|
| `get_tools()` | `get_tools` | 列出工具 |
| `get_active_tools()` | `get_active_tools` | 活跃工具 |
| `set_active_tools(names)` | `set_active_tools` | 设置活跃工具 |
| `get_context_usage()` | `get_context_usage` | 上下文用量 |
| `get_system_prompt()` | `get_system_prompt` | 系统提示词 |
| `get_settings(scope?)` | `get_settings` | 获取设置 |
| `set_settings(settings)` | `set_settings` | 设置设置 |
| `get_commands()` | `get_commands` | 斜杠命令列表 |
| `get_skills()` | `get_skills` | 技能列表 |
| `get_extensions()` | `get_extensions` | 扩展列表 |

### Agent（7）

| 方法 | RPC | 说明 |
|------|-----|------|
| `get_agents()` | `get_agents` | 列出 agents |
| `switch_agent(name)` | `switch_agent` | 切换 agent |
| `get_current_agent()` | `get_current_agent` | 当前 agent |
| `get_agent_detail(name)` | `get_agent_detail` | agent 详情 |
| `get_all_tools()` | `get_all_tools` | 所有工具定义 |
| `set_permission_mode(mode)` | `set_permission_mode` | 权限模式 |
| `get_modified_files(opts?)` | `get_modified_files` | 修改的文件 |

### Channel（4）

| 方法 | RPC | 说明 |
|------|-----|------|
| `channel(name).send(data)` | `channel_send` | 发消息到通道 |
| `channel(name).on_receive(handler)` | — | 订阅通道消息 |
| `channel(name).invoke(data)` | `channel_invoke` | 发送并等响应 |
| `channel(name).call(method, params)` | `channel_call` | 具名调用 |

### Worker 管理（7）

| 方法 | RPC | 说明 |
|------|-----|------|
| `create_worker(config)` | `create_worker` | 创建子 Worker |
| `list_workers(filter?)` | `list_workers` | 列出 Worker |
| `get_worker(id)` | `get_worker` | 获取 Worker 信息 |
| `kill()` | `kill_worker` | 关闭 Worker |
| `pause()` | `pause_worker` | 暂停 Worker |
| `resume()` | `resume_worker` | 恢复 Worker |
| `subscribe()` | `subscribe_worker` | 订阅 Worker 事件 |

### 订阅

```rust
let handle = api.create_worker(config).await?;
let mut events = handle.subscribe().await?;

tokio::spawn(async move {
    while let Some(event) = events.recv().await {
        match event {
            WorkerEvent::TextDelta(text) => ui.show(text),
            WorkerEvent::ToolCall(tool) => ui.show_tool(tool),
            WorkerEvent::Result(result) => handle_result(result),
            WorkerEvent::ChildEvent(child_id, event) => {
                // 子 Worker 的实时事件
            }
            _ => {}
        }
    }
});
```

---

## Manager API（HTTP/REST）

### Worker

| 方法 | 端点 | 说明 |
|------|------|------|
| GET | `/api/workers` | 列出所有 Worker |
| GET | `/api/workers/:id` | Worker 详情 |
| POST | `/api/workers` | 创建 Worker |
| DELETE | `/api/workers/:id` | 销毁 Worker |
| POST | `/api/workers/:id/prompt` | 向 Worker 发 prompt |
| POST | `/api/workers/:id/steer` | 向 Worker 发 steer |

### Project

| 方法 | 端点 | 说明 |
|------|------|------|
| GET | `/api/projects` | 列出所有项目 |
| GET | `/api/projects/:id/workers` | 项目下的 Worker |

### Channel

| 方法 | 端点 | 说明 |
|------|------|------|
| POST | `/api/channels/:name/send` | 向 channel 发消息 |
| GET | `/api/channels/:name/subscribers` | channel 订阅者 |
| WS | `/ws/channel/:name` | WebSocket 监听 channel |

### Session

| 方法 | 端点 | 说明 |
|------|------|------|
| GET | `/api/sessions/:id` | 会话统计 |
| GET | `/api/sessions/:id/export` | 导出 HTML |
| GET | `/api/sessions` | 列出所有会话 |

---

## 父子 Worker 通信

```
Worker A (parent)
  │ api.create_worker({ channel: "review", parent: "wkr_a" })
  ▼
Worker D (child)
  │ stdout 事件 → Manager 检测到 parent
  │ → 转发到 Worker A 的 stdin
  ▼
Worker A 收到 child_event
  │ 可以观察、拦截、修改
```

### 事件回传格式

```json
{"type":"child_event","worker_id":"wkr_d","relation":"child",
 "event":{"type":"text_delta","delta":"子任务进度..."}}
```

---

## 会话存储

```
~/.ion/agent/
├── sessions/
│   ├── abc-123.jsonl          ← 会话消息（JSONL，每行一个 entry）
│   └── def-456.jsonl
├── sessions.index.json        ← 实时索引（O(1) 统计）
├── config.json                ← 全局配置
└── agents/                    ← 自定义 Agent .md 文件
    └── my-agent.md
```

### JSONL 格式 (v3)

```
第 1 行: {"type":"session","version":3,"id":"uuid","timestamp":"...","cwd":"..."}
第 2 行: {"type":"message","id":"a1b2c3","parentId":"uuid","timestamp":"...","message":{...}}
第 N 行: {"type":"message","id":"...","parentId":"...","message":{...}}
```

---

## WASM 插件

插件编译为 `.wasm`，通过 `--extension plugin.wasm` 加载：

```rust
// 插件导出的函数
plugin_version() -> u32
plugin_init()                    // 注册工具、设置钩子
plugin_execute_tool(name, args) -> result
plugin_on_event(type, payload)   // 生命周期钩子

// 宿主提供的函数
host_register_tool(name, desc, schema)
host_send_message(text)
host_set_session_name(name)
host_get_flag(name) -> value
```

---

## CLI 参数（41 个核心参数）

| 参数 | 说明 |
|------|------|
| `[message] @file @多消息` | 输入 |
| `--provider` / `--model` | Provider/模型 |
| `--api-key` / `--base-url` | API 配置 |
| `--thinking` | 思考级别 |
| `--models` | 多模型列表 |
| `--prompt` / `-P` | 系统提示词 |
| `--append-system-prompt` | 追加提示词 |
| `--max-turns` | 最大轮数 |
| `--json` / `--json-schema` | JSON 输出 |
| `--session` / `--continue-session` | 会话管理 |
| `--name` / `-n` | 会话命名 |
| `--fork` | 会话分叉 |
| `--agent` | 命名 Agent |
| `--tools` / `--exclude-tools` | 工具控制 |
| `--extension` / `-e` | 加载扩展 |
| `--skill` | 加载技能 |
| `--export` | 导出 HTML |
| `--verbose` | 调试日志 |

---

## 开发指南

### 构建

```bash
cargo build --bin ion
cargo build --target wasm32-wasip1 --release -p stock-plugin
```

### 测试

```bash
cargo test --lib           # 35 个单元测试
cargo test --test child_worker
cargo test --test concurrency
```

### 运行

```bash
# 单实例 CLI
ion "hello"

# 守护进程模式
ion manager start --port 8080

# RPC 模式
echo '{"method":"prompt","params":{"text":"hello"}}' | ion rpc

# WASM 插件
ion --extension plugin.wasm "hello"
```
