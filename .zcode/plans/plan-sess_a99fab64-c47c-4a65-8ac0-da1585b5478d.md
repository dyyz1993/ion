# Active Memory sub-agent 实现

## 目标
MEMORY_AGENT.md 设计的"系统级常驻 Worker"——ion serve 启动时由 GlobalMemoryExtension 创建一个带 LLM 的 memory-agent Worker，用户 Worker 通过 send_to_worker 主动问它跨项目记忆问题。

数据层（GlobalMemoryStore）已 100% 就绪，缺的是 agent 化（0%→100%）。

## 设计决策

**方案 B：加 `on_singleton_post_init` 钩子**（不改 on_singleton_init 签名，向后兼容）
- `on_singleton_init` 保持原样（开 DB + 迁移）
- 新增 `on_singleton_post_init(&self, registry: &Arc<Mutex<WorkerRegistry>>)` —— init 之后调，专门用来 spawn agent
- 因为 host 端 create_worker 直接操作 registry（不走 bridge），所以 post_init 拿到 registry 就能 spawn

## 实现步骤（6 步）

### 步骤 1：WorkerRelation::System + SpawnRelation::System
- `src/worker_registry.rs`: WorkerRelation 加 `System` 变体
- `src/runtime.rs`: SpawnRelation 加 `System` 变体
- create_worker 分支（handle_manager_command）加 System 处理：无 creator、无汇报指令注入、立即返回 worker_id

### 步骤 2：on_singleton_post_init 钩子
- `src/agent/extension.rs`: Extension trait 加 `async fn on_singleton_post_init(&self, _registry: &std::sync::Arc<tokio::sync::Mutex<crate::worker_registry::WorkerRegistry>>) -> AgentResult<()> { Ok(()) }`
- `src/worker_registry.rs`: init_singletons 之后调 post_init（遍历单例，传入 registry）

### 步骤 3：memory-agent.md agent 定义
- 新建 `~/.ion/agents/memory-agent.md`（或 examples/agents/），system prompt 定义 memory agent 的角色（理解查询意图 → 调 global_memory_search → 返回结构化结果）

### 步骤 4：GlobalMemoryExtension.on_singleton_post_init 实现
- `src/global_memory_ext.rs`: 加字段 `active_memory_worker: Mutex<Option<String>>`
- post_init 里 `registry.lock().await.create_worker(...)` spawn memory-agent（relation=System）
- 记录 worker_id；on_singleton_shutdown 里 kill

### 步骤 5：补 singleton_user_join 调用点
- `src/worker_registry.rs`: create_worker 成功后，如果是普通用户 Worker，调 singleton_user_join（让 memory agent 知道有新用户）

### 步骤 6：memory-agent 的工具
- 复用现有 GlobalMemorySearchTool / GlobalMemorySaveTool（已在 tool.rs）
- memory-agent.md 的 agent 定义指定这些工具

## 改动文件清单

| 文件 | 改动 |
|------|------|
| `src/agent/extension.rs` | +on_singleton_post_init 钩子 |
| `src/runtime.rs` | +SpawnRelation::System |
| `src/worker_registry.rs` | +WorkerRelation::System + create_worker System 分支 + init 后调 post_init + singleton_user_join 调用点 |
| `src/global_memory_ext.rs` | +active_memory_worker 字段 + post_init spawn + shutdown kill |
| `examples/agents/memory-agent.md` | 新建 agent 定义 |
| `docs/design/MEMORY_AGENT.md` | 状态修正（Phase 1-2→正在实现 Phase 3-6） |

## 验证
- cargo build --lib --bin ion --bin ion-worker
- 起 ion serve → memory-agent Worker 自动 spawn（日志可见）
- ion rpc send_to_worker memory-agent "查 Rust 异步记忆" → 返回结果
- memory_agent_ci.sh（起 serve → 验证 agent 存在 → 查询 → 返回）

## 不做
- 不改 on_singleton_init 签名（向后兼容）
- 不碰 MEMORY_ACTIVE.md 的被动注入（两条路线并存）
- 不做 memory-agent 的主动整理/归档（后续 Phase）