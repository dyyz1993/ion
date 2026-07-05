# Bash 插件 + 会话消息系统 — 完整教程

> **状态：已验证** — 所有功能已在真实 LLM 和真实 API 上通过测试

## 目录
1. [系统概述](#1-系统概述)
2. [构建与启动](#2-构建与启动)
3. [Worker 管理](#3-worker-管理)
4. [会话基础](#4-会话基础)
5. [消息类型](#5-消息类型)
6. [Bash 插件](#6-bash-插件)
7. [实时流式](#7-实时流式)
8. [三个核心场景](#8-三个核心场景)
9. [Web UI](#9-web-ui)
10. [验证脚本](#10-验证脚本)

---

## 1. 系统概述

ION 是一个 Rust 实现的 AI Agent 编排平台。核心架构：

```
ion "hello"           — 单实例 CLI
ion manager start     — Manager 守护进程 (管理多个 Worker)
ion-worker --mode rpc — Worker 子进程 (JSONL over stdin/stdout)
```

**通信协议**：JSONL over stdin/stdout（对齐 pi）

## 2. 构建与启动

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

## 3. Worker 管理

Worker = 一个 LLM Agent 进程。Manager 管理多个 Worker 的完整生命周期。

### 创建 Worker

```bash
# 创建 worker，cwd 决定会话文件存放位置
ion rpc --session x --method create_worker --params '{"cwd":"/tmp"}'

# 响应中包含 sessionId（UUID）和 workerId
# {"data":{"sessionId":"xxxx-xxxx-xxxx","workerId":"wkr_xxxxxxxx"}}

# 使用 SID 变量方便后续
SID="xxxx-xxxx-xxxx"
```

### 列表

```bash
ion rpc --method list_workers
```

### 杀死 Worker

```bash
ion rpc --session <SID> --method kill
```

### 重建 Worker（同 SID 恢复）

```bash
# worker 死后重建，保留相同 sessionId
ion rpc --session <SID> --method create_worker \
  --params '{"cwd":"/tmp","session":"<SID>"}'
```

注意：必须在 params 中显式传 `session` 字段，Manager 才知道要用旧 SID。

## 4. 会话基础

### 发送消息

```bash
# 发送 prompt（触发 LLM 对话）
ion rpc --session <SID> --method prompt --params '{"text":"你好"}'

# 无等待 fire-and-forget（事件通过 subscribe 接收）
```

### 获取历史消息

```bash
ion rpc --session <SID> --method get_messages
```

### 发送自定义消息

```bash
ion rpc --session <SID> --method append_custom_message \
  --params '{"custom_type":"debug","text":"自定义消息","display":false}'
```

### 直接工具调用（绕开 LLM）

```bash
ion rpc --session <SID> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"ls","description":"测试","background":false}}'
```

## 5. 消息类型

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

BashExecution 对象结构：

```json
{
  "role": "bashExecution",
  "command": "echo hello",
  "output": "hello\n",
  "exit_code": 0,
  "cancelled": false,
  "truncated": false,
  "full_output_path": null,
  "timestamp": 1712345678,
  "exclude_from_context": true
}
```

## 6. Bash 插件

Bash 插件是内核内置的进程执行引擎，支持前台同步 / 后台异步 / 超时自动切后台三种模式。

### BID（Bash ID）

每个 bash 进程有一个 6 字符 base36 的唯一标识（如 `100000`、`100001`），是字母数字混合。**不暴露 os_pid 给 UI/LLM**。

### LLM 工具

| 工具名 | 功能 | 参数 |
|--------|------|------|
| `bash_run` | 执行命令 | `command`, `description`, `background`, `timeout`, `timeoutBackground` |
| `bash_kill` | 杀死进程 | `pid` (BID) |
| `bash_send` | 发送 stdin | `pid` (BID), `input` |

### Plugin RPC 调试入口

```bash
# 进程列表
ion rpc --session <SID> --method plugin_rpc \
  --params '{"plugin":"bash","method":"list"}'

# 查看进程详情（含 tail 输出）
ion rpc --session <SID> --method plugin_rpc \
  --params '{"plugin":"bash","method":"inspect","args":{"bid":"100000","tail":50}}'

# 杀死进程
ion rpc --session <SID> --method plugin_rpc \
  --params '{"plugin":"bash","method":"kill","args":{"bid":"100000"}}'

# 发送 stdin
ion rpc --session <SID> --method plugin_rpc \
  --params '{"plugin":"bash","method":"send","args":{"bid":"100000","input":"Y"}}'

# 清理已结束进程
ion rpc --session <SID> --method plugin_rpc \
  --params '{"plugin":"bash","method":"clean"}'
```

### 后台进程生命周期

1. 调用 `bash_run background=true` → 立即返回 BID
2. 进程启动 → 发出 `process_started` 事件
3. 进程输出 → 发出 `process_output` 事件（逐行）
4. 进程完成 → 发出 `process_completed` 事件 + `follow_up` 注入 `Message::Custom`
5. 进程被杀 → 发出 `process_killed` 事件

### 进程持久化

进程状态自动保存到 `~/.ion/tmp/ion-bash/processes.json`，worker 重启后恢复。

## 7. 实时流式

### Subscribe 事件流

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
{"type":"event","event":{"type":"plugin_event","plugin":"bash","customType":"process_started","data":{"bid":"100000","command":"ls"}}}
{"type":"event","event":{"type":"plugin_event","plugin":"bash","customType":"process_output","data":{"bid":"100000","output":"file1.txt"}}}
{"type":"event","event":{"type":"plugin_event","plugin":"bash","customType":"process_completed","data":{"bid":"100000","exit_code":0,"elapsed_secs":0.3}}}
{"type":"event","event":{"type":"plugin_event","plugin":"bash","customType":"process_killed","data":{"bid":"100000"}}}
```

## 8. 三个核心场景

### 场景 1：实时流式

```bash
# 终端 1
ion subscribe --session <SID>

# 终端 2
ion rpc --session <SID> --method prompt --params '{"text":"你好"}'

# 终端 1 会看到 text_delta 增量推送
```

### 场景 2：刷新恢复

```bash
# 获取历史消息
ion rpc --session <SID> --method get_messages

# 执行 bash
ion rpc --session <SID> --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"echo hello","description":"test","background":true}}'

# 再次获取历史（消息数增加）
ion rpc --session <SID> --method get_messages
```

### 场景 3：重启恢复

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

## 9. Web UI

ION 附带一个简易 Web UI，用于可视化调试。

### 启动

```bash
python3 /tmp/chat_ui.py
# → http://localhost:8888
```

### 功能

- **页面加载**：自动创建 Worker，URL 中带 `?sid=<SID>` 持久化
- **消息发送**：输入文本 → POST 到 /prompt → SSE 流式接收 text_delta
- **历史恢复**：刷新页面 → GET /history → 加载所有历史消息
- **进程列表**：右侧面板显示 BID/status/command，带 Kill/Stdin/Log 按钮
- **工具调用**：POST /rpc → 直接工具调用（绕开 LLM）

### API 端点

| 路径 | 方法 | 参数 | 说明 |
|------|------|------|------|
| `/` | GET | `sid` (可选) | 首页，无 sid 则自动创建 worker 并 302 跳转 |
| `/stream` | GET | `sid` | SSE 事件流（text_delta、tool 事件、进程事件） |
| `/history` | GET | `sid` | 获取历史消息 |
| `/procs` | GET | `sid` | 获取进程列表 |
| `/prompt` | POST | `{"text":"..."}` | 发送 prompt（fire-and-forget） |
| `/rpc` | POST | `{"method":"...","params":{...}}` | 直接工具调用 |

## 10. 验证脚本

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

---

## 附录：所有涉及源码文件

| 文件 | 行数 | 功能 |
|------|------|------|
| `src/agent/bash.rs` | 644 | Bash 插件完整实现 |
| `src/worker_registry.rs` | 1595 | Worker 管理 + 死锁修复（oneshot 模式） |
| `src/bin/ion_worker.rs` | 1644 | StreamingExtension + 11 个 append_* RPC |
| `src/bin/ion.rs` | 2600 | Manager socket handler + create_worker 注入 |
| `ion-provider/src/types.rs` | — | Message enum 7 变体 |
| `ion-provider/src/provider/openai.rs` | — | 新变体的 LLM 转换 |
| `src/session_jsonl.rs` | — | 4 个新 Entry 类型 |

## 附录：常用命令速查

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
ion rpc --session $SID --method plugin_rpc --params '{"plugin":"bash","method":"list"}'

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
