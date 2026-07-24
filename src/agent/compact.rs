//! 分批并发压缩 (Batched Concurrent Compaction)
//!
//! 三阶段流程（动态决策，兼容小模型）：
//!   Step 0: 计算批次 N（优先按 user message 切，user 不够用 turn 切）
//!   Step 1: 分批并发压缩 → N 个 partial summary（单批失败重试，整体失败 circuit breaker）
//!   Step 2+3 动态决策：
//!     - 快路径（输入 < context_window * 70%）：合并 Step 2+3，1 次 LLM 调用
//!     - 慢路径（输入 >= context_window * 70%）：Step 2 (merge) → Step 3 (compress with recent)，2 次 LLM 调用
//!
//! 失败兜底：
//!   - 单批失败：重试（RetryConfig）
//!   - 全部批次失败：circuit breaker
//!   - 总 tokens > 2x window：emergency truncation（纯文本，不调 LLM）
//!   - 批次数 > 10：emergency truncation（太复杂）

use super::error::{AgentError, AgentResult};
use crate::agent::extension::ExtensionRegistry;
use crate::retry::RetryConfig;
use ion_provider::types::*;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

// ──────────────────────────────────────────────────────────────
// 配置
// ──────────────────────────────────────────────────────────────

/// 压缩配置
#[derive(Clone, Debug)]
pub struct CompactConfig {
    /// 触发阈值（estimated tokens 超过此值就压缩）
    pub threshold: usize,
    /// 压缩后保留的最近 token 量（对齐 pi keepRecentTokens）
    pub keep_recent_tokens: usize,
    /// 给 summary prompt + 输出预留的 token（对齐 pi reserveTokens）
    pub reserve_tokens: usize,
    /// 单批最大 token（控制单次 LLM 调用输入大小）
    pub batch_max_tokens: usize,
    /// 最大批次数（超过走 emergency truncation）
    pub max_batches: usize,
    /// 上下文窗口（用于动态决策快/慢路径，0 表示未知，走慢路径）
    pub context_window: u64,
}

impl Default for CompactConfig {
    fn default() -> Self {
        Self {
            threshold: 32000,
            keep_recent_tokens: 20000,
            reserve_tokens: 16384,
            batch_max_tokens: 8000,
            max_batches: 10,
            context_window: 0,
        }
    }
}

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type SummarizerFn = Arc<dyn Fn(&[Message]) -> BoxFuture<'_, AgentResult<String>> + Send + Sync>;

/// 本地重试辅助：支持捕获外部变量的 closure
async fn retry_summarizer<F, Fut>(
    config: &RetryConfig,
    mut operation: F,
) -> AgentResult<String>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = AgentResult<String>>,
{
    let mut last_err: Option<AgentError> = None;
    for attempt in 0..=config.max_retries {
        if attempt > 0 {
            let delay = std::time::Duration::from_millis(
                500u64 * 2u64.pow((attempt - 1)).min(6),
            );
            tokio::time::sleep(delay).await;
        }
        match operation().await {
            Ok(s) => return Ok(s),
            Err(e) => {
                tracing::warn!("compaction retry {}/{}: {}", attempt + 1, config.max_retries + 1, e);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| AgentError::Compact("retry exhausted with no error".into())))
}

// ──────────────────────────────────────────────────────────────
// Token 估算
// ──────────────────────────────────────────────────────────────

/// 估算单条 message 的 token 数（chars/4 启发式，图片按 1000）
pub fn msg_tokens(msg: &Message) -> usize {
    match msg {
        Message::User(m) => m
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text.len() / 4,
                ContentBlock::Image(_) => 1000,
            })
            .sum(),
        Message::Assistant(m) => m
            .content
            .iter()
            .map(|b| match b {
                AssistantContentBlock::Text(t) => t.text.len() / 4,
                AssistantContentBlock::Thinking(th) => th.thinking.len() / 4,
                AssistantContentBlock::ToolCall(tc) => tc.arguments.to_string().len() / 4,
            })
            .sum(),
        Message::ToolResult(m) => m
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text(t) => t.text.len() / 4,
                ContentBlock::Image(_) => 1000,
            })
            .sum(),
        Message::BashExecution(m) => (m.command.len() + m.output.len()) / 4,
        Message::Custom(m) => match &m.content {
            CustomContent::Text(s) => s.len() / 4,
            CustomContent::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) => t.text.len() / 4,
                    ContentBlock::Image(_) => 1000,
                })
                .sum(),
        },
        Message::BranchSummary(m) => m.summary.len() / 4,
        Message::CompactionSummary(m) => m.summary.len() / 4,
    }
}

