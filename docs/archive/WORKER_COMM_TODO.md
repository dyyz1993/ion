# Worker 通信补全任务清单

> **状态：开发中** — 本文档为下一轮 sprint 的执行清单。
> 上文已完成去 leader 化的 `cmd_team` 重写 + `spawn_worker`/`send_to_worker` 工具（child 同步阻塞已 E2E 验证）。
> 本轮补全 peer 异步、resume 同步、并行 child、follow_up 机制、结构化返回值。

## 背景：本轮已完成的能力（不要重做）

| 能力 | 实现位置 | 验证状态 |
|------|---------|---------|
| `cmd_team` 启动器（spawn coordinator + pump + idle 判定） | `src/bin/ion.rs::cmd_team` | ✅ E2E（demo2） |
| `spawn_worker(child, agent, task)` 同步阻塞到首轮 agent_end | `worker_registry.rs::process_pending_commands` | ✅ E2E |
| `spawn_worker(peer, agent, task)` 立即返回 + 内核注入汇报段 | `worker_registry.rs::create_worker` | ⚠️ 代码完成未跑 |
| `send_to_worker(target, text)` fire-and-forget | `worker_registry.rs::process_pending_commands` | ⚠️ 代码完成未跑 |
| `_reply_to` correlation（ManagerBridge ↔ Manager） | `ion_worker.rs::ManagerBridge` + `write_manager_response` | ✅ |
| `WorkerRuntime` 包装器（让 LLM 工具够到 Manager） | `runtime.rs::WorkerRuntime` | ✅ |
| `ion_worker` stdin async 化 | `ion_worker.rs` tokio::spawn 读 task | ✅ |
| GLM-4.7/4.6 builtin（max_tokens: 32000） | `ion-provider/src/registry.rs` | ✅ |

## 本轮缺口（已和用户确认全包）

### 🔴 P0 - 必做

#### ~~缺口 H — Agent 加 `follow_up_queue`~~（误报，已存在）

**已存在，不要重做：**
- `Agent.follow_up_queue: VecDeque<Message>` —— `agent_loop.rs:72`
- `Agent::follow_up(msg)` —— `agent_loop.rs:178`
- `Agent::follow_up_queue_len()` —— `agent_loop.rs:190`
- `run()` outer loop 里 drain follow_up_queue 继续跑 —— `agent_loop.rs:265-274`
- ion_worker 的 `follow_up` RPC 分支已接好 —— `ion_worker.rs:666-677`

这条链路本来就是通的，对齐 pi 的 `agent.ts:197,327,443`。

#### 缺口 I — peer → creator 用 follow_up 推结果（核心异步响应机制）

**位置：** `src/worker_registry.rs::process_pending_commands`

当前 peer 模式立即返回 worker_id 后就不管了。要改成：
1. peer 创建时 subscribe（和 child 一样）
2. **后台 task** 等 peer 的 agent_end（不阻塞 manager_response）
3. peer agent_end 后，调 `send_command(creator, "follow_up", {"text": output})` 推给 creator
4. creator Agent 自动消化 follow_up queue（已有逻辑）→ 继续跑

**关键：不需要新增任何 Agent 字段或方法**，只需要在 Manager 端写一个后台 task 调用现有的 `send_command(target, "follow_up", ...)`。

伪代码：
```rust
"create_worker" => {
    let relation = ...;
    let config = ...;
    match self.create_worker(config).await {
        Ok(info) => {
            let child_id = info.worker_id.clone();
            
            if relation == Child {
                // 同步：subscribe + 阻塞等 agent_end（已实现，保留）
                ...
            } else {
                // Peer：立即返回 worker_id 给 caller（已实现）
                self.write_manager_response(...).await;
                
                // 【新增】后台 task：等 peer agent_end，把结果 follow_up 给 creator
                // 通过 send_command(creator, "follow_up", {text: output})，
                // creator 的 ion_worker follow_up 分支会调 agent.follow_up()
                let creator_id = from_worker.clone();
                tokio::spawn(async move {
                    // subscribe peer，等 agent_end，累积 text_delta
                    // 完成后 send_command(creator_id, "follow_up", {"text": output})
                });
            }
        }
    }
}
```

**难点：** registry 当前是 `&mut self`，不能简单 clone 进 spawn task。需要把 registry 改成 `Arc<Mutex<WorkerRegistry>>`（cmd_team 已经这么用了，但 Manager 入口的 registry 还要同步）。

#### 缺口 F — `resume_worker` 工具（阻塞到下一轮 agent_end）

**位置：** `src/agent/tool.rs`（新工具） + `src/runtime.rs`（trait 方法） + `worker_registry.rs::process_pending_commands`（新分支）

用户原话："A Worker 可以恢复 B 子 Worker 同步的"

```rust
pub struct ResumeWorkerTool;
// 参数: { worker_id: "xxx", text: "继续做 X" }
// 行为: send_to_worker(target, text) + 阻塞等 target 下一轮 agent_end
// 返回结构化 JSON:
// { "type": "worker_resumed", "worker_id": "xxx", "status": "turn_completed",
//   "response_output": "B 的回复" }
```

