# Bash 进程管理扩展设计

> **状态：设计稿（后台进程管理）+ 已实现（同步直接执行）**
>
> 本文档两部分：
> - **§0（已实现）**：同步直接执行路径 — `bash_command` RPC / `!cmd` 直发 / bash 执行结果入库
> - **§1-§12（设计稿）**：未来后台进程管理扩展的目标设计，**当前未实现**
>
> ### 实现状态核查清单
>
> | # | 功能 | 状态 | 验证 |
> |---|------|------|------|
> | 0.1 | `bash_command` RPC（同步直接执行） | ✅ 已实现 | `ion rpc --method bash_command --params '{"command":"echo hi"}'` |
> | 0.2 | `!cmd` 前缀拦截（用户直发） | ✅ 已实现 | `ion rpc --method prompt --params '{"text":"!echo hi"}'` |
> | 0.3 | `Message::BashExecution` 序列化 | ✅ 已实现 | session.jsonl 中 `variant=BashExecution role=bashExecution` |
> | 0.4 | Provider 转换（bashExecution → user text） | ✅ 已实现 | LLM 看到 `Ran \`cmd\`\n```\n...\n```\n` |
> | 1.0 | `BashExtension` 注册 + `extension_rpc` 路由 | ✅ 已实现 | `extension_rpc --params '{"plugin":"bash","method":"list"}'` |
> | 1.1 | `bash_run` LLM 工具（前台同步） | ✅ 已实现 | 调用 `bash_run` tool，见 `bash list` |
> | 1.2 | `bash_run` LLM 工具（后台异步） | ✅ 已实现 | `background=true` 立即返回，完成后 `follow_up` 通知 |
> | 1.3 | `bash_kill` LLM 工具 + RPC | ✅ 已实现 | `extension_rpc --params '{"plugin":"bash","method":"kill","args":{"pid":N}}'` |
> | 1.4 | `extension_rpc inspect`（含 tail 输出预览） | ✅ 已实现 | `extension_rpc --params '{"plugin":"bash","method":"inspect","args":{"pid":N,"tail":50}}'` |
> | 1.5 | `extension_rpc clean`（清理已结束进程） | ✅ 已实现 | `extension_rpc --params '{"plugin":"bash","method":"clean"}'` |
> | 1.6 | `follow_up` 通道（后台完成通知） | ✅ 已实现 | 后台进程结束后自动注入 `Message::Custom` |
| 1.7 | `emit_extension_event`（process_started/completed/killed/error） | ✅ 已实现 | `process_started` 在 spawn 后发出，`process_completed`/`killed`/`error` 在完成后发出 |
| 1.8 | 文件日志 `/tmp/ion-bash/{pid}.log` | ✅ 已实现 | 后台进程的 stdout+stderr 写入 `{pid}.log` |
| 1.9 | `extension_rpc inspect` 输出预览 | ✅ 已实现 | `tail=N` 参数控制返回最后 N 行 |
> | 1.10 | `bash_send` LLM 工具 + `extension_rpc send` | ✅ 已实现 | `bash_send pid=10000 input="Y"` / `extension_rpc send` |
> | 1.11 | 超时自动切后台（timeoutBackground） | ✅ 已实现 | `bash_run timeoutBackground=true` — 超时后转为后台 |
> | 1.12 | processes.json 持久化 | ✅ 已实现 | 进程状态自动保存到 `~/.ion/tmp/ion-bash/processes.json` |
> | 1.13 | `call_tool` RPC（绕开 LLM 直接调任意工具） | ✅ 已实现 | `ion rpc --method call_tool --params '{"tool":"bash_run","args":{...}}'` |
> | 1.14 | `bash_background` 工具 | ✅ 已实现 | `call_tool --params '{"tool":"bash_background","args":{"pid":10000}}'` |
> | — | 全部 21 项完成 | ✅ 21/21 | 文档与代码完全对齐 |

---

## 0. 已实现部分（同步直接执行）

