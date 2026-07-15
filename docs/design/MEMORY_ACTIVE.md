# Memory Active — V0.2 主动注入 + 自动整理 设计文档

> **状态：待定** — 给 GlobalMemory（V0.2）补两个能力：(1) 被动注入（on_input → on_context 自动检索）；(2) 自动整理（去重/归档/更新大纲索引）。
>
> 对齐 V0.1 已有的 consolidation + injection 机制，但作用于跨项目的全局记忆库。

---

## 何时使用这个文档

- 想让 GlobalMemory（V0.2）像 V0.1 一样**自动注入**相关记忆到上下文（不用 LLM 主动调 `global_memory_search`）
- 想让全局记忆库**自动整理**（记忆多了之后去重/归档低价值的/更新大纲索引）
- 想把 AGENTS.md 里"Active Memory sub-agent — 待定"落地

**前置阅读**：[MEMORY_AGENT.md](./MEMORY_AGENT.md)（V0.2 设计）、[MEMORY_EXTENSION.md](./MEMORY_EXTENSION.md)（V0.1 设计）

---

## 1. 问题

ION 有两套 Memory，各有短板：

| | V0.1（项目级扩展） | V0.2（跨项目 GlobalMemory） |
|---|---|---|
| **被动注入** | ✅ on_input 检索 → on_context 注入 | ❌ 只有 LLM 主动调 `global_memory_search` 才会想起 |
| **自动整理** | ✅ 每 5 轮 consolidation（更新 outline 计数） | ❌ 记忆只增不减，没有去重/归档 |
| **跨项目** | ❌ 只搜当前项目的 | ✅ 搜所有项目的 |
| **FTS5** | ❌ 关键词匹配 | ✅ 全文搜索 |

**V0.2 缺的是 V0.1 已有的两个能力，但要作用于全局库。**

用户感受：在项目 B 里干活时，如果之前在项目 A 存了相关记忆，**V0.2 不会自动告诉我**——除非 LLM 碰巧调了 `global_memory_search`。而且记忆越存越多，没有整理，搜索结果越来越嘈杂。

## 2. 设计

### 2.1 能力 A：被动注入（on_input → on_context）

**目标**：用户每次输入时，自动用 FTS5 搜全局记忆库，如果有相关的就注入上下文。

**为什么不做在 GlobalMemoryExtension（单例扩展）里**：单例扩展的钩子触发时机和普通扩展不同（它在 host 级，不在每个 Worker 里）。on_input/on_context 是 **per-Worker** 的钩子，单例扩展不参与每个 Worker 的 agent loop。

**方案**：在 V0.1 的 `MemoryExtension`（`src/agent/memory.rs`）里增加全局记忆检索——它已经实现了 on_input/on_context，只需要在检索时**同时搜全局库**。

```rust
// src/agent/memory.rs 的 on_input 里，在搜完项目级记忆后，加一步搜全局库
async fn on_input(&self, ctx: &mut InputContext) -> AgentResult<()> {
    // ... 现有逻辑：搜项目级记忆（V0.1 store）...

    // ── 新增：搜全局记忆库（V0.2 store）──
    if let Some(ref global_store) = self.global_store {
        let global_results = global_store.search(&ctx.text, None)
            .unwrap_or_default();
        if !global_results.is_empty() {
            // 标记待注入（跟 V0.1 的注入队列同一个机制）
            self.pending_global_inject.push(global_results);
        }
    }

    Ok(())
}

// on_context 里消费待注入的全局记忆
async fn on_context(&self, messages: &mut Vec<Message>) -> AgentResult<()> {
    // ... 现有逻辑：注入项目级记忆 ...

    // ── 新增：注入全局记忆 ──
    if !self.pending_global_inject.is_empty() {
        let inject_text = self.pending_global_inject.drain(..)
            .flatten()
            .take(5)  // 最多注入 5 条，防 token 爆炸
            .map(|e| format!("[{}] {}", e.category, e.content))
            .collect::<Vec<_>>()
            .join("\n");
        if !inject_text.is_empty() {
            // 注入为 system 消息
            messages.insert(0, Message::system(format!(
                "<global_memory>\n以下是跨项目相关记忆：\n{inject_text}\n</global_memory>"
            )));
        }
    }

    Ok(())
}
```

**去重**：跟 V0.1 一样用 hash 对比——如果全局记忆的内容 hash 跟上次注入的一样，跳过（20 轮去重窗口）。

**注入格式**：
```xml
<global_memory>
以下是跨项目相关记忆：
[设计决策] 认证用 DeepSeek API，key 在 auth.json
[bug 修复] WASM 沙箱路径逃逸用 safe_join 修复
</global_memory>
```

