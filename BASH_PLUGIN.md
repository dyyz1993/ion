# Bash 进程管理插件设计

> **状态：设计稿** — 等待确认后开发。

## 1. 角色与入口

| 角色 | 入口 | 用途 |
|------|------|------|
| LLM | Tool RPC | 自主启动/管理/交互后台进程 |
| CLI / 开发者 | call_tool + plugin_rpc | 调试、手动管理 |
| 前端 / UI | subscribe | 实时看进程状态和输出流 |

## 2. 进程生命周期

```
bash_run(command, background?, timeout?)
  │
  ├── foreground (background=false, 默认)
  │   ├── 正常结束 → 返回 stdout/stderr/exit_code
  │   ├── 异常退出 → 返回错误
  │   ├── 超过 timeout 秒 → 切后台（timeout_reached 原因）
  │   └── 用户发 bash_background → 切后台（manual 原因）
  │
  ├── background (background=true)
  │   ├── 立即返回 {pid, status:"running"}
  │   ├── 通过 subscribe 看实时输出流
  │   ├── 通过 bash_send 交互
  │   ├── 正常结束 → completed 事件 + follow_up creator
  │   ├── 用户 kill → completed(manual) 事件 + follow_up creator
  │   └── 超时 → completed(timeout) 事件
  │
  └── 异常
      ├── bash 不存在 → process_error 事件
      └── PID 不存在 → 结构化错误
```

## 3. 退出原因

| reason | 说明 | 触发场景 |
|--------|------|---------|
| `completed` | 正常结束 exit 0 | 前台/后台进程自然结束 |
| `abnormal` | 异常退出 exit != 0 | 命令执行失败 |
| `timeout` | 超时关闭 | `bash_run(timeout=30)` 30s 后没结束 |
| `manual` | 用户手动关闭 | `bash_kill(pid)` |
| `background_timeout` | 前台超时自动转入后台 | 前台无 timeout 参数，运行超 300s |
| `background_manual` | 用户手动转入后台 | `bash_background` 调用 |
| `service_shutdown` | Manager 退出清理 | `ion manager stop` 时清理残留 |

## 4. LLM Tools（5 个）

### `bash_run`

前台执行（阻塞等结果）：

```
参数: {command: "sleep 5 && echo ok", timeout?: 60, background?: false}
响应（前台）: {stdout: "ok", stderr: "", exitCode: 0, pid: 12345}
响应（后台）: {pid: 12345, status: "running", note: "background=true returns immediately"}
```

- `timeout` 默认 300s。超时后前台进程自动转入后台（原因 `background_timeout`），摘取已有输出。
- `background=true` 立即返回 PID，进程在后台跑。

### `bash_background`

将当前前台进程转入后台：

```
参数: {}   ← 无参数，只能操作当前 session 正在前台跑的进程
响应: {pid: 12345, status: "background", partial_output: "已执行的部分输出", reason: "manual"}
```

### `bash_kill`

```
参数: {pid: 12345}
响应: {pid: 12345, status: "killed", reason: "manual"}
```

注意：kill 时会发送 follow_up 给创建者，告知进程已被关闭。

### `bash_send`

向进程发送 stdin（用于交互，如输入 Y/N）：

```
参数: {pid: 12345, input: "Y\n"}
响应: {pid: 12345, sent: "Y\n", status: "delivered"}
```

### `bash_list`

```
参数: {status?: "running" | "all"}
响应: [{pid: 12345, command: "sleep 60", status: "running", started_at: 1700000, elapsed_secs: 30}]
```

## 5. Plugin RPC（CLI 管理，5 个）

LLM 不可见，通过 `plugin_rpc` 调用：

| 方法 | 参数 | 响应 | 说明 |
|------|------|------|------|
| `list` | `{status?}` | `[{pid, command, status, elapsed}]` | 列进程 |
| `kill` | `{pid}` | `{status:"killed", reason}` | 杀进程 |
| `send` | `{pid, input}` | `{status:"delivered"}` | 发 stdin |
| `inspect` | `{pid}` | `{pid, command, status, stdout_preview, elapsed}` | 查进程详情 |
| `subscribe` | `{pid}` | 进入 stream mode，持续推 `process_output` 事件 | 实时看输出 |

## 6. Events（6 种）

| customType | 触发 | data 字段 |
|-----------|------|-----------|
| `process_started` | 任何方式启动 | `{pid, command, background, session}` |
| `process_completed` | 进程退出 | `{pid, exit_code, reason, output?}` |
| `process_background` | 前台切后台 | `{pid, reason, partial_output}` |
| `process_output` | 进程实时输出 | `{pid, data, stream:"stdout"|"stderr"}` |
| `process_killed` | 被 kill | `{pid, reason, by}` |
| `process_error` | 启动失败 | `{pid?, error}` |

## 7. 完整场景流程

### 场景 A：前台执行（默认）

```bash
# LLM 视角
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"echo hello && sleep 2","timeout":30}}'
# → 阻塞 ~2s，返回：
# {"stdout":"hello\n","stderr":"","exitCode":0,"pid":12345}
```

### 场景 B：前台超时自动转后台

```bash
# 无 timeout 参数，默认 300s（5min）
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 600"}}'
# → 300s 后超时，切后台：
# {"pid":12345, "status":"background", "partial_output":"", "reason":"background_timeout"}
```

### 场景 C：前台手动转后台

