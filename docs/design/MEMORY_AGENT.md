# Memory V0.2 设计文档 — 跨项目记忆 Agent

> **状态：Phase 1-8 已实现** — 独立的系统级 Agent，随 `ion serve` 启动，跨项目检索记忆。不是 V0.1 的升级，是不同的功能——V0.1 是项目级 Extension（被动注入），V0.2 是跨项目 Agent（主动检索）。
>
> **配套**：V0.1（项目级插件）已修复可用，见 [MEMORY_EXTENSION.md](./MEMORY_EXTENSION.md)。

---

## 0. 这个东西是什么

一个**常驻的系统级 Agent**，在 `ion serve` 启动时被 Extension 注册创建，关闭时被通知退出。所有用户 Worker 都能通过 `send_to_worker` 问它："上次类似的项目怎么处理的？" / "用户偏好是什么？" 它用自己的 LLM 理解查询意图，检索全局记忆库，返回结构化结果。

### 0.1 和 V0.1 的关系

| 维度 | V0.1（插件） | V0.2（Agent） |
|------|------------|--------------|
| **形态** | Extension（钩子） | Agent（系统级 Worker） |
| **范围** | per-project | 跨项目（全局） |
| **召回方式** | 被动注入（agent 不知情） | 主动检索（Worker 问它） |
| **需要 LLM** | 不需要（关键词匹配） | 需要（理解查询意图） |
| **存储** | JSON 文件 | SQLite + FTS5 |
| **生命周期** | 随会话 | 随 `ion serve` |
| **状态** | ✅ 已修复可用 | 🔧 本文档 |

**两者不冲突**——可以同时启用。V0.1 负责项目内的快速上下文注入，V0.2 负责跨项目的深度检索。

### 0.2 三场景适配

| 场景 | Memory Agent | 理由 |
|------|-------------|------|
| 场景 1（`ion "msg"`） | ❌ 不启动 | 进程跑完即退 |
| 场景 2（`ion --host`） | ❌ 不启动 | 临时 host，短任务 |
| 场景 3（`ion serve`） | ✅ 随 serve 启动/关闭 | 常驻，所有 Worker 能问 |

---

## 1. 生命周期

### 1.1 启动流程

```
ion serve 启动
    ↓
host 引擎初始化（WorkerRegistry）
    ↓
加载 Extensions（包括 GlobalMemoryExtension）
    ↓
GlobalMemoryExtension.on_init():
    "我需要一个后台记忆 Agent"
    → 调 host 的 create_worker(memory-agent, system=true)
    ↓
Memory Agent Worker 启动：
    - 加载全局记忆库 ~/.ion/agent/global-memory.db
    - 如果 DB 不存在，初始化 schema
    - 如果有旧 V0.1 JSON 数据，自动迁移
    - 进入 idle，等待查询
    ↓
host 就绪，Memory Agent 可被任何 Worker 查询
```

### 1.2 关闭流程

```
ion serve 收到 shutdown
    ↓
host 关闭前遍历所有 Extension 的 on_shutdown 钩子
    ↓
GlobalMemoryExtension.on_shutdown():
    → kill_worker(memory-agent)
    → Memory Agent Worker 正常退出
    ↓
host 退出
```

### 1.3 单例保证

**当前架构天然单例**：`ion serve` 绑定 `~/.ion/host.sock`，第二个 `ion serve` 启动失败（socket 占用）。所以 Memory Agent 随唯一的 host 启动，**天然只有一个实例**。

未来如果支持多 host（多 socket），Memory Agent 需要抽成独立守护进程（PID 文件锁）。当前不做（YAGNI）。

### 1.4 Extension 钩子（需要新增）

当前 Extension trait 没有全局初始化/关闭钩子。需要加：

```rust
// src/agent/extension.rs
trait AgentExtension {
    // ... 现有钩子 ...

    /// host 启动时调用（仅场景 3）。Extension 可在此创建系统级 Worker。
    /// 默认 no-op。
    async fn on_host_init(&self, _api: &ExtensionApi) -> AgentResult<()> { Ok(()) }

    /// host 关闭前调用。Extension 可在此清理资源/关闭 Worker。
    /// 默认 no-op。
    async fn on_host_shutdown(&self, _api: &ExtensionApi) -> AgentResult<()> { Ok(()) }
}
```

