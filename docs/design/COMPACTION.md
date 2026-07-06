# Compaction 会话压缩系统

> **状态：已验证** — 分批并发压缩 + LLM summarizer + emergency fallback 全部实现并通过测试。

---

## 概览

ION 的 Compaction 系统在对话超过阈值时自动压缩历史，对齐 pi 的 `keepRecentTokens` / `reserveTokens` 设计。

| 能力 | 入口 | 状态 |
|------|------|------|
| 自动压缩（每轮 turn 检查） | `Agent::maybe_compact` | ✅ |
| 手动压缩（RPC 触发） | `ion rpc --method compact` | ✅ |
| LLM summarizer（用当前 provider 压缩） | `make_llm_summarizer` | ✅ |
| Emergency truncate（LLM 不可用时兜底） | `emergency_truncate` | ✅ |
| 分批并发压缩 | `compact_batched` | ✅ |
| 动态快/慢路径决策 | `compact_batched` | ✅ |

### 实现状态核查清单

| # | 功能 | 状态 | 验证 |
|---|------|------|------|
| 1.1 | `CompactConfig` 配置（threshold / keep_recent / batch_max / ...） | ✅ | `cargo test --lib compact_config_defaults` |
| 1.2 | `needs_compact` 阈值检查 | ✅ | `cargo test --lib needs_compact_below_threshold` |
| 1.3 | `plan_batches` 批次规划（按 user message 切） | ✅ | `cargo test --lib plan_batches_cuts_by_user_messages` |
| 1.4 | `compact_batched` 三阶段流程 | ✅ | `cargo test --lib compact_single_batch_with_summarizer` |
| 1.5 | `emergency_truncate` 兜底（无 LLM） | ✅ | `cargo test --lib compact_without_summarizer_goes_emergency` |
| 1.6 | `apply_compaction` 应用压缩结果 | ✅ | 11 单元测试覆盖 |
| 1.7 | `make_llm_summarizer` LLM 压缩器 | ✅ | e2e: 长对话触发 |
| 2.1 | `maybe_compact` 自动触发（每轮 turn） | ✅ | e2e: 50 轮对话 |
| 2.2 | `needs_compact` 检查避免小消息压缩 | ✅ | `ion "hi"` 无 truncation warning |
| 2.3 | LLM 失败 fallback 到 emergency | ✅ | 无 API key 场景 |
| 2.4 | `compact_now` 手动触发（RPC） | ✅ | `ion rpc --method compact` |
| 3.1 | 空 messages panic 修复 | ✅ | `cargo test --test unit_rpc_test u16` |
| 3.2 | transform_messages 接入 summarizer | ✅ | `src/agent/compact.rs:666` |
| 4.1 | i21/i22/i30 事件订阅测试修复 | ✅ | `cargo test --test manager_integration -- --ignored i21 i22` |

---

## 1. 配置