> 同步执行路径已上线。本节记录接口规格与 JSON 结构，不含实现细节。

### 0.1 三种入口

| 入口 | 调用方式 | 是否走 agent loop | 入库类型 |
|------|---------|------------------|---------|
| `!cmd` 直发 | `prompt` RPC，text 以 `!` 开头 | ❌ | bash 执行消息（role: `bashExecution`） |
| `bash_command` RPC | `ion rpc --method bash_command` | ❌ | bash 执行消息（role: `bashExecution`） |
| `bash` LLM 工具 | LLM 调用 `bash` tool | ✅ | 工具结果（tool role） |

前两个入口的结果会以 `role: bashExecution` 写入对话历史，跟真实用户消息（`role: user`）区分开，UI 渲染时可识别为 bash 卡片。第三个走标准工具结果路径（后续切换到 bashExecution 是 §1-§12 后台扩展的工作）。

### 0.2 `bash_command` RPC

直接执行 bash 命令，结果作为 bash 执行消息入历史。不走 agent loop，不调用 LLM。

**请求：**
```bash
ion rpc --session <sid> --method bash_command \
  --params '{"command":"ls -la","timeout":30,"excludeFromContext":false}'
```

**请求 JSON：**

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `command` | string | 必填 | shell 命令 |
| `timeout` | number | 30 | 超时秒数，超时杀进程并返回 error |
| `excludeFromContext` | boolean | null | true 时 LLM 看不到这条（对应 pi 的 `!!cmd` 静默执行） |

**响应 JSON（成功）：**
```json
{
  "status": "ok",
  "exitCode": 0,
  "output": "total 8\n-rw-r--r--  1 user  Cargo.toml\n",
  "truncated": false
}
```

**响应 JSON（失败）：**
```json
{
  "status": "error",
  "error": "bash timed out after 30s",
  "exitCode": null,
  "output": null
}
```

### 0.3 `!cmd` 直发（prompt 拦截）

`prompt` RPC 的 text 以 `!` 开头时，自动拦截成 bash 执行（不进 agent loop）。`!` 后面的内容作为命令。

**请求：**
```bash
ion rpc --session <sid> --method prompt --params '{"text":"!ls -la"}'
```

**响应 JSON：**
```json
{
  "status": "bash_executed",
  "command": "ls -la",
  "exitCode": 0,
  "output": "...",
  "truncated": false
}
```

同时推送 `agent_start` / `text_delta` / `agent_end` 事件，UI 可渲染为 bash 卡片。

### 0.4 bash 执行消息入库结构

写入 session.jsonl 的样子（externally-tagged 格式，跟其他消息一致）：

```json
{
  "type": "message",
  "id": "...",
  "parentId": "...",
  "timestamp": "...",
  "message": {
    "BashExecution": {
      "role": "bashExecution",
      "command": "echo hello",
      "output": "hello\n",
      "exit_code": 0,
      "cancelled": false,
      "truncated": false,
      "full_output_path": null,
      "timestamp": 1783222298196,
      "exclude_from_context": null
    }
  }
}
```

**字段说明：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `role` | string | 固定 `"bashExecution"`，跟 `"user"`/`"assistant"` 区分 |
| `command` | string | 执行的命令 |
| `output` | string | stdout + stderr 合并 |
| `exit_code` | number \| null | 退出码，null 表示未拿到（spawn 失败/超时） |
| `cancelled` | boolean | 是否被取消 |
| `truncated` | boolean | 输出是否被截断 |
| `full_output_path` | string \| null | 完整输出的文件路径（截断时提供），当前实现总是 null |
| `exclude_from_context` | boolean \| null | true 时 LLM 看不到这条 |

**LLM 看到的转换：** `role: bashExecution` → `role: user`，content 变成：

```
Ran `{command}`
```
{output}
```
```

加上追加提示：cancelled / exit_code != 0 / truncated 时分别追加对应说明。`exclude_from_context == true` 时完全不发给 LLM。

### 0.5 输出截断策略

