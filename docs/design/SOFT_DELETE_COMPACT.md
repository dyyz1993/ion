# 软删除与软压缩设计文档

> **状态：已实现** — deletion + segment_summary + restoration + 双层过滤 + LLM 摘要 + on_entries_invalidated 钩子全部实现,6 E2E 测试通过。
>
> 覆盖：deletion entry / segment_summary entry / 双层过滤 / LLM context 构建 / token 统计 / 钩子连锁。

---

## 一、问题背景

当前 ION 的消息可见性过滤**只在拉取层**（`message_retrieval.rs::apply_visibility_filter`）做了预留，**LLM context 层完全没有任何过滤**。这意味着：

- 即便加了 deletion entry，被删的消息仍留在 `self.messages` 里，**仍然会进 LLM context**
- segment_summary 的折叠替换在拉取层都没实现（只有注释），更别说 context 层
- token 统计（`total_tokens` / `needs_compact`）会误算被删消息的 token

**软删除和软压缩必须同时影响两层**：拉取层（用户看到的）+ LLM context 层（模型看到的）。

---

## 二、核心概念区分

### 2.1 三种"让消息消失"的机制

| 机制 | 触发 | 效果 | 产物 |
|------|------|------|------|
| **compaction（硬压缩）** | 自动（token 超阈值） | 整段早期消息**硬截断**，替换成一条 CompactionSummary | 内存数组被替换，旧消息丢失 |
| **segment_summary（软压缩/手动折叠）** | 手动（用户/AI 选中一批） | 选中的一批消息**替换成一条 BranchSummary**（摘要），原文在 JSONL 留痕 | 追加 SegmentSummaryEntry，内存数组替换 |
| **deletion（软删除）** | 手动（用户删某条） | 被删的消息**直接排除**（不替换成任何东西），原文在 JSONL 留痕 | 追加 DeletionEntry，内存数组移除 |

### 2.2 为什么"替换成 BranchSummary"而不是"变空"或"移除"

| 方案 | 问题 |
|------|------|
| 内容变空 | LLM 看到空消息会困惑（"为什么有条空消息"），浪费一轮理解 |
| 直接移除 | LLM 丢失这段对话的上下文衔接（前后消息突兀断裂） |
| **替换成 BranchSummary** ✅ | LLM 看到一段摘要，知道"这里有一段对话被折叠了，内容如下"，上下文完整 |

**关键**：折叠产物复用 `Message::BranchSummary`（`ion-provider/src/types.rs:182`），它已有四个 provider 的渲染逻辑（openai/anthropic/google/openai_responses），不需要新增渲染代码。

### 2.3 为什么不替换成 tool_call / tool_result

tool_call 有特殊语义——provider 会期待对应的 tool_result 存在。如果折叠段里有 tool_call 但折叠后只剩摘要，provider 可能报"orphan tool call"错误。BranchSummary 渲染成纯文本 `role:"user"`，没有这种约束。

---

## 三、数据结构

### 3.1 DeletionEntry（软删除标记）

```json
{
  "type": "deletion",
  "id": "del_001",
  "parentId": null,
  "timestamp": "2026-07-10T10:00:00Z",
  "targetIds": ["msg_014", "msg_015"],
  "reason": "用户手动删除（错误输出）"
}
```

| 字段 | 说明 |
|------|------|
| `targetIds` | 被删除的 entry id 列表 |
| `reason` | 删除原因（可选，审计用） |

### 3.2 SegmentSummaryEntry（软压缩/折叠标记）

```json
{
  "type": "segment_summary",
  "id": "ss_001",
  "parentId": null,
  "timestamp": "2026-07-10T10:00:00Z",
  "targetIds": ["msg_010", "msg_011", "msg_012", "msg_013", "msg_014"],
  "summary": "用户讨论了游标分页方案的设计，Agent 读了 rpc.rs 和 session_jsonl.rs，提出了 View × Granularity 正交矩阵",
  "summaryEntryId": "bs_001"
}
```