### 2.2 能力 B：自动整理（consolidation）

**目标**：全局记忆库定期整理——去重、归档低价值、更新大纲索引。

**触发时机**：跟 V0.1 一样，在 on_input 里每 N 轮触发一次（复用 V0.1 的 turn_count）。V0.2 的整理逻辑：

```rust
// src/agent/memory.rs 的 on_input 里，consolidation 段
if store.turn_count % 10 == 0 {  // 每 10 轮整理一次全局库
    if let Some(ref global_store) = self.global_store {
        let stats = global_store.consolidate()?;
        self.emit("global_memory_consolidated", serde_json::json!({
            "deduplicated": stats.deduplicated,
            "archived": stats.archived,
            "total_remaining": stats.total,
        }));
    }
}
```

**GlobalMemoryStore 新增 `consolidate()` 方法**（`src/global_memory.rs`）：

```rust
impl GlobalMemoryStore {
    /// 整理全局记忆库：去重 + 归档 + 更新大纲索引
    pub fn consolidate(&self) -> Result<ConsolidationStats, String> {
        let conn = self.conn.lock().await;
        let mut stats = ConsolidationStats::default();

        // 1. 去重：内容 hash 相同的记忆，保留 importance 最高的，其余 archived
        //    SELECT content, COUNT(*) c FROM entries WHERE archived=0 GROUP BY content HAVING c > 1
        //    → 对每组保留 importance 最大的，其余 UPDATE archived=1

        // 2. 归档：importance=0 且 created_at 超过 30 天的 → archived=1
        //    UPDATE entries SET archived=1 WHERE importance=0 AND created_at < ?

        // 3. 更新大纲索引（outlines 表）
        //    对每个 project，统计未归档的 entry 数，更新 outlines.entry_count
        //    如果有 N 条以上同 category 的，生成 summary（取最高 importance 的前 3 条 content 拼接）

        Ok(stats)
    }
}

pub struct ConsolidationStats {
    pub deduplicated: usize,  // 去重了多少条
    pub archived: usize,      // 归档了多少条
    pub total: usize,         // 整理后剩余活跃条数
}
```

### 2.3 大纲索引增强（outlines 表）

V0.2 已有 `outlines` 表但没被用起来。整理时更新它：

```sql
-- 每个 project 的大纲
-- summary = 该 project 下 importance 最高的前 3 条记忆拼接
-- entry_count = 未归档的记忆数
REPLACE INTO outlines (id, summary, project, entry_count, updated_at)
SELECT
    project,                              -- id = project 名
    GROUP_CONCAT(                         -- summary = top 3 内容拼接
        SUBSTR(content, 1, 200), ' ... | '
    ),
    project,
    COUNT(*),
    unixepoch()
FROM entries
WHERE archived = 0
GROUP BY project;
```

**用途**：on_system_prompt 里注入 `<global_memory_outline>`，让 LLM 知道有哪些项目的记忆可用（不预加载全部内容，省 token）：

```xml
<global_memory_outline>
项目 A (ion): 扩展系统设计、hooks 实现、权限记忆...
项目 B (web-app): React 组件封装、API 认证...
项目 C (cli-tool): Rust 命令行解析、文件监控...
</global_memory_outline>
```

## 3. 改动文件清单

| 文件 | 改动 | 行数 |
|------|------|------|
| `src/agent/memory.rs` | on_input 加全局检索 + on_context 加全局注入 + consolidation 加全局整理 | ~80 |
| `src/global_memory.rs` | consolidate() 方法 + ConsolidationStats + outlines 更新 | ~60 |
| `src/agent/memory.rs` | pending_global_inject 字段 + 去重 hash 对比 | ~20 |
| `tests/memory_active_ci.sh` | CLI 测试 | ~80 |
| **总计** | | **~240** |

## 4. 验证方案（5 维度）

> Memory 的核心价值不是"能存能搜"（那是数据库的事），而是"搜出来的东西相不相关、注入的开销值不值、延迟会不会卡用户"。以下 5 个维度必须全部覆盖。

### Group A：检索精度（Recall + Precision）

**目标**：验证 FTS5 搜出来的记忆跟用户输入**真正相关**，不相关的不会误注入。

**测试数据**（存入全局库）：

| ID | content | category | project |
|----|---------|----------|---------|
| m1 | "认证用 DeepSeek API，key 在 auth.json" | 设计决策 | ion |
| m2 | "文件快照用 content-addressed object store" | 架构 | ion |
| m3 | "React 组件用 useState + useEffect 封装" | 前端 | web-app |
| m4 | "Rust 命令行解析用 clap derive" | 工具链 | cli-tool |
| m5 | "认证 token 过期后自动刷新" | bug 修复 | web-app |