stdout 和 stderr 各自独立截断，单流上限 100,000 字节。超出后追加 `...[truncated N bytes]` 标记。截断后两流合并：`{stdout}\n[stderr]\n{stderr}`。

### 0.6 已实现 vs §1-§12 设计稿的差异

| 维度 | 已实现（§0） | 设计稿（§1-§12） |
|------|-------------|------------------|
| 执行模式 | 同步阻塞 | 同步 + 后台双模式 |
| LLM 工具 | `bash`（单工具，仅同步） | `bash_run` / `bash_background` / `bash_kill` / `bash_send`（4 个） |
| 管理 RPC | `bash_command` | `list` / `kill` / `send` / `inspect` / `clean`（5 个） |
| 事件推送 | 无 | `process_started` / `completed` / `killed` / `error` / `background` |
| 输出存储 | in-memory，100KB 截断 | `/tmp/ion-bash/{pid}.log` 持久日志 |
| 进程状态 | 不保存 | `~/.ion/tmp/ion-bash/processes.json` |
| 后台通知 | 不支持 | `send_custom_message(deliverAs="followUp")` |
| 超时切后台 | 不支持（超时直接杀） | 前台超时自动转后台 |

> **跨文档注**：§9 旧版用 `agent.follow_up(Message::User(...))` 描述后台完成通知，现已统一对齐到 `send_custom_message` + `Message::Custom` 路径（参 [SESSION_MESSAGE.md §三](./SESSION_MESSAGE.md)）。后台通知消息以 `role: custom` 入库（不是 `role: user`），UI 可区分渲染。

---

## 1. 角色与入口

| 角色 | 入口 | 用途 |
|------|------|------|
| LLM | Tool RPC | 启动/后台/交互/杀进程（不自查列表，靠 system prompt 注入） |
| CLI / 开发者 | extension_rpc | 调试、手动管理、查列表 |
| 前端 / UI | subscribe（长连接） | 实时看进程状态变化和输出流 |
| 同 session | follow_up | 后台进程完成后通知创建者 |

## 2. 进程生命周期

```
bash_run(command, background?, timeout?)
  │
  ├── foreground (background=false, 默认)
  │   ├── 正常结束 → 返回 stdout/stderr/exit_code（同步，不 follow_up）
  │   ├── 异常退出 → 返回错误
  │   ├── 超过 timeout 秒 → 切后台（background_timeout 原因）→ 摘取已有输出
  │   └── 用户发 bash_background → 切后台（background_manual 原因）
  │
  ├── background (background=true)
  │   ├── 立即返回 {pid, status:"running"}
  │   ├── 通过 subscribe 实时看输出流
  │   ├── 通过 bash_send 交互（LLM tool + plugin RPC 双入口）
  │   ├── 正常结束 → completed 事件 + follow_up 给 creator
  │   ├── 用户手动 kill → completed(manual) 事件 + follow_up 给 creator
  │   ├── 超时 → completed(timeout) 事件 + follow_up 给 creator
  │   └── 异常退出 → completed(abnormal) 事件 + follow_up 给 creator
  │
  └── 异常
      ├── 命令不存在 → process_error 事件
      └── PID 不存在 → 结构化错误
```

### follow_up 规则

| 执行方式 | 结束后 | 示例 |
|---------|--------|------|
| 前台同步 | 不 follow_up，直接返回结果 | `bash_run echo ok` → 返回 stdout |
| 后台异步 | **必须** follow_up 给 creator | `bash_run(background=true)` → 完成后 creator 收到一条 user message |

## 3. 退出原因

| reason | 说明 | 触发场景 |
|--------|------|---------|
| `completed` | 正常结束 exit 0 | 前台/后台进程自然结束 |
| `abnormal` | 异常退出 exit != 0 | 命令失败 |
| `timeout` | 超时关闭 | `bash_run(timeout=30)` 30s 后没结束 |
| `manual` | 用户手动关闭 | `bash_kill(pid)` 或 `extension_rpc kill` |
| `background_timeout` | 前台超时自动转入后台 | 前台无 timeout 参数，运行超 300s |
| `background_manual` | 用户手动转入后台 | `bash_background` 调用 |
| `service_shutdown` | Manager 退出清理 | `ion manager stop` 时清理残留 |