| 字段 | 说明 |
|------|------|
| `targetIds` | 被折叠的 entry id 列表（这一段消息） |
| `summary` | LLM 生成的摘要文本 |
| `summaryEntryId` | 替换后的 BranchSummary entry 的 id（在内存数组里的位置） |

---

## 四、双层过滤架构（含钩子层）

### 4.0 完整四层架构图（对齐 pi buildSessionContext + transformContext 管线）

**核心原则（pi 验证）**：可见性过滤是"源头过滤"（session 层一次性完成），扩展注入是"出口注入"（每次发请求时在干净消息上 append）。扩展注入一定在过滤之后。

```
JSONL 文件（append-only，所有 entry 都在）
    │
    ▼
SessionFile::load / load_entries_cached
    │
    ▼
┌─────────────────────────────────────────────────────────────┐
│ 第 1 层：视点过滤（apply_view_filter）                        │
│ live / since_compaction / branch:<leaf> / full               │
│ → 决定"从哪个分支、从哪个点开始看"                             │
│ 对齐 pi: buildSessionContext 里 leaf 解析 + getBranch path   │
└────────────────────────────┬────────────────────────────────┘
                             │
                             ▼
                    ┌─────────────────────────┐
                    │ 🔧 钩子: on_view_filtered │  ← 通知型：视点切换
                    └────────────┬────────────┘
                                 │
                                 ▼
┌─────────────────────────────────────────────────────────────┐
│ 第 2 层：可见性过滤（apply_visibility_filter）                 │
│ deletion: 排除 targetIds                                      │
│ segment_summary: 替换成 BranchSummary                         │
│ compaction: 从 firstKeptEntryId 截断                          │
│ → 决定"哪些消息可见"                                          │
│ 对齐 pi: buildSessionContext L469-561 收集 deletedIds +      │
│        segmentTargets + strippedToolCallIds，appendMessage 过滤│
│ ★ 这一步产出"干净的 messages"（self.messages 或 snapshot）    │
└────────────────────────────┬────────────────────────────────┘
                             │
                             ▼
                    ┌──────────────────────────────┐
                    │ 🔧 钩子: on_entries_invalidated │ ← 通知型：消息数组变了
                    └────────────┬─────────────────┘
                                 │
                    ┌────────────┴─────────────┐
                    │                          │
                    ▼                          ▼
          ══════════════════          ═══════════════════════════
          拉取层（用户看到的）          LLM context 层（模型看到的）
          ══════════════════          ═══════════════════════════
                    │                          │
                    │               ┌──────────┴───────────┐
                    │               │ ★ 拿的是已过滤的干净消息 │
                    │               └──────────┬───────────┘
                    │                          ▼
                    │              ┌─────────────────────────────┐
                    │              │ 🔧 钩子: on_context          │ ← 修改型
                    │              │ （扩展在干净消息上 append 注入）│   记忆注入/系统提示
                    │              │ 对齐 pi: transformContext    │   注入的是临时的、
                    │              │ = emitContext（每次发请求时）  │   不持久化到 session
                    │              └──────────┬──────────────────┘
                    │                         │
                    ▼                         ▼
          get_messages / list_turns  ┌──────────────────────┐
          （直接返回给前端）           │ transform_messages   │ ← 跨 provider 规范化
                                     └──────────┬───────────┘
                                                │
                                                ▼
                                      provider（LLM API 调用）
                                      对齐 pi: streamFunction
```

### 4.0.1 为什么要"过滤在前、注入在后"

对齐 pi 的设计理由（经源码验证）：

1. **注入的消息是临时的**——记忆注入（`<memory_context>`）不持久化到 session tree，每次发请求时动态加。如果注入在过滤前，注入的消息可能被过滤掉（逻辑错误）。
2. **过滤是静态的**——deletion/segment_summary 是 session tree 里的持久化决策（用户删了就是删了），不随 turn 变化。
3. **注入是动态的**——每个 turn 的记忆检索结果可能不同，需要在干净的 base 上 append。
4. **token 统计正确**——on_context 注入的临时消息不应算入 compaction 的 token 判定（它们不持久化），过滤在前保证了 compaction 看的是持久化的干净消息。

### 4.0.2 钩子的两种用途

| 用途 | 钩子 | 做什么 | 对齐 pi |
|------|------|--------|---------|
| **通知型**（扩展感知变化，不改 messages）| on_view_filtered / on_entries_invalidated | 扩展重建索引/缓存 | pi 无等价（pi 用事件总线，但语义类似） |
| **修改型**（扩展直接改 messages）| on_context / on_session_compact | 扩展 append 注入（记忆等）| pi 的 `context` 事件 = transformContext |

### 4.0.3 四层钩子清单

| 层级 | 过滤内容 | 后续钩子 | 钩子类型 | pi 对齐 |
|------|---------|---------|---------|---------|
| 第 1 层 视点过滤 | view（live/branch/compaction）| **on_view_filtered**（新增） | 通知型 | buildSessionContext leaf 解析 |
| 第 2 层 可见性过滤 | deletion + segment_summary | **on_entries_invalidated**（已定义未调用） | 通知型 | buildSessionContext 过滤 |
| 第 3 层 context 分流 | — | **on_context**（已有，位置修正） | 修改型 | transformContext = emitContext |
| 第 4 层 transform | 跨 provider 规范化 | 无（纯转换） | — | convertToLlm |

### 4.1 拉取层过滤（已有，需补完）

`message_retrieval.rs::apply_visibility_filter`：

```rust
fn apply_visibility_filter(entries: &[Value]) -> Vec<Value> {
    // 1. 收集所有 deletion 的 targetIds
    let deleted_ids: HashSet<String> = ...;

    // 2. 收集所有 segment_summary 的 targetIds → summary 映射
    let segment_map: HashMap<String, (String, String)> = ...; // firstTargetId → (summary, summaryEntryId)

    // 3. 过滤 + 替换
    entries.iter().filter_map(|e| {
        let id = e["id"].as_str()?;
        if deleted_ids.contains(id) { return None; }           // deletion: 排除
        if let Some((summary, _)) = segment_map.get(id) {      // segment_summary: 首个 target 替换
            return Some(make_branch_summary_entry(summary));   // 其余 target 在下面跳过
        }
        if is_segment_target(id, &segment_map) { return None; } // 其余 target: 跳过
        Some(e.clone())
    }).collect()
}
```

### 4.2 LLM context 层过滤（新增，关键缺口）

在 `agent_loop.rs:444` 拍 snapshot 后、transform_messages 前加：

```rust
// agent_loop.rs inner_loop，原 line 444
let mut messages_snapshot = self.messages.clone();

// 新增：内存可见性过滤（排除被删除的、替换被折叠的）
messages_snapshot = self.apply_context_visibility_filter(messages_snapshot);

let transformed_messages = transform_messages(messages_snapshot, ...);
```

**内存版过滤函数**（从 JSONL entries 的 deletion/segment_summary 元标记推导）：

```rust
impl Agent {
    fn apply_context_visibility_filter(&self, messages: Vec<Message>) -> Vec<Message> {
        // 从 session JSONL 读 deletion/segment_summary entry
        // （或从 Agent 内部维护的 soft_delete_state 推导）
        let deleted_ids = self.soft_delete_state.deleted_ids.clone();
        let segment_map = self.soft_delete_state.segment_map.clone();

        messages.into_iter().filter_map(|m| {
            let id = self.message_id(&m)?;  // 内存 Message 需要能查到 entryId
            if deleted_ids.contains(&id) { return None; }
            if let Some(summary) = segment_map.get(&id) {
                return Some(Message::BranchSummary(BranchSummaryMessage {
                    role: "branchSummary".into(),
                    summary: summary.clone(),
                    from_id: id,
                    timestamp: 0,
                }));
            }
            if is_segment_extra_target(&id, &segment_map) { return None; }
            Some(m)
        }).collect()
    }
}
```

