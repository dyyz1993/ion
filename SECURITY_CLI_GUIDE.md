# Security & Runtime CLI 测试指南

> **状态：设计稿** — 部分 CLI 已就绪，权限审批流程为架构设计。

---

## 概览

本文档描述 ION 安全体系的两层 CLI 测试：

| 层 | 能力 | CLI | 状态 |
|----|------|-----|------|
| **Runtime 层** | 进程管理、文件操作全部统一走 Runtime trait，经 `SecuredRuntime` 中间件 | `ion rpc --method call_tool` | ✅ 已实现 |
| **权限层** | PermissionEngine + CommandGuard 检查，Ask 结果走 UI 通道异步审批 | `ion subscribe --events permission` + `ion permission respond` | 🔧 设计稿 |

---

## Group A：Runtime 进程管理 CLI

### A1 前台执行（同步）

```bash
# 通过 call_tool 调 BashTool
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo hello"}}'

# 预期：返回 stdout/stderr/exit_code
# {"type":"response","success":true,"data":"hello\n"}
```

### A2 后台进程（spawn + kill）

```bash
# 启动后台进程（sleep 60）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 60","background":true}}'

# 返回数据含 pid / os_pid / bid
# 预期：{"bid":"000001","os_pid":12345,"status":"running","exit_code":null}

# kill 后台进程
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_kill","args":{"pid":12345}}'

# 预期：{"status":"killed"}
```

### A3 send_stdin

```bash
# 启动 cat 后台进程
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"cat","background":true}}'
# → {"bid":"000002","os_pid":12346,...}

# 向 cat 的 stdin 写入
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_send","args":{"pid":12346,"input":"hello\n"}}'

# kill
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_kill","args":{"pid":12346}}'
```

### A4 超时自动切后台

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 30","timeoutBackground":true,"timeout":3}}'
# → 3 秒超时后自动转为后台进程，返回 bid
```

---

## Group B：权限安全 CLI（当前可用）

### B1 配置权限规则

权限规则通过 `PermissionEngine` 配置，当前通过代码硬编码或 `profile` 注入：

```bash
# 在 config.json 中配置安全配置文件
# ~/.ion/config.json
{
  "security": {
    "profile": "strict",     # none | strict | custom
    "rules": [
      {"name": "block-ssh", "actions": ["Read"], "pattern": "**/.ssh/*", "policy": "Deny", "priority": 100},
      {"name": "ask-tmp", "actions": ["Write"], "pattern": "/tmp/**", "policy": "Ask", "priority": 50}
    ],
    "command_guard": {
      "whitelist": ["npm", "git", "cargo", "echo", "ls", "cat"],
      "risk_patterns": [
        {"pattern": "rm -rf /", "level": "High", "message": "危险: 删除根目录"}
      ]
    }
  }
}
```

### B2 权限拦截验证（Deny）

```bash
# 配置规则禁止读 ~/.ssh/，尝试读取应被拦截
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"path":"/Users/test/.ssh/id_rsa"}}'

# 预期：{"success":false,"error":"[Permission] 规则 'block-ssh' 拒绝了 read on ..."}
```

### B3 命令守卫拦截（Deny）

```bash
# 高危命令被拦截
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"rm -rf /"}}'

# 预期：{"success":false,"error":"spawn rejected: ..."}
```

### B4 安全命令放行

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"echo safe"}}'

# 预期：{"success":true,"data":"safe\n"}
```

---

## Group C：UI 事件通道（权限审批 — 架构设计）

### 三条独立通道

ION 有三条并行的事件通道，每条用途不同：

| 通道 | CLI 参数 | 事件类型 | 方向 |
|------|----------|---------|------|
| **Instance** | `--session x` | `text_delta`, `agent_start`, `agent_end`, `tool_call_delta` 等 | 只读推送 |
| **Extension** | `--extension memory` | 插件自定义事件（`memory_saved`, `process_completed` 等） | 推送 + RPC（`extension_rpc`） |
| **UI** | `--ui` | `Ask`, `Confirm`, `Notif`, `Alert`, `Prompt` | **推送 + 回复** ← 新增 |

其中 **UI 通道** 是专门为需要用户交互的场景设计的，跟其他两条通道的区别：

