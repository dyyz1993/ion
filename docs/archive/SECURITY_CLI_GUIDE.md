# Security & Runtime CLI 测试指南

> **状态：设计稿** — 部分 CLI 已就绪，权限审批流程为架构设计。

---

## 概览

本文档描述 ION 安全体系的两层 CLI 测试：

| 层 | 能力 | CLI | 状态 |
|----|------|-----|------|
| **Runtime 层** | 进程管理、文件操作全部统一走 Runtime trait，经 `SecuredRuntime` 中间件 | `ion rpc --method call_tool` | ✅ 已实现 |
| **UI 事件通道** | 通用人类交互推送（Ask/Confirm/Notif/Alert/Prompt） | `ion subscribe --ui` + `ion rpc --method ui_respond` | 🔧 设计稿 |
| **权限 Extension** | 在 UI 通道之上实现权限策略，支持"记住本次/本会话/全局" | 内置 Extension + 可选 WASM | 🔧 设计稿 |

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

## Group C：UI 事件通道 CLI（Ask/Confirm/Prompt/Notif/Alert）

### C1 订阅 UI 事件

```bash
# Terminal 1：订阅 UI 事件通道
ion subscribe --ui

# 预期：连接保持，收到事件时逐行打印 JSON
# {"type":"ui_event","ui_type":"Ask","request_id":"req_abc123","data":{...}}
# {"type":"ui_event","ui_type":"AskResolved","data":{"response":"allow"}}
# {"type":"ui_event","ui_type":"Notif","data":{"title":"...","message":"..."}}
```

### C2 触发权限 Ask（端到端）

```bash
# Terminal 1（先订阅）：
ion subscribe --ui

# Terminal 2（触发权限规则命中 Ask）：
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"path":"/tmp/ask-test"}}'

# Terminal 1 收到的 Ask 事件：
# {"type":"ui_event","ui_type":"Ask","request_id":"req_xxx",
#  "title":"权限请求","message":"工具想要 Read 路径: /tmp/ask-test"}

# Terminal 1 回复：
ion rpc --method ui_respond \
  --params '{"request_id":"req_xxx","response":"allow"}'

# Terminal 1 收到 AskResolved：
# {"type":"ui_event","ui_type":"AskResolved",
#  "data":{"request_id":"req_xxx","response":"allow"}}

# Terminal 2 的 call_tool 继续执行并返回结果
```

### C3 Extension 触发 Ask（WASM）

```bash
# 一个 WASM extension 调用 host_ui_ask 时：
# WASM 端：host_ui_ask("确认删除?", "确定要删除任务 xxx 吗？")
# → 返回 0（拒绝）或 1（允许）

# 订阅者收到：
# {"type":"ui_event","ui_type":"Ask","source":"extension:todo_plugin",
#  "request_id":"req_def456","title":"确认删除","message":"确定要删除任务 xxx 吗？"}
```

### C4 通知和告警

```bash
# Notif（通知，不需要回复）
# {"type":"ui_event","ui_type":"Notif","data":{"title":"任务完成","message":"编译成功"}}

# Alert（告警，不需要回复）
# {"type":"ui_event","ui_type":"Alert","data":{"title":"磁盘不足","message":"剩余 1GB","level":"warning"}}
```

### C5 UI 事件通道架构

```
三条独立通道：
Instance (--session x)  → agent_start/text_delta/agent_end（机器消费）
Extension (--extension) → 插件自定义事件 + extension_rpc（调试/前端）
UI (--ui)              → Ask/Confirm/Prompt/Notif/Alert（人类交互）
                          回复走 ui_respond RPC（不走订阅通道）
```

---

## Group D：E2E 测试清单

### D1 Runtime 进程管理

| # | 测试 | CLI | 预期 | 状态 |
|---|------|-----|------|------|
| D1.1 | 前台 spawn 收集输出 | call_tool bash "echo ok" | stdout=ok, exit_code=0 | ✅ 通过 |
| D1.2 | 前台 spawn 非零退出 | call_tool bash "exit 42" | exit_code=42 | ✅ 通过 |
| D1.3 | 后台 spawn + kill | bash_run background=true + bash_kill | os_pid>0, kill 成功 | ✅ 通过 |
| D1.4 | send_stdin | bash_run cat + bash_send | stdin 写入成功 | ✅ 通过 |
| D1.5 | 超时兜底 | bash_run timeoutBackground=true | 超时后转后台 | ✅ 通过 |

### D2 权限拦截