**文件**：[src/agent/compact.rs:29-57](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs#L29-L57)

```rust
pub struct CompactConfig {
    pub threshold: usize,           // 触发阈值（默认 32000 tokens）
    pub keep_recent_tokens: usize,  // 保留最近 token（默认 20000）
    pub reserve_tokens: usize,      // summary 预留（默认 16384）
    pub batch_max_tokens: usize,    // 单批最大 token（默认 8000）
    pub max_batches: usize,         // 最大批次（默认 10）
    pub context_window: u64,        // 上下文窗口（0 表示未知，走慢路径）
}
```

默认值：`threshold=32000` / `keep_recent_tokens=20000` / `reserve_tokens=16384` / `batch_max_tokens=8000` / `max_batches=10` / `context_window=0`

---

## 2. 三阶段压缩流程

**文件**：[src/agent/compact.rs:264-442](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs#L264)

```rust
pub async fn compact_batched(
    messages: &mut Vec<Message>,
    config: &CompactConfig,
    extensions: &ExtensionRegistry,
    summarizer: Option<SummarizerFn>,
    retry_config: RetryConfig,
) -> AgentResult<CompactionResult>
```

### Step 0：Emergency 检查

```rust
// src/agent/compact.rs:273-280
let too_large = context_window > 0 && total > context_window * 2;
let no_summarizer = summarizer.is_none();
if too_large || no_summarizer {
    return emergency_truncate(messages, config, extensions, total).await;
}
```

### Step 1：分批并发压缩

- `plan_batches` 优先按 user message 切，user 不够用 turn 切
- 每批独立 LLM 调用，失败重试（`RetryConfig`）
- 全部失败 → circuit breaker → emergency truncate

### Step 2+3：动态决策

**文件**：[src/agent/compact.rs:361-392](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs#L361)

```rust
let threshold = if context_window > 0 {
    (context_window as usize * 70) / 100
} else {
    usize::MAX  // 未知 context window，走慢路径
};

if merged_input_tokens < threshold {
    // ── 快路径：合并 Step 2+3，1 次 LLM 调用 ──
    // stage = "batched_merged"
} else {
    // ── 慢路径：Step 2 (merge) → Step 3 (compress with recent)，2 次 LLM 调用 ──
    // stage = "batched_three_step"
}
```

### CompactionResult.stage 取值

| stage | 含义 |
|-------|------|
| `single` | 单批搞定，直接压缩 |
| `batched_merged` | 多批 + 快路径（1 次 LLM 合并） |
| `batched_three_step` | 多批 + 慢路径（2 次 LLM） |
| `emergency` | 紧急截断（无 LLM） |

---

## 3. Emergency Truncate

**文件**：[src/agent/compact.rs:572-602](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs#L572)

触发条件：
1. `summarizer` 为 `None`（LLM 不可用）
2. 总 tokens > 2x context_window

```rust
async fn emergency_truncate(...) -> AgentResult<CompactionResult> {
    tracing::warn!("compaction: emergency truncation ({} tokens, skipped LLM)", tokens_before);
    let summary = format!(
        "[Emergency truncation] {} messages ({}k tokens) were truncated to fit within the model's context window.",
        omitted_count, tokens_before / 1000
    );
    apply_compaction(messages, config, extensions, &summary, tokens_before).await?;
    Ok(result)  // stage = "emergency"
}
```

---

## 4. apply_compaction + 空 messages 修复

**文件**：[src/agent/compact.rs:532-569](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs#L532)

### 空 messages panic 修复（关键 bug fix）

**修复前**：空 messages 上 `messages[skip..]` 越界 panic

**修复后**（[line 539-542](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs#L539)）：

```rust
async fn apply_compaction(...) -> AgentResult<()> {
    // 空 messages 无需压缩（避免 messages[skip..] 越界 panic）
    if messages.is_empty() {
        return Ok(());
    }
    let keep_count = config.keep_recent_tokens / 4;
    let start = messages.len().saturating_sub(keep_count);
    // ... 保留首条 + CompactionSummary + 保留区
}
```

---

## 5. make_llm_summarizer

**文件**：[src/agent/compact.rs:666-697](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs#L666)

用当前 provider + model 做压缩，自动接入 transform_messages：

```rust
pub fn make_llm_summarizer(
    provider: Arc<ion_provider::registry::ApiRegistry>,
    model: Model,
) -> SummarizerFn {
    Arc::new(move |old_messages: &[Message]| {
        // 跨 provider 消息规范化（压缩时历史也可能混合多 provider）
        let transformed = ion_provider::transform_messages::transform_messages(
            msgs, &m, None,
        );
        let ctx = ion_provider::Context::new(
            Some("Summarize key information from these conversation messages.".into()),
            transformed,
        );
        let msg = ion_provider::registry::complete(&p, &m, &ctx, None).await?;
        Ok(msg.content.iter().filter_map(|b| match b {
            AssistantContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        }).collect::<Vec<_>>().join(""))
    })
}
```

---

## 6. maybe_compact 自动触发

**文件**：[src/agent/agent_loop.rs:877-918](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/agent_loop.rs#L877)

每轮 turn 自动检查 + LLM 失败 fallback：

```rust
async fn maybe_compact(&mut self) -> AgentResult<()> {
    if !self.config.enable_compact { return Ok(()); }
    let mut config = self.config.compact_config.clone();
    config.context_window = self.model.context_window;

    // 1. 先检查阈值（低于 threshold 不压缩）
    if !compact::needs_compact(&self.messages, &config) {
        return Ok(());
    }

    let summarizer = compact::make_llm_summarizer(self.registry.clone(), self.model.clone());

    // 2. 尝试用 LLM summarizer 压缩
    match compact::compact_batched(
        &mut self.messages, &config, &self.extensions,
        Some(summarizer), retry_config,
    ).await {
        Ok(_) => Ok(()),
        Err(e) => {
            // 3. LLM 失败 fallback 到 emergency truncate
            tracing::warn!("LLM compaction failed, falling back to emergency truncate: {e}");
            compact::compact_batched(
                &mut self.messages, &config, &self.extensions,
                None,  // ← None 触发 emergency_truncate
                RetryConfig::default(),
            ).await.map(|_| ())
        }
    }
}
```

调用点：[agent_loop.rs:405](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/agent_loop.rs#L405)（每轮 turn 开始时）

```rust
self.extensions.on_turn_start(...).await?;
self.drain_steering().await?;
self.maybe_compact().await?;  // ← 自动检查
// ... 调用 provider
```

---

## 7. compact_now 手动触发

**文件**：[src/agent/agent_loop.rs:863-875](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/agent_loop.rs#L863)

```rust
pub async fn compact_now(&mut self) -> AgentResult<compact::CompactionResult> {
    let mut config = self.config.compact_config.clone();
    config.context_window = self.model.context_window;
    compact::compact_batched(
        &mut self.messages, &config, &self.extensions,
        None,  // ⚠️ compact_now 不传 LLM summarizer，直接走 emergency truncate
        retry_config,
    ).await
}
```

**注意**：`compact_now` 传 `None`，所以 RPC 调用始终走 emergency truncate。这是有意设计——手动压缩应该立即完成，不等 LLM。

---

## 8. compact RPC 接口

**文件**：[src/bin/ion_worker.rs:872-900](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion_worker.rs#L872)

```rust
"compact" => {
    let before_msgs = agent.messages().len();
    let before_tokens = ion::agent::compact::total_tokens(agent.messages());
    match agent.compact_now().await {
        Ok(result) => {
            output_response(&id, "compact", &serde_json::json!({
                "compacted": true,
                "beforeMessages": before_msgs,
                "beforeTokens": before_tokens,
                "afterMessages": agent.messages().len(),
                "afterTokens": after_tokens,
                "stage": result.stage,           // single / batched_merged / batched_three_step / emergency
                "batchCount": result.batch_count,
                "batchSummaries": result.batch_summaries.len(),
                "hasMergedSummary": result.merged_summary.is_some(),
                "summaryPreview": result.summary.chars().take(200).collect::<String>(),
            }));
        }
        Err(e) => { /* ... */ }
    }
}
```

---

## 9. CLI 测试指南

本文档的 CLI 测试分 4 组，对齐 [SECURITY_CLI_GUIDE.md](./SECURITY_CLI_GUIDE.md) 格式。

### compact RPC 接口规格

**请求：**
```bash
ion rpc --session <sid> --method compact
```

**请求参数：** 无（method 固定 `compact`，params 为空 `{}`）

**响应 JSON（成功）：**
```json
{
  "compacted": true,
  "stage": "emergency",
  "beforeMessages": 12,
  "beforeTokens": 45000,
  "afterMessages": 3,
  "afterTokens": 12000,
  "batchCount": 0,
  "batchSummaries": 0,
  "hasMergedSummary": false,
  "summaryPreview": "[Emergency truncation] 9 messages (45k tokens) were truncated..."
}
```

**响应字段：**

| 字段 | 类型 | 说明 |
|------|------|------|
| `compacted` | bool | 是否成功压缩 |
| `stage` | string | `single` / `batched_merged` / `batched_three_step` / `emergency` |
| `beforeMessages` | number | 压缩前消息数 |
| `beforeTokens` | number | 压缩前 token 数 |
| `afterMessages` | number | 压缩后消息数 |
| `afterTokens` | number | 压缩后 token 数 |
| `batchCount` | number | 批次数（emergency=0） |
| `batchSummaries` | number | 批次摘要数 |
| `hasMergedSummary` | bool | 是否有合并摘要 |
| `summaryPreview` | string | 摘要预览（前 200 字符） |

**响应 JSON（失败）：**
```json
{
  "compacted": false,
  "error": "compact failed: ...",
  "beforeMessages": 12,
  "beforeTokens": 45000
}
```

---

### Group A：compact RPC（手动触发）

> `compact_now` 传 `None` 当 summarizer，所以 RPC 始终走 emergency truncate。这是有意设计——手动压缩应该立即完成，不等 LLM。

#### A1 空 session 压缩（验证 panic 修复）

```bash
# 1. 创建空 session
ion rpc --method create_session --params '{"agent":"developer"}'
# → {"session_id":"sess_xxx", ...}

# 2. 立即调 compact（无任何 prompt）
ion rpc --session sess_xxx --method compact
```

**预期（修复前 panic，修复后正常返回）：**
```json
{
  "compacted": true,
  "stage": "emergency",
  "beforeMessages": 0,
  "afterMessages": 0,
  "beforeTokens": 0,
  "afterTokens": 0,
  "summaryPreview": ""
}
```

#### A2 长对话压缩

```bash
# 1. 累积对话
for i in {1..20}; do
  ion rpc --session sess_xxx --method prompt --params '{"text":"讲个故事"}'
done

# 2. 手动压缩
ion rpc --session sess_xxx --method compact
```

**预期：**
```json
{
  "compacted": true,
  "stage": "emergency",
  "beforeMessages": 40,
  "afterMessages": 5,
  "beforeTokens": 38000,
  "afterTokens": 5000,
  "summaryPreview": "[Emergency truncation] 35 messages (38k tokens) were truncated..."
}
```

#### A3 查看压缩前后对比

```bash
# 压缩前查看消息数
ion rpc --session sess_xxx --method get_state --params '{"fields":["messages"]}'

# 压缩
ion rpc --session sess_xxx --method compact

# 压缩后查看消息数
ion rpc --session sess_xxx --method get_state --params '{"fields":["messages"]}'
# 预期：afterMessages < beforeMessages
```

---

### Group B：自动压缩（maybe_compact）

> `maybe_compact` 在每轮 turn 开始时自动检查，超 threshold 才压缩，LLM 失败时 fallback 到 emergency。

#### B1 短对话不压缩（needs_compact 检查）

```bash
# 短对话（< 32000 tokens）不应触发 compaction
ion "用一句话介绍你自己" --provider anthropic --model glm-4.6
```

**预期：**
- 直接返回 LLM 回答
- **无** `emergency truncation` warning
- **无** `compacted to N msgs` 日志

**验证 bug 已修复**：修复前会触发 emergency truncation（maybe_compact 传 None 当 summarizer + 无 needs_compact 检查）。

#### B2 长对话自动压缩（LLM summarizer）

```bash
# 1. 启动 Manager + session
ion manager start
ion rpc --method create_session --params '{"agent":"developer"}'

# 2. 累积超过 32000 tokens 的对话
ion rpc --session sess_xxx --method prompt \
  --params '{"text":"<粘贴 40000+ 字符的长文本>"}'

# 3. 通过 subscribe 观察 compaction 事件
ion subscribe --session sess_xxx
```

**预期：**
- 看到 `LLM compaction` 日志（走 LLM summarizer 路径）
- 或 `emergency truncation` 日志（LLM 失败 fallback）
- agent 不崩溃，继续可对话

#### B3 LLM 失败 fallback 到 emergency

```bash
# 1. 不设置 API key（或设置无效 key）
ion config set api-key ""

# 2. 累积长对话超过 threshold
ion rpc --session sess_xxx --method prompt --params '{"text":"<长文本>"}'

# 3. 触发压缩（自动 or 手动）
ion rpc --session sess_xxx --method compact
```

**预期：**
- warning 日志：`LLM compaction failed, falling back to emergency truncate: ...`
- 返回 `"stage": "emergency"`
- agent 不崩溃，继续可对话

---

### Group C：事件订阅测试（i21/i22/i30）

> 验证 Worker 事件流（agent_start / text_delta / agent_end）的正确转发。

#### C1 i21 单 Worker 事件订阅

```bash
cargo test --test manager_integration -- --ignored --nocapture i21_subscribe_worker_events
```

**预期：**
- 5-10 秒内通过（修复前 25 秒超时）
- 收到 `agent_start` + `text_delta` + `agent_end` 三种事件

#### C2 i22 事件顺序验证

```bash
cargo test --test manager_integration -- --ignored --nocapture i22_event_ordering
```

**预期：**
- 5-10 秒内通过
- `agent_start` 在 `agent_end` 之前

#### C3 i30 多 Worker 同时订阅

```bash
cargo test --test manager_integration --nocapture i30_multiple_subscriptions
```

**预期：**
- 1-2 秒内通过（修复前 25s+ 超时，CI 失败）
- 两个 Worker 都收到事件

---

### Group D：单元测试 + 集成测试

#### D1 compact 单元测试

```bash
cargo test --lib compact
```

**预期：11 tests passed**

| 测试 | 验证 |
|------|------|
| `compact_config_defaults` | 默认配置 |
| `needs_compact_below_threshold` | 低于阈值不压缩 |
| `needs_compact_above_threshold` | 高于阈值触发压缩 |
| `plan_batches_empty_when_small` | 小消息不分批 |
| `plan_batches_cuts_by_user_messages` | 按 user message 切批 |
| `plan_batches_caps_at_max_batches` | 批次上限 |
| `compact_without_summarizer_goes_emergency` | 无 summarizer 走 emergency |
| `compact_single_batch_with_summarizer` | 单批 LLM 压缩 |
| `inject_batch_prompt_prepends_instruction` | 批次 prompt 注入 |
| `build_merged_input_combines_summaries_and_recent` | 合并输入构造 |
| `total_tokens_calculation` | token 估算 |

#### D2 u16 RPC 协议测试（含 compact）

```bash
cargo test --test unit_rpc_test u16_all_supported_commands
```

**预期：** 1-2 秒内通过（修复前 613s 失败，空 messages panic）

#### D3 修复前后对比

| 测试 | 修复前 | 修复后 |
|------|--------|--------|
| i21 | 25s 超时（持锁阻塞 reader task） | 5.27s 通过 |
| i22 | 25s 超时 | 4.22s 通过 |
| i30 | 25s+ 超时（默认跑，CI 失败） | 1.11s 通过 |
| u16 | 613s 失败（空 messages panic） | 1.02s 通过 |
| `ion "hi"` | emergency truncation warning | 无 warning |

#### D4 全套测试

```bash
cargo test
```

**预期：** 210 passed, 11 ignored, 0 failed

---

## 10. 修复根因总结

### 10.1 i21/i22/i30 测试偶发超时

**根因 1**：`send_async(...).await` 阻塞等 prompt response，LLM 慢时 25s deadline 不够

**修复**：改用非阻塞 [send_command](file:///Users/xuyingzhou/Project/study-rust/ion/src/worker_registry.rs#L667)

```rust
// 修复前
let _ = WorkerRegistry::send_async(&registry, &info.worker_id, "prompt", ...).await;

// 修复后
reg.send_command(&info.worker_id, "prompt", ...).await.unwrap();
```

**根因 2**：测试循环持有 registry 锁，[reader task](file:///Users/xuyingzhou/Project/study-rust/ion/src/worker_registry.rs#L322) 无法拿锁转发 event

**修复**：drain 后释放锁，recv 期间不持锁

```rust
// 修复前
let mut reg = registry.lock().await;  // ← 持锁
loop {
    reg.drain_events(...).await;
    events.recv().await;  // ← 持锁等 recv，reader task 无法拿锁
}

// 修复后
loop {
    {
        let mut reg = registry.lock().await;
        reg.drain_events(...).await;
    }  // ← 锁释放
    events.recv().await;  // ← 不持锁，reader task 能拿锁转发
}
```

### 10.2 compact 空 messages panic

**根因**：[apply_compaction](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs#L532) 在空 messages 上 `messages[skip..]` 越界

**修复**：加空检查直接返回

### 10.3 maybe_compact LLM 失败崩溃

**根因**：接了 LLM summarizer 后，LLM 不可用时直接传播错误导致 worker 崩溃

**修复**：LLM 失败时 fallback 到 emergency truncate

### 10.4 runtime.rs 语法错误

**根因**：`&dyn Fn(String) + Send + Sync` 是非法语法（优先级问题）

**修复**：改为 `&(dyn Fn(String) + Send + Sync)`（[4 处](file:///Users/xuyingzhou/Project/study-rust/ion/src/runtime.rs#L43)）

---

## 11. 文件路径速查

| 文件 | 关键行号 | 内容 |
|------|---------|------|
| [compact.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/compact.rs) | 29-57 | CompactConfig |
| 同上 | 142-144 | needs_compact |
| 同上 | 264-442 | compact_batched |
| 同上 | 532-569 | apply_compaction + 空检查 |
| 同上 | 572-602 | emergency_truncate |
| 同上 | 666-697 | make_llm_summarizer |
| 同上 | 737-914 | 11 单元测试 |
| [agent_loop.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/agent/agent_loop.rs) | 405 | maybe_compact 调用点 |
| 同上 | 417 | transform_messages 接入点 |
| 同上 | 863-875 | compact_now（手动） |
| 同上 | 877-918 | maybe_compact（自动 + fallback） |
| [ion_worker.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/bin/ion_worker.rs) | 872-900 | compact RPC 分支 |
| [worker_registry.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/worker_registry.rs) | 667-684 | send_command（非阻塞） |
| 同上 | 703+ | send_async（阻塞） |
| 同上 | 322-389 | reader task 转发 |
| [runtime.rs](file:///Users/xuyingzhou/Project/study-rust/ion/src/runtime.rs) | 43, 278, 465, 800 | 语法修复 4 处 |