| 维度 | Instance/Extension 通道 | UI 通道 |
|------|------------------------|---------|
| 消费者 | LLM / Dashboard / 日志 | **人类用户** |
| 事件类型 | 数据流（文本增量、工具调用） | 交互意图（询问、确认、通知） |
| 是否需要回复 | ❌ 不需要 | ✅ 通常需要用户回复 |
| 谁可以发 | Worker / Extension | **内核 + Extension 都可以** |
| 用途 | 监控 Agent 执行过程 | 用户参与决策流程 |

### UI 事件类型

```json
{
  "type": "ui_event",
  "ui_type": "Ask",           // Ask | Confirm | Notif | Alert | Prompt
  "source": "kernel",          // "kernel" | "extension:memory" | "extension:bash"
  "request_id": "req_abc123", // 需要回复的事件有 request_id
  "title": "权限请求",
  "message": "工具想要 Read /tmp/secret",
  "context": { ... },          // 额外上下文（扩展用）
  "timeout_secs": 60,
  "correlation_id": "..."     // 关联到原始请求
}
```

### 完整审批流程

```
Terminal 1（UI 订阅者）                    Terminal 2（触发者）
─────────────────                      ─────────────────
ion subscribe --ui
       │
       │  (等待 UI 事件)
       │
       │                              ion rpc --method call_tool \
       │                                --params '{"tool":"read","args":{"path":"/tmp/secret"}}'
       │                                       │
       │                              SecuredRuntime.check → Ask
       │                                       │
       │  ◄─── Ask ─────────────────            │
       │  {                                    │
       │    "type":"ui_event",                  │
       │    "ui_type":"Ask",                    │
       │    "source":"kernel",                  │
       │    "request_id":"req_abc123",          │
       │    "title":"权限请求: ask-tmp",         │
       │    "message":"工具想要 read 路径: /tmp/secret"  │
       │  }                                     │
       │                                       │
       │  ion rpc --method ui_respond \         │  (阻塞等待)
       │    --params '{"request_id":"req_abc123","response":"allow"}'  │
       │                                       │
       │                             命令继续执行完成
       │                             返回结果
```

### Extension 也能用 UI 通道

```json
// 一个 WASM extension 通过 host_ui_ask 宿主函数向用户提问
// WASM 端调用: host_ui_ask("确认删除?", "确定要删除任务 xxx 吗？")
// 宿主端收到后通过 UI 通道推送:

{
  "type": "ui_event",
  "ui_type": "Ask",
  "source": "extension:todo_plugin",
  "request_id": "req_def456",
  "title": "确认删除",
  "message": "确定要删除任务 xxx 吗？"
}

// 用户回复后，结果返回给 WASM extension
// 回复: {"request_id":"req_def456","response":"allow"}
```

### UI 通道的宿主函数

| 宿主函数 | 用途 | WASM 侧调用 |
|----------|------|-------------|
| `host_ui_ask(title, message) -> u32` | 询问用户确认 | 0=拒绝, 1=允许 |
| `host_ui_confirm(title, message) -> u32` | 确认操作 | 同上 |
| `host_ui_notif(title, message)` | 发通知（不需要回复） | 无返回 |
| `host_ui_alert(title, message)` | 告警 | 无返回 |
| `host_ui_prompt(title, message, out_buf, cap) -> u32` | 向用户提问并获取输入 | 返回用户输入的字节数 |

### CLI 回复命令

```bash
# 回复 Ask/Confirm
ion rpc --method ui_respond \
  --params '{"request_id":"req_abc123","response":"allow"}'
ion rpc --method ui_respond \
  --params '{"request_id":"req_abc123","response":"deny"}'

# 回复 Prompt（带输入内容）
ion rpc --method ui_respond \
  --params '{"request_id":"req_def456","response":"input","data":"用户输入的内容"}'
```

---

## Group D：E2E 安全测试清单

### D1 Runtime 进程管理