## 4. 系统提示词注入（代替 bash_list）

每次 Agent 启动 / 进程状态变化时，system prompt 末尾自动追加：

```xml
<running_processes>
  <process pid="12345" command="sleep 600" elapsed="30s"/>
  <process pid="23456" command="ping 127.0.0.1" elapsed="120s"/>
</running_processes>
```

- 只展示正在运行中的进程
- 已退出/已结束的不展示（通过 follow_up 通知）
- 实时更新：进程启动/结束/状态变化时重新注入

**因此 `bash_list` 不需要作为 LLM Tool。** LLM 靠 system prompt 知道当前有哪些后台进程。

## 5. 输出存储与查看

每次 `bash_run` 的输出写入 OS 临时目录（被系统自动清理，无需手动回收）：

```
/var/folders/.../T/ion-bash/     ← macOS（系统自动清理 old files）
/tmp/ion-bash/                   ← Linux（系统自动清理）
```

通过 `system_tmp_dir()`（`paths.rs`）获取路径，支持 `ION_TMP_DIR` 环境变量覆盖。

**CLI 查看方法：**

```bash
# 直接读日志文件
cat $(ion rpc --session x --method extension_rpc --params '{"plugin":"bash","method":"inspect","args":{"pid":12345}}' | grep log_path | cut -d'"' -f4)
# 或通过 inspect 看摘要
ion rpc --session x --method extension_rpc \
  --params '{"plugin":"bash","method":"inspect","args":{"pid":12345}}'
```

**inspect 响应的 output 处理逻辑：**

| 日志大小 | 展示方式 |
|---------|---------|
| ≤ 2000 字符 | 全部返回 |
| > 2000 字符 | 前 600 字 + 折叠标记 + 后 600 字 |
| 超长 | 同上 + `log_path` 指向完整文件路径 |

输出文件保留策略：Manager 重启或 `bash_clean` 时清理已退出进程的日志。

### 实时输出查看

执行过程中（同步或异步），用户可通过以下方式看实时输出：

```bash
# 方式 A：直接 tail 日志文件（最轻量）
tail -f /tmp/ion-bash/12345.stdout.log

# 方式 B：定期 inspect 拉取最新 N 行
watch -n 2 ion rpc --session x --method extension_rpc \
  --params '{"plugin":"bash","method":"inspect","args":{"pid":12345,"tail":50}}'

# 方式 C：CLI 终端接管（同步执行时另开 terminal）
# Terminal 1: 执行命令
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"long task","description":"run"}}'
# Terminal 2: 看实时输出
tail -f /tmp/ion-bash/<pid>.stdout.log
```

实时输出**不经过 EventBus**，走文件，零事件开销。

## 6. LLM Tools（4 个）

所有工具统一参数：

| 参数 | 必填 | 说明 |
|------|------|------|
| `command` | 是 | bash 命令 |
| `description` | 是 | 描述用途，最长 30 字符（如"install deps""start server"） |
| `timeout` | 否 | 超时秒数，默认 300 |
| `background` | 否 | 是否后台运行，默认 false |

Tool 返回给 LLM 的是**自然语言文本**，不是 JSON。结构化数据是 CLI/外层的事。

### 落库格式（session.jsonl）

Tool 执行记录写入 session.jsonl 时，附带 `details` 字段存储结构化元数据（UI 重建展示用，不影响 LLM）：

```json
{
  "type": "message",
  "message": {
    "role": "tool",
    "content": [{"ToolResult": {
      "tool_call_id": "call_xxx",
      "name": "bash_run",
      "content": "✅ Process #12345 (install deps) completed\n  Output: added 150 packages...",
      "details": {
        "pid": 12345,
        "command": "npm install",
        "description": "install deps",
        "exit_code": 0,
        "duration_secs": 45,
        "started_at": 1700000000,
        "ended_at": 1700000045,
        "reason": "completed",
        "killed_by": null,
        "timeout_secs": 120,
        "log_path": "/tmp/ion-bash/12345.stdout.log"
      }
    }}]
  }
}
```

