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

## 4. CLI 测试指南

### Group A：被动注入

```bash
# A1 存一条跨项目记忆
ion rpc --session <sid> --method extension_rpc --params \
  '{"extension":"global-memory","method":"save","params":{"content":"认证用 DeepSeek API","category":"设计决策","project":"ion"}}'

# A2 在新会话里输入相关关键词，验证自动注入
ion rpc --session <sid2> --method prompt --params '{"text":"认证怎么做的"}'
# 验证：FauxProvider 收到的 messages 里应有 <global_memory> 认证用 DeepSeek API </global_memory>
```

### Group B：自动整理

```bash
# B1 存 20 条记忆（含重复）
for i in $(seq 1 20); do
  ion rpc --session <sid> --method extension_rpc --params \
    "{\"extension\":\"global-memory\",\"method\":\"save\",\"params\":{\"content\":\"test memory $i\",\"category\":\"test\",\"project\":\"test-proj\"}}"
done

# B2 存重复内容
ion rpc --session <sid> --method extension_rpc --params \
  '{"extension":"global-memory","method":"save","params":{"content":"test memory 1","category":"test","project":"test-proj"}}'

# B3 触发整理（跑 10 轮 on_input）
# 验证：deduplicated >= 1（重复的被归档）
```

### Group C：大纲索引

```bash
# C1 存几条记忆后，查 outlines 表
ion rpc --session <sid> --method extension_rpc --params \
  '{"extension":"global-memory","method":"list_outlines","params":{}}'
# 验证：每个 project 有 summary + entry_count
```

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
