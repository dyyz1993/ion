# ION 部署架构 — 配置 + 场景 + CLI 验证

---

## 一、前置配置总览

### 运行时选择

```jsonc
// ~/.ion/config.json
{
  "runtime": {
    // 默认运行时
    "default": "local",              // local | sandbox | remote

    // 远程执行配置
    "remote": {
      "default_host": "xyz-mac",
      "hosts": {
        "xyz-mac": {
          "user": "admin",
          "hostname": "xyz-mac.local",
          "port": 22,
          "key": "~/.ssh/id_ed25519",
          "transport": "ssh"         // ssh | http | grpc
        },
        "jd": {
          "user": "deploy",
          "hostname": "jd.server.com",
          "port": 22,
          "key": "~/.ssh/id_rsa"
        }
      }
    },

    // 沙箱配置
    "sandbox": {
      "profile": "workspace",        // readonly | workspace | full-access
      "allow_escape_with_approval": true,
      "escape_approval_mode": "ask"  // ask | auto_approve | deny
    },

    // 命令级路由（按工具+模式匹配选择 Runtime）
    "routes": [
      {"tool": "bash", "pattern": "kubectl *", "runtime": "remote", "host": "xyz-mac"},
      {"tool": "bash", "pattern": "npm install *", "runtime": "sandbox"},
      {"tool": "bash", "pattern": "ssh *", "runtime": "remote", "host": "xyz-mac"}
    ]
  }
}
```

### 沙箱权限分级

```jsonc
{
  "permissions": {
    "sandbox_profiles": {
      // :read-only — 所有命令在沙箱内只读
      "readonly": {
        "filesystem": {
          ":minimal": "read",
          ":workspace_roots": { ".": "read" }
        },
        "network": { "enabled": false }
      },

      // :workspace — 工作区可写，其余只读
      "workspace": {
        "filesystem": {
          ":minimal": "read",
          ":workspace_roots": { ".": "write", ".git": "read", "**/*.env": "deny" },
          "~/.ssh": "deny"
        },
        "network": { "enabled": true, "domains": { "api.openai.com": "allow" } }
      },

      // :danger-full-access — 完全放开（等同无沙箱）
      "full-access": {
        "filesystem": { ":root": "write" },
        "network": { "enabled": true, "domains": { "*": "allow" } }
      }
    }
  }
}
```

### Agent 主动申请提权（Sandbox Escape）

Agent 默认在沙箱内运行。当它判断需要沙箱外执行时（如写系统配置、访问 SSH key），会**主动触发 UI Ask**，用户批准后该命令在沙箱外执行：

```jsonc
{
  "runtime": {
    "sandbox": {
      "allow_escape_with_approval": true,   // 允许 Agent 申请提权
      "escape_approval_mode": "ask"         // ask | auto_approve | deny
    }
  }
}
```

**提权流程：**
```
Agent 需要写 /etc/hosts
  ├── 沙箱内拒绝（deny rule）
  ├── Agent 检测到 → 主动触发 Ask
  │   └── Terminal: subscribe --ui ← 收到 Ask 事件
  │       └── 用户批准 → 该命令在沙箱外执行 ✅
  │       └── 用户拒绝 → Agent 报告"无权限" ❌
  └── escape_approval_mode=deny → 直接拒绝，不提权
```

---

## Three Execution Scenarios

ION has three execution scenarios. Scenario 1 is a **direct spawn** (no host process).
Scenarios 2 and 3 share the **same host engine** — `WorkerRegistry` + `spawn_worker` +
event forwarding — and differ only in how they expose the outside world.

```
              ┌─ Scenario 1: direct child-process spawn, no host
              │   runs and exits, no event forwarding
              │
   same       ├─ Scenario 2: temporary host + event pump → stdout
   low-level  │   auto-exit on recursive idle
   API        │
  (spawn,    └─ Scenario 3: always-on host + Unix socket → external UI
   await,         never auto-exits, external UI can connect at any time
   channel)
```

| Scenario | CLI | Engine | Event Output | Sync Subtask | Async Task | Exit |
|----------|-----|--------|-------------|-------------|-----------|------|
| **1. Quick execution** | `ion "msg"` | direct spawn (no host) | ❌ none | ✅ spawn → await | ❌ killed when process exits | runs and exits |
| **2. Quick orchestration** | `ion --host "msg"` | host engine | event pump → stdout | ✅ | ✅ host holds them | auto-exit on recursive idle |
| **3. Persistent service** | `ion serve` | host engine + socket | socket → external UI | ✅ | ✅ host holds them | manual shutdown |

