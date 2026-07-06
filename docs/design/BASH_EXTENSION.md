# Bash 进程管理扩展设计

> **状态：设计稿（后台进程管理）+ 已实现（同步直接执行）+ 综合教程**
>
> 本文档三部分：
> - **§0（已实现）**：同步直接执行路径 — `bash_command` RPC / `!cmd` 直发 / bash 执行结果入库
> - **§1-§12（设计稿）**：未来后台进程管理扩展的目标设计，**当前未实现**
> - **§13（CLI 测试指南）**：API 核查清单，覆盖 Group A-E 共 18 项验证（已实测通过）
> - **附录 A（综合教程）**：系统概述、构建启动、Worker 管理、会话基础、消息类型、实时流式、三个核心场景、Web UI、验证脚本
>
> ### 实现状态核查清单
>
> | # | 功能 | 状态 | 验证 |
> |---|------|------|------|
> | 0.1 | `bash_command` RPC（同步直接执行） | ✅ 已实现 | `ion rpc --method bash_command --params '{"command":"echo hi"}'` |
> | 0.2 | `!cmd` 前缀拦截（用户直发） | ✅ 已实现 | `ion rpc --method prompt --params '{"text":"!echo hi"}'` |
> | 0.3 | `Message::BashExecution` 序列化 | ✅ 已实现 | session.jsonl 中 `variant=BashExecution role=bashExecution` |
> | 0.4 | Provider 转换（bashExecution → user text） | ✅ 已实现 | LLM 看到 `Ran \`cmd\`\n```\n...\n```\n` |
> | 1.0 | `BashExtension` 注册 + `extension_rpc` 路由 | ✅ 已实现 | `extension_rpc --params '{"extension":"bash","method":"list"}'` |
> | 1.1 | `bash_run` LLM 工具（前台同步） | ✅ 已实现 | 调用 `bash_run` tool，见 `bash list` |
> | 1.2 | `bash_run` LLM 工具（后台异步） | ✅ 已实现 | `background=true` 立即返回，完成后 `follow_up` 通知 |
> | 1.3 | `bash_kill` LLM 工具 + RPC | ✅ 已实现 | `extension_rpc --params '{"extension":"bash","method":"kill","args":{"pid":N}}'` |
> | 1.4 | `extension_rpc inspect`（含 tail 输出预览） | ✅ 已实现 | `extension_rpc --params '{"extension":"bash","method":"inspect","args":{"pid":N,"tail":50}}'` |
> | 1.5 | `extension_rpc clean`（清理已结束进程） | ✅ 已实现 | `extension_rpc --params '{"extension":"bash","method":"clean"}'` |
> | 1.6 | `follow_up` 通道（后台完成通知） | ✅ 已实现 | 后台进程结束后自动注入 `Message::Custom` |
> | 1.7 | `emit_extension_event`（process_started/completed/killed/error） | ✅ 已实现 | `process_started` 在 spawn 后发出，`process_completed`/`killed`/`error` 在完成后发出 |
> | 1.8 | 文件日志 `/tmp/ion-bash/{pid}.log` | ✅ 已实现 | 后台进程的 stdout+stderr 写入 `{pid}.log` |
> | 1.9 | `extension_rpc inspect` 输出预览 | ✅ 已实现 | `tail=N` 参数控制返回最后 N 行 |
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

> **跨文档注**：§9 旧版用 `agent.follow_up(Message::User(...))` 描述后台完成通知，现已统一对齐到 `send_custom_message` + `Message::Custom` 路径（参 [SESSION_MESSAGE.md](./SESSION_MESSAGE.md)）。后台通知消息以 `role: custom` 入库（不是 `role: user`），UI 可区分渲染。

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
  │   ├── 通过 bash_send 交互（LLM tool + extension RPC 双入口）
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
cat $(ion rpc --session x --method extension_rpc --params '{"extension":"bash","method":"inspect","args":{"pid":12345}}' | grep log_path | cut -d'"' -f4)
# 或通过 inspect 看摘要
ion rpc --session x --method extension_rpc \
  --params '{"extension":"bash","method":"inspect","args":{"pid":12345}}'
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
  --params '{"extension":"bash","method":"inspect","args":{"pid":12345,"tail":50}}'

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

