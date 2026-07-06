# Extension 生态系统 — P4 验证文档

> **状态：已验证** — 2026-07-07 全部通过

## 概述

P4 验证覆盖 Extension 生态的 3 个核心能力：

| # | 能力 | 状态 | 测试方式 |
|---|------|------|---------|
| 1 | Extension → `create_worker` → 子 Worker → 回传结果 | ✅ | `tests/p4_extension_ci.sh` (9 项) |
| 2 | `emit_extension_event` → EventBus → Subscriber | ✅ | `tests/p4_events_ci.sh` (7 项) |
| 3 | 扩展 emit 自定义事件 + 外部调用扩展 custom method | ✅ | 集成在 lib tests 中 |

## 验证链路 1: Extension → create_worker

```
CLI → ion rpc --method call_tool spawn_worker
  → Manager → worker stdin
  → agent.call_tool("spawn_worker", args)
  → SpawnWorkerTool.execute()
  → WorkerRuntime::spawn_worker()
  → ManagerBridge::send_command("create_worker")   [JSON → stdout]
  → Manager process_pending_commands()
  → WorkerRegistry::create_worker()                 [spawn child]
  → write_manager_response()                         [JSON → parent stdin]
  → manager_bridge.deliver_response()                [读取任务中拦截 _reply_to]
  → oneshot 完成 → 工具返回 → CLI 获得 child worker_id
```

### 修复的生产级 Bug

| Bug | 症状 | 修复位置 |
|-----|------|---------|
| `write_manager_response` 用 session_id 查 worker | `[manager] cannot write response: worker p4-parent not found` | `worker_registry.rs:1245` — 先查 worker_id，再查 session_id |
| stdin 主循环死锁 | `spawn_worker` 调用卡死，response 在 channel 缓冲中无法处理 | `ion_worker.rs:344` — 在 stdin 读取任务中提前拦截 `_reply_to` 消息直接投递 |

## 验证链路 2: emit_extension_event

```
bash_run background process
  → BashExtension::emit_extension_event("process_started")
  → println! JSON → worker stdout
  → Manager stdout reader → "extension_event" 检测
  → ExtensionEventBus::broadcast()
  → Subscribers 收到事件
```

### 事件类型（Bash Extension）

| 事件 | 触发时机 | 数据 |
|------|---------|------|
| `process_started` | 后台进程启动 | `{pid, command, description}` |
| `process_completed` | 进程正常退出 | `{pid, exit_code, output}` |
| `process_output` | 进程输出 | `{pid, lines[]}` |
| `process_error` | 进程异常退出 | `{pid, error}` |

## 测试结果

### P4 Extension 子 Worker 测试 (9/9 ✅)

```
  ✅ build ion + ion-worker
  ✅ manager started
  ✅ create_worker p4-parent (SID=p4-parent, WID=wkr_9e57a382)
  ✅ parent worker is alive (bash echo)
  ✅ spawn_worker created child (worker_id=wkr_c711e4ee)
  ✅ child created (session_id not in response, worker_id=wkr_c711e4ee)
  ✅ parent → child send_to_worker (non-critical)
  ✅ kill child worker (wkr_c711e4ee)
  ✅ kill parent worker (wkr_9e57a382)
  ✅ manager stopped
```

### P4 Extension 事件发射测试 (7/7 ✅)

```
  ✅ build ion + ion-worker
  ✅ manager started
  ✅ create_worker (SID=p4-events)
  ✅ bash_run background process started
  ✅ subscription channel works
  ✅ extension_event detected in manager log (customType=process_started)
  ✅ cleanup
```

## CLI 验证命令速查

```bash
# 启动 Manager
ion manager start

# 创建 Worker
ion rpc --method create_worker --params '{"session":"test"}'

# 子 Worker 创建（通过 spawn_worker 工具，不经过 LLM）
ion rpc --session test --method call_tool \
  --params '{"tool":"spawn_worker","args":{"task":"echo hello","relation":"child","wait":false}}'

# 订阅扩展事件
ion subscribe --extension bash

# 触发扩展事件（后台进程）
ion rpc --session test --method call_tool \
  --params '{"tool":"bash_run","args":{"command":"echo hi","background":true}}'
```

## 核心代码文件

| 文件 | 角色 |
|------|------|
| `src/worker_api.rs` | `ExtensionApi`, `BridgeHandle` trait, `WorkerHandle` |
| `src/bin/ion_worker.rs` | `ManagerBridge` (impl `BridgeHandle`), stdin 主循环, `_reply_to` 拦截 |
| `src/worker_registry.rs` | `process_pending_commands`, `write_manager_response`, 子 Worker 创建 |
| `src/event_bus.rs` | `ExtensionEventBus`, `ExtensionEvent` |
| `tests/p4_extension_ci.sh` | 子 Worker 创建 CLI 验证 |
| `tests/p4_events_ci.sh` | 事件发射 CLI 验证 |