pub fn total_tokens(messages: &[Message]) -> usize {
    messages.iter().map(msg_tokens).sum()
}

pub fn needs_compact(messages: &[Message], config: &CompactConfig) -> bool {
    total_tokens(messages) > config.threshold
}

/// Estimate total token count of a message list using a rough heuristic:
/// sum of all content text lengths divided by 4.
pub fn estimate_compact_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|msg| match msg {
            Message::User(m) => m
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) => t.text.len(),
                    ContentBlock::Image(_) => 0,
                })
                .sum::<usize>(),
            Message::Assistant(m) => m
                .content
                .iter()
                .map(|b| match b {
                    AssistantContentBlock::Text(t) => t.text.len(),
                    AssistantContentBlock::Thinking(th) => th.thinking.len(),
                    AssistantContentBlock::ToolCall(tc) => tc.arguments.to_string().len(),
                })
                .sum::<usize>(),
            Message::ToolResult(m) => m
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text(t) => t.text.len(),
                    ContentBlock::Image(_) => 0,
                })
                .sum::<usize>(),
            Message::BashExecution(m) => m.command.len() + m.output.len(),
            Message::Custom(m) => match &m.content {
                CustomContent::Text(s) => s.len(),
                CustomContent::Blocks(blocks) => blocks
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text(t) => t.text.len(),
                        ContentBlock::Image(_) => 0,
                    })
                    .sum::<usize>(),
            },
            Message::BranchSummary(m) => m.summary.len(),
            Message::CompactionSummary(m) => m.summary.len(),
        })
        .sum::<usize>()
        / 4
}

// ──────────────────────────────────────────────────────────────
// 批次规划
// ──────────────────────────────────────────────────────────────

/// 单个批次
#[derive(Clone, Debug)]
pub struct Batch {
    pub start_idx: usize,
    pub end_idx: usize,
    pub est_tokens: usize,
    pub anchor_user_preview: String,
}

