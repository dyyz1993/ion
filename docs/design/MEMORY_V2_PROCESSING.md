# Memory V0.2 会话加工 — SessionEnd LLM 提炼 设计文档

> **状态：待定** — 会话结束后自动用 LLM 提炼精华存到全局库，替代当前"原样存"。
>
> 配套：[MEMORY_AGENT.md](./MEMORY_AGENT.md)（V0.2 基础设施）、[MEMORY_ACTIVE.md](./MEMORY_ACTIVE.md)（被动注入+整理）、[MEMORY_EXTENSION.md](./MEMORY_EXTENSION.md)（V0.1）

---

## 何时使用这个文档

- 想让 Memory 自动加工（不是原样存）
- 想让会话结束后自动提炼精华
- 想给后续的向量检索 / 知识图谱铺路（entities 字段）

**前置阅读**：[MEMORY_AGENT.md](./MEMORY_AGENT.md)、[MEMORY_ACTIVE.md](./MEMORY_ACTIVE.md)

---

## 1. 整体架构

```
                    内核（提供能力，不内化业务）
                    ├── Extension trait: on_session_shutdown 钩子
                    ├── Runtime: spawn_worker（后台跑加工 agent）
                    ├── 工具: read（读会话 JSONL）/ global_memory_save（存记忆）
                    ├── hooks: SessionEnd 事件
                    └── emit_extension_event（加工完通知）

                              ↑ 内核提供这些能力

                    V0.2 GlobalMemoryExtension（扩展，可开关）
                    ├── on_session_shutdown: 触发加工
                    ├── 加工 Pipeline: 4 步
                    ├── config.json: extensions.global-memory.enabled
                    └── 纯扩展逻辑，不进内核
```

**原则**：
- 用户想开启就开启（`config.json` 开关）
- 以扩展形式介入（GlobalMemoryExtension，不是内核硬编码）
- 内核只提供能力（钩子 + 工具 + spawn），不提供策略（加工逻辑在扩展里）

## 2. V0.1 vs V0.2 分工

| | V0.1 MemoryExtension | V0.2 GlobalMemoryExtension |
|---|---|---|
| **角色** | 即时记忆（当前会话上下文注入） | 长期记忆（跨会话加工 + 检索） |
| **触发** | on_input → on_context（每轮实时） | SessionEnd（会话结束后，非实时） |
| **存什么** | 原文（LLM 自己调 memory_save） | LLM 提炼的精华（加工后的结构化信息） |
| **加工** | 无 | 有（LLM 提炼 + 去重） |
| **范围** | 当前 session / project | 跨 session / 跨 project |
| **开关** | `extensions.memory.enabled` | `extensions.global-memory.enabled` |

**不冲突，互补**：V0.1 管当前对话的即时记忆，V0.2 管长期记忆。用户可以只用 V0.1（关掉 V0.2），也可以只用 V0.2（关掉 V0.1），也可以都用。

## 3. 触发时机

### SessionEnd — 会话关闭时

`on_session_shutdown` 在会话真正结束时触发（agent_loop.rs:560，reason="quit"）。一次会话只触发一次。

**为什么不每轮加工**：每轮调 LLM 花 ~20K token，太贵。会话结束时才有完整上下文，加工质量更好。

**为什么不实时加工**：实时加工干扰用户（后台跑 agent 占资源），延迟大（2-10s）。

### 异步执行（不阻塞退出）

`on_session_shutdown` 是同步钩子——直接加工会卡 2-10s。解法：**异步 spawn 后台 Worker**：

```rust
async fn on_session_shutdown(&self, ctx: &SessionContext) -> AgentResult<()> {
    // 异步 spawn 后台加工 Worker，不阻塞会话退出
    let session_id = self.session_id.clone();
    let session_dir = self.storage.session_dir().to_string_lossy().to_string();
    tokio::spawn(async move {
        if let Err(e) = run_memory_processing(&session_id, &session_dir).await {
            tracing::warn!("[memory-v2] processing failed: {e}");
        }
    });
    Ok(())  // 立即返回，不等待加工
}
```

**注意**：异步 spawn 要求进程不立即退出。场景 1（直接执行）进程跑完即退，后台任务会被干掉。**只适合场景 2（--host）和场景 3（serve）**。

### 触发频率

| 场景 | 触发 | 加工 | 适合 |
|------|------|------|------|
| 场景 1（`ion "msg"`） | SessionEnd | ❌ 进程退出后台任务被杀 | 不做加工 |
| 场景 2（`ion --host`） | SessionEnd | ✅ host 兜着 | 做 |
| 场景 3（`ion serve`） | SessionEnd | ✅ 常驻 | 最适合 |

## 4. 加工 Pipeline（4 步）

### Step 1：读取会话内容

```rust
fn read_session_messages(session_dir: &str) -> Vec<Message> {
    let jsonl_path = format!("{session_dir}/session.jsonl");
    let content = std::fs::read_to_string(&jsonl_path).unwrap_or_default();
    // 逐行解析 JSONL
    let messages: Vec<Message> = content.lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    // 限制：超过 200 条取最近 200 条
    messages.into_iter().rev().take(200).collect::<Vec<_>>().into_iter().rev().collect()
}
```

| 指标 | 值 |
|------|-----|
| LLM | 不需要 |
| 时间复杂度 | O(N)，N = 消息条数 |
| 空间复杂度 | O(N)，全部加载到内存 |
| 耗时 | ~1ms（文件 IO） |
| 限制 | 超过 200 条截断取最近 |

### Step 2：LLM 提炼（核心步骤）

```rust
async fn llm_extract_memories(
    messages: &[Message],
    registry: &ApiRegistry,
    model: &Model,
) -> Result<Vec<ExtractedMemory>, String> {
    // 把消息列表转成文本
    let conversation_text = messages.iter()
        .map(|m| format!("{}: {}", m.role(), m.text_content()))
        .collect::<Vec<_>>()
        .join("\n");

    let system_prompt = r#"你是记忆加工 Agent。从会话中提取值得长期记住的关键信息。

规则：
1. 只提取"跨会话有价值"的信息（设计决策/用户偏好/bug修复方案/重要配置/架构选择）
2. 忽略闲聊、问候、临时调试、无意义内容
3. 每条记忆不超过 100 字
4. 最多提取 5 条
5. 输出 JSON 数组

输出格式：
[{"content":"精简内容","category":"设计决策|用户偏好|bug修复|配置|架构|其他","importance":1-5,"entities":["关键概念"]}]

如果会话内容不值得记住，返回空数组 []"#;

    let context = Context {
        system_prompt: Some(system_prompt.into()),
        messages: vec![Message::user(format!("会话内容：\n{conversation_text}"))],
        tools: None,
    };

    let response = complete(registry, model, &context, None).await?;
    // 解析 LLM 返回的 JSON
    let extracted: Vec<ExtractedMemory> = serde_json::from_str(&response.text_content())
        .unwrap_or_default();
    Ok(extracted)
}
```

| 指标 | 值 |
|------|-----|
| LLM | **需要（1 次调用）** |
| 时间复杂度 | O(1)（1 次 API 调用） |
| 空间复杂度 | O(1)（输入 + 输出 token） |
| 耗时 | 2-10s（取决于 LLM 响应） |
| Token 输入 | ~20K（200 条消息） |
| Token 输出 | ~500（5 条提炼） |
| 总成本 | ~20.5K token/次会话 |

### Step 3：去重检查

```rust
fn dedup_memories(
    extracted: &[ExtractedMemory],
    store: &GlobalMemoryStore,
) -> Vec<ExtractedMemory> {
    extracted.iter()
        .filter(|m| {
            let hash = content_hash(&m.content);
            // 查全局库是否已有相同 content 的记忆
            !store.has_content_hash(&hash).unwrap_or(false)
        })
        .cloned()
        .collect()
}
```