**关键问题**：内存 `Vec<Message>` 没有 entryId（entryId 是落盘时 `message_to_entry` 生成的）。需要：
- **方案 A**：Agent 维护一个 `message_id_map: HashMap<usize, String>`（数组索引 → entryId），落盘时同步更新
- **方案 B**：Agent 维护 `soft_delete_state`（deleted_ids + segment_map），在内存消息数组上按索引操作

推荐 **方案 B**——更简单，不需要给每条 Message 打 entryId。

---

## 五、操作流程

### 5.1 软删除流程（delete_entries RPC）

```
用户调用 delete_entries(targetIds: ["msg_014", "msg_015"])
    │
    ▼
1. 追加 DeletionEntry 到 JSONL（append_deletion）
    │
    ▼
2. 更新 Agent.soft_delete_state（deleted_ids 加入 targetIds）
    │
    ▼
3. 从 self.messages 移除对应的消息
    │
    ▼
4. 触发 on_entries_invalidated 钩子（通知扩展）
    │
    ▼
下一轮 LLM 调用：
    messages_snapshot 已不含被删消息
    → LLM 看不到它们
    → token 统计自动正确
```

### 5.2 软压缩流程（summarize_entries RPC）

```
用户调用 summarize_entries(targetIds: ["msg_010"~"msg_014"], summary?: "可选自定义摘要")
    │
    ├─ summary 未提供 → 调 LLM 生成摘要（用 compact_model 或主模型）
    │
    ▼
1. 追加 SegmentSummaryEntry 到 JSONL（append_segment_summary）
    │
    ▼
2. 更新 Agent.soft_delete_state（segment_map: firstTarget → summary）
    │
    ▼
3. 在 self.messages 里，把 targetIds 对应的消息替换成一条 Message::BranchSummary
    │  （第一个 target 的位置放 BranchSummary，其余移除）
    │
    ▼
4. 触发 on_entries_invalidated 钩子
    │
    ▼
下一轮 LLM 调用：
    messages_snapshot 含一条 BranchSummary（而不是原来的 5 条）
    → LLM 看到摘要 + 前后文衔接完整
    → token 减少（1 条摘要 << 5 条原文）
```

---

## 六、token 统计连锁

### 6.1 为什么"从内存数组移除/替换"是对的

| 统计函数 | 数据来源 | 软删除/折叠后 |
|---------|---------|-------------|
| `total_tokens`（compact.rs:138）| `self.messages` 遍历 | ✅ 自动正确（被删/折叠的已从数组移除） |
| `needs_compact`（compact.rs:142）| `total_tokens > threshold` | ✅ 自动正确 |
| `get_session_stats`（ion_worker.rs:537）| `agent.messages()` | ✅ totalMessages 减少 |
| `get_session_stats` tokens | `Message::Assistant(a).usage` 求和 | ⚠️ 累计 token 不变（usage 是历史值，不受删除影响）|

**结论**：只要在内存数组层做移除/替换（不是只在 snapshot 层），所有 token 统计自动正确。

### 6.2 get_session_stats 的两种 token 语义

| 字段 | 语义 | 软删除后 |
|------|------|---------|
| `tokens.input/output` | **累计消耗**（历史所有 LLM 调用的 usage 求和）| 不变（历史花费不退） |
| `totalMessages` | **当前可见消息数** | 减少（被删的不算） |

这俩语义不同，不矛盾。

---

## 七、钩子连锁

### 7.1 受影响的钩子

| 钩子 | 定义位置 | 软删除/折叠时的行为 |
|------|---------|-------------------|
| `on_entries_invalidated` | extension.rs:132 | **新增触发**：软删除/折叠后调用，通知扩展"消息数组变了" |
| `on_session_compact` | extension.rs:82 | 不受影响（compaction 是独立机制） |
| `on_context` | agent_loop.rs:483 | ⚠️ 注意：它改的是 `self.messages`（持久态），软删除/折叠应在它**之前**生效 |
| `on_session_before_compact` | extension.rs:81 | 定义了但未调用——软压缩可考虑触发它 |

### 7.2 钩子执行顺序（每轮 turn，对齐 pi）