| # | 测试 | CLI | 预期 | 状态 |
|---|------|-----|------|------|
| D2.1 | Deny 规则拦截读取 | call_tool read "~/.ssh/id_rsa" | Err [Permission] | ✅ 通过 |
| D2.2 | CommandGuard 拦截高危 | call_tool bash "rm -rf /" | Err "rejected" | ✅ 通过 |
| D2.3 | 白名单命令放行 | call_tool bash "echo safe" | Ok | ✅ 通过 |
| D2.4 | grep_search 也走权限检查 | grep_search 命中 Deny 规则 | Err | ✅ 通过 |

### D3 UI 事件通道

| # | 测试 | 步骤 | 预期 | 状态 |
|---|------|------|------|------|
| D3.1 | subscribe_ui 过滤 | 插件事件 vs UI 事件 | subscribe_ui 只收 UI 事件 | ✅ 通过 |
| D3.2 | subscribe 不过滤 UI | UI 事件 vs 插件事件 | 普通 subscribe 不过滤 UI | ✅ 通过 |
| D3.3 | subscribe_all 收全部 | UI + 插件事件 | 两条都收到 | ✅ 通过 |
| D3.4 | Ask 事件构造 | ExtensionEvent::new_ui | route="ui", custom_type="Ask" | ✅ 通过 |
| D3.5 | AskResolved 事件 | 带 response/resolved_by | data 格式正确 | ✅ 通过 |
| D3.6 | SecuredRuntime 异步 Ask | resolve_ask 三路径 | 同步/异步/拒绝 | ✅ 通过 |
| D3.7 | WASM host_ui_ask | 宿主函数注册 | 能推 Ask 事件并等回复 | ✅ 通过 |

### D4 混合场景

| # | 测试 | 说明 | 状态 |
|---|------|------|------|
| D4.1 | 权限 + 进程管理 | 后台进程也过 CommandGuard | ✅ 通过 |
| D4.2 | PermissionExtension | before_tool_call 规则匹配 | ✅ 通过 |
| D4.3 | extension_rpc add_rule | CLI 添加规则 | ✅ 通过 |

---

## 验证结果（142 tests 全绿）

```
=== cargo test --lib ===
test result: ok. 98 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.52s

=== cargo test --test ui_event_tests ===
test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

=== cargo test --test runtime_tests ===
test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 3.01s

=== cargo test --test secured_runtime_tests ===
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

=== cargo test --test plugin_tests ===
test result: ok. 26 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.76s
```

## 架构：两层设计

### 第 1 层：核心 — 通用 UI 事件通道

核心只提供 5 种通用 UI 事件类型，**不包含任何权限专用的语义**：

| 事件 | 推送时机 | 需要回复 | 回复格式 |
|------|---------|---------|---------|
| `Ask` | 需要用户确认/拒绝 | ✅ | `"allow"` / `"deny"` |
| `Confirm` | 需要用户确认操作 | ✅ | `"confirm"` / `"cancel"` |
| `Prompt` | 需要用户输入文本 | ✅ | `"input"` + `data:"..."` |
| `Notif` | 通知/提示 | ❌ | — |
| `Alert` | 告警 | ❌ | — |

任何 Extension（包括内核的 `SecuredRuntime`、内置 Extension、WASM Extension）都可以通过这 5 种事件与人类用户交互。

### 第 2 层：策略 — 权限 Extension

权限审批是一个**策略层问题**，不内嵌在核心中。权限 Extension 在核心之上实现：

```
Permission Extension（内置或 WASM）
├── 通过 before_tool_call 钩子拦截工具调用
├── 检查自有规则表（支持三种作用域）:
│   ├── "本次允许"（仅本次生效，不持久化）
│   ├── "本会话允许"（当前会话内有效）
│   └── "全局允许"（持久化到文件，永久生效）
├── 需要用户决策时 → 通过 UI 通道发 Ask
├── 收到 AskResolved → 存储决策到规则表
└── 下次同一规则命中 → 从规则表直接 Allow（不触发 Ask）
```

这样设计的好处：

| 能力 | 没有 Permission Extension | 有内置 Permission Extension |
|------|--------------------------|----------------------------|
| 基础 Deny/Allow 规则 | ✅ PermissionEngine 直接处理 | ✅ 一样 |
| Ask 弹出 | ✅ 通过 UI 通道 | ✅ 通过 UI 通道 |
| "记住本次选择" | ❌ | ✅ Extension 存会话级规则 |
| "本会话允许所有读" | ❌ | ✅ |
| "除非我改，永远允许这个路径" | ❌ | ✅ 持久化规则 |
| 钉钉/邮件审批 | ❌ | ✅ 写个 WASM Extension 即可 |

### 三条独立通道

ION 有三条并行的事件通道：

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