**测试用例**：

```bash
# A1 高精度：搜"认证"应命中 m1 + m5（都跟认证相关），不命中 m2/m3/m4
RESULT=$(ion rpc ... --method extension_rpc --params \
  '{"extension":"global-memory","method":"search","params":{"query":"认证 API key"}}')
# 验证：结果包含 m1 和 m5，不包含 m2/m3/m4

# A2 跨项目召回：搜"认证"能召回不同项目（ion 的 m1 + web-app 的 m5）
# 验证：结果的 project 字段有 "ion" 和 "web-app"

# A3 无关查询不误注入：搜"天气"应返回空
RESULT=$(ion rpc ... --method extension_rpc --params \
  '{"extension":"global-memory","method":"search","params":{"query":"今天天气怎么样"}}')
# 验证：结果为空（FTS5 不匹配）

# A4 importance 排序：m1 importance=5，m5 importance=1，搜"认证"时 m1 应排前面
# 验证：m1 在 m5 前面（ORDER BY bm25 + importance DESC）

# A5 中文分词：搜"密钥"能命中 m1（"key" → "密钥"语义关联）
# 注意：FTS5 默认不分词中文，可能需要 unicode61 或自定义 tokenizer
# 这个 case 可能 XFail（取决于 FTS5 中文支持），记录基线
```

**验证点**：
- ✅ 相关查询命中最相关的记忆（recall > 80%）
- ✅ 无关查询不误命中（precision > 90%）
- ✅ 跨项目召回正常
- ✅ importance 影响排序

### Group B：注入延迟（Latency）

**目标**：on_input 里搜全局库 + on_context 里注入，**不能让用户感知到卡顿**。

**测试方法**：在 on_input 前后打时间戳，测量全局检索耗时。

```rust
// memory.rs on_input 里
let t0 = std::time::Instant::now();
let global_results = global_store.search(&ctx.text, None)?;
let search_ms = t0.elapsed().as_millis();
tracing::info!("[memory] global search took {search_ms}ms, found {} results", global_results.len());
```

**测试用例**：

```bash
# B1 小库延迟：100 条记忆，检索应 < 5ms
# 存 100 条 → on_input → 测量 search 耗时
# 验证：search_ms < 5

# B2 中库延迟：1000 条记忆，检索应 < 20ms
# 存 1000 条 → on_input → 测量 search 耗时
# 验证：search_ms < 20

# B3 大库延迟：5000 条记忆，检索应 < 50ms
# 存 5000 条 → on_input → 测量 search 耗时
# 验证：search_ms < 50（FTS5 索引查询是 O(log N)）

# B4 注入总延迟：search + format + inject 的完整链路
# 验证：on_input + on_context 总增量 < 10ms（不含 LLM 调用）
```

**验证点**：
- ✅ 100 条 < 5ms，1000 条 < 20ms，5000 条 < 50ms
- ✅ 用户无感知（< 100ms 是人类感知阈值）

### Group C：Token 开销（Context Size）

**目标**：注入的记忆内容**不能撑爆上下文窗口**。每轮注入的 token 量要有上限。

**测试方法**：统计注入前后的 message token 数差异。

```bash
# C1 单次注入 token 数
# 存 10 条各 200 字的记忆 → 搜"测试" → 注入
# 统计：注入的 <global_memory> 块占多少 token
# 验证：注入 token < 500（约 350 中文字 / 750 英文字符）

# C2 大纲注入 token 数
# 5 个项目各 20 条记忆 → on_system_prompt 注入 <global_memory_outline>
# 验证：outline token < 200（每个项目一行摘要）

# C3 累计注入：连续 5 轮对话的注入总量
# 验证：5 轮累计注入 < 2000 token（去重机制生效，不重复注入同一条）

# C4 注入上限：即使搜出 20 条相关记忆，最多只注入 5 条
# 验证：注入条数 <= 5（代码里的 take(5) 限制）
```

**验证点**：
- ✅ 单次注入 < 500 token
- ✅ 大纲注入 < 200 token
- ✅ 5 轮累计 < 2000 token（去重生效）
- ✅ 最多注入 5 条（硬上限）

### Group D：去重 + 整理效果

**目标**：自动整理真正减少了噪音——重复的被去掉、低价值的被归档。