## 7. Extension RPC（CLI 管理）

Extension RPC 返回结构化 JSON（给 CLI 解析用）。

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

### 事件数据字段

| customType | 触发 | data 字段 | 推送方式 |
|-----------|------|-----------|---------|
| `process_started` | 任何方式启动 | `{pid, command, background, session}` | subscribe + system prompt |
| `process_completed` | 进程退出 | `{pid, exit_code, reason, output?}` | subscribe + follow_up（异步） |
| `process_background` | 前台切后台 | `{pid, reason, partial_output}` | subscribe |
| `process_output` | 进程实时输出 | `{pid, data, stream:`stdout`|`stderr`}` | subscribe（按 PID 过滤） |
| `process_killed` | 被 kill | `{pid, reason, by}` | subscribe + follow_up |
| `process_error` | 启动失败 | `{pid?, error}` | subscribe |

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
  --params '{"extension":"bash","method":"inspect","args":{"pid":12345,"tail":50}}'
```

## 9. 扩展通知（Extension Notification）

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

**通知文案模板（按 reason 区分）：**

| reason | 文案 |
|--------|------|
| `completed` | `[bash:completed] Process #N (desc) finished, exit=0` |
| `abnormal` | `[bash:abnormal] Process #N (desc) exited with code=N` |
| `timeout` | `[bash:timeout] Process #N (desc) timed out after Ns` |
| `manual` | `[bash:killed] Process #N (desc) terminated by user` |
| `service_shutdown` | `[bash:shutdown] Process #N (desc) terminated (manager shutdown)` |
| `background_timeout` | `[bash:background] Process #N (desc) timeout, moved to background` |
| `background_manual` | `[bash:background] Process #N (desc) moved to background` |

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
  --params '{"tool":"bash_run","args":{"command":"ping -c 5 127.0.0.1","background":true}}'
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

**入口 2：用户/CLI 通过 Extension RPC 交互**

```bash
ion rpc --session x --method extension_rpc \
  --params '{"extension":"bash","method":"send","args":{"pid":23456,"input":"N\n"}}'
# → {"pid":23456,"sent":"N\n","status":"delivered"}
```

### 场景 F：超时关闭

```bash
ion rpc --session x --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 600","background":true,"timeout":30}}'
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

**入口 2：用户/CLI 通过 Extension RPC**

```bash
ion rpc --session x --method extension_rpc \
  --params '{"extension":"bash","method":"kill","args":{"pid":23456}}'
# → {"pid":23456,"status":"killed","reason":"manual"}
```

### 场景 H：查看进程详情 + 日志分页

```bash
# 列进程
ion rpc --session x --method extension_rpc \
  --params '{"extension":"bash","method":"list","args":{"status":"all"}}'
# → [{pid:12345, command:"sleep 600", status:"running", elapsed:30}]

# 查看详情（含输出摘要）
ion rpc --session x --method extension_rpc \
  --params '{"extension":"bash","method":"inspect","args":{"pid":12345}}'
# → {pid, command, status, output_preview, log_path, elapsed}

# 直接读完整日志文件
cat ~/.ion/agent/project-data/--hash--name--/bash/logs/12345.stdout.log
```

### 场景 I：清理已完成进程

```bash
ion rpc --session x --method extension_rpc \
  --params '{"extension":"bash","method":"clean","args":{}}'
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
| Extension RPC | 5 | `list`, `kill`, `send`, `inspect`, `clean` |
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

---

## 13. CLI 测试指南（API 核查清单）