实现要点：和 spawn_worker(child) 的等待逻辑一样，但 target 是已存在的 Worker。

#### 缺口 G — 并行 child（spawn_worker 立即返回 + await_worker）

**位置：** 修改 `SpawnWorkerTool` + 新增 `AwaitWorkerTool`

当前 spawn_worker(child) 阻塞，LLM 只能串行。改成：
- spawn_worker(child) **立即返回** `{type:"worker_spawned", relation:"child", worker_id, status:"running_in_background"}`
- 新增 `await_worker(worker_id)` 阻塞到目标 agent_end，返回 `{type:"worker_awaited", worker_id, status:"turn_completed", first_turn_output}`

**注意：** 这会改变 child 的默认语义（从同步变异步）。要在 spawn_worker 加一个 `wait` 参数：
- `wait: true`（默认）—— 老语义，阻塞
- `wait: false` —— 立即返回，后续用 await_worker

#### 缺口 J — 所有 worker 工具返回结构化 JSON

**位置：** `src/agent/tool.rs`

用户强调："响应值需要有固定的格式，使得 LLM 知道它是干嘛的"

所有工具的 `execute()` 返回值改成 JSON 字符串（LLM 能稳定解析）：

```json
// spawn_worker
{ "type": "worker_spawned", "relation": "child"|"peer",
  "worker_id": "wkr_xxx",
  "status": "first_turn_completed" | "running_in_background",
  "first_turn_output": "...",  // child wait=true 才有
  "report_channel": "main" }   // peer 才有

// resume_worker / await_worker
{ "type": "worker_resumed"|"worker_awaited", "worker_id": "wkr_xxx",
  "status": "turn_completed", "response_output"|"first_turn_output": "..." }

// send_to_worker
{ "type": "message_sent", "target": "wkr_xxx", "status": "delivered_async" }
```

工具描述里要明确写"返回 JSON 格式"，让 LLM 知道怎么解析。

### 🟡 P1 - 修复（让现有功能跑通）

#### 缺口 A — pump 加回 CHANNEL_SEND 解析

**位置：** `src/bin/ion.rs::cmd_team` pump task

上一版 pump 有 CHANNEL_SEND 文本协议解析 + 路由，重写时被我删了。加回来作为**调试/可视化**用（结构化的 follow_up 是主路径，CHANNEL_SEND 是辅助）。

```rust
// pump 处理 text_delta 时：
if delta.contains("CHANNEL_SEND") {
    let parts: Vec<&str> = delta.splitn(3, ' ').collect();
    if parts.len() >= 3 {
        let channel = parts[1].trim();
        let msg_text = parts[2].trim();
        let mut reg = pump_registry.lock().await;
        reg.channel_send(channel, wid, json!({"text": msg_text})).await;
    }
}
```

#### 缺口 B — channel_msg 进入 Agent 上下文

**位置：** `src/bin/ion_worker.rs:364` `"channel_msg"` 分支

当前只是 `tracing::info!` 就 ack 了。要塞进 `agent.messages`：

```rust
"channel_msg" => {
    let from = ...;
    let msg_text = msg.get("text").and_then(|v| v.as_str()).unwrap_or("");
    // 【新增】把 channel 消息作为新的 user message 注入 agent
    let user_msg = Message::User(UserMessage {
        content: vec![ContentBlock::Text(TextBlock {
            text: format!("[channel {} from {}] {}", channel, from, msg_text),
        })],
    });
    agent.follow_up(user_msg);  // 走 follow_up queue，不抢当前轮次
    output_response(&id, "channel_msg", &Value::Null);
}
```

### 🟢 P2 - 完善（可选）

#### 缺口 C — LLM 可调用的 channel_send 工具

结构化工具，让 LLM 不用靠文本协议广播：
```rust
pub struct ChannelSendTool;
// 参数: { channel: "main", text: "..." }
// 行为: bridge.send_command("channel_send", ...)
```

#### 缺口 D — LLM 可调用的 kill_worker 工具

```rust
pub struct KillWorkerTool;
// 参数: { worker_id: "xxx" }
// 行为: bridge.send_command("kill_worker", ...)
```

## 工具最终清单（本轮完成后）

| 工具 | 同步/异步 | 返回值关键字段 |
|------|---------|--------------|
| `spawn_worker` | wait=true: 同步 / wait=false: 异步 | `worker_spawned` + status |
| `await_worker` | 同步 | `worker_awaited` + first_turn_output |
| `resume_worker` | 同步 | `worker_resumed` + response_output |
| `send_to_worker` | 异步 | `message_sent` + delivered_async |
| `channel_send` (P2) | 异步 | `channel_sent` |
| `kill_worker` (P2) | 异步 | `worker_killed` |

## 验证场景（从简到复杂）

### 场景 1：单 child 同步（回归测试）
```
PRD: 创建 hello.rs 输出 "Hello"
coordinator.md: 调 spawn_worker(child, developer, task, wait=true)
预期: coordinator 拿到 developer 首轮输出，汇总后 agent_end
```
**已跑通**（demo2），本轮作为回归。