> **Scenarios 2 and 3 share the same host engine** — `WorkerRegistry`, `spawn_worker`,
> and event forwarding are identical. The **only difference** is how they expose to the
> outside world: Scenario 2 pumps events to stdout (auto-exit on idle), Scenario 3
> exposes a Unix socket for external UI (stays alive).
>
> **Note:** Scenarios 2 and 3 both rely on the `ion-worker` binary for `spawn_worker`.
> The host spawns `ion-worker` child processes (JSONL over stdin/stdout). Build both
> binaries with `cargo build --bin ion --bin ion-worker`.

---

### Scenario 1: Quick Execution

```bash
ion "summarize this repo"
```

Direct spawn — no host process. The CLI builds a tool set + Agent, runs a single
agent turn, then the process exits.

```
Terminal                     In-process
┌──────┐   ┌──────────────────────────┐
│ ion  │──▶│  cmd_run()               │
│      │   │  build tools + Agent     │
│      │   │  agent.run(message)      │
│      │   │    ├─ LLM loop            │
│      │   │    ├─ call tool (read/write)│
│      │   │    ├─ spawn_worker (sync)  │
│      │   │    │    └─ spawn child proc│
│      │   │    │        await done     │
│      │   │    └─ return              │
│      │   └─ process exits            │
└──────┘                              │
    ❌ no host, no async tasks
    ❌ no event forwarding
    ✅ sync subtasks work
```

---

### Scenario 2: Quick Orchestration

```bash
ion --host "refactor the auth module and add tests"
```

A **temporary host** is spawned with an event pump. The host provides
`WorkerRegistry` + `spawn_worker` for multi-agent coordination (sync and async
subtasks). Events are pumped to stdout in real-time. The host auto-exits when all
workers are idle (recursive idle detection).

```
Terminal                        Temporary host
┌──────┐  ┌──────────────────────────────────────────────┐
│ ion  │──│  WorkerRegistry + command loop + event pump  │
│      │  │  spawn coordinator Worker (child process)    │
│--host│  │    ├─ spawn_worker(dev, sync)                 │
│      │  │    │    └─ host creates child → await done   │
│      │  │    ├─ spawn_worker(dev, async)                │
│      │  │    │    └─ host creates child                 │
│      │  │    │       └─ child runs → agent_end         │
│      │  │    └─ channel_send ← inter-worker comms      │
│      │  │  event pump → stdout (text_delta in realtime)│
│      │  │  ...all idle → cleanup & exit                 │
└──────┘  └──────────────────────────────────────────────┘
    ✅ host engine: sync + async subtasks
    ✅ event pump → stdout
    ❌ no socket, external tools cannot connect
```

**Exit condition (recursive idle):**
```
entry Worker (coordinator) idle?
├─ its child Worker 1 idle?
│   └─ child of child idle?
├─ child Worker 2 idle?
└─ ...all idle
  → no background processes running → cleanup & exit
```

---

### Scenario 3: Persistent Service

```bash
ion serve              # always-on host via Unix socket (~/.ion/host.sock)
ion "do something"     # any client connects to the running host
```

An always-on host listening on a Unix domain socket. External UI / TUI / IDE
plugins connect and stay connected. Multiple CLI invocations talk to the same
long-lived host. The host does **not** auto-exit — it requires manual shutdown.

```
External UI / TUI / IDE                Always-on host
┌─────────────────┐   ┌───────────────────────────────────────┐
│        socket    │   │  WorkerRegistry + command loop        │
│  Web UI          │   │  Unix socket → ~/.ion/host.sock       │
│  ┌───────────┐   │   │  spawn Worker (child process)         │
│  │progress   │◄──│───│  ├─ sync: spawn → await (UI visible)  │
│  │cards      │   │   │  │  └─ push text_delta via socket     │
│  │real-time  │   │   │  ├─ async: spawn → agent_end (visible)│
│  └─────────��─┘   │   │  │  └─ push agent_start → text_delta   │
│                  │   │  │        → agent_end                  │
│  ion rpc CLI     │   │  ├─ channel_send ← inter-worker comms │
│  ┌───────────┐   │   │  ├─ subscribe → event stream to socket│
│  │create_    │───│───│  └─ keeps running (no auto-exit)       │
│  │worker     │   │   │                                       │
│  └───────────┘   │   │                                       │
└─────────────────┘   └───────────────────────────────────────┘
    ✅ host engine: sync + async subtasks
    ✅ events via socket → external UI (cards / progress bars)
    ❌ no auto-exit, requires manual shutdown
```

---

## 场景 4：沙箱隔离

### 4a：只读沙箱

**前置配置：**
```json
{
  "runtime": { "default": "sandbox" },
  "permissions": {
    "sandbox_profiles": { "active": "readonly" }
  }
}
```