`on_host_init` 在 host 引擎初始化后、用户 Worker 创建前调用。Extension 拿到 `ExtensionApi`（可以 `create_worker` / `kill_worker`）。

---

## 2. 架构

### 2.1 整体位置

```
┌─ ion serve（场景 3）──────────────────────────────────┐
│                                                       │
│  ┌─ WorkerRegistry ──────────────────────────┐       │
│  │                                           │       │
│  │  用户 Worker A（项目 X）                   │       │
│  │    ↓ send_to_worker(memory-agent, query)  │       │
│  │                                           │       │
│  │  Memory Agent（系统级 Worker）             │       │
│  │    ├─ 全局记忆库 global-memory.db         │       │
│  │    ├─ LLM（理解查询意图）                  │       │
│  │    ├─ FTS5 全文检索                       │       │
│  │    └─ 持续维护大纲（后台 consolidate）     │       │
│  │                                           │       │
│  │  用户 Worker B（项目 Y）                   │       │
│  │    ↓ send_to_worker(memory-agent, query)  │       │
│  │                                           │       │
│  └───────────────────────────────────────────┘       │
└───────────────────────────────────────────────────────┘
```

### 2.2 Memory Agent 的内部结构

Memory Agent 是一个特殊的 ion-worker，它的 system prompt 是：

```
你是记忆检索 Agent。别的 Worker 会问你关于历史项目/用户偏好的问题。
你的工作：
1. 理解查询意图
2. 检索全局记忆库
3. 返回结构化结果（不是自由文本）
你有工具：memory_global_search / memory_outline_list / memory_consolidate
```

它的工具集（Agent 自主调用）：

| 工具 | 参数 | 作用 |
|------|------|------|
| `memory_global_search` | `query`, `category?`, `project?` | FTS5 搜索全局记忆 |
| `memory_outline_list` | `project?` | 列出大纲（全局或某项目） |
| `memory_consolidate` | — | 整理大纲（合并/去重/排序） |
| `memory_get_preferences` | — | 快速取用户偏好（高频查询优化） |

### 2.3 查询协议

用户 Worker → Memory Agent 的通信用现有的 `send_to_worker`：

```json
// 用户 Worker 发给 Memory Agent
{
    "method": "prompt",
    "params": {
        "text": "上次做 web 项目用的什么框架？用户有偏好吗？"
    }
}
```

Memory Agent 收到后：
1. LLM 理解查询 → 调 `memory_global_search("web 框架 偏好")`
2. 整理结果 → 返回结构化文本

```json
// Memory Agent 回复
{
    "type": "response",
    "success": true,
    "data": {
        "text": "找到 3 条相关记忆：\n1. [项目X] 用户偏好 React（2026-06）\n2. [项目Y] 技术决策：选了 Next.js（2026-07）\n3. [全局] 用户不喜欢 Vue（偏好）"
    }
}
```

---

## 3. 全局记忆库（SQLite + FTS5）

### 3.1 Schema

```sql
-- 主表：所有记忆条目（跨项目）
CREATE TABLE entries (
    id TEXT PRIMARY KEY,              -- "gmem_uuid"（UUID，不再用 len+1）
    project TEXT NOT NULL,            -- 来源项目名（或 "global" 表示跨项目）
    content TEXT NOT NULL,            -- 记忆内容
    category TEXT DEFAULT '',         -- 分类：preference / decision / status / note
    tags TEXT DEFAULT '',             -- 逗号分隔标签
    importance INTEGER DEFAULT 5,     -- 重要性 1-10（影响召回排序）
    archived INTEGER DEFAULT 0,       -- 软删除
    created_at INTEGER DEFAULT (unixepoch()),
    updated_at INTEGER DEFAULT (unixepoch())
);

-- FTS5 全文索引（content + category + tags）
CREATE VIRTUAL TABLE entries_fts USING fts5(
    content, category, tags,
    content=entries, content_rowid=rowid
);

-- 大纲索引（Memory Agent 维护，类似 V0.1 的 index.json）
CREATE TABLE outlines (
    id TEXT PRIMARY KEY,              -- "preferences" / "tech-decisions" 等
    summary TEXT,                     -- 大纲摘要
    project TEXT,                     -- 所属项目（或 "global"）
    entry_count INTEGER DEFAULT 0,
    updated_at INTEGER DEFAULT (unixepoch())
);
```

