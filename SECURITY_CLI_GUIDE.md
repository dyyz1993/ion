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

## Group C：权限审批流程（订阅模型 — 架构设计）

### 核心流程

权限审批是**异步**的，不走一问一答的 RPC，而是走 **订阅 + 事件 + 回复** 模型：

```
Terminal 1（订阅者）                    Terminal 2（触发者）
─────────────────                      ─────────────────
ion subscribe --events permission
       │
       │  (等待事件)
       │
       │                              ion rpc --method call_tool \
       │                                --params '{"tool":"read","args":{"path":"/tmp/secret"}}'
       │                                       │
       │                              SecuredRuntime.check → Ask
       │                                       │
       │  ◄─── permission_request ────          │
       │  {                                    │
       │    "type":"permission_request",        │
       │    "request_id":"req_abc123",          │
       │    "title":"权限请求: ask-tmp",         │
       │    "message":"工具想要 read 路径: /tmp/secret", │
       │    "tool":"read",                      │
       │    "action":"Read"                     │
       │  }                                     │
       │                                       │
       │  (用户决定)                             │  (阻塞等待)
       │                                       │
ion permission respond \
  --request-id req_abc123 \
  --allow
       │                                       │
       │  ◄─── permission_response ──          │
       │                                       │
       │                             命令继续执行完成
       │                             返回结果
```

### 关键设计点

**1. Permission Event 通道**

不同于 Instance subscribe（worker 事件流）和 Plugin subscribe（插件事件），权限事件走独立的 `--events permission` 通道：

```bash
# 订阅权限事件
ion subscribe --events permission

# 权限事件的 JSON 格式：
# {
#   "type": "permission_request",
#   "request_id": "req_<8位hex>",
#   "title": "权限请求: <rule名>",
#   "message": "工具想要 <action> 路径: <path>",
#   "tool": "<工具名>",
#   "action": "Read|Write|Execute|Edit|Delete"
# }
```

**2. 回复通道**

通过 `ion permission respond` CLI 回复：

```bash
# 允许
ion permission respond --request-id req_abc123 --allow

# 拒绝（带可选原因）
ion permission respond --request-id req_abc123 --deny --reason "我不信任这个操作"
```

**3. SecuredRuntime 的异步化改造**

当前 `resolve_ask` 是同步阻塞的，需要改为：

```rust
fn resolve_ask(&self, title: &str, message: &str) -> bool {
    // 如果 UiSystem 有 confirm_handler → 同步确认
    // 否则 → 发 PermissionEvent + 等 response（异步）
    //   1. 生成 request_id
    //   2. emit PermissionEvent 到 EventBus
    //   3. 等待 permission_response（有超时）
    //   4. 返回 allow/deny
    if let Some(ref ui) = self.ui_system {
        if ui.has_confirm_handler() {
            return ui.confirm(title, message);
        }
    }
    // 异步路径：发事件 → 等回复
    self.emit_permission_request(request_id, title, message);
    self.wait_for_permission_response(request_id, timeout_secs)
}
```

**4. Permission EventBus 路由**

```rust
// 现有 EventBus 扩展一个 route 类型
pub enum EventRoute {
    Instance,      // text_delta / agent_start / agent_end
    Extension,     // 插件自定义事件
    Permission,    // 权限请求事件 ← 新增
}
```

CLI subscribe 扩展：

```bash
ion subscribe --session x              # Instance 事件（现有）
ion subscribe --extension memory       # Extension 事件（现有）
ion subscribe --events permission      # Permission 事件（新增）
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

## 实现总览

```
CLI 命令                     状态         说明
──────────────────────────    ─────       ──────────────────────
ion rpc --method call_tool    ✅ 已实现    工具调用（经 Runtime）
ion subscribe --session       ✅ 已实现    订阅 worker 事件流
ion subscribe --extension     ✅ 已实现    订阅扩展事件
ion subscribe --events perm   🔧 设计稿    订阅权限事件
ion permission respond        🔧 设计稿    回复权限确认
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