**CLI 验证：**
```bash
# 读文件 → 沙箱内可读
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"src/main.rs"}}'
# → ✅ 沙箱内可读

# 写文件 → 沙箱内拒绝（只读）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","args":{"file_path":"src/main.rs","content":"hack"}}'
# → ❌ sandbox-exec 拒绝写入
```

### 4b：工作区可写沙箱

**前置配置：**
```json
{
  "runtime": { "default": "sandbox" },
  "permissions": {
    "sandbox_profiles": { "active": "workspace" }
  }
}
```

**CLI 验证：**
```bash
# 写工作区文件 → 沙箱内允许
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","args":{"file_path":"/Users/me/project/test.txt","content":"ok"}}'
# → ✅ 工作区可写

# 读 SSH key → 沙箱内拒绝（deny rule）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"read","args":{"file_path":"~/.ssh/id_rsa"}}'
# → ❌ sandbox-exec deny ~/.ssh
```

### 4c：Agent 提权（沙箱外执行）

**前置配置：**
```json
{
  "runtime": {
    "default": "sandbox",
    "sandbox": {
      "profile": "readonly",
      "allow_escape_with_approval": true,
      "escape_approval_mode": "ask"
    }
  }
}
```

**CLI 验证：**
```bash
# Terminal 1：订阅 UI 事件
ion subscribe --ui

# Terminal 2：Agent 需要写 /etc/hosts（沙箱内拒绝）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"write","args":{"file_path":"/etc/hosts","content":"127.0.0.1 test.local"}}'

# Terminal 1 收到 Ask：
# {"type":"ui_event","ui_type":"Ask",
#  "title":"提权申请","message":"Agent 请求在沙箱外执行: 写 /etc/hosts"}

# 用户批准：
ion rpc --method ui_respond \
  --params '{"request_id":"req_xxx","response":"allow"}'

# Terminal 2 的写操作 → 沙箱外执行 ✅
```

---

## 场景 5：Worker 全远程

**前置配置：**
```json
{
  "remote_workers": {
    "jd": {
      "host": "jd.server.com",
      "user": "deploy",
      "transport": "ssh",
      "worker_bin": "/usr/local/bin/ion-worker",
      "cwd": "/home/deploy/project"
    }
  }
}
```

**CLI 验证：**
```bash
# Manager 在本地，Worker 在远程
ion rpc --method create_worker --params '{"host":"jd","cwd":"/home/deploy/project"}'
# → Manager SSH 到 jd → 启动 ion-worker → 返回 sessionId ✅

# 命令在远程执行
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"cargo build --release"}}'
# → ✅ jd 上编译

# 事件流实时回来
ion subscribe --session <sid>
# → ✅ 收到 text_delta / agent_start / agent_end
```

---

## 场景 6：权限 + 远程（组合）

**前置配置：**
```json
{
  "runtime": {
    "default": "remote",
    "remote": { "default_host": "xyz-mac" }
  },
  "extensions": {
    "permission": { "enabled": true }
  }
}
```

**CLI 验证：**
```bash
# 添加权限规则
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"permission","method":"add_rule",
    "args":{"subject":"command.run","pattern":"kubectl delete *","decision":"deny","scope":"session"}}'

# 被拒绝的操作 → SSH 都没发出去
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"kubectl delete pod xxx"}}'
# → ❌ 本机权限拒绝，无 SSH 调用 ✅

# 允许的操作 → SSH 到远程执行
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"kubectl get pods"}}'
# → ✅ PermissionExtension 放行 → RemoteRuntime SSH → xyz-mac 执行

# 配合沙箱提权 + 权限 + 远程三个一起
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash","args":{"command":"ssh prod-server 'systemctl restart app'"}}'
# → 1. PermissionExtension 检查 command.run
# → 2. SecuredRuntime CommandGuard 检查
# → 3. 沙箱判断是否需要提权（如果需要 → UI Ask）
# → 4. RouterRuntime 路由到 RemoteRuntime
# → 5. SSH 到 xyz-mac 执行
```

---

## 实现路线

| 组件 | 需要实现 | 状态 |
|------|---------|------|
| `RemoteRuntime` | SSH/HTTP/gRPC 传输 Runtime | 🔧 待做 |
| `SandboxRuntime` | macOS sandbox-exec 包装 | 🔧 待做 |
| `RouterRuntime` | 命令级路由 Runtime | 🔧 待做 |
| 沙箱权限分级 | readonly/workspace/full-access 配置 | ✅ 设计完成 |
| Agent 提权 | escape_with_approval + Ask | ✅ 架构就绪 |
| Worker 远程 spawn | Manager SSH 启动远程 Worker | 🔧 待做 |
