# Memory Active — V0.2 主动注入 + 自动整理 设计文档

> **状态：已完成** — V0.2 被动注入（on_input→on_context 搜全局库）+ 自动整理（去重/归档/大纲索引）已实现并验证。
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

## 4. 验证方案（核心用户场景驱动）

> 测试围绕**用户真实使用 Memory 的 5 条核心链路**设计，每条链路覆盖"能不能用 + 好不好用 + 会不会出问题"三个层次。不按技术维度（精度/延迟/token）分，而是按用户场景分——因为用户不关心 precision，关心的是"我问认证的事，它帮我找到了吗"。

### 测试数据准备

所有 Group 共享以下全局记忆库（模拟真实多项目场景）：

| ID | content | category | project | importance |
|----|---------|----------|---------|-----------|
| m1 | "认证用 DeepSeek API，key 在 auth.json，base_url 是 opencode.ai/zen/go/v1" | 设计决策 | ion | 5 |
| m2 | "文件快照用 content-addressed object store，zstd 压缩 >64B 文件" | 架构 | ion | 3 |
| m3 | "React 表单组件用 useState + useEffect 封装，props 传 onChange" | 前端 | web-app | 2 |
| m4 | "Rust CLI 参数解析用 clap derive，subcommand 枚举" | 工具链 | cli-tool | 3 |
| m5 | "认证 token 过期后用 refresh_token 自动刷新，别让用户重新登录" | bug 修复 | web-app | 4 |
| m6 | "WASM 扩展用 wasmtime 3.x，host functions 加 extension_ 前缀" | 架构 | ion | 3 |
| m7 | "认证 key 不要硬编码，用 auth.json（权限 600）" | 安全 | ion | 5 |
| m8 | "React 列表渲染必须加 key prop，不然 diff 出 bug" | bug 修复 | web-app | 4 |
| m9 | "git commit 不要带 --no-verify，测试必须跑" | 规范 | ion | 2 |
| m10 | "测试用 FauxProvider 驱动，不调真实 LLM，确定性" | 测试 | ion | 3 |

---

### Group A：存了能找到（最核心链路）

> **用户场景**：用户之前说过"记住认证用 DeepSeek"，现在换了个项目遇到认证问题，问"认证怎么配置"——Memory 应该自动把之前存的找出来。

```bash
# A1 精确命中：用户问"认证 API key 在哪"→ 自动注入 m1 + m7
#   预期：on_context 注入的 <global_memory> 包含 m1（DeepSeek API key）和 m7（auth.json 权限 600）
#   不包含 m3/m4/m8（React/Rust/git 无关）
INPUT='认证 API key 在哪'
# 验证注入内容含 "DeepSeek" 和 "auth.json"

# A2 模糊命中：用户问"登录过期怎么办"→ 命中 m5（token 刷新）
#   "登录过期"跟"token 过期"不是完全匹配，但语义相关
#   预期：FTS5 能匹配到 m5
INPUT='用户登录过期了怎么办'
# 验证注入内容含 "refresh_token"

# A3 多条相关：用户问"ion 项目的架构决策"→ 命中 m1/m2/m6/m7/m9（都是 ion 项目）
#   预期：搜出 5 条，但注入最多 5 条（take(5) 限制），importance 高的优先
#   m1(5) + m7(5) 应在 m9(2) 前面
INPUT='ion 项目的架构决策有哪些'
# 验证：注入条数 <= 5，m1 和 m7 排前面

# A4 不误触发：用户问"今天写个 hello world"→ 不应注入任何记忆
#   "hello world" 跟所有记忆都不相关
#   预期：on_context 不注入 <global_memory>（搜索结果为空）
INPUT='帮我写个 hello world'
# 验证：无 <global_memory> 注入

# A5 重复问不重复注入：同一轮里 on_input 搜了，on_context 注入了
#   下一轮用户追问"那 base_url 是什么"→ 应注入 m1（如果还没被去重窗口跳过）
#   但如果 m1 上一轮已注入（hash 在 injected 窗口里），跳过
INPUT='那 base_url 是什么'
# 验证：要么注入 m1（新 hash），要么跳过（hash 去重），不重复注入
```