/// 计算需要切几批，优先按 user message 切，user 不够用 turn 切
pub fn plan_batches(messages: &[Message], config: &CompactConfig) -> Vec<Batch> {
    let total = total_tokens(messages);
    let compressible = total.saturating_sub(config.keep_recent_tokens);

    if compressible == 0 {
        return Vec::new();
    }

    // 目标批次数：compressible / batch_max_tokens，向上取整
    let n = ((compressible as f64) / (config.batch_max_tokens as f64)).ceil() as usize;
    let n = n.max(1).min(config.max_batches);

    // 找所有 user message 索引作为候选切点
    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::User(_)))
        .map(|(i, _)| i)
        .collect();

    // 决定切点：优先 user_indices，不够则用所有 message 索引
    let cut_points: Vec<usize> = if user_indices.len() > n {
        // user 足够，按 N 均分 user_indices
        let step = user_indices.len() as f64 / n as f64;
        (0..n)
            .map(|i| user_indices[(i as f64 * step) as usize])
            .collect()
    } else {
        // user 不够，用所有 message 索引均分
        let total_msgs = messages.len();
        if total_msgs <= n {
            return Vec::new();
        }
        let step = total_msgs as f64 / n as f64;
        (0..n)
            .map(|i| (i as f64 * step) as usize)
            .collect()
    };

    // 构造批次：每个批次从切点 k 到切点 k+1（最后一个到 messages.len()）
    let mut batches = Vec::with_capacity(n);
    let cut_end = messages
        .len()
        .saturating_sub(config.keep_recent_tokens / 4); // 保留区起点（粗略）
    for i in 0..n {
        let start = if i == 0 { 0 } else { cut_points[i] };
        let end = if i + 1 < n {
            cut_points[i + 1]
        } else {
            // 最后一个批次到保留区起点
            cut_end.max(start + 1)
        };
        if start >= end || start >= messages.len() {
            continue;
        }
        let end = end.min(messages.len());
        let est_tokens: usize = messages[start..end].iter().map(msg_tokens).sum();
        let anchor_user_preview = messages[start..end]
            .iter()
            .find_map(|m| match m {
                Message::User(u) => u.content.iter().find_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.chars().take(80).collect::<String>()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default();
        batches.push(Batch {
            start_idx: start,
            end_idx: end,
            est_tokens,
            anchor_user_preview,
        });
    }

    batches
}

// ──────────────────────────────────────────────────────────────
// 压缩结果
// ──────────────────────────────────────────────────────────────

/// 压缩结果（含批次元信息，写入 CompactionEntry）
#[derive(Clone, Debug, Default)]
pub struct CompactionResult {
    pub summary: String,
    pub tokens_before: u64,
    pub batch_count: usize,
    pub batch_summaries: Vec<String>,
    pub merged_summary: Option<String>,
    pub stage: String, // "single" | "batched_merged" | "batched_three_step" | "emergency"
}

// ──────────────────────────────────────────────────────────────
// 核心压缩函数
// ──────────────────────────────────────────────────────────────

/// 主入口：分批并发压缩
///
/// `summarizer`：单批压缩函数（包装 LLM 调用）
/// `merge_summarizer`：合并多批 summary 的函数（包装 LLM 调用）
/// `final_summarizer`：与保留区合并压缩的函数（包装 LLM 调用）
///
/// 如果任一 summarizer 为 None，走 emergency truncation
pub async fn compact_batched(
    messages: &mut Vec<Message>,
    config: &CompactConfig,
    extensions: &ExtensionRegistry,
    summarizer: Option<SummarizerFn>,
    retry_config: RetryConfig,
) -> AgentResult<CompactionResult> {
    let total = total_tokens(messages) as u64;

    // Emergency: 总 tokens > 2x context window（如果已知）或 summarizer 不可用
    let context_window = config.context_window;
    let too_large = context_window > 0 && total > context_window * 2;
    let no_summarizer = summarizer.is_none();

    if too_large || no_summarizer {
        return emergency_truncate(messages, config, extensions, total).await;
    }

    let summarizer = summarizer.unwrap();
    let batches = plan_batches(messages, config);

    // 单批就能搞定 → 直接压缩，不走三阶段
    if batches.len() <= 1 {
        return single_batch_compact(messages, config, extensions, summarizer, total, retry_config)
            .await;
    }

    tracing::info!(
        "compaction: {} batches for {} tokens (window={})",
        batches.len(),
        total,
        context_window
    );

    // ── Step 1: 分批并发压缩 ──
    let batch_summaries: Arc<Mutex<Vec<Option<String>>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();

    for (i, batch) in batches.iter().enumerate() {
        let batch_msgs: Vec<Message> = messages[batch.start_idx..batch.end_idx].to_vec();
        let summarizer_clone = Arc::clone(&summarizer);
        let retry = retry_config.clone();
        let batch_summaries_clone = Arc::clone(&batch_summaries);
        let anchor = batch.anchor_user_preview.clone();
        let total_batches = batches.len();

        let handle = tokio::spawn(async move {
            let batch_idx = i + 1;
            let result = retry_summarizer(&retry, || {
                let msgs = batch_msgs.clone();
                let s = Arc::clone(&summarizer_clone);
                let anchor = anchor.clone();
                Box::pin(async move {
                    let prompted = inject_batch_prompt(msgs, batch_idx, total_batches, &anchor);
                    s(&prompted).await
                })
            })
            .await;

            let mut summaries = batch_summaries_clone.lock().await;
            match result {
                Ok(summary) => {
                    tracing::info!("compaction batch {}/{} ok ({} chars)", batch_idx, total_batches, summary.len());
                    summaries.push(Some(summary));
                }
                Err(e) => {
                    tracing::warn!("compaction batch {}/{} failed: {}", batch_idx, total_batches, e);
                    summaries.push(None);
                }
            }
            summaries.len()
        });
        handles.push(handle);
    }

    // 等所有批次完成
    for handle in handles {
        let _ = handle.await;
    }

    let summaries_guard = batch_summaries.lock().await;
    let partial_summaries: Vec<String> = summaries_guard.iter().filter_map(|s| s.clone()).collect();

    // circuit breaker：全部失败
    if partial_summaries.is_empty() {
        tracing::error!("compaction: all {} batches failed, emergency truncate", batches.len());
        return emergency_truncate(messages, config, extensions, total).await;
    }

    let partial_tokens: usize = partial_summaries.iter().map(|s| s.len() / 4).sum();
    let recent_tokens = messages
        .iter()
        .skip(messages.len().saturating_sub(config.keep_recent_tokens / 4))
        .map(msg_tokens)
        .sum::<usize>();
    let merged_input_tokens = partial_tokens + recent_tokens;

    // ── 动态决策：快路径 vs 慢路径 ──
    let threshold = if context_window > 0 {
        (context_window as usize * 70) / 100
    } else {
        // 未知 context window，走慢路径
        usize::MAX
    };

    if merged_input_tokens < threshold {
        // ── 快路径：合并 Step 2+3 ──
        tracing::info!(
            "compaction: fast path (merged_input={} < threshold={})",
            merged_input_tokens,
            threshold
        );

        // 构造合并输入：partial summaries + recent messages
        let merged_msgs = build_merged_input(&partial_summaries, messages, config);
        let final_summary = summarizer(&merged_msgs).await?;

        let result = CompactionResult {
            summary: final_summary,
            tokens_before: total,
            batch_count: batches.len(),
            batch_summaries: partial_summaries,
            merged_summary: None, // 快路径没单独 merge
            stage: "batched_merged".into(),
        };

        apply_compaction(messages, config, extensions, &result.summary, total).await?;
        return Ok(result);
    }

    // ── 慢路径：Step 2 (merge) → Step 3 (compress with recent) ──
    tracing::info!(
        "compaction: slow path (merged_input={} >= threshold={})",
        merged_input_tokens,
        threshold
    );

    // Step 2: 合并 N 个 partial summary
    let merge_input: Vec<Message> = partial_summaries
        .iter()
        .enumerate()
        .map(|(i, s)| Message::User(UserMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: format!("Batch {} summary: {}", i + 1, s),
                text_signature: None,
            })],
            timestamp: 0,
            source: ion_provider::types::MessageSource::Prompt,
        }))
        .collect();

    let merged_summary = retry_summarizer(&retry_config, || {
        let msgs = merge_input.clone();
        let s = Arc::clone(&summarizer);
        Box::pin(async move { s(&msgs).await })
    })
    .await?;

    // Step 3: 与保留区合并压缩
    let final_input = build_merged_input(&[merged_summary.clone()], messages, config);
    let final_summary = retry_summarizer(&retry_config, || {
        let msgs = final_input.clone();
        let s = Arc::clone(&summarizer);
        Box::pin(async move { s(&msgs).await })
    })
    .await?;

    let result = CompactionResult {
        summary: final_summary,
        tokens_before: total,
        batch_count: batches.len(),
        batch_summaries: partial_summaries,
        merged_summary: Some(merged_summary),
        stage: "batched_three_step".into(),
    };

    apply_compaction(messages, config, extensions, &result.summary, total).await?;
    Ok(result)
}

// ──────────────────────────────────────────────────────────────
// 辅助函数
// ──────────────────────────────────────────────────────────────

/// 给批次消息注入"当前是第 k/N 批"提示
fn inject_batch_prompt(msgs: Vec<Message>, batch_idx: usize, total: usize, anchor: &str) -> Vec<Message> {
    let prompt = format!(
        "[Compaction Batch {}/{}] You are summarizing batch {} of {} from a longer conversation. \
         This batch covers messages around: \"{}\". \
         Extract key information: goals, decisions, progress, files modified, user constraints. \
         Be concise but preserve all important details.",
        batch_idx,
        total,
        batch_idx,
        total,
        if anchor.is_empty() { "(no anchor)" } else { anchor }
    );

    let mut result = Vec::with_capacity(msgs.len() + 1);
    result.push(Message::User(UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: prompt,
            text_signature: None,
        })],
        timestamp: 0,
        source: ion_provider::types::MessageSource::Prompt,
    }));
    result.extend(msgs);
    result
}

/// 构造合并输入（partial summaries + recent messages）
fn build_merged_input(
    partial_summaries: &[String],
    messages: &[Message],
    config: &CompactConfig,
) -> Vec<Message> {
    let mut input = Vec::new();

    // 加入 partial summaries
    let combined = if partial_summaries.len() == 1 {
        format!(
            "Previous conversation summary:\n\n{}",
            partial_summaries[0]
        )
    } else {
        let parts: Vec<String> = partial_summaries
            .iter()
            .enumerate()
            .map(|(i, s)| format!("--- Batch {} ---\n{}", i + 1, s))
            .collect();
        format!(
            "Previous conversation summaries ({} batches, merged):\n\n{}",
            partial_summaries.len(),
            parts.join("\n\n")
        )
    };

    input.push(Message::User(UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: combined,
            text_signature: None,
        })],
        timestamp: 0,
        source: ion_provider::types::MessageSource::Prompt,
    }));

    // 加入保留区（最近 keep_recent_tokens 的消息）
    let keep_count = config.keep_recent_tokens / 4;
    let start = messages.len().saturating_sub(keep_count);
    input.extend(messages[start..].iter().cloned());

    // 加入最终压缩指令
    input.push(Message::User(UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent {
            text: "Based on the above summary and recent messages, generate a final consolidated summary. \
                   Preserve recent context continuity. Use sections: ## Goal, ## Progress, ## Key Decisions, ## Files Modified, ## Remaining Work."
                .into(),
            text_signature: None,
        })],
        timestamp: 0,
        source: ion_provider::types::MessageSource::Prompt,
    }));

    input
}