> **来源：** 整合自 `BASH_API_CHECKLIST.md`，覆盖 18 项 API 验证，全部实测通过。
>
> **注意：** 所有 API 用 `bid`（bash ID，6 位十六进制哈希，如 `"a3f1c2"`）标识进程，不是 OS PID。部分接口示例中仍以 `pid` 字段名传 BID 值（数值形式），实际语义为 BID。

### 13.1 Group A：直接执行（不走 agent loop）

#### A1 `bash_command` RPC

**用途：** CLI/扩展直接执行 bash，结果入对话历史（`role: bashExecution`）。

```bash
ion rpc --session <sid> --method bash_command \
  --params '{"command":"echo hello","timeout":5,"excludeFromContext":false}'
```

**响应：**
```json
{"status":"ok", "exitCode":0, "output":"hello\n", "truncated":false}
```

**✅ 实测通过**

**审计日志：** 结果写入 `session.jsonl` 的 `Message::BashExecution` entry。LLM 下次看到时 provider 自动转成 user text。

#### A2 `!cmd` 前缀拦截（prompt 直发）

**用途：** 用户在输入框直接打 `!ls`，不走 agent loop，实时返回 stdout。

```bash
ion rpc --session <sid> --method prompt \
  --params '{"text":"!echo from-bang"}'
```

**响应：**
```json
{"status":"bash_executed", "command":"echo from-bang", "exitCode":0, "output":"from-bang\n", "truncated":false}
```

**✅ 实测通过**

**审计日志：** 同步写入 `session.jsonl` 的 `Message::BashExecution` entry。同时推送 `agent_start`/`text_delta`/`agent_end` 事件。

### 13.2 Group B：LLM 工具直调（via `call_tool` RPC）

绕过 LLM 直接调工具。

#### B1 `bash_run` 前台同步

**用途：** 前台执行短命令，阻塞等待结果。

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"echo sync-test","description":"sync test"}}'
```

**响应：** `"sync-test\n"`
**✅ 实测通过**

#### B2 `bash_run` 后台异步

**用途：** 后台执行长命令，立即返回 PID，完成后 follow_up 通知。

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 60","description":"bg test","background":true}}'
```

**响应：** `"✅ Process #10000 started in background: bg test"`
**✅ 实测通过**

**审计日志：**
- emit `process_started` 事件
- 写入 `/tmp/ion-bash/{pid}.log`
- 完成后 emit `process_completed` 事件 + follow_up 通道注入 `<bash_result>` XML
- 状态保存到 `processes.json`

#### B3 `bash_run` `timeoutBackground`

**用途：** 前台命令超时后自动转后台，不死。

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"sleep 30","description":"timeout test","timeout":2,"timeoutBackground":true}}'
```

**预期：** `"⏱️ Process #{pid} moved to background..."`
**✅ 已修复** — 超时后返回 `"⏱️ Process moved to background..."`

#### B4 `bash_send` 发 stdin

**用途：** 给运行中的后台进程发 stdin（适用于交互式进程如 `cat`、`nc`）。

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_send","args":{"pid":10000,"input":"hello"}}'
```

**响应：** `"✅ Sent to process #10000: hello"`
**✅ 实测通过**

#### B5 `bash_kill` 杀进程（前台 + 后台）

**用途：** 通过 hex PID（8 位十六进制字符串，如 `"0000000a"`）杀前台或后台进程。

```bash
# 杀后台进程
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_kill","args":{"pid":10000}}'

# 杀前台进程（前台正在执行时调用）
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_kill","args":{"pid":10000}}'
```

**响应（后台）：** `"✅ Process #00000001 killed"`
**响应（前台）：** `success: True`，前台 execute() 立即返回，进程被终止
**LLM 通知：** 自动注入 `<bash_result>🛑 Process #00000001 was killed by user.</bash_result>` 到对话历史
**✅ 实测通过（前台 + 后台）**

