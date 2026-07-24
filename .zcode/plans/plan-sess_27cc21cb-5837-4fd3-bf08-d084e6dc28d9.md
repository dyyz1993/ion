# 内核能力补全：让 WASM 扩展能做 multi-compaction / session-supervisor / coordinator

## 背景

WASM 扩展已有 36 个钩子 + 17 个 host functions，但缺**内核数据访问**和**流程控制**类能力。补齐后，用户可以用纯 WASM 写出强大的扩展（压缩策略、质量监控、多任务协调），不需要改 ION 源码。

## 设计方案：AgentRpc 通道

核心思路：在 Context 里加一个 `agent_rpc` 通道（trait object），让 host functions 通过它读写 agent 状态。复用 ion_worker.rs 里已有的 RPC 逻辑（`get_context_usage`、`get_full_messages`、`steer` 等）。

```rust
// wasm_extension.rs — 新增
trait AgentRpcHandle: Send + Sync {
    fn call(&self, method: &str, params_json: &str) -> Result<String, String>;
}

// Context 新增字段
pub struct Context {
    // ...existing fields...
    pub agent_rpc: Option<Arc<dyn AgentRpcHandle>>,  // ← 新增
}
```

## 改动清单（3 个文件）

### 1. `src/wasm_extension.rs`（+150 行）

**新增 trait + 9 个 host functions：**

| host function | 签名 | 通过 agent_rpc 调 | 价值 |
|---|---|---|---|
| `host_get_token_count` | `(out_buf, out_cap) -> u32` | `get_context_usage` → `{total_tokens, context_window, usage_percent}` | P0：压缩触发条件 |
| `host_get_messages` | `(out_buf, out_cap) -> u32` | `get_full_messages` → JSON 数组 | P0：读对话历史 |
| `host_get_state` | `(out_buf, out_cap) -> u32` | `get_state` → `{model, message_count, is_running}` | P0：读 agent 状态 |
| `host_steer` | `(text_ptr, text_len) -> u32` | `steer` → 注入 steer 消息 | P0：强制 agent 继续 |
| `host_inject_follow_up` | `(text_ptr, text_len) -> u32` | `follow_up` → 注入 follow-up | P1：自动重试 |
| `host_llm_call` | `(prompt_ptr, prompt_len, model_ptr, model_len, out_buf, out_cap) -> u32` | `llm_call` → 调小模型 | P1：质量检查/标题生成 |
| `host_get_worker_status` | `(id_ptr, id_len, out_buf, out_cap) -> u32` | `get_worker_status` → 查子 worker | P1：coordinator 监控 |
| `host_compact_now` | `() -> u32` | `compact_now` → 立即触发压缩 | P2：按需压缩 |
| `host_create_worktree` | `(branch_ptr, branch_len, out_buf, out_cap) -> u32` | `create_worktree` → git worktree | P2：并行隔离 |

每个 host function 都用现有 pattern：
1. `caller.data().clone()` 拿 Context
2. 从 Context 取 `agent_rpc`
3. `block_in_place + handle.block_on(agent_rpc.call(method, params))` 
4. 结果写回 WASM 内存

### 2. `src/bin/ion_worker.rs`（+80 行）

**实现 `AgentRpcHandle` for WorkerAgentRpc：**

```rust
struct WorkerAgentRpc {
    agent: Arc<Mutex<Agent>>,      // 或 Agent 的状态快照
    api_registry: Option<Arc<ApiRegistry>>,  // 给 host_llm_call 用
    tokio_handle: tokio::runtime::Handle,
}

impl AgentRpcHandle for WorkerAgentRpc {
    fn call(&self, method: &str, params_json: &str) -> Result<String, String> {
        let params: serde_json::Value = serde_json::from_str(params_json)?;
        let handle = self.tokio_handle.clone();
        tokio::task::block_in_place(|| handle.block_on(async {
            match method {
                "get_context_usage" => self.get_context_usage().await,
                "get_full_messages" => self.get_full_messages().await,
                "steer" => self.steer(params).await,
                "llm_call" => self.llm_call(params).await,
                // ... 其他方法
            }
        }))
    }
}
```

这些方法**复用 ion_worker.rs 已有的 RPC handler 逻辑**（get_context_usage 在 line 2036，get_full_messages 在 line 2101 等），只是从 RPC handler 抽取成独立函数。

### 3. 注入到 WASM Context

在 ion_worker.rs 构建 WASM extension 时，把 `AgentRpcHandle` 注入到 Context：

```rust
// 已有：ctx.fs = Some(fs); ctx.tokio_handle = Some(handle);
// 新增：
ctx.agent_rpc = Some(Arc::new(WorkerAgentRpc { ... }));
```

## 实现顺序

| 步骤 | 做什么 | 预计行数 |
|------|--------|---------|
| 1 | 定义 `AgentRpcHandle` trait + 加到 Context | ~20 行 |
| 2 | 实现 P0：`host_get_token_count` + `host_get_messages` + `host_get_state` + `host_steer` | ~80 行 |
| 3 | 实现 P1：`host_llm_call` + `host_get_worker_status` + `host_inject_follow_up` | ~60 行 |
| 4 | 实现 P2：`host_compact_now` + `host_create_worktree` | ~30 行 |
| 5 | ion_worker.rs：实现 `WorkerAgentRpc` + 注入 | ~80 行 |
| 6 | 编译 + 测试 | — |

## 不改的东西

- **不改 Extension trait** — 钩子已经够了（on_context 传 messages，on_session_before_compact 传 messages）
- **不改 agent_loop.rs** — steer/follow_up 队列机制不变
- **不改 Cargo.toml** — 不加新依赖
- **不改现有 host functions** — 只加新的

## 验证方式

```bash
# 1. 编译
cargo build --bin ion --bin ion-worker

# 2. 全量测试（不能 break 现有 769 个测试）
cargo test --lib

# 3. 手动验证 host_get_token_count
ion serve
ion rpc --method extension_rpc --params '{"extension":"test","method":"token_count"}'

# 4. 用 B 跑一个依赖新能力的 WASM 扩展
#    （比如简单的 auto-session-title，用 host_llm_call + host_get_messages）
```

## 风险

| 风险 | 缓解 |
|------|------|
| Agent 状态不是 Send+Sync | 用 `tokio::task::block_in_place` + channel，不直接传 &mut Agent |
| host_llm_call 可能很慢（等 API 响应） | 同步阻塞在 WASM 里是安全的（worker 是独立线程） |
| 9 个 host function 一次加太多 | 分 P0/P1/P2 三批，先做 P0 验证 pattern |