**验证点**：
- ✅ 相关问题命中正确记忆（A1/A2）
- ✅ 多条结果按 importance 排序 + 上限 5 条（A3）
- ✅ 无关问题不误注入（A4）
- ✅ 去重窗口生效（A5）

---

### Group B：跨项目回忆（V0.2 核心价值）

> **用户场景**：用户在项目 web-app 遇到认证问题，之前在项目 ion 存过认证方案——Memory 应该跨项目找出来。

```bash
# B1 跨项目召回：在 web-app 项目里，用户问"认证怎么做"
#   全局库里有 ion 项目的 m1 + web-app 的 m5
#   预期：两个都搜到（不限定 project），注入时标注来源项目
INPUT='认证怎么做'
# 验证：注入内容含 ion 的 m1 和 web-app 的 m5

# B2 项目过滤：用户说"只看本项目的"
#   通过 global_memory_search(query, project="web-app") 只搜 web-app 的
#   预期：只返回 m3/m5/m8，不返回 ion 的
RESULT=$(ion rpc ... --method extension_rpc --params \
  '{"extension":"global-memory","method":"search","params":{"query":"认证","project":"web-app"}}')
# 验证：结果只有 project=web-app 的条目

# B3 大纲感知：用户第一次进项目，on_system_prompt 注入 <global_memory_outline>
#   预期：outline 列出所有项目的摘要，让 LLM 知道有哪些记忆可用
#   验证：system prompt 含 <global_memory_outline>，列出 ion/web-app/cli-tool
```

**验证点**：
- ✅ 跨项目搜索正常（B1）
- ✅ 项目过滤生效（B2）
- ✅ 大纲索引注入让 LLM 感知全局记忆（B3）

---

### Group C：不卡用户（延迟）

> **用户场景**：用户每说一句话，Memory 都要搜一遍全局库——如果慢了，用户会感觉"每句话都要卡一下"。

```bash
# C1 日常延迟：10 条记忆（测试数据），用户输入 → 全局检索
#   预期：search_ms < 5ms（FTS5 索引查询）
#   测量：on_input 里记录 search_ms（需代码加计时）

# C2 中规模：1000 条记忆（批量存入后）
INPUT='认证'
#   预期：search_ms < 20ms

# C3 大规模：5000 条记忆
INPUT='认证'
#   预期：search_ms < 50ms（人类感知阈值 100ms 以下）

# C4 注入链路总延迟：on_input(search) + on_context(format+inject)
#   预期：总增量 < 10ms（不含 LLM 调用时间）
```

**验证点**：
- ✅ 10 条 < 5ms，1000 条 < 20ms，5000 条 < 50ms
- ✅ 用户无感知

---

### Group D：不撑爆上下文（Token 开销）

> **用户场景**：用户存了上百条记忆，每次对话都注入——如果注入太多，上下文窗口被记忆占满了，LLM 没空间处理用户实际任务。

```bash
# D1 单次注入上限：搜出 10 条相关记忆，只注入 5 条
#   预期：注入条数 <= 5（take(5) 硬限制）
#   验证：<global_memory> 块最多 5 条记忆

# D2 注入 token 数：每条记忆约 50-200 字，5 条约 250-1000 字
#   预期：注入 token < 500（约 350 中文字）
#   测量：统计 <global_memory>...</global_memory> 的字符数

# D3 大纲 token 数：5 个项目的 outline
#   预期：<global_memory_outline> token < 200（每项目一行摘要）
#   验证：outline 每行 < 40 字

# D4 多轮累计：连续 5 轮对话
#   预期：累计注入 < 2000 token（去重窗口生效，同一条不重复注入）
#   如果第 1 轮注入了 m1，第 3 轮再搜到 m1 → 跳过（20 轮去重窗口内）
```