**审计日志：** 更新进程状态为 `killed` + 持久化 + 清除 stdin 通道。

#### B6 `bash_background` 前台转后台

**用途：** 前台执行中的命令转到后台继续跑。

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"bash_background","args":{"pid":10000}}'
```

**预期：** `"✅ Process #{pid} moved to background"`
**✅ 已修复** — 前台通过 `notify_map` oneshot 可中断，`success: True`

#### B7 不存在的工具

```bash
ion rpc --session <sid> --method call_tool \
  --params '{"tool":"nonexistent","args":{}}'
```

**预期：** 报错
**✅ 实测通过** → `"tool not found: nonexistent"`

### 13.3 Group C：Extension RPC（bash 管理）

#### C1 `list` 列进程

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"bash","method":"list"}'
```

**响应：**
```json
{"count":2, "processes":[
  {"pid":10000, "os_pid":18008, "command":"sleep 60", "description":"bg test", "status":"completed", "background":true, "elapsed_secs":2},
  {"pid":10001, "os_pid":18028, "command":"sleep 30", "status":"killed"}
]}
```
**✅ 实测通过**

#### C2 `inspect` 查详情

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"bash","method":"inspect","args":{"pid":10001,"tail":50}}'
```

**响应：** `{pid, os_pid, command, status, exit_code, elapsed_secs, output_preview, ...}`
**✅ 实测通过**

#### C3 `kill`（RPC 版）

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"bash","method":"kill","args":{"pid":10001}}'
```

**响应：** `{"status":"killed"}`
**✅ 实测通过**

#### C4 `send`（RPC 版发 stdin）

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"bash","method":"send","args":{"pid":10002,"input":"Y"}}'
```

**响应：** `{"status":"delivered","pid":10002,"input":"Y"}`
**✅ 实测通过**

#### C5 `clean` 清理已结束进程

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"bash","method":"clean"}'
```

**响应：** `{"cleaned":2}`
**✅ 实测通过**

#### C6 不存在的 method

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"bash","method":"nonexistent"}'
```

**预期：** 报错
**✅ 实测通过** → `"bash extension_rpc: unknown method nonexistent"`

#### C7 不存在的 extension

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"nonexistent","method":"list"}'
```

**预期：** 报错
**✅ 实测通过** → `"extension 'nonexistent' not found"`

### 13.4 Group D：持久化文件

#### D1 日志文件

后台进程的 stdout+stderr 写入 `/tmp/ion-bash/{pid}.log`：

```bash
cat /tmp/ion-bash/10000.log
```

**✅ 实测通过** — 文件存在

#### D2 `processes.json`

进程状态持久化到 `~/.ion/tmp/ion-bash/processes.json`，重启后恢复：

```json
{"10001": {"pid":10001, "os_pid":18028, "status":"killed", "command":"sleep 30"}}
```

**✅ 实测通过**

### 13.5 Group E：事件推送（Web UI 实时监听）

通过 `ion subscribe` 实时收到 bash 事件：

```bash
ion subscribe --session <sid> --extension bash
```

收到的事件类型：

| customType | 触发时机 | 包含字段 |
|-----------|---------|---------|
| `process_started` | bash_run 后台/前台启动后 | `{pid, command, description, background, session}` |
| `process_output` | 进程运行时每 ~1s 或缓冲区满（实时 stdout 片段） | `{pid, output, lines}` |
| `process_completed` | 进程正常退出 | `{pid, exit_code, elapsed_secs, log_path, reason}` |
| `process_killed` | 超时/手动杀 | `{pid, command, elapsed_secs, log_path, reason}` |
| `process_error` | spawn 失败 | `{pid, command, error}` |

订阅后立即收到 `process_started`，然后 `process_output` 持续推送直到 `process_completed`/`process_killed`。

```bash
ion subscribe --session <sid>
```

### 13.6 汇总