| # | 测试 | CLI | 预期 |
|---|------|-----|------|
| D1.1 | 前台 spawn 收集输出 | `call_tool bash "echo ok"` | stdout=ok, exit_code=0 |
| D1.2 | 前台 spawn 非零退出 | `call_tool bash "exit 42"` | exit_code=42 |
| D1.3 | 后台 spawn + kill | `bash_run background=true` + `bash_kill` | os_pid>0, kill 成功 |
| D1.4 | send_stdin | `bash_run cat background=true` + `bash_send` | stdin 写入成功 |
| D1.5 | 超时兜底 | `bash_run timeoutBackground=true` | 超时后转为后台 |

### D2 权限拦截

| # | 测试 | CLI | 预期 |
|---|------|-----|------|
| D2.1 | Deny 规则拦截读取 | `call_tool read "~/.ssh/id_rsa"` | Err [Permission] Deny |
| D2.2 | Deny 规则拦截写入 | `call_tool write "~/.ssh/test"` | Err [Permission] Deny |
| D2.3 | CommandGuard 拦截高危 | `call_tool bash "rm -rf /"` | Err "spawn rejected" |
| D2.4 | 白名单命令放行 | `call_tool bash "echo safe"` | Ok |
| D2.5 | 权限规则放行安全路径 | `call_tool read "/tmp/test"` | Ok（无规则匹配） |

### D3 权限审批（异步 Ask）

| # | 测试 | 步骤 | 预期 |
|---|------|------|------|
| D3.1 | 订阅权限事件 | `subscribe --events permission` | 连接保持 |
| D3.2 | 触发 Ask 规则 | 另一终端调工具匹配 Ask 规则 | 订阅收到 permission_request |
| D3.3 | 回复 Allow | `permission respond --allow` | 原命令继续执行 |
| D3.4 | 回复 Deny | `permission respond --deny` | 原命令返回 Err |
| D3.5 | 超时不回复 | 不回复，等待超时 | 原命令超时取消 |

### D4 混合场景

| # | 测试 | 说明 |
|---|------|------|
| D4.1 | 权限 + 进程管理 | 后台进程也要过 CommandGuard |
| D4.2 | Worker 编排 + 权限 | 子 Worker 工具调用也走 SecuredRuntime |
| D4.3 | WASM Extension + 权限 | WASM 宿主函数走 Runtime，过权限检查 |

---

## 通道架构总览

```
ION 事件系统
│
├── Instance 通道 (--session x)
│   ├── 推送: text_delta / agent_start / agent_end / tool_call_*
│   ├── 消费者: LLM / Dashboard / 日志
│   └── 无需回复
│
├── Extension 通道 (--extension memory)
│   ├── 推送: 插件自定义事件 (memory_saved, process_completed)
│   ├── 交互: extension_rpc (一问一答)
│   └── 消费者: Dashboard / CLI 调试
│
└── UI 通道 (--ui) ← 新增
    ├── 推送: Ask / Confirm / Notif / Alert / Prompt
    ├── 交互: ui_respond (异步回复)
    ├── 消费者: 人类用户（需要做决策）
    └── 谁可以发: 内核 (SecuredRuntime) + 所有 Extension (WASM/Rust)
```

## CLI 命令总览

```
命令                             状态         通道
────                              ─────       ────────
ion rpc --method call_tool         ✅ 已实现    RPC（工具经 Runtime）
ion subscribe --session x           ✅ 已实现    Instance 通道
ion subscribe --extension memory    ✅ 已实现    Extension 通道
ion subscribe --ui                  🔧 设计稿    UI 通道 ← 第三通道
ion rpc --method ui_respond         🔧 设计稿    UI 通道（回复）
host_ui_ask / host_ui_confirm       🔧 设计稿    WASM Extension → UI 通道
```

| 组件 | 状态 | 说明 |
|------|------|------|
| LocalRuntime 进程管理 | ✅ 已实现 | spawn_process/kill_process/send_stdin |
| SecuredRuntime 装饰器 | ✅ 已实现 | CommandGuard + PermissionEngine |
| PermissionEngine | ✅ 已实现 | Allow/Deny/Ask 三级 |
| UiSystem | ✅ 已实现 | confirm_handler 同步确认 |
| 权限事件订阅通道 | 🔧 设计稿 | EventBus 扩展 EventRoute::Permission |
| 异步 Ask 流程 | 🔧 设计稿 | SecuredRuntime 异步化 |
| `ion permission respond` CLI | 🔧 设计稿 | 回复命令 |
