# Bash 工具扩展设计（v0.4.0）

> 对标 pi `bash-ext`。4 个 LLM 工具 + 5 个 Extension RPC + DeliverAs 三态 + graceful drain。

## §0 同步执行（已实现）

三种入口（不走 agent loop）：

| 入口 | 调用方式 | 入库类型 |
|------|---------|---------|
| `!cmd` 直发 | `prompt` RPC，text 以 `!` 开头 | `BashExecution`（role: bashExecution） |
| `bash_command` RPC | `ion rpc --method bash_command` | `BashExecution` |
| `bash` LLM 工具 | LLM 调 `bash` tool | `ToolResult`（tool role） |

### `bash_command` RPC

```bash
ion rpc --session <sid> --method bash_command \
  --params '{"command":"ls -la","timeout":30}'
```

| 字段 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `command` | string | 必填 | shell 命令 |
| `timeout` | number | 30 | 超时秒数 |
| `excludeFromContext` | boolean | null | true 时 LLM 看不到 |

### 输出截断

stdout + stderr 各自截断，单流上限 100,000 字节。

---

## §1 架构总览

```
LLM 工具（4 个）             Extension RPC（5 个，CLI/UI 用）
┌─────────────────────┐     ┌─────────────────────────┐
│ bash                │     │ list    — 列所有进程     │
│ get_background_process │  │ inspect — 查详情（头尾截断）│
│ kill_process        │     │ kill    — 杀进程         │
│ write_stdin         │     │ send    — 发 stdin       │
└────────┬────────────┘     │ clean   — 清理已结束     │
         │                  └────────────┬────────────┘
         ▼                               ▼
    ┌─────────────────────────────────────────┐
    │        BashManageTool（共享引擎）          │
    │   process_map / stdin_map / follow_up_tx │
    └─────────────────────────────────────────┘
```

---

## §2 `bash` 工具

```json
{
  "command": "npm test",
  "description": "run tests",
  "timeout": 30,
  "background": false,
  "timeoutBackground": false,
  "bgTimeout": 0,
  "deliverAs": "followUp"
}
```

| 参数 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `command` | string | 必填 | shell 命令 |
| `description` | string | 必填 | 命令描述 |
| `timeout` | number | 30 | 前台超时秒数 |
| `background` | boolean | false | true=后台立即返回 BID |
| `timeoutBackground` | boolean | false | true=前台超时后自动转后台 |
| `bgTimeout` | number | 0 | 后台超时秒数（0=无限，>0=N 秒后杀进程报 exit=timeout） |
| `deliverAs` | enum | followUp | 完成通知投递时机（steer/followUp/nextTurn） |

### 前台同步返回

```
stdout 文本（阻塞等待）
```

### 后台异步返回

```
✅ Process #100000 started in background: run tests
```

---

## §3 管理工具（3 个，对标 pi）

### `get_background_process`

```json
{"bid": "100000"}     // 传 bid=查单个，不传=列所有
```

返回（单个）：
```json
{
  "bid": "100000",
  "command": "sleep 3; echo done",
  "status": "completed",
  "exit_code": 0,
  "background": true,
  "elapsed_secs": 3,
  "started_at": "2026-08-04T06:00:00.000Z",
  "output_bytes": 5,
  "output_size": "5 B",
  "output_lines": 1,
  "output_head": "done",
  "output_tail": "",
  "output_truncated": false
}
```

### `kill_process`

```json
{"bid": "100000"}
```

### `write_stdin`

```json
{"bid": "100000", "input": "Y"}
```

---

## §4 后台进程生命周期

```
bash(background=true)
  │
  ├─ spawn child + spawn_watcher（tokio task）
  ├─ 立即返回 BID
  │
  ├─ spawn_watcher 读 stdout → emit process_output 事件
  ├─ 进程结束 → 拿 exit_code
  ├─ emit process_completed 事件
  └─ follow_up_tx.send(Message::Custom(bash_result))
         │
         ▼
    agent outer_loop drain follow_up_rx
         │
         ▼
    注入 <bash_result> 到对话历史
         │
         ▼
    触发新 turn（LLM 看到完成通知）
```

### graceful drain（agent.run 返回后）

如果 outer_loop 的 30s 等待期间进程没完成，agent.run 返回后 `graceful_drain_follow_ups` 再等 60s（可配 `ION_GRACEFUL_DRAIN_MS`），把残留 bash_result 写入 session.jsonl。

---

## §5 DeliverAs 三态

| 值 | 行为 | 适用场景 |
|---|---|---|
| `steer` | 立即中断当前 LLM turn | kill 通知、紧急结果 |
| `followUp` | 当前 turn 结束后下个 turn | 后台完成通知（默认） |
| `nextTurn` | agent.run 完成后才触发 | 低优先级通知 |

bash 后台完成默认 `followUp`。kill 通知用 `steer`。

---

## §6 bash_result 格式

```xml
<bash_result bid="100000" exit="0" elapsed="3s">
done-marker
</bash_result>
```