```bash
# D1 去重：存 5 条内容完全相同的记忆（不同 importance）
# consolidate 后：保留 importance 最高的 1 条，其余 archived
# 验证：active 条数从 5 → 1

# D2 近似去重：存内容 90% 相似的两条（只差几个字）
# 注意：当前用 content 完全匹配，近似去重可能不支持
# 这个 case 可能 XFail，记录基线

# D3 归档：存 10 条 importance=0 的记忆，created_at 设为 31 天前
# consolidate 后：全部 archived
# 验证：active 条数从 10 → 0

# D4 整理后搜索更干净：整理前搜出 15 条（含重复），整理后搜出 8 条
# 验证：整理后搜索结果条数减少，精度提升

# D5 outlines 更新：整理后 outlines 表的 entry_count 减少
# 验证：entry_count 反映 archived 后的真实活跃数
```

**验证点**：
- ✅ 完全重复去重有效
- ✅ importance=0 + 过期 → 归档
- ✅ 整理后搜索结果更少更精
- ✅ outlines 表同步更新

### Group E：压力 + 边界

**目标**：极端场景不崩溃。

```bash
# E1 空库搜索：全局库为空时 on_input 搜全局 → 返回空，不 panic
# E2 超长输入：用户输入 10000 字 → FTS5 搜索不报错（可能截断 query）
# E3 并发写入：两个 Worker 同时 save → SQLite 锁不冲突（已有 Mutex）
# E4 consolidate 幂等：连续跑两次 consolidate → 第二次 deduplicated=0 archived=0
# E5 注入格式安全：记忆内容含 XML 特殊字符（< > &）→ 注入的 <global_memory> 块不被破坏
```

**验证点**：
- ✅ 空库 / 超长输入 / 并发 / 幂等 / XML 安全

### 验证脚 本结构

```
tests/memory_active_ci.sh
├── Group A：检索精度（5 case，recall + precision + 排序 + 中文）
├── Group B：注入延迟（4 case，100/1000/5000 条 + 总链路）
├── Group C：Token 开销（4 case，单次/大纲/累计/上限）
├── Group D：去重整理（5 case，完全去重/归档/整理后精度/outlines）
└── Group E：压力边界（5 case，空库/超长/并发/幂等/XML安全）
```

**总计 23 case**，覆盖精度/延迟/token/整理/边界五个维度。

**注意**：Group B（延迟）和 Group C（token）需要在测试里有可测量的指标输出——on_input 记录 `search_ms`，on_context 记录 `injected_tokens`。这些指标通过 tracing 或 RPC 返回值暴露给 CI 脚本断言。

## 5. 并行开发注意事项

- 改动集中在 `src/agent/memory.rs`（V0.1 扩展）和 `src/global_memory.rs`（V0.2 存储）
- **不改** Extension trait、不改 hooks、不改 ion-provider
- 与其他功能的文件不重叠
- 测试用 FauxProvider 驱动，不调真实 LLM

## 6. 与 V0.1 / V0.2 的关系

```
用户输入
    ↓
on_input（V0.1 MemoryExtension）
    ├─ 搜项目级记忆（V0.1 store，关键词匹配）     ← 已有
    ├─ 搜全局记忆库（V0.2 store，FTS5）           ← 新增（能力 A）
    └─ 每 10 轮整理全局库（consolidate）           ← 新增（能力 B）
    ↓
on_context
    ├─ 注入项目级记忆（<memory_context>）          ← 已有
    └─ 注入全局记忆（<global_memory>）             ← 新增（能力 A）
    ↓
on_system_prompt
    └─ 注入全局大纲（<global_memory_outline>）     ← 新增（能力 A）
```

**V0.1 和 V0.2 不是竞争关系，是互补**——V0.1 负责项目级快速匹配，V0.2 负责跨项目深度检索。这个文档让 V0.2 获得跟 V0.1 一样的被动注入 + 自动整理能力。

## 7. 对标

| 对比项 | V0.1 | V0.2 现状 | V0.2 本文档后 |
|--------|------|----------|-------------|
| 被动注入 | ✅ on_input→on_context | ❌ 只能 LLM 主动调 | ✅ 自动注入 |
| 自动整理 | ✅ 每 5 轮 | ❌ 无 | ✅ 每 10 轮 |
| 去重 | ✅ hash 对比 | ❌ 无 | ✅ content hash |
| 归档 | ✅ archived 标记 | ❌ 无 | ✅ importance + 时间 |
| 大纲索引 | ✅ outline.json | ✅ outlines 表（但没用） | ✅ 整理时更新 + 注入 |
| 跨项目 | ❌ | ✅ | ✅ |
| FTS5 | ❌ | ✅ | ✅ |