| 组 | 方法 | 状态 |
|----|------|------|
| A1 | `bash_command` RPC | ✅ |
| A2 | `!cmd` 拦截 | ✅ |
| B1 | `bash_run` 前台同步 | ✅ |
| B2 | `bash_run` 后台异步 | ✅ |
| B3 | `bash_run` timeoutBackground | ✅ |
| B4 | `bash_send` stdin | ✅ |
| B5 | `bash_kill` 杀进程（前台 + 后台） | ✅ |
| B6 | `bash_background` 前台转后台 | ✅ 已修复 |
| B7 | call_tool 错误处理 | ✅ |
| C1 | `list` 列进程 | ✅ |
| C2 | `inspect` 查详情 | ✅ |
| C3 | `kill` RPC 版 | ✅ |
| C4 | `send` RPC 版 | ✅ |
| C5 | `clean` 清理 | ✅ |
| C6-C7 | 错误处理 | ✅ |
| D1 | `/tmp/ion-bash/{pid}.log` | ✅ |
| D2 | `processes.json` | ✅ |
| E | 事件推送 | 框架就位，待订阅端验证 |

**18 项测试，18 项通过 ✅**

---

## 附录 A：综合教程

> **来源：** 整合自 `BASH_EXTENSION_TUTORIAL.md`，覆盖系统概述、构建启动、Worker 管理、会话基础、消息类型、实时流式、三个核心场景、Web UI、验证脚本。
>
> **去重说明：** 教程原 §6 "Bash 扩展" 与正文 §0-§12 内容重叠（bash_command RPC、!cmd 直发、消息入库结构、extension_rpc 入口），已合并到正文，本附录不再重复。如需查阅 Bash 扩展本身的接口规格，请参 §0（已实现部分）与 §1-§12（设计稿）。

### A.1 系统概述

ION 是一个 Rust 实现的 AI Agent 编排平台。核心架构：

```
ion "hello"           — 单实例 CLI
ion manager start     — Manager 守护进程 (管理多个 Worker)
ion-worker --mode rpc — Worker 子进程 (JSONL over stdin/stdout)
```

**通信协议**：JSONL over stdin/stdout（对齐 pi）

### A.2 构建与启动

```bash
# Build
cargo build --bin ion --bin ion-worker

# Copy to PATH（如果使用 ~/.cargo/bin/）
cp target/debug/ion ~/.cargo/bin/
cp target/debug/ion-worker ~/.cargo/bin/

# 启动 Manager 守护进程
ion manager start &
```

Manager 启动后，会在 Unix socket `~/.ion/manager.sock` 上监听。

### A.3 Worker 管理

Worker = 一个 LLM Agent 进程。Manager 管理多个 Worker 的完整生命周期。

#### 创建 Worker

```bash
# 创建 worker，cwd 决定会话文件存放位置
ion rpc --session x --method create_worker --params '{"cwd":"/tmp"}'

# 响应中包含 sessionId（UUID）和 workerId
# {"data":{"sessionId":"xxxx-xxxx-xxxx","workerId":"wkr_xxxxxxxx"}}

# 使用 SID 变量方便后续
SID="xxxx-xxxx-xxxx"
```

#### 列表

```bash
ion rpc --method list_workers
```

#### 杀死 Worker

```bash
ion rpc --session <SID> --method kill
```

#### 重建 Worker（同 SID 恢复）

```bash
# worker 死后重建，保留相同 sessionId
ion rpc --session <SID> --method create_worker \
  --params '{"cwd":"/tmp","session":"<SID>"}'
```

注意：必须在 params 中显式传 `session` 字段，Manager 才知道要用旧 SID。

### A.4 会话基础

#### 发送消息

```bash
# 发送 prompt（触发 LLM 对话）
ion rpc --session <SID> --method prompt --params '{"text":"你好"}'

# 无等待 fire-and-forget（事件通过 subscribe 接收）
```

#### 获取历史消息