| 属性 | 说明 |
|---|---|
| `bid` | 进程 ID |
| `exit` | 退出码（0=成功，1=失败，timeout=超时，unknown=未拿到） |
| `elapsed` | 运行秒数 |

### 输出截断（头尾保留）

输出 > 500 字节时：
```
前 300 字节
...[truncated N bytes]...
后 200 字节
```

---

## §7 Extension RPC

```bash
ion rpc --session <sid> --method extension_rpc \
  --params '{"extension":"bash","method":"inspect","args":{"bid":"100000"}}'
```

| 方法 | 参数 | 说明 |
|------|------|------|
| `list` | `{status?}` | 列所有进程 |
| `inspect` | `{bid, head?, tailLines?, tail?, offset?, limit?}` | 查详情（头尾截断） |
| `kill` | `{bid}` | 杀进程 |
| `send` | `{bid, input}` | 发 stdin |
| `clean` | `{}` | 清理已结束进程 |

---

## §8 inspect 返回字段

| 字段 | 类型 | 说明 |
|---|---|---|
| `bid` | string | 进程 ID |
| `command` | string | 执行的命令 |
| `status` | string | running/completed/killed/error |
| `exit_code` | number/null | 退出码 |
| `elapsed_secs` | number | 运行秒数 |
| `started_at` | string (ISO) | 开始时间 |
| `output_bytes` | number | 输出字节数 |
| `output_size` | string | 友好大小（如 "3.8 KB"） |
| `output_lines` | number | 输出行数 |
| `output_head` | string | 头 N 行 |
| `output_tail` | string | 尾 N 行 + 截断标记 |
| `output_truncated` | boolean | 是否截断 |

---

## §9 drain_follow_ups RPC

```bash
ion rpc --session <sid> --method drain_follow_ups \
  --params '{"wait_ms":1000}'
```

用于 call_tool 路径（绕过 agent loop）手动 drain follow_up 消息并写入 session.jsonl。

---

## §10 CommandGuard

默认 **Blacklist** 模式（默认放行所有命令，只拦高危如 `rm -rf /`、`dd`、`mkfs`）。

白名单包含：npm/cargo/git/ls/cat/grep/echo/seq/wc/sort/awk/sed/curl 等常见命令。

可通过 `ION_SECURITY_PROFILE` 环境变量或 `.ion/config.json` 配置。

---

## §11 接口数量汇总

| 类型 | 数量 | 列表 |
|---|---|---|
| LLM Tools | 4 | `bash`, `get_background_process`, `kill_process`, `write_stdin` |
| Extension RPC | 5 | `list`, `inspect`, `kill`, `send`, `clean` |
| 特殊 RPC | 1 | `drain_follow_ups` |
| DeliverAs | 3 | `steer`, `followUp`, `nextTurn` |
| Events | 5 | `process_started`, `process_output`, `process_completed`, `process_killed`, `process_error` |

---

## §12 文件存储

```
{system_tmp_dir}/ion-bash/
├── {bid}.log          ← stdout + stderr
└── processes.json     ← 进程状态持久化
```

路径通过 `paths::system_tmp_dir()` 获取，支持 `ION_TMP_DIR` 环境变量覆盖。

---

## §13 CLI 测试指南

### Group A：直接执行

| # | 命令 | 验证 |
|---|---|---|
| A1 | `ion rpc --method bash_command --params '{"command":"echo hi"}'` | exitCode=0 |
| A2 | `ion rpc --method prompt --params '{"text":"!echo hi"}'` | bash_executed |

### Group B：LLM 工具直调（via call_tool）

| # | 命令 | 验证 |
|---|---|---|
| B1 | `call_tool bash {command:"echo sync"}` | 前台输出 |
| B2 | `call_tool bash {command:"sleep 30",background:true}` | 返回 BID |
| B3 | `call_tool bash {command:"sleep 10",timeout:2,timeoutBackground:true}` | 转后台 |
| B4 | `call_tool write_stdin {bid:...,input:"hi"}` | sent |
| B5 | `call_tool kill_process {bid:...}` | killed |
| B6 | `call_tool get_background_process {bid:...}` | inspect 返回 |
| B7 | `call_tool nonexistent` | 报错 |

### Group C：Extension RPC

| # | 命令 | 验证 |
|---|---|---|
| C1 | `extension_rpc bash list` | 返回进程列表 |
| C2 | `extension_rpc bash inspect {bid:...}` | 返回详情 |
| C3 | `extension_rpc bash kill {bid:...}` | killed |
| C4 | `extension_rpc bash send {bid:...,input:"hi"}` | delivered |
| C5 | `extension_rpc bash clean` | cleaned=N |
| C6 | `extension_rpc bash nonexistent` | 报错 |
| C7 | `extension_rpc nonexistent list` | 报错 |

### Group D：持久化

| # | 验证 |
|---|---|
| D1 | `/tmp/ion-bash/{bid}.log` 存在 |
| D2 | `processes.json` 存在 |

### Group E：事件推送

| # | 验证 |
|---|---|
| E | `ion subscribe --extension bash` 收到 process_started/completed |