### 3.2 存储位置

```
~/.ion/agent/
├── global-memory.db          ← 全局记忆库（所有项目共享）
└── project-data/
    └── <hash>--<project>/
        └── memory/           ← V0.1 的项目级记忆（保持不动）
```

**V0.1 和 V0.2 的数据完全隔离**——V0.1 在 `project-data/`，V0.2 在 `global-memory.db`。

### 3.3 从 V0.1 迁移（一次性）

首次启动 Memory Agent 时，如果 `global-memory.db` 不存在但有 V0.1 数据，自动迁移：

```rust
fn auto_migrate() {
    if global_db.exists() { return; }  // 已迁移
    // 扫描所有 project-data/*/memory/outlines/*.json
    for project_dir in scan_projects() {
        for outline_file in project_dir.join("memory/outlines").read_dir() {
            for entry in parse_json(outline_file) {
                // INSERT INTO entries (project=project_name, content, ...)
            }
        }
    }
    // 标记迁移完成
}
```

迁移后 V0.1 的 JSON 文件保留（不删，向后兼容）。

---

## 4. Extension 实现

### 4.1 GlobalMemoryExtension

```rust
pub struct GlobalMemoryExtension {
    db_path: PathBuf,
    memory_agent_id: Option<String>,  // 启动后填充
}

impl AgentExtension for GlobalMemoryExtension {
    async fn on_host_init(&self, api: &ExtensionApi) -> AgentResult<()> {
        // 创建 Memory Agent Worker（系统级）
        let agent_id = api.create_worker(
            "memory-agent",                    // worker_id
            Some("memory-agent".into()),       // agent name（.md 定义）
            "global memory retrieval",         // initial prompt
            WorkerRelation::System,            // 系统级，不是某用户的子任务
        ).await?;
        // 存 agent_id，关闭时用
        // （需要可变状态——用 Arc<Mutex<>> 或 Cell）
        Ok(())
    }

    async fn on_host_shutdown(&self, api: &ExtensionApi) -> AgentResult<()> {
        if let Some(ref id) = self.memory_agent_id {
            api.kill_worker(id).await?;
        }
        Ok(())
    }

    // 不实现 on_system_prompt / on_input / on_context
    // —— V0.2 不做被动注入，只做主动检索
}
```

### 4.2 agent 定义（memory-agent.md）

```
# Memory Agent

你是一个跨项目记忆检索 Agent。其他 Worker 会通过 send_to_worker 向你提问。

## 你的职责
1. 理解查询意图（什么类型的记忆？哪个项目？什么关键词？）
2. 调用 memory_global_search 检索
3. 整理结果，返回结构化摘要

## 返回格式
- 列出找到的记忆（按相关性排序）
- 标注来源项目和时间
- 如果没找到，明确说"未找到相关记忆"

## 工具
- memory_global_search(query, category?, project?)
- memory_outline_list(project?)
- memory_get_preferences()
```

---

## 5. 通信流程（完整示例）

### 场景：用户 Worker 问 Memory Agent

```
用户 Worker A（项目 ion-web）：
  agent 调 send_to_worker("memory-agent", "上次 web 项目用了什么框架？用户有偏好吗？")
      ↓
  send_to_worker → Memory Agent 收到 prompt
      ↓
  Memory Agent 的 LLM 思考：
    "查询意图：web 框架 + 用户偏好"
    → 调 memory_global_search("web 框架 偏好", category="preference")
    → 调 memory_global_search("框架 技术决策")
      ↓
  FTS5 返回结果：
    [1] project=ion-web, "用户偏好 React", category=preference, importance=8
    [2] project=ion-api, "选了 Next.js 做 SSR", category=decision, importance=6
    [3] project=global, "不喜欢 Vue", category=preference, importance=7
      ↓
  Memory Agent 回复：
    "找到 3 条相关记忆：
     1. [ion-web] 用户偏好 React（偏好，重要度 8）
     2. [ion-api] 选了 Next.js 做 SSR（决策，重要度 6）
     3. [全局] 不喜欢 Vue（偏好，重要度 7）"
      ↓
  send_to_worker 返回给用户 Worker A
      ↓
  用户 Worker A 的 agent 继续工作（基于记忆做决策）
```

---