`details` 字段在各场景下的差异：

| 场景 | exit_code | duration | reason | killed_by | timeout_secs |
|------|-----------|----------|--------|-----------|-------------|
| 正常结束 | 0 | 实际值 | completed | null | 传入值 |
| 异常退出 | 非0 | 实际值 | abnormal | null | 传入值 |
| 用户 kill | null | 实际值 | manual | "user"/"cli" | 传入值 |
| 超时 | null | timeout 值 | timeout | null | 传入值 |
| service shutdown | null | 实际值 | service_shutdown | "manager" | 传入值 |
| 转后台（超时） | null | 300 | background_timeout | null | 传入值 |
| 转后台（手动） | null | 实际值 | background_manual | "user" | 传入值 |

### `bash_run`

```
Foreground (sync):
args: bash_run(command="npm install", description="install deps", timeout=120)
ok    → ✅ Process #12345 (install deps) completed
          Output: added 150 packages...
          Duration: 45s
          Exit code: 0

fail  → ❌ Process #12345 (install deps) failed
          Error: error: permission denied
          Duration: 5s
          Exit code: 1

killed → ⛔ Process #12345 (install deps) terminated by user
           Duration: 18s

timeout→bg → ⏳ Process #12345 (install deps) timeout, moved to background
               Partial output: npm...
               Elapsed: 300s
               PID: 12345 (use bash_kill to stop)

Background (async):
args: bash_run(command="npm install", description="install deps", background=true)
started → ✅ Process #12345 (install deps) started in background

timeout  → ⛔ Process #12345 (install deps) timed out and was killed
             Duration: 120s
```

### `bash_background`

```
args: bash_background()
→ ⏳ Process #12345 (install deps) moved to background
     Partial output: first 10s of output...
     Elapsed: 10s
     PID: 12345
```

### `bash_kill`

```
args: bash_kill(pid=12345)
→ ⛔ Process #12345 (install deps) terminated
     Duration: <seconds>

Sync processes can also be killed. Return is the same.
```

### `bash_send`

```
args: bash_send(pid=12345, input="Y\n")
→ ✅ Sent to process #12345: Y
```

## 7. Plugin RPC（CLI 管理）

Plugin RPC 返回结构化 JSON（给 CLI 解析用）。

| 方法 | 参数 | 响应 | 说明 |
|------|------|------|------|
| `list` | `{status?}` | `[{pid, command, description, status, elapsed}]` | 列进程 |
| `kill` | `{pid}` | `{status:"killed", reason, duration}` | 杀进程 |
| `send` | `{pid, input}` | `{status:"delivered"}` | 发 stdin |
| `inspect` | `{pid, tail?:100}` | `{pid, command, description, status, output_preview, log_path, elapsed, exit_code?}` | 查详情。`tail` 控制返回最后 N 行，默认 100 行，防止日志爆炸 |
| `clean` | `{}` | `{cleaned: N}` | 清理已结束进程 |

## 8. 事件路由总表

### 实际通道（基于代码现状）

| 通道 | 调用方式 | 数据流 |
|------|---------|--------|
| `agent → emit()` | Agent 内置，tool 生命周期自动触发 | Agent loop → `println!` stdout →  Worker stdout reader → pump → event_subscribers → `subscribe --session x` |
| `extension → emit_extension_event()` | 扩展调 `ExtensionApi::emit_extension_event()` | `println!` stdout(type=extension_event) → pump 检测 `extension_event` → EventBus broadcast → `subscribe --extension bash` |
| `extension → notify()` | 扩展直接插入 user message | 追加到 agent.messages → 下一轮 LLM 调用自动带上 |

### 事件清单