```bash
ion rpc --session <SID> --method get_messages
```

#### 发送自定义消息

```bash
ion rpc --session <SID> --method append_custom_message \
  --params '{"custom_type":"debug","text":"自定义消息","display":false}'
```

#### 直接工具调用（绕开 LLM）

```bash
ion rpc --session <SID> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"ls","description":"测试","background":false}}'
```

### A.5 消息类型

Message enum 共 7 个变体（从 3 个扩展而来）：

| 变体 | 用途 | Provider 转换 |
|------|------|---------------|
| `User` | 用户消息 | `role: "user"` |
| `Assistant` | LLM 回复 | `role: "assistant"` |
| `ToolResult` | 工具调用结果 | `role: "tool"` |
| **`BashExecution`** | bash 执行记录 | `role: "user"`，格式化为代码块 |
| **`Custom`** | 自定义消息 | `role: "user"`，提取文本 |
| **`BranchSummary`** | 分支摘要 | `role: "user"` |
| **`CompactionSummary`** | 上下文压缩摘要 | `role: "user"` |

> **BashExecution 对象结构：** 见正文 §0.4「bash 执行消息入库结构」，此处不再重复列出 JSON 示例，避免与正文产生不一致。

### A.6 实时流式

#### Subscribe 事件流

```bash
ion subscribe --session <SID>
```

输出格式：

```
# text_delta（增量）
{"type":"event","event":{"type":"text_delta","delta":"你好"}}

# agent 周期
{"type":"event","event":{"type":"agent_start","model":"xxx"}}
{"type":"event","event":{"type":"agent_end","finishReason":"stop"}}

# 工具执行
{"type":"event","event":{"type":"tool_execution_start","toolName":"bash_run","toolCallId":"call_xxx"}}
{"type":"event","event":{"type":"tool_execution_update","toolCallId":"call_xxx","partialResult":"..."}}
{"type":"event","event":{"type":"tool_execution_end","toolCallId":"call_xxx","isError":false}}

# Bash 进程事件
{"type":"event","event":{"type":"extension_event","extension":"bash","customType":"process_started","data":{"bid":"100000","command":"ls"}}}
{"type":"event","event":{"type":"extension_event","extension":"bash","customType":"process_output","data":{"bid":"100000","output":"file1.txt"}}}
{"type":"event","event":{"type":"extension_event","extension":"bash","customType":"process_completed","data":{"bid":"100000","exit_code":0,"elapsed_secs":0.3}}}
{"type":"event","event":{"type":"extension_event","extension":"bash","customType":"process_killed","data":{"bid":"100000"}}}
```

### A.7 三个核心场景

#### 场景 1：实时流式

```bash
# 终端 1
ion subscribe --session <SID>

# 终端 2
ion rpc --session <SID> --method prompt --params '{"text":"你好"}'

# 终端 1 会看到 text_delta 增量推送
```

#### 场景 2：刷新恢复

```bash
# 获取历史消息
ion rpc --session <SID> --method get_messages

# 执行 bash
ion rpc --session <SID> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"echo hello","description":"test","background":true}}'

# 再次获取历史（消息数增加）
ion rpc --session <SID> --method get_messages
```

#### 场景 3：重启恢复

```bash
# 1. 创建 worker
SID=$(ion rpc --session x --method create_worker --params '{"cwd":"/tmp"}' | ... 提取 sessionId)

# 2. 发送消息
ion rpc --session $SID --method prompt --params '{"text":"记住我叫 Alice"}'

# 3. 获取历史确认
ion rpc --session $SID --method get_messages

# 4. 杀死 worker
ion rpc --session $SID --method kill

# 5. 等待清理
sleep 3

# 6. 重建 worker（显式传 session）
ion rpc --session $SID --method create_worker \
  --params '{"cwd":"/tmp","session":"'$SID'"}'

# 7. 恢复历史
ion rpc --session $SID --method get_messages
# → 返回应有之前的所有消息
```