## 6. CLI 测试指南

> **前置条件：**
> 1. `cargo build --bin ion --bin ion-worker`
> 2. `ion serve` 能正常启动
> 3. 全局记忆库 `~/.ion/agent/global-memory.db` 存在（或自动创建）

### Group A：Memory Agent 生命周期

#### A1 Memory Agent 随 serve 启动

```bash
# Terminal 1: 启动 serve
ion serve &

# 等 2 秒让 Memory Agent 启动
sleep 2

# 验证 memory-agent Worker 存在
ion rpc --method get_state --params '{}'
```

**预期：** workers 列表中有 `memory-agent`，status 为 `idle`。

**验证点：**
- ✅ memory-agent Worker 存在
- ✅ status = idle（不是 busy/dead）
- ✅ 只有一个实例（天然单例）

#### A2 Memory Agent 随 serve 关闭

```bash
# 获取 serve 的 PID
SERVE_PID=$(pgrep -f "ion serve")

# 关闭 serve
kill $SERVE_PID

# 等 2 秒
sleep 2

# 验证 memory-agent 也退出了
ion rpc --method get_state 2>&1 || echo "serve 已关闭"
```

**预期：** serve 关闭后，Memory Agent 也退出。

**验证点：**
- ✅ serve 关闭后 socket 不存在
- ✅ 无残留的 memory-agent 进程

### Group B：记忆检索

#### B1 写入记忆到全局库

```bash
# 直接往全局库写入测试数据（通过 extension_rpc 或 SQL）
ion rpc --method extension_rpc --params '{
    "extension": "global-memory",
    "method": "save",
    "args": {
        "content": "用户偏好 Rust 的 async/await",
        "category": "preference",
        "tags": "rust,async",
        "project": "test-project"
    }
}'
```

**预期：**

```json
{
    "type": "response",
    "success": true,
    "data": {"id": "gmem_xxx"}
}
```

**验证点：**
- ✅ 返回 ID（UUID 格式）
- ✅ `~/.ion/agent/global-memory.db` 有数据

#### B2 FTS5 搜索

```bash
ion rpc --method extension_rpc --params '{
    "extension": "global-memory",
    "method": "search",
    "args": {"query": "rust async"}
}'
```

**预期：**

```json
{
    "type": "response",
    "success": true,
    "data": {
        "results": [
            {
                "id": "gmem_xxx",
                "content": "用户偏好 Rust 的 async/await",
                "category": "preference",
                "project": "test-project"
            }
        ]
    }
}
```

**验证点：**
- ✅ 搜 "rust async" 命中刚写入的记忆
- ✅ FTS5 全文匹配（不是子串扫描）

#### B3 跨项目搜索

```bash
# 写入另一个项目的记忆
ion rpc --method extension_rpc --params '{
    "extension": "global-memory",
    "method": "save",
    "args": {
        "content": "项目用 TypeScript",
        "category": "preference",
        "project": "other-project"
    }
}'

# 搜全局（不指定 project）
ion rpc --method extension_rpc --params '{
    "extension": "global-memory",
    "method": "search",
    "args": {"query": "type"}
}'
```

**预期：** 返回 other-project 的记忆（跨项目检索）。

**验证点：**
- ✅ 搜全局命中不同项目的记忆
- ✅ 结果标注来源项目

### Group C：通过 Memory Agent 检索（LLM 驱动）

#### C1 Worker 问 Memory Agent

```bash
# 通过 send_to_worker 向 memory-agent 发查询
ion rpc --method send_to_worker --params '{
    "worker_id": "memory-agent",
    "method": "prompt",
    "params": {"text": "用户对编程语言有什么偏好？"}
}'
```

**预期：** Memory Agent 用 LLM 理解查询，调 FTS5 检索，返回结构化结果。

**验证点：**
- ✅ Memory Agent 响应（不是超时）
- ✅ 返回内容含历史记忆（不是空）
- ✅ 结果结构化（标注来源项目 + 类别）

#### C2 Memory Agent 没找到记忆

```bash
ion rpc --method send_to_worker --params '{
    "worker_id": "memory-agent",
    "method": "prompt",
    "params": {"text": "上次用 COBOL 的经验"}
}'
```

**预期：** 明确回复"未找到相关记忆"。

**验证点：**
- ✅ 不编造记忆（不 hallucinate）
- ✅ 明确说"未找到"