```bash
# 先跑一个前台命令
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 300","timeout":600}}'
# 等待 10 秒后，在另一个 terminal 执行：
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_background","args":{}}'
# → {"pid":12345, "status":"background", "partial_output":"", "reason":"background_manual"}
```

### 场景 D：后台执行 + 事件流

```bash
# Terminal 1：订阅进程事件
ion subscribe --session x --plugin bash

# Terminal 2：后台启动
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"ping -c 10 127.0.0.1","background":true,"timeout":60}}'
# → {"pid":23456,"status":"running"}

# Terminal 1 实时收到：
# {"customType":"process_started","data":{"pid":23456,"command":"ping -c 10 127.0.0.1","background":true}}
# {"customType":"process_output","data":{"pid":23456,"data":"PING 127.0.0.1 (127.0.0.1): 56 data bytes\n","stream":"stdout"}}
# ... 每行 ping 输出一条 process_output ...
# {"customType":"process_completed","data":{"pid":23456,"exit_code":0,"reason":"completed"}}
```

### 场景 E：后台交互（两种入口）

**入口 1：LLM 通过 Tool 交互**

```bash
# LLM 调 bash_send 给进程发输入
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_send","args":{"pid":23456,"input":"Y\n"}}'
# → {"pid":23456,"sent":"Y\n","status":"delivered"}
```

**入口 2：用户/CLI 通过 Plugin RPC 交互**

```bash
# 用户手动通过插件跟进程交互
ion rpc --session x --method plugin_rpc \
  --params '{"plugin":"bash","method":"send","args":{"pid":23456,"input":"Y\n"}}'
# → {"pid":23456,"sent":"Y\n","status":"delivered"}
```

两入口走同一底层逻辑，只是调用方不同（LLM vs CLI）。

### 场景 F：后台超时关闭

```bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 600","background":true,"timeout":30}}'
# → 立即返回 PID，30s 后进程被 kill
# subscribe 收到：{"customType":"process_completed","data":{"pid":23456,"reason":"timeout"}}
```

### 场景 G：手动杀进程

```bash
# 先启动后台
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 600","background":true}}'

# 通过 bash_kill 杀掉
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_kill","args":{"pid":23456}}'
# → {"pid":23456,"status":"killed","reason":"manual"}

# 或者通过 plugin_rpc 杀（LLM 不可见）
ion rpc --session x --method plugin_rpc \
  --params '{"plugin":"bash","method":"kill","args":{"pid":23456}}'
```

### 场景 H：查进程详情

```bash
# 列所有进程
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_list","args":{"status":"all"}}'

# 查单进程详情
ion rpc --session x --method plugin_rpc \
  --params '{"plugin":"bash","method":"inspect","args":{"pid":23456}}'
# → {pid, command, status, stdout_preview, elapsed_secs, started_at}

# 订阅单进程输出流
ion rpc --session x --method plugin_rpc \
  --params '{"plugin":"bash","method":"subscribe","args":{"pid":23456}}'
# → 长连接，持续推 process_output
```

### 场景 I：异常退出

```bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"ls /not_exist"}}'
# → {"stderr":"ls: /not_exist: No such file or directory","exitCode":1,"pid":23456}
```

### 场景 J：Manager 退出清理

```bash
# 启动后台进程
# 直接关 Manager
kill $(cat ~/.ion/manager.pid)
# Manager 退出前遍历所有 live pid → kill
# subscribe 收到：process_completed {reason: "service_shutdown"}
```

## 8. 设计要点

- **PID 管理**：Bash 进程启动后记录 PID、命令、启动时间、creator session
- **实时流**：后台进程的 stdout/stderr 读取 reader task → push 到 EventBus → subscribe
- **交互**：通过进程 stdin 的写入管道发送输入
- **清理**：Manager 退出时遍历所有进程记录 → kill
- **follow_up**：后台进程 completed/killed 时，发送 follow_up 给创建者的 session
- **超时**：每个进程创建时启动 timeout task，到期 kill

## 9. 接口数量汇总

| 类型 | 数量 | 列表 |
|------|------|------|
| LLM Tools | 5 | `bash_run`, `bash_background`, `bash_kill`, `bash_send`, `bash_list` |
| Plugin RPC | 5 | `list`, `kill`, `send`, `inspect`, `subscribe` |
| Events | 6 | `process_started`, `process_completed`, `process_background`, `process_output`, `process_killed`, `process_error` |

## 10. 测试流程

```bash
# 前置
ion manager start
ion rpc --method create_session --params '{"agent":"developer"}'

# 场景 A 前台
ion rpc --session x --method call_tool --params '{"tool":"bash_run","args":{"command":"echo ok"}}'

# 场景 D 后台 + subscribe
ion subscribe --session x --plugin bash
ion rpc --session x --method call_tool --params '{"tool":"bash_run","args":{"command":"sleep 10","background":true}}'

# 场景 E 交互
ion rpc --session x --method call_tool --params '{"tool":"bash_send","args":{"pid":23456,"input":"Y\n"}}'

# 场景 G 杀进程
ion rpc --session x --method call_tool --params '{"tool":"bash_kill","args":{"pid":23456}}'

# 场景 H 查进程
ion rpc --session x --method plugin_rpc --params '{"plugin":"bash","method":"list"}'
ion rpc --session x --method plugin_rpc --params '{"plugin":"bash","method":"inspect","args":{"pid":23456}}'