**pi 验证的核心原则**：
1. runLoop 内部**零压缩逻辑**——压缩在外层 AgentSession 的 post-run 循环里
2. 压缩判断在**可见性过滤之后**（过滤后的才是真正给 LLM 的）
3. 压缩 = **跳出 runLoop → 外层 while 重进**（不是 runLoop 内部 continue）

```
内层 runLoop（纯净 turn 循环，零压缩）:
  1. on_turn_start
  2. drain_steering
  3. apply_view_filter（视点过滤）
     └─ 🔧 on_view_filtered（通知型）
  4. apply_visibility_filter（可见性过滤：deletion + segment_summary）
     → 产出：干净 messages（持久态，注入前）
     └─ 🔧 on_entries_invalidated（通知型）
  5. ★ needs_compact(干净 messages)?    ← 压缩判断在过滤之后、注入之前！
     ├─ 没超 → 继续步骤 6
     └─ 超了 → compact → break 跳出 runLoop
              → 外层 post-run while 重进 → 回到步骤 1
  6. on_context（修改型：在干净 messages 上注入记忆）
     对齐 pi: transformContext = emitContext
     ★ 注入是临时的，不持久化，不参与压缩判断
  7. transform_messages（跨 provider 规范化）
     对齐 pi: convertToLlm
  8. provider 调用（LLM API）
  9. on_turn_end → persist_turn_summary

外层 post-run 循环（跳出重进机制，对齐 pi _runPostAgentLoop）:
  while (_handlePostAgentRun()) {     // 检查是否需要继续
      // 压缩了？return true → 继续循环
      // 有 followUp 消息？return true → 继续循环
      agent.continue();                // 启动新 runLoop
  }
```

### 7.2.1 为什么压缩判断在注入之前（不是之后）

直觉上"注入后才是最终发给 LLM 的，应该用注入后的 token 判断"。但 pi 选择**注入前判断**，原因：

**注入的记忆是临时的（不持久化）**。如果用注入后的 token 判断压缩：
```
持久 messages: 4900 tokens（没超 5000 阈值）
注入记忆: +200 tokens
注入后: 5100 tokens（超了）
→ 触发压缩 → 砍持久消息到 3000
→ 但如果不注入记忆，4900 根本不需要压缩
→ 临时注入导致了不必要的持久压缩
```

**pi 的折中方案**（ION 对齐）：
- 压缩判断用**注入前**的持久 messages token
- 接受"注入后可能溢出"的 gap
- 溢出靠 **overflow 兜底**：LLM 返回 context overflow 错误时，触发 emergency 压缩 + 重试

| 方案 | 压缩判断 | 优点 | 缺点 |
|------|---------|------|------|
| 注入前判断（pi / ION）| on_context 之前 | 临时注入不导致不必要的持久压缩 | 可能低估实际 token，靠 overflow 兜底 |
| 注入后判断 | on_context 之后 | 精确，不会因注入溢出 | 临时注入的 token 参与压缩决策，可能误触发 |

**关键设计要点**：
- **压缩判断基于过滤后、注入前的 messages**——持久态，不含临时注入
- **压缩不在 runLoop 内部做**——runLoop 检测到需压缩时 break，外层重进
- **注入是临时的**——on_context 注入的记忆不持久化、不参与压缩判断
- **重进有上限保护**——防止压缩-重跑死循环（对齐 pi MAX_POST_RUN_ITERATIONS）

### 7.3 为什么压缩必须在过滤之后

```
错误顺序（ION 当前代码）:
  maybe_compact(self.messages 全量)  ← 算全量 token
  → 可见性过滤                        ← 被删的消息已经算了 token
  → 过早/误触发压缩

正确顺序（pi 验证 + 你的逻辑）:
  可见性过滤                          ← 先拿到干净消息
  → needs_compact(干净 messages)     ← 只算真正给 LLM 的 token
  → 没超就继续，超了就压缩+跳出重进
```