| 事件 | 触发者 | 实际通道 | 接收方 |
|------|--------|---------|--------|
| `tool_execution_start` | LLM 调 `bash_run` | `agent → emit()` | `subscribe --session x` |
| `tool_execution_update` | bash 实时输出 | `agent → emit()` | `subscribe --session x` |
| `tool_execution_end` | bash 执行完毕 | `agent → emit()` | `subscribe --session x` |
| `process_started` | bash 进程启动 | `extension → emit_extension_event()` | `subscribe --extension bash` |
| `process_completed` | 后台进程结束 | `extension → emit_extension_event()` | `subscribe --extension bash` |
| `process_killed` | 后台进程被关 | `extension → emit_extension_event()` | `subscribe --extension bash` |
| `process_error` | 进程启动失败 | `extension → emit_extension_event()` | `subscribe --extension bash` |
| `<bash_result>` 通知 | 后台异步完成 | `extension → notify()` | 创建者会话下一轮 |

### 查看方式

```bash
# tool_execution 流（LLM 调 bash 时）
ion subscribe --session x
# → tool_execution_start → tool_execution_update(逐行) → tool_execution_end

# 进程状态变化
ion subscribe --session x --extension bash
# → process_started / completed / killed / error

# 实时输出（走文件，零事件开销）
tail -f /tmp/ion-bash/12345.stdout.log

# 查摘要
ion rpc --session x --method extension_rpc \
  --params '{"plugin":"bash","method":"inspect","args":{"pid":12345,"tail":50}}'
```

## 9. 扩展通知（Plugin Notification）

后台进程**异步结束**后，注入一条带 XML 标签的消息到会话中：

```xml
<bash_result>
  ✅ Process #12345 (install deps) completed. Output: added 150 packages.
</bash_result>
```

- 通过 RPC 插入到 messages（不是 `follow_up`，是直接追加一条 user message）
- Agent 下一轮自动看到（属于对话上下文的一部分）
- 前端检测 `<bash_result>` 标签，渲染为卡片样式，不混在普通消息里
- 详情数据存在 session.jsonl 的 `details` 字段，UI 可通过 call_id 查询
| `manual` | `[bash:killed] Process #N (desc) terminated by user` |
| `service_shutdown` | `[bash:shutdown] Process #N (desc) terminated (manager shutdown)` |
| `background_timeout` | `[bash:background] Process #N (desc) timeout, moved to background` |
| `background_manual` | `[bash:background] Process #N (desc) moved to background` |

## 10. 完整场景流程

### 事件清单

| customType | 触发 | data 字段 | 推送方式 |
|-----------|------|-----------|---------|
| `process_started` | 任何方式启动 | `{pid, command, background, session}` | subscribe + system prompt |
| `process_completed` | 进程退出 | `{pid, exit_code, reason, output?}` | subscribe + follow_up（异步） |
| `process_background` | 前台切后台 | `{pid, reason, partial_output}` | subscribe |
| `process_output` | 进程实时输出 | `{pid, data, stream:`stdout`|`stderr`}` | subscribe（按 PID 过滤） |
| `process_killed` | 被 kill | `{pid, reason, by}` | subscribe + follow_up |
| `process_error` | 启动失败 | `{pid?, error}` | subscribe |

## 10. 完整场景流程

### 场景 A：前台执行（同步）

```bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"echo hello","timeout":30}}'
# → 阻塞等待，返回：
# {"stdout":"hello\n","stderr":"","exitCode":0,"pid":12345}
```

### 场景 B：前台超时自动转后台

```bash
# timeout 默认 300s
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 600"}}'
# → 300s 后切后台：
# {"pid":12345,"status":"background","partial_output":"","reason":"background_timeout"}
# subscribe 同时收到 process_background 事件
```

### 场景 C：前台手动转后台

```bash
# Terminal 1: 跑一个前台命令
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 300"}}'
# 10s 后 Terminal 2:
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_background","args":{}}'
# → {"pid":12345,"status":"background","partial_output":"","reason":"background_manual"}
```

### 场景 D：后台执行 + 实时流