| 指标 | 值 |
|------|-----|
| LLM | 不需要 |
| 时间复杂度 | O(K)，K = 提炼条数（通常 3-5） |
| 空间复杂度 | O(K) |
| 耗时 | ~1ms（K 次 SQLite 查询） |

### Step 4：写入全局库

```rust
fn save_to_global_store(
    memories: &[ExtractedMemory],
    store: &GlobalMemoryStore,
    project: &str,
    session_id: &str,
) -> usize {
    let mut saved = 0;
    for m in memories {
        let _ = store.save(
            &m.content,
            &m.category,
            &m.entities.join(","),
            project,
            m.importance,
        );
        saved += 1;
    }
    saved
}
```

| 指标 | 值 |
|------|-----|
| LLM | 不需要 |
| 时间复杂度 | O(K)（K 条 INSERT） |
| 空间复杂度 | O(K)（磁盘存储） |
| 耗时 | ~1ms |

### 总开销

| 指标 | 值 |
|------|-----|
| LLM 调用次数 | **1 次**（只有 Step 2） |
| 总耗时 | **2-10s**（99% 是 LLM 等待） |
| Token 成本 | **~20.5K/次会话** |
| 时间复杂度 | O(N) + O(1) |
| 空间复杂度 | O(N) |
| 触发频率 | 每次会话结束（不是每轮） |

## 5. 数据结构

```rust
/// LLM 加工后提取的记忆条目
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExtractedMemory {
    /// 精简后的记忆内容（≤100 字）
    pub content: String,
    /// 类别：设计决策 / 用户偏好 / bug修复 / 配置 / 架构 / 其他
    pub category: String,
    /// 重要性 1-5
    pub importance: i32,
    /// 涉及的关键概念（为后续向量检索 / 知识图谱铺路）
    pub entities: Vec<String>,
}
```

`entities` 字段存储到 GlobalMemoryStore 的 `tags` 列（已有字段，复用），逗号分隔。

## 6. 实现方式

### 方式 A：在 GlobalMemoryExtension 的 on_session_shutdown 里直接调 LLM

```rust
impl Extension for GlobalMemoryExtension {
    // ...
    async fn on_session_shutdown(&self, ctx: &SessionContext) -> AgentResult<()> {
        // 读会话 → 调 LLM → 去重 → 存库
        // 需要 registry + model（像 prompt handler 一样注入）
    }
}
```

**优点**：简单直接。
**缺点**：需要给 GlobalMemoryExtension 注入 registry + model。

### 方式 B：用 hooks 配置触发 agent handler（零代码）

```json
// ~/.ion/hooks.json 或 <project>/.ion/hooks.json
{
  "SessionEnd": [{
    "hooks": [{
      "type": "agent",
      "agent": "memory-processor",
      "prompt": "读取当前会话的 session.jsonl，提取 3-5 条值得记住的关键信息，用 global_memory_save 存到全局库。忽略闲聊。每条不超过 100 字。",
      "model": "fast",
      "max_turns": 10,
      "allowed_tools": ["read", "global_memory_save"],
      "timeout": 60
    }]
  }]
}
```

**优点**：零代码，纯配置，用户可自定义 prompt。
**缺点**：依赖 hooks 系统 + agent handler + SessionEnd 事件映射到 hooks。

**推荐方式 B**——因为：
- 零内核改动（hooks SessionEnd 已有，agent handler 已有）
- 用户可自定义加工逻辑（改 prompt 就行）
- 符合"内核提供能力，扩展提供策略"原则

但需要一个前置——**hooks 的 SessionEnd 事件当前没映射到 HookExtension**（只在 GlobalMemoryExtension 的 on_session_shutdown 里）。如果用方式 B，需要确认 hooks 的 SessionEnd 映射是否生效。如果没有，方式 A 更稳妥。

### 方式 A 的改动