### A.8 Web UI

ION 附带一个简易 Web UI，用于可视化调试。

#### 启动

```bash
python3 /tmp/chat_ui.py
# → http://localhost:8888
```

#### 功能

- **页面加载**：自动创建 Worker，URL 中带 `?sid=<SID>` 持久化
- **消息发送**：输入文本 → POST 到 /prompt → SSE 流式接收 text_delta
- **历史恢复**：刷新页面 → GET /history → 加载所有历史消息
- **进程列表**：右侧面板显示 BID/status/command，带 Kill/Stdin/Log 按钮
- **工具调用**：POST /rpc → 直接工具调用（绕开 LLM）

#### API 端点

| 路径 | 方法 | 参数 | 说明 |
|------|------|------|------|
| `/` | GET | `sid` (可选) | 首页，无 sid 则自动创建 worker 并 302 跳转 |
| `/stream` | GET | `sid` | SSE 事件流（text_delta、tool 事件、进程事件） |
| `/history` | GET | `sid` | 获取历史消息 |
| `/procs` | GET | `sid` | 获取进程列表 |
| `/prompt` | POST | `{"text":"..."}` | 发送 prompt（fire-and-forget） |
| `/rpc` | POST | `{"method":"...","params":{...}}` | 直接工具调用 |

### A.9 验证脚本

自动化验证脚本 `/tmp/verify_all.py` 覆盖 4 个场景共 15 项检查：

```bash
# 确保 Manager 在运行
ion manager start &

# 运行验证
cd /tmp && python3 verify_all.py
```

检查项：
1. text_delta ≥ 1 条
2. agent_start 1 条
3. agent_end 1 条
4. delta 是增量片段
5. delta 内容非空
6. 刷新后消息数 ≥ 刷新前
7. 刷新后包含 User 消息
8. 刷新后包含 Assistant 消息
9. 刷新后包含 BashExecution
10. 二次刷新一致
11. session.jsonl 已写入
12. Worker 确实死了
13. 重启后消息恢复
14. 重启后包含历史消息
15. 进程列表可查

### A.10 涉及源码文件

| 文件 | 行数 | 功能 |
|------|------|------|
| `src/agent/bash.rs` | 644 | Bash 扩展完整实现 |
| `src/worker_registry.rs` | 1595 | Worker 管理 + 死锁修复（oneshot 模式） |
| `src/bin/ion_worker.rs` | 1644 | StreamingExtension + 11 个 append_* RPC |
| `src/bin/ion.rs` | 2600 | Manager socket handler + create_worker 注入 |
| `ion-provider/src/types.rs` | — | Message enum 7 变体 |
| `ion-provider/src/provider/openai.rs` | — | 新变体的 LLM 转换 |
| `src/session_jsonl.rs` | — | 4 个新 Entry 类型 |

### A.11 常用命令速查

```bash
# 构建
cargo build --bin ion --bin ion-worker

# 测试
cargo test --lib              # 91 个核心测试
cargo test                    # 全部测试

# Manager
ion manager start &           # 启动

# Worker
SID=$(ion rpc --session x --method create_worker --params '{"cwd":"/tmp"}' | ...)

# Prompt
ion rpc --session $SID --method prompt --params '{"text":"你好"}'

# Subscribe
ion subscribe --session $SID

# Bash
ion rpc --session $SID --method call_tool --params '{"tool":"bash_run","args":{"command":"ls","background":true}}'
ion rpc --session $SID --method extension_rpc --params '{"extension":"bash","method":"list"}'

# Session
ion rpc --session $SID --method get_messages
ion rpc --session $SID --method kill

# Recreate
ion rpc --session $SID --method create_worker --params '{"cwd":"/tmp","session":"'$SID'"}'

# Validation
python3 /tmp/verify_all.py

# UI
python3 /tmp/chat_ui.py
```