/// 应用压缩结果到 messages（替换压缩区为 CompactionSummary）
async fn apply_compaction(
    messages: &mut Vec<Message>,
    config: &CompactConfig,
    extensions: &ExtensionRegistry,
    summary: &str,
    tokens_before: u64,
) -> AgentResult<()> {
    // 空 messages 无需压缩（避免 messages[skip..] 越界 panic）
    if messages.is_empty() {
        return Ok(());
    }
    let keep_count = config.keep_recent_tokens / 4;
    let original_start = messages.len().saturating_sub(keep_count);
    let mut start = original_start;

    // Split-turn 调整（对齐 pi findCutPoint）：
    // 如果切点落在 turn 中间（保留区首条不是 User 消息），
    // 向前找到下一个 User 消息，避免保留区以孤儿 Assistant/ToolResult 开头。
    if start > 1 && start < messages.len() {
        while start < messages.len() && !is_turn_boundary(&messages[start]) {
            start += 1;
        }
        // 边界保护：如果整个保留区都没有 turn boundary（全是 Assistant/ToolResult），
        // 回退到原始切点，容忍孤儿消息（比丢弃整个保留区好）
        if start >= messages.len() {
            start = original_start;
        }
    }

    let mut new_msgs = Vec::with_capacity(keep_count + 2);

    // 保留首条（通常是 system message）
    if let Some(first) = messages.first() {
        new_msgs.push(first.clone());
    }

    // 加入压缩总结
    new_msgs.push(Message::CompactionSummary(CompactionSummaryMessage {
        role: "compactionSummary".into(),
        summary: summary.to_string(),
        tokens_before,
        timestamp: 0,
    }));

    // 保留区
    let skip = if start <= 1 { 1 } else { start };
    new_msgs.extend(messages[skip..].iter().cloned());

    *messages = new_msgs;
    extensions.on_session_compact(messages).await?;
    tracing::info!("compacted to {} msgs", messages.len());
    Ok(())
}

/// 判断一条消息是否是一个 turn 的合法起始（User 消息或 CompactionSummary/BranchSummary）。
/// 用于 split-turn 调整：保留区不应该以孤儿 Assistant/ToolResult 开头。
fn is_turn_boundary(msg: &Message) -> bool {
    matches!(
        msg,
        Message::User(_) | Message::CompactionSummary(_) | Message::BranchSummary(_)
    )
}

/// Emergency truncation：纯文本摘要，不调 LLM
async fn emergency_truncate(
    messages: &mut Vec<Message>,
    config: &CompactConfig,
    extensions: &ExtensionRegistry,
    tokens_before: u64,
) -> AgentResult<CompactionResult> {
    tracing::warn!(
        "compaction: emergency truncation ({} tokens, skipped LLM)",
        tokens_before
    );

    let omitted_count = messages.len().saturating_sub(config.keep_recent_tokens / 4);
    let summary = format!(
        "[Emergency truncation] {} messages ({}k tokens) were truncated to fit within the model's context window. \
         The conversation history has been preserved in the session file.",
        omitted_count,
        tokens_before / 1000
    );

    let result = CompactionResult {
        summary: summary.clone(),
        tokens_before,
        batch_count: 0,
        batch_summaries: Vec::new(),
        merged_summary: None,
        stage: "emergency".into(),
    };

    apply_compaction(messages, config, extensions, &summary, tokens_before).await?;
    Ok(result)
}

/// 单批压缩（批次数 = 1 时直接走原逻辑）
async fn single_batch_compact(
    messages: &mut Vec<Message>,
    config: &CompactConfig,
    extensions: &ExtensionRegistry,
    summarizer: SummarizerFn,
    total: u64,
    retry_config: RetryConfig,
) -> AgentResult<CompactionResult> {
    let keep_count = config.keep_recent_tokens / 4;
    let summarize_end = messages.len().saturating_sub(keep_count);
    let to_summarize: Vec<Message> = if summarize_end > 1 {
        messages[1..summarize_end].to_vec()
    } else {
        Vec::new()
    };

    let summary = if to_summarize.is_empty() {
        "[No messages to summarize]".to_string()
    } else {
        retry_summarizer(&retry_config, || {
            let msgs = to_summarize.clone();
            let s = Arc::clone(&summarizer);
            Box::pin(async move { s(&msgs).await })
        })
        .await?
    };

    let result = CompactionResult {
        summary: summary.clone(),
        tokens_before: total,
        batch_count: 1,
        batch_summaries: vec![summary],
        merged_summary: None,
        stage: "single".into(),
    };

    apply_compaction(messages, config, extensions, &result.summary, total).await?;
    Ok(result)
}

