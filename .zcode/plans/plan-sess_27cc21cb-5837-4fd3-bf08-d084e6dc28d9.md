# Plan: create_worker 锁拆分 — 注册与启动分离

## 问题

`create_worker()` 从 line 187 到 line 700+ 全程持有 `registry.lock()`。其中包含：
1. **worktree 创建**（git init/add/commit）—— 1-3 秒
2. **child_cmd.spawn()**（fork+exec）—— 50-200ms
3. **stderr pipe + stdout reader** —— 微秒级
4. **SessionIndex 写入**（文件 IO）—— 毫秒级
5. **singleton_user_join** —— 可能触发 LLM 调用

这期间所有 RPC 被阻塞，导致 monitor 触发时整个 serve 无响应。

## 改动

把 `create_worker()` 拆成 3 阶段，只有阶段 1 和 3 持锁：

```
阶段 1（持锁，微秒级）：
  - 分配 worker_id + session_id
  - 读取 config（project_path/model/agent 等）
  - 注册 WorkerRecord { status: Spawning } 占位
  - 写入 parent.children + channels
  - 返回 worker_id + 预分配的变量（project_path/model/agent等）

阶段 2（无锁，秒级）：
  - worktree 创建（git init/add/commit）
  - child_cmd 构建 + env 设置
  - child_cmd.spawn()
  - stderr pipe 创建
  - stdout reader task 启动
  - SessionIndex 写入

阶段 3（持锁，微秒级）：
  - 更新 WorkerRecord：child_process / stdin / stdout_rx / stderr_path
  - 更新 status: Idle
  - singleton_user_join
```

## 具体代码改动

### 文件：`src/worker_registry.rs`

1. **新增 `WorkerStatus::Spawning`**（占位状态）

2. **新增 `create_worker_phase1()`**：
   - 分配 worker_id + session_id
   - 读 config 参数（project_path/model/agent 等）
   - 注册占位 WorkerRecord
   - 返回 `(WorkerInfo, SpawnContext)` — SpawnContext 包含阶段 2 需要的所有变量

3. **新增 `create_worker_phase2()`**（无锁）：
   - 接收 SpawnContext
   - worktree 创建
   - child_cmd 构建 + spawn
   - stderr pipe + stdout reader
   - SessionIndex 写入
   - 返回 `SpawnResult`（child_process + stdin 等）

4. **新增 `create_worker_phase3()`**：
   - 接收 SpawnResult
   - 更新 WorkerRecord 字段
   - singleton_user_join

5. **修改 `create_worker()`**：改为调用 phase1 → phase2 → phase3

6. **修改 `process_pending_commands()`**：phase2 在 `tokio::spawn` 中执行，不阻塞命令处理循环

### 影响范围

- `create_worker` 调用方：`process_pending_commands` + `cmd_serve_start` (do_create_session) + `post_init_singletons`
- 所有调用方都通过 `registry.lock().await.create_worker()` 调用，改成 `phase1 → spawn phase2 → phase3` 后，调用方也需要适配
- **monitor_extension.rs** 的 spawn 逻辑也需要适配（它目前直接 lock + create_worker）

### 测试验证

- `cargo test --lib` 全过（931 测试）
- `cargo build --bin ion` 编译通过
- 串行跑 monitor_ci（应该不再有锁竞争 timeout）
- 并行跑 P=3（abort/extension_fs/mcp 应该不再 FAIL）
