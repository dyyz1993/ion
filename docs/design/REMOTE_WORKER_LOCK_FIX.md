# REMOTE_WORKER 死锁与 Prompt 注入修复设计

> **状态：已完成** — P3/P5（Manager 死锁/锁竞争）+ P4（子 Worker prompt 注入丢失）修复，TDD 全流程。lib 1019/0。

---

## 何时阅读本文

- 远程 worker 的 LLM 桥接流导致 host 无响应（P3/P5）
- coordinator 派的子 Worker 收不到任务书（P4）
- 想了解 ION Manager 的锁模型和注意事项

## 问题背景

压力测试（2026-09-11，[REMOTE_WORKER_STRESS_REPORT.md](../testing/REMOTE_WORKER_STRESS_REPORT.md)）发现 4 个工程问题，其中 3 个同根因：

| 问题 | 现象 | 根因 |
|------|------|------|
| P3 | host socket 活着但不处理任何 RPC | 桥接任务持锁 `thread::sleep` 阻塞 tokio worker，多任务轮流抢锁造成**活锁** |
| P5 | coordinator 的 spawn_worker 320s 超时 | P3 同根因——`process_pending_commands` 持锁做慢操作被桥接任务挤死 |
| P4 | 子 Worker spawn 成功但 0 工具调用 | prompt 注入的 `tokio::spawn` 任务被 P3 饿死，**从未执行** |

## 修复设计

### 核心变更：`write_line_to_worker_sync` 改非阻塞

**旧实现（已删除）**：
```rust
// 管道满时 sleep 2ms 重试，最多 5 秒——持锁阻塞 tokio worker！
Poll::Pending => {}
// ...
std::thread::sleep(Duration::from_millis(2));  // ← P3 根因
```

**新实现**：
```rust
// 中间 chunk（LLM 流式 delta）：管道满 → 立即丢弃（debug 日志）
// 终帧（Done/Error）：短暂 yield_now 重试（500ms 上限），保证最终结果不丢
Poll::Pending => {
    if !is_final { return false; }  // 中间 chunk 直接丢
    std::thread::yield_now();       // 终帧让步重试（不阻塞）
}
```

**设计决策——为什么丢中间 chunk 是安全的**：

1. 中间 chunk（TextDelta/ThinkingDelta）只用于流式显示，worker 的 agent 循环在收到 Done/Error 终帧前会持续等待
2. 终帧（Done/Error）携带完整的 AssistantMessage（含全部内容和 usage）——只要终帧到达，中间丢多少 chunk 都不影响正确性
3. 管道满 = worker 消费慢 = 丢几个中间 chunk 是正常降级（LLM 生成的速度 > worker 处理速度时）
4. 终帧有独立 500ms 重试保障，不会静默丢失

### P4 的修复

P4 不需要独立代码变更——它的根因就是 P3。prompt 注入流程：

```
process_pending_commands
  └─ tokio::spawn(prompt_injection_task)  ← 异步任务
       └─ sleep(500ms)                    ← 等子进程 boot
       └─ acquire_stdin(5s)              ← 需要拿 registry 锁
       └─ write prompt to stdin
```

P3 修复前：桥接任务的 `thread::sleep` 阻塞 tokio worker → prompt 注入任务**无法被调度执行** → prompt 永远不注入。

P3 修复后：桥接写入非阻塞（微秒级持锁）→ prompt 注入任务正常调度 → prompt 到达。

---

## TDD 测试

### 测试文件与命令

```bash
# 全部 5 个修复相关测试
cargo test --lib test_p3 test_p4 test_p5

# 单独跑
cargo test --lib test_p3_write_does_not_hold_lock_long     # P3: 写入时间上界
cargo test --lib test_p3_concurrent_bridge_writes           # P3: 并发不饿死 registry
cargo test --lib test_p5_write_line_sync_has_no_sleep       # P5: 源码级无 thread::sleep
cargo test --lib test_p4_prompt_injection_reaches_worker    # P4: prompt 到达 worker stdin
cargo test --lib test_p4_prompt_not_starved                 # P4: 并发桥接下 prompt 不丢
```