| 文件 | 改动 | 行数 |
|------|------|------|
| `src/global_memory_ext.rs` | on_session_shutdown 加加工逻辑 + 注入 registry/model | ~80 |
| `src/global_memory.rs` | has_content_hash() 方法 | ~10 |
| `src/bin/ion.rs` | GlobalMemoryExtension 注册时加 config 开关 + 注入 registry/model | ~10 |
| `src/bin/ion_worker.rs` | MemoryExtension 构造时传 session_dir（给 SessionEnd 用） | ~5 |
| `tests/memory_v2_processing_ci.sh` | CLI 测试（FauxProvider 驱动） | ~80 |
| **总计** | | **~185** |

## 7. 配置

```json
{
  "extensions": {
    "global-memory": {
      "enabled": true,             // 总开关
      "processing": {
        "enabled": true,            // 加工开关（可单独关）
        "model": "fast",            // 加工用什么模型（省 token）
        "max_memories": 5,          // 每次最多提取几条
        "max_messages": 200         // 最多读几条消息
      }
    }
  }
}
```

用户不想自动加工？关掉 `processing.enabled` 就行——退回到"LLM 自己调 memory_save"的原样存模式。

## 8. CLI 测试方案

### Group A：加工链路（FauxProvider 驱动）

```bash
# 准备一个有内容的会话 JSONL
# A1 SessionEnd 触发加工 → FauxProvider 返回提取的 JSON → 存到全局库
# 验证：全局库有新条目，content 是 FauxProvider 返回的提炼内容（不是原文）

# A2 加工后去重：同一条会话退出两次
# 验证：第二次加工不产生重复条目（content hash 去重）

# A3 无价值会话不加工：纯闲聊会话
# FauxProvider 返回空数组 []
# 验证：全局库没新增条目
```

### Group B：配置开关

```bash
# B1 processing.enabled=false → SessionEnd 不触发加工
# 验证：全局库无新条目

# B2 extensions.global-memory.enabled=false → 整个 V0.2 关掉
# 验证：global_memory_search 工具不可用
```

### Group C：边界

```bash
# C1 空会话（无消息）→ 不崩溃，不加工
# C2 超长会话（500 条消息）→ 截断取最近 200 条
# C3 LLM 返回非法 JSON → 不崩溃，跳过加工
```

## 9. 并行开发注意事项

- 改动集中在 `src/global_memory_ext.rs`（加工逻辑）+ `src/global_memory.rs`（has_content_hash）+ `src/bin/ion.rs`（config 开关 + 注入）
- 不改 Extension trait、不改 hooks、不改 ion-provider
- 与其他功能的文件不重叠
- 测试用 FauxProvider 驱动（不调真实 LLM）

## 10. 后续路径

| 步骤 | 内容 | 本文档 | 后续 |
|------|------|--------|------|
| Step 1 | 会话结束后 LLM 加工 | ✅ 本文档 | — |
| Step 2 | 向量语义检索（embedding） | — | [MEMORY_V2_VECTOR.md]（待写） |
| Step 3 | 知识图谱（entities → graph） | — | [MEMORY_V2_GRAPH.md]（待写） |

Step 1 的 `entities` 字段为 Step 3 铺路——加工时 LLM 已经提取了实体，后续 Graph 只是把这些实体连起来。

## 11. 与当前方案对比

| 维度 | 当前方案 | 本文档后 |
|------|---------|---------|
| 触发 | LLM 自己调 memory_save（随机） | SessionEnd 自动触发（确定） |
| 存什么 | 原文 | LLM 提炼的精华（≤100 字） |
| 重复控制 | 无 | content hash 去重 |
| 实体信息 | 无 | entities 字段（为 Graph 铺路） |
| 用户感知 | LLM 自己决定存不存 | 用户无感（后台自动） |
| 成本 | 零 | ~20.5K token/次会话 |
| 开关 | 无 | config.json processing.enabled |