### Group D：迁移 + 大纲

#### D1 V0.1 数据自动迁移

```bash
# 前置：有 V0.1 的 project-data/*/memory/outlines/*.json
# 删除 global-memory.db（模拟首次）
rm ~/.ion/agent/global-memory.db

# 重启 serve（触发自动迁移）
ion serve &
sleep 3

# 验证迁移后数据存在
ion rpc --method extension_rpc --params '{
    "extension": "global-memory",
    "method": "list",
    "args": {}
}'
```

**预期：** V0.1 的记忆被迁移到全局库。

**验证点：**
- ✅ 迁移后 global-memory.db 有数据
- ✅ 迁移数据含原项目名
- ✅ V0.1 JSON 文件仍在（不删）

#### D2 大纲列表

```bash
ion rpc --method extension_rpc --params '{
    "extension": "global-memory",
    "method": "outline_list",
    "args": {}
}'
```

**预期：** 返回大纲列表（全局或按项目）。

**验证点：**
- ✅ 大纲含 id/summary/entry_count
- ✅ 跨项目的大纲都列出

### Group E：单元测试 + 集成测试

#### E1 单元测试

```bash
cargo test --lib global_memory
```

**预期覆盖：**

| 测试 | 验证点 |
|------|--------|
| `test_db_init` | global-memory.db 创建 + schema 正确 |
| `test_fts_search` | FTS5 MATCH 命中 |
| `test_cross_project_search` | 跨项目检索 |
| `test_importance_ranking` | 重要性排序 |
| `test_soft_delete` | archived 标记 + 搜索过滤 |
| `test_id_unique` | UUID 无重复 |
| `test_migrate_from_v01` | V0.1 JSON → SQLite 迁移 |

#### E2 集成测试

```bash
cargo test --test global_memory_e2e -- --nocapture
```

**预期覆盖：**

| 测试 | 验证点 |
|------|--------|
| `memory_agent_starts_with_serve` | serve 启动后 memory-agent 存在 |
| `memory_agent_stops_with_serve` | serve 关闭后 memory-agent 退出 |
| `worker_queries_memory_agent` | send_to_worker 查询 Memory Agent |
| `cross_project_retrieval` | 跨项目检索命中 |

---

## 7. 实现顺序

| Phase | 内容 | 预估 |
|-------|------|------|
| 1 | `global_memory.rs` 数据层（SQLite schema + FTS5 + CRUD） | 1.5 天 |
| 2 | V0.1 → V0.2 自动迁移 | 0.5 天 |
| 3 | Extension 钩子（on_host_init / on_host_shutdown） | 1 天 |
| 4 | GlobalMemoryExtension（启动/关闭 Memory Agent） | 1 天 |
| 5 | Memory Agent 工具（memory_global_search 等） | 1 天 |
| 6 | memory-agent.md agent 定义 + system prompt | 0.5 天 |
| 7 | CLI 测试（Group A-E） | 1 天 |
| **合计** | | **~6.5 天** |

---

## 8. 不做（明确排除）

| 功能 | 排除理由 |
|------|---------|
| 多 host 共享 Memory Agent | 当前单 host 天然单例；YAGNI |
| Memory Agent 常驻独立于 serve | 场景 1/2 不需要；场景 3 随 serve 足够 |
| 语义向量检索（embedding） | FTS5 关键词检索够用；embedding 后续可选 |
| Memory Agent 自主学习（无查询时整理） | 先做被动响应；主动整理后续 |
| 记忆自动遗忘（TTL） | 先靠 importance 排序；TTL 后续 |

---

## 9. 与其他功能的关系

| 功能 | 关系 |
|------|------|
| **V0.1（项目级插件）** | 数据隔离（V0.1 在 project-data/，V0.2 在 global-memory.db）。可同时启用。迁移是一次性的。 |
| **Session Tree** | Memory Agent 可被 Session Tree 的分支 Worker 查询（send_to_worker 跨 Worker） |
| **FauxProvider** | Memory Agent 测试时用 faux 驱动（不调真实 LLM） |
| **Record/Replay** | Memory Agent 的查询可录制回放 |
| **Worker 崩溃恢复** | Memory Agent 崩溃时走标准 crash recovery（Dead + 通知 + 引导恢复） |