```bash
# Terminal 1: 订阅事件
ion subscribe --session x --extension bash

# Terminal 2: 后台启动
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"ping -c 5 127.0.0.1","background":true}}
# → {"pid":23456,"status":"running"}

# Terminal 1 实时收到：
# process_started → process_output × 5 → process_completed
```

### 场景 E：后台交互（两种入口）

**入口 1：LLM 通过 Tool 交互**

```bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_send","args":{"pid":23456,"input":"Y\n"}}'
# → {"pid":23456,"sent":"Y\n","status":"delivered"}
```

**入口 2：用户/CLI 通过 Plugin RPC 交互**

```bash
ion rpc --session x --method extension_rpc \
  --params '{"plugin":"bash","method":"send","args":{"pid":23456,"input":"N\n"}}'
# → {"pid":23456,"sent":"N\n","status":"delivered"}
```

### 场景 F：超时关闭

```bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 600","background":true,"timeout":30}}
# → 立即返回 PID，30s 后进程被杀
# subscribe 收到：process_completed {reason:"timeout"}
# creator 收到 follow_up："[bash] 进程 12345 已超时关闭"
```

### 场景 G：手动杀进程（两种入口）

**入口 1：LLM 通过 Tool**

```bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_kill","args":{"pid":23456}}'
# → {"pid":23456,"status":"killed","reason":"manual"}
```

**入口 2：用户/CLI 通过 Plugin RPC**

```bash
ion rpc --session x --method extension_rpc \
  --params '{"plugin":"bash","method":"kill","args":{"pid":23456}}'
# → {"pid":23456,"status":"killed","reason":"manual"}
```

### 场景 H：查看进程详情 + 日志分页

```bash
# 列进程
ion rpc --session x --method extension_rpc \
  --params '{"plugin":"bash","method":"list","args":{"status":"all"}}'
# → [{pid:12345, command:"sleep 600", status:"running", elapsed:30}]

# 查看详情（含输出摘要）
ion rpc --session x --method extension_rpc \
  --params '{"plugin":"bash","method":"inspect","args":{"pid":12345}}
# → {pid, command, status, output_preview, log_path, elapsed}

# 直接读完整日志文件
cat ~/.ion/agent/project-data/--hash--name--/bash/logs/12345.stdout.log
```

### 场景 I：清理已完成进程

```bash
ion rpc --session x --method extension_rpc \
  --params '{"plugin":"bash","method":"clean","args":{}}'
# → {"cleaned":5}  # 清理了 5 条已结束进程的记录和日志
```

### 场景 J：异常退出

```bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"ls /not_exist"}}'
# → {"stderr":"ls: /not_exist: No such file or directory","exitCode":1,"pid":12345}
```

### 场景 K：Manager 退出清理

```bash
kill $(cat ~/.ion/manager.pid)
# Manager 遍历所有 live pid → kill
# subscribe 收到：process_completed {reason:"service_shutdown"}
```

## 11. 接口数量汇总

| 类型 | 数量 | 列表 |
|------|------|------|
| LLM Tools | 4 | `bash_run`, `bash_background`, `bash_kill`, `bash_send` |
| Plugin RPC | 5 | `list`, `kill`, `send`, `inspect`, `clean` |
| Subscribe | 1 | 长连接事件推送（不是方法） |
| Events | 6 | `process_started`, `process_completed`, `process_background`, `process_output`, `process_killed`, `process_error` |

## 12. 文件存储

```
{system_tmp_dir}/ion-bash/                ← OS 临时目录，系统自动清理
├── {pid}.stdout.log
├── {pid}.stderr.log
└── processes.json                        ← 当前 Manager 的进程记录
```

- 路径通过 `paths::system_tmp_dir()` 获取
- 默认 `/tmp/ion-bash/`（Linux）或 `/var/folders/.../T/ion-bash/`（macOS）
- 支持 `ION_TMP_DIR` 环境变量覆盖
- **系统自动清理**，ION 不需要额外做 cleanup