**原因**：可见性过滤后才是真正要交给 LLM 的数据。压缩判断应该基于这个"真实交给 LLM 的数据"，而不是含被删除/折叠消息的全量数据。

### 7.3 on_entries_invalidated 触发时机

```rust
// 软删除后
self.messages.retain(|m| !deleted_indices.contains(&index_of(m)));
self.extensions.on_entries_invalidated(&self.messages).await?;
// → 扩展可以重建索引、更新缓存、触发记忆整理等
```

---

## 八、与现有 compaction 的关系

| 维度 | compaction（硬压缩） | segment_summary（软压缩） |
|------|---------------------|------------------------|
| 触发 | 自动（token 超阈值） | 手动（用户/AI 选一段） |
| 范围 | 整段早期消息（从开头到保留区） | 用户选的任意一段 |
| 产物 | CompactionSummary（独立 Message 变体） | BranchSummary（复用现有变体） |
| 旧数据 | **内存丢失**（硬替换） | **JSONL 留痕**（可恢复） |
| LLM 感知 | 知道（CompactionSummary 渲染成"history was compacted"） | 知道（BranchSummary 渲染成"branch summary"） |
| 可逆 | 不可逆（内存已丢） | **可逆**（删掉 SegmentSummaryEntry 即恢复） |

**软压缩是 compaction 的手动版替代**——用户可以主动折叠一段，而不是等系统自动压缩整段。

---

## 九、RPC 接口

### 9.1 delete_entries

```json
// 请求
{
  "method": "delete_entries",
  "params": {
    "targetIds": ["msg_014", "msg_015"],
    "reason": "错误输出，删除"
  }
}

// 响应
{
  "deleted": 2,
  "remaining": 40
}
```

### 9.2 summarize_entries

```json
// 请求（自动生成摘要）
{
  "method": "summarize_entries",
  "params": {
    "targetIds": ["msg_010", "msg_011", "msg_012", "msg_013", "msg_014"]
  }
}

// 或手动指定摘要
{
  "method": "summarize_entries",
  "params": {
    "targetIds": ["msg_010", "msg_011"],
    "summary": "讨论了分页方案"
  }
}

// 响应
{
  "summarized": 5,
  "summaryEntryId": "bs_001",
  "tokensBefore": 1200,
  "tokensAfter": 50
}
```

---

## 十、实现优先级

| 优先级 | 任务 | 状态 |
|--------|------|------|
| **P0** | Agent 加 `deleted_entry_ids` + `summarized_entry_ids` | ✅ 已完成 |
| **P0** | append_deletion / append_segment_summary / append_restoration | ✅ 已完成 |
| **P0** | mark_deleted / mark_summarized 直接改 self.messages | ✅ 已完成 |
| **P0** | delete_entries RPC（entryId 映射 + 落 DeletionEntry）| ✅ 已完成 |
| **P1** | summarize_entries RPC（LLM 生成摘要 + 替换成 BranchSummary）| ✅ 已完成 |
| **P1** | apply_visibility_filter 补完 segment_summary + restoration 过滤 | ✅ 已完成 |
| **P1** | on_entries_invalidated 触发（ExtensionRegistry fan-out）| ✅ 已完成 |
| **P2** | 软删除/折叠可逆（restore_entries RPC + restoration entry）| ✅ 已完成 |

---

## 十一、关键设计决策

| 决策 | 选择 | 原因 |
|------|------|------|
| 折叠产物 | `Message::BranchSummary` | 复用现有四 provider 渲染逻辑，不新增代码 |
| 操作层 | **内存数组层**（self.messages）| 让 token 统计/compaction 判定自动正确 |
| 过滤时机 | maybe_compact 后、snapshot 前 | 软删除/折叠先于 compaction 判定生效 |
| 内存 Message 无 entryId | 用数组索引 + soft_delete_state | 不需要给每条 Message 打 id |
| 软删除 vs 硬删除 | 软删除（JSONL 留痕）| only-append 不变量，可恢复 |
| segment_summary vs compaction | 并存，手动 vs 自动 | 用户主动折叠 vs 系统自动压缩 |