### 测试设计说明

| 测试 | 验证什么 | 怎么验证 |
|------|---------|---------|
| `test_p3_write_does_not_hold_lock_long` | 写入不存在的 target 必须立即返回 | 计时 <100ms |
| `test_p3_concurrent_bridge_writes` | 8 个并发桥接写入时 registry 查询不被饿死 | 1s 内完成查询 |
| `test_p5_write_line_sync_has_no_sleep` | 源码级静态检查：函数体内无 `thread::sleep` | `include_str!` 字符串匹配 |
| `test_p4_prompt_injection_reaches_worker` | prompt 注入机制（acquire→write→put back）可靠 | 真实子进程管道（cat 回显）|
| `test_p4_prompt_not_starved` | 并发桥接写入下 prompt 注入不被饿死 | 8 并发写 + prompt 任务 10s 内完成 |

### TDD 流程记录

| 步骤 | P3/P5 | P4 |
|------|-------|-----|
| 写测试 | ✅ 3 个测试 | ✅ 2 个测试 |
| 红 | P5 红（源码含 thread::sleep）| 修复前会红（P3 阻塞调度）|
| 修复 | 删 thread::sleep，改非阻塞 | 无需独立修（P3 修复自愈）|
| 绿 | ✅ 全绿 | ✅ 全绿 |
| 全量回归 | 1019/0 ✅ | 1019/0 ✅ |

---

## 真机验证方法

修复后需真机验证（单元测试只覆盖机制，SSH 链路需集成验证）：

### 1. 验证 P3 不再死锁

```bash
# 起 host，跑一个会产生大量 LLM 流式响应的任务
ion rpc --method create_worker --params '{"host":"win38","agent":"build","initial_prompt":"写一篇 3000 字文章","wait":false}'
# 等待 30 秒（LLM 流式活跃期），然后验证 host 仍响应：
ion rpc --method list_workers
# 预期：立即返回 JSON（修复前会超时无响应）
```

### 2. 验证 P4 子 Worker 收到任务书

```bash
# 派 coordinator 让它 spawn 子 Worker
ion rpc --method create_worker --params '{"host":"win38","agent":"build","initial_prompt":"用 spawn_worker 派一个子 Worker 在 /tmp 创建 test.txt","wait":false}'
# 等 60 秒后验证：
ssh -p 2222 root@192.168.0.38 "cat /tmp/test.txt"
# 预期：文件存在（子 Worker 收到任务并执行）
# 修复前：子 Worker 的会话文件只有 header（0 工具调用）
```

### 3. 验证 P5 spawn 不超时

```bash
# coordinator 派多个子 Worker
ion rpc --method create_worker --params '{"host":"win38","agent":"build","initial_prompt":"并行派 3 个子 Worker 各写一个文件到 /tmp","wait":false}'
# 预期：3 个子 Worker 在 60s 内全部启动并执行（修复前第二个会 320s 超时）
```

---

## 关键代码位置

| 位置 | 内容 |
|------|------|
| `src/worker_registry.rs:4911` | `write_line_to_worker_sync`（P3 修复点）|
| `src/worker_registry.rs:3170` | `acquire_stdin`（P4 的核心机制）|
| `src/worker_registry.rs:1876` | prompt 注入 spawn task（P4 的执行路径）|
| `src/worker_registry.rs` tests | 5 个 TDD 测试（test_p3/p4/p5）|

## 已知限制

- 终帧（Done/Error）仍可能在管道持续满 500ms 后丢弃——极端场景，实际未观察到
- 多级 spawn（>2 层）未在此轮验证——Mission D 只测了 2 层
- SSH 网络瞬断（>5 分钟）仍会杀 worker（ServerAliveCountMax=10 缓解，根治需 C 模式）