// ──────────────────────────────────────────────────────────────
// 向后兼容：旧 API
// ──────────────────────────────────────────────────────────────

/// 旧版 compact API（保留向后兼容，内部走新实现）
pub async fn compact(
    messages: &mut Vec<Message>,
    config: &CompactConfig,
    extensions: &ExtensionRegistry,
    summarizer: Option<SummarizerFn>,
) -> AgentResult<()> {
    if !needs_compact(messages, config) {
        return Ok(());
    }
    let retry_config = RetryConfig::default();
    compact_batched(messages, config, extensions, summarizer, retry_config)
        .await
        .map(|_| ())
}

/// 创建 LLM summarizer（用于压缩时调 LLM）
pub fn make_llm_summarizer(
    provider: Arc<ion_provider::registry::ApiRegistry>,
    model: Model,
    api_key: Option<String>,
) -> SummarizerFn {
    Arc::new(move |old_messages: &[Message]| {
        let p = Arc::clone(&provider);
        let m = model.clone();
        let key = api_key.clone();
        let msgs = old_messages.to_vec();
        Box::pin(async move {
            // 检测已有的 CompactionSummary（增量更新模式，对齐 pi UPDATE_SUMMARIZATION_PROMPT）
            let existing_summary = extract_existing_summary(&msgs);
            let system_prompt = if let Some(ref prev) = existing_summary {
                format!(
                    "You are updating an existing conversation summary with new information.\n\n\
                     PREVIOUS SUMMARY:\n{}\n\n\
                     Update the summary to incorporate the new messages below. \
                     Preserve all important details from the previous summary, \
                     add new progress/decisions/files, and remove outdated information. \
                     Use sections: ## Goal, ## Progress, ## Key Decisions, ## Files Modified, ## Remaining Work.",
                    prev
                )
            } else {
                "Summarize key information from these conversation messages. \
                 Use sections: ## Goal, ## Progress, ## Key Decisions, ## Files Modified, ## Remaining Work."
                    .to_string()
            };

            // 跨 provider 消息规范化（压缩时历史也可能混合多 provider）
            let transformed = ion_provider::transform_messages::transform_messages(
                msgs,
                &m,
                None,
            );
            let ctx = ion_provider::Context::new(Some(system_prompt), transformed);
            let opts = ion_provider::types::StreamOptions {
                api_key: key.clone(),
                reasoning: None,
                timeout_ms: Some(60000),
                max_retries: Some(5),
                max_tokens: Some(16000),  // reasoning models need large budget
                response_format: None,
            };
            let msg = ion_provider::registry::complete(&p, &m, &ctx, Some(&opts)).await?;
            Ok(msg
                .content
                .iter()
                .filter_map(|b| match b {
                    AssistantContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(""))
        })
    })
}

/// 从 messages 里提取已有的 CompactionSummary 文本（增量更新用）。
/// 如果有多个，合并成一个。
fn extract_existing_summary(messages: &[Message]) -> Option<String> {
    let summaries: Vec<&str> = messages.iter().filter_map(|m| {
        if let Message::CompactionSummary(cs) = m {
            Some(cs.summary.as_str())
        } else {
            None
        }
    }).collect();

    if summaries.is_empty() {
        None
    } else if summaries.len() == 1 {
        Some(summaries[0].to_string())
    } else {
        Some(summaries.join("\n\n---\n\n"))
    }
}

// ──────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_user_msg(text: &str) -> Message {
        Message::User(UserMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            timestamp: 0,
            source: ion_provider::types::MessageSource::Prompt,
        })
    }

    fn make_assistant_msg(text: &str) -> Message {
        Message::Assistant(AssistantMessage {
            role: "assistant".into(),
            content: vec![AssistantContentBlock::Text(TextContent {
                text: text.into(),
                text_signature: None,
            })],
            api: "".into(),
            provider: "".into(),
            model: "".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })
    }

    #[test]
    fn compact_config_defaults() {
        let cfg = CompactConfig::default();
        assert_eq!(cfg.threshold, 32000);
        assert_eq!(cfg.max_batches, 10);
    }

    #[test]
    fn needs_compact_below_threshold() {
        let msgs = vec![make_user_msg("hello")];
        let cfg = CompactConfig {
            threshold: 1000,
            ..Default::default()
        };
        assert!(!needs_compact(&msgs, &cfg));
    }

    #[test]
    fn needs_compact_above_threshold() {
        let big = "x".repeat(5000);
        let msgs = vec![make_user_msg(&big)];
        let cfg = CompactConfig {
            threshold: 100,
            ..Default::default()
        };
        assert!(needs_compact(&msgs, &cfg));
    }

    #[test]
    fn plan_batches_empty_when_small() {
        let msgs = vec![make_user_msg("hello"), make_assistant_msg("hi")];
        let cfg = CompactConfig {
            keep_recent_tokens: 10000,
            ..Default::default()
        };
        let batches = plan_batches(&msgs, &cfg);
        assert!(batches.is_empty());
    }

    #[test]
    fn plan_batches_cuts_by_user_messages() {
        // 5 个 user message，每个 4k tokens
        let mut msgs = vec![make_user_msg("system")];
        for i in 0..5 {
            let big = format!("user message {} {}", i, "x".repeat(16000));
            msgs.push(make_user_msg(&big));
            msgs.push(make_assistant_msg(&format!("assistant {}", i)));
        }
        let cfg = CompactConfig {
            threshold: 1000,
            keep_recent_tokens: 100,
            batch_max_tokens: 4000,
            max_batches: 10,
            ..Default::default()
        };
        let batches = plan_batches(&msgs, &cfg);
        // 应该切出多个批次
        assert!(batches.len() >= 2, "expected >= 2 batches, got {}", batches.len());
        // 每个批次应该有 anchor_user_preview
        for b in &batches {
            assert!(!b.anchor_user_preview.is_empty() || b.start_idx == 0);
        }
    }

    #[test]
    fn plan_batches_caps_at_max_batches() {
        let mut msgs = vec![make_user_msg("system")];
        for i in 0..20 {
            msgs.push(make_user_msg(&format!("user {} {}", i, "x".repeat(16000))));
            msgs.push(make_assistant_msg(&format!("asst {}", i)));
        }
        let cfg = CompactConfig {
            threshold: 1000,
            keep_recent_tokens: 100,
            batch_max_tokens: 1000,
            max_batches: 5,
            ..Default::default()
        };
        let batches = plan_batches(&msgs, &cfg);
        assert!(batches.len() <= 5, "expected <= 5 batches, got {}", batches.len());
    }

    #[tokio::test]
    async fn compact_without_summarizer_goes_emergency() {
        let mut msgs = vec![
            make_user_msg("system: you are helpful"),
            make_user_msg(&"x".repeat(5000)), // 大消息触发压缩
            make_assistant_msg("response"),
        ];
        let cfg = CompactConfig {
            threshold: 100,
            keep_recent_tokens: 100,
            ..Default::default()
        };
        let ext = ExtensionRegistry::new();
        let retry = RetryConfig::default();
        let result = compact_batched(&mut msgs, &cfg, &ext, None, retry).await.unwrap();
        assert_eq!(result.stage, "emergency");
        assert!(result.summary.contains("Emergency truncation"));
    }

    #[tokio::test]
    async fn compact_single_batch_with_summarizer() {
        let mut msgs = vec![
            make_user_msg("system"),
            make_user_msg(&"x".repeat(1000)),
            make_assistant_msg("response"),
            make_user_msg("more"),
            make_assistant_msg("more response"),
        ];
        let cfg = CompactConfig {
            threshold: 100,
            keep_recent_tokens: 4, // 只保留 1 条消息，留出空间给 summarizer
            ..Default::default()
        };
        let ext = ExtensionRegistry::new();
        let summarizer: SummarizerFn = Arc::new(|_msgs: &[Message]| {
            Box::pin(async { Ok("test summary".to_string()) })
        });
        let retry = RetryConfig::default();
        let result = compact_batched(&mut msgs, &cfg, &ext, Some(summarizer), retry)
            .await
            .unwrap();
        assert_eq!(result.stage, "single");
        assert_eq!(result.summary, "test summary");
        assert_eq!(result.batch_count, 1);
    }

    #[test]
    fn inject_batch_prompt_prepends_instruction() {
        let msgs = vec![make_user_msg("hello")];
        let prompted = inject_batch_prompt(msgs, 2, 5, "what is rust");
        assert!(prompted.len() >= 2);
        match &prompted[0] {
            Message::User(u) => {
                if let Some(ContentBlock::Text(t)) = u.content.first() {
                    assert!(t.text.contains("Batch 2/5"));
                    assert!(t.text.contains("what is rust"));
                } else {
                    panic!("expected text content");
                }
            }
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn build_merged_input_combines_summaries_and_recent() {
        let mut msgs = vec![make_user_msg("system")];
        for i in 0..10 {
            msgs.push(make_user_msg(&format!("user {}", i)));
            msgs.push(make_assistant_msg(&format!("asst {}", i)));
        }
        let cfg = CompactConfig::default();
        let input = build_merged_input(&["summary 1".into(), "summary 2".into()], &msgs, &cfg);
        // 应该包含：1 summary + N recent + 1 instruction
        assert!(input.len() >= 2);
        // 第一条应该是 summary
        match &input[0] {
            Message::User(u) => {
                if let Some(ContentBlock::Text(t)) = u.content.first() {
                    assert!(t.text.contains("summary 1"));
                    assert!(t.text.contains("summary 2"));
                }
            }
            _ => panic!("expected user message"),
        }
    }

    #[test]
    fn total_tokens_calculation() {
        let msgs = vec![
            make_user_msg("hello world"), // 11 chars / 4 = 2
            make_assistant_msg("hi"),     // 2 chars / 4 = 0
        ];
        let total = total_tokens(&msgs);
        assert!(total >= 2);
    }

    #[test]
    fn test_estimate_compact_tokens() {
        // 3 messages with known content
        let msgs = vec![
            make_user_msg("hello world"),           // 11 chars
            make_assistant_msg("this is a test"),   // 14 chars
            make_user_msg("another message here"),  // 20 chars
        ];
        let total_chars: usize = 11 + 14 + 20; // = 45
        let expected = total_chars / 4; // = 11
        let result = estimate_compact_tokens(&msgs);
        assert_eq!(result, expected, "estimate_compact_tokens({total_chars} chars) = {result}, expected {expected}");
    }

    fn make_compaction_summary(summary: &str) -> Message {
        Message::CompactionSummary(CompactionSummaryMessage {
            role: "compactionSummary".into(),
            summary: summary.into(),
            tokens_before: 1000,
            timestamp: 0,
        })
    }

    #[test]
    fn extract_existing_summary_none() {
        let msgs = vec![make_user_msg("hello"), make_assistant_msg("hi")];
        assert!(extract_existing_summary(&msgs).is_none());
    }

    #[test]
    fn extract_existing_summary_single() {
        let msgs = vec![
            make_compaction_summary("Previous goal: implement auth"),
            make_user_msg("new message"),
        ];
        let summary = extract_existing_summary(&msgs).unwrap();
        assert!(summary.contains("implement auth"));
    }

    #[test]
    fn extract_existing_summary_multiple_merged() {
        let msgs = vec![
            make_compaction_summary("Summary 1"),
            make_user_msg("msg"),
            make_compaction_summary("Summary 2"),
        ];
        let summary = extract_existing_summary(&msgs).unwrap();
        assert!(summary.contains("Summary 1"));
        assert!(summary.contains("Summary 2"));
        assert!(summary.contains("---")); // merged separator
    }

    #[test]
    fn is_turn_boundary_correct() {
        assert!(is_turn_boundary(&make_user_msg("hello")));
        assert!(is_turn_boundary(&make_compaction_summary("summary")));
        assert!(!is_turn_boundary(&make_assistant_msg("reply")));
    }

    #[tokio::test]
    async fn split_turn_keeps_region_when_no_user_in_keep() {
        // 保留区全是 Assistant 消息——split-turn 调整不应丢弃它们
        let mut msgs = vec![
            make_user_msg("start"),           // 0
            make_assistant_msg("a1"),         // 1
            make_assistant_msg("a2"),         // 2
            make_assistant_msg("a3"),         // 3
            make_assistant_msg("a4"),         // 4
        ];
        let config = CompactConfig {
            threshold: 1,
            keep_recent_tokens: 8, // keep_count = 8/4 = 2 → 保留区从 index 3 开始
            ..Default::default()
        };
        let ext = ExtensionRegistry::new();

        // 用 emergency truncate（summarizer=None），直接测 apply_compaction 逻辑
        let result = emergency_truncate(&mut msgs, &config, &ext, 100).await;
        assert!(result.is_ok());

        // 验证：messages 不能只剩首条+摘要（保留区不该被全丢）
        assert!(msgs.len() > 2, "keep region should be preserved, got {} msgs", msgs.len());
        // 至少有一条原始 Assistant 消息保留
        let has_assistant = msgs.iter().any(|m| matches!(m, Message::Assistant(a) if a.content.iter().any(|b| {
            matches!(b, AssistantContentBlock::Text(t) if t.text.starts_with("a"))
        })));
        assert!(has_assistant, "at least one assistant message should survive");
    }
}
