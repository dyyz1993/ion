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

---

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