### 场景 2：peer 创建 + follow_up 异步响应
```
PRD: 创建一个文件 A.rs，再创建一个文件 B.rs（两个独立任务）
coordinator.md:
  1. spawn_worker(peer, developer, "创建 A.rs", report_channel="main")
  2. spawn_worker(peer, developer, "创建 B.rs", report_channel="main")
  3. 等 follow_up 消息到达，汇总
预期:
  - 两个 peer 并行跑
  - 每个 peer agent_end 后，Manager 把结果 follow_up 给 coordinator
  - coordinator 的 follow_up_queue 触发新一轮，最终汇总
验证点: A.rs 和 B.rs 都被创建；events.jsonl 看到 follow_up 路径
```

### 场景 3：child resume（缺口 F）
```
PRD: 创建 hello.rs
coordinator.md:
  1. spawn_worker(child, developer, "创建 hello.rs 空文件", wait=true)
  2. resume_worker(child_id, "再添加一行注释")
  3. 汇总
预期: hello.rs 创建后被 resume 添加注释
验证点: resume_worker 阻塞到第二轮 agent_end 返回
```

### 场景 4：并行 child（缺口 G）
```
PRD: 创建 3 个文件 a.rs / b.rs / c.rs
coordinator.md:
  1. spawn_worker(child, developer, "a.rs", wait=false)  # 立即返回
  2. spawn_worker(child, developer, "b.rs", wait=false)
  3. spawn_worker(child, developer, "c.rs", wait=false)
  4. await_worker(wkr_a) + await_worker(wkr_b) + await_worker(wkr_c)
  5. 汇总
预期: 3 个 developer 并行跑，coordinator 用 await 串行收结果
验证点: 3 个文件都被创建；时间日志显示并行
```

### 场景 5（终极）：混合
```
PRD: 创建主模块 + 测试 + 文档
coordinator.md:
  1. spawn_worker(child, developer, "main.rs", wait=true)  # 同步等
  2. spawn_worker(peer, developer, "tests/main_test.rs")  # 异步
  3. spawn_worker(peer, reviewer, "审查 main.rs")          # 异步
  4. resume_worker(child_id, "根据 review 反馈修改")        # 同步 resume
  5. 等 follow_up 汇总
预期: 主路径同步，旁路异步，最后 resume 收尾
```

## 实施顺序建议

1. **J** (结构化返回值) → 改 SpawnWorkerTool 返回 JSON
2. **F** (resume_worker) → 复用 spawn_worker 的等待逻辑，新增工具
3. **G** (await_worker + spawn_worker wait 参数) → 拆分阻塞逻辑
4. **I** (peer → creator follow_up) → Manager 端后台 task 调 send_command(creator, "follow_up", ...)
5. **B** (channel_msg 进上下文) → ion_worker channel_msg 分支调 agent.follow_up()
6. **A** (pump 加回 CHANNEL_SEND) → 调试用
7. 跑场景 1 → 2 → 3 → 4 → 5

**注意：不需要做 H** —— follow_up_queue 链路（Agent 字段 + 方法 + run() 消化 + ion_worker RPC 分支）本来就完整存在。

## 已知风险

- **风险 1**：`process_pending_commands` 是 `&mut self`，spawn 后台 task 等 peer agent_end 时持有 registry lock 可能死锁。
  对策：subscribe 在持锁时调用拿 rx，rx.recv 在 spawn task 里独立做（不持锁）。

- **风险 2**：follow_up 推给 creator 时，creator 可能正在跑 LLM 调用，RPC `follow_up` 进不去。
  对策：`follow_up` RPC 走 stdin，agent_loop 在每轮开始前 drain follow_up_queue（对齐 pi agent.ts:443）。

- **风险 3**：GLM 推理时间长，await_worker 可能 300s 不够。
  对策：保留 300s 上限，超时返回 partial output。

## 参考实现

- pi 的 followUpQueue: `packages/agent/src/agent.ts:197,327,443`
- pi 的 RPC 类型: `packages/coding-agent/src/modes/rpc/rpc-types.ts:33,39,59,60`
- pi 的 follow_up 处理: agent 当前轮要停时 drain followUpQueue 作为新 prompt 继续

## 文件改动预估

| 文件 | 改动 |
|------|------|
| `src/agent/agent_loop.rs` | **无需改动**（follow_up_queue 链路已完整） |
| `src/runtime.rs` | +resume_worker / await_worker / channel_send / kill_worker trait 方法 + SpawnWorkerRequest 加 wait 字段（约 60 行） |
| `src/agent/tool.rs` | 改 SpawnWorkerTool 返回 JSON + 新增 ResumeWorkerTool / AwaitWorkerTool / ChannelSendTool / KillWorkerTool（约 250 行） |
| `src/worker_registry.rs` | process_pending_commands 加 peer 后台 follow_up task + resume_worker / await_worker 分支（约 100 行） |
| `src/bin/ion_worker.rs` | channel_msg 分支调 agent.follow_up()（约 10 行） |
| `src/bin/ion.rs` | pump 加回 CHANNEL_SEND 解析（约 15 行） |
| `.ion/agents/*.md` | 重写 coordinator.md / developer.md 适配新工具（约 100 行） |