**验证点**：
- ✅ 最多注入 5 条（D1）
- ✅ 单次 < 500 token（D2）
- ✅ 大纲 < 200 token（D3）
- ✅ 多轮去重，累计可控（D4）

---

### Group E：自动整理（保持记忆库干净）

> **用户场景**：用户用了几个月，存了上百条记忆，很多重复的、过时的——Memory 应该自动清理，保持搜索结果干净。

```bash
# E1 去重：用户三次说了"认证用 DeepSeek"（LLM 调了三次 memory_save）
#   consolidate 后：保留 importance 最高的 1 条，其余 2 条 archived
#   验证：active 条数 3 → 1

# E2 归档低价值：importance=0 且超过 30 天没更新的
#   consolidate 后：archived=1
#   验证：active 条数减少

# E3 整理后搜索更干净：整理前搜"认证"出 5 条（3 条重复），整理后出 2 条
#   验证：搜索结果条数减少，精度提升（没有重复内容）

# E4 整理不误删高价值：importance=5 的记忆即使超过 30 天也不归档
#   验证：高 importance 记忆保留

# E5 整理幂等：连续跑两次 consolidate
#   验证：第二次 deduplicated=0, archived=0（已经整理过了）
```

**验证点**：
- ✅ 完全重复去重（E1）
- ✅ 低价值过期归档（E2）
- ✅ 整理后搜索更精（E3）
- ✅ 高价值不误删（E4）
- ✅ 幂等不重复整理（E5）

---

### Group F：边界安全（不崩溃）

> **用户场景**：各种极端输入和状态。

```bash
# F1 空库：全局库为空时 on_input 搜 → 返回空，不 panic
# F2 超长输入：用户输入 10000 字 → FTS5 query 截断或正常处理，不报错
# F3 特殊字符：记忆内容含 < > & " → 注入的 <global_memory> 块 XML 转义
# F4 并发写入：两个 Worker 同时 save → SQLite Mutex 不死锁
# F5 搜索结果为空时的注入：搜索返回空 → on_context 不注入（不插入空 <global_memory> 块）
```

**验证点**：
- ✅ 空库/超长/特殊字符/并发/空结果 都不崩溃

---

### 验证脚本结构

```
tests/memory_active_ci.sh（共 28 case）
├── 准备：存入 10 条测试数据（m1-m10，模拟多项目场景）
├── Group A：存了能找到（5 case）— 精确/模糊/多条/不误触发/去重
├── Group B：跨项目回忆（3 case）— 跨项目召回/项目过滤/大纲感知
├── Group C：不卡用户（4 case）— 10/1000/5000 条延迟 + 总链路
├── Group D：不撑爆上下文（4 case）— 上限/token/大纲/累计去重
├── Group E：自动整理（5 case）— 去重/归档/精度/不误删/幂等
├── Group F：边界安全（5 case）— 空库/超长/特殊字符/并发/空结果
└── 清理：删除测试数据
```

### 实现时的指标输出要求

Group C（延迟）和 Group D（token）需要代码里加可测量指标：

```rust
// on_input 里
let t0 = std::time::Instant::now();
let results = global_store.search(&text, None)?;
let search_ms = t0.elapsed().as_millis();
tracing::info!("[memory] global search: {search_ms}ms, {} hits", results.len());
// 同时通过 emit_extension_event 暴露给 CI
self.emit("memory_search_stat", json!({"search_ms": search_ms, "hits": results.len()}));

// on_context 里
let inject_chars: usize = inject_text.chars().count();
tracing::info!("[memory] global inject: {} chars (~{} tokens)", inject_chars, inject_chars / 2);
self.emit("memory_inject_stat", json!({"chars": inject_chars, "entries": count}));
```

CI 脚本通过 `ion subscribe` 捕获 `memory_search_stat` / `memory_inject_stat` 事件，断言延迟和 token 指标。

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
