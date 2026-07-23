//! 跨 provider 消息规范化
//!
//! 对齐 pi packages/ai/src/providers/transform-messages.ts
//! 作用：当对话历史混合多个 provider 的消息时，规范化为当前模型可接受的格式
//!
//! 主要处理：
//! 1. 图片降级：模型不支持图片时，image → 占位符文本
//! 2. thinking block 跨模型处理：
//!    - redacted thinking：跨模型丢弃（只有原模型能解密）
//!    - 带 signature 的 thinking：同模型保留，跨模型转纯文本
//!    - 空 thinking：丢弃
//! 3. tool call ID 规范化：跨 provider 时 ID 格式不同（OpenAI Responses 用 `|` 分隔，Anthropic 要求 `^[a-zA-Z0-9_-]+$`）
//! 4. thought signature 清理：跨模型时删除
//! 5. 孤儿 tool call：为没有 result 的 tool call 插入合成 error result
//! 6. 跳过 error/aborted 的 assistant 消息（不回放）

use crate::types::*;

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str = "(tool image omitted: model does not support images)";

/// 规范化 tool call ID 的回调类型
/// 返回新的 ID（如果需要规范化）
pub type NormalizeToolCallIdFn = Box<dyn Fn(&str, &Model, &AssistantMessage) -> String + Send + Sync>;

/// 主入口：规范化消息列表
pub fn transform_messages(
    messages: Vec<Message>,
    model: &Model,
    normalize_tool_call_id: Option<&NormalizeToolCallIdFn>,
) -> Vec<Message> {
    // 1. 图片降级
    let messages = downgrade_unsupported_images(messages, model);

    // 2. 构建 toolCallId 映射 + thinking/text/toolCall 跨模型转换
    let mut tool_call_id_map: std::collections::HashMap<String, String> = std::collections::HashMap::new();

    let transformed: Vec<Message> = messages
        .into_iter()
        .map(|msg| transform_single_message(msg, model, normalize_tool_call_id, &mut tool_call_id_map))
        .collect();

    // 3. 第二遍：为孤儿 tool call 插入合成 result，跳过 error/aborted assistant
    insert_synthetic_tool_results(transformed)
}

// ──────────────────────────────────────────────────────────────
// 1. 图片降级
// ──────────────────────────────────────────────────────────────

fn downgrade_unsupported_images(messages: Vec<Message>, model: &Model) -> Vec<Message> {
    if model.input.iter().any(|i| i == "image") {
        return messages;
    }
    messages
        .into_iter()
        .map(|msg| match msg {
            Message::User(u) => {
                let has_image = u.content.iter().any(|b| matches!(b, ContentBlock::Image(_)));
                if !has_image {
                    return Message::User(u);
                }
                Message::User(UserMessage {
                    content: replace_images_with_placeholder(u.content, NON_VISION_USER_IMAGE_PLACEHOLDER),
                    ..u
                })
            }
            Message::ToolResult(tr) => {
                let has_image = tr.content.iter().any(|b| matches!(b, ContentBlock::Image(_)));
                if !has_image {
                    return Message::ToolResult(tr);
                }
                Message::ToolResult(ToolResultMessage {
                    content: replace_images_with_placeholder(tr.content, NON_VISION_TOOL_IMAGE_PLACEHOLDER),
                    ..tr
                })
            }
            other => other,
        })
        .collect()
}

fn replace_images_with_placeholder(content: Vec<ContentBlock>, placeholder: &str) -> Vec<ContentBlock> {
    let mut result: Vec<ContentBlock> = Vec::new();
    let mut previous_was_placeholder = false;

    for block in content {
        if let ContentBlock::Image(_) = block {
            if !previous_was_placeholder {
                result.push(ContentBlock::Text(TextContent {
                    text: placeholder.into(),
                    text_signature: None,
                }));
            }
            previous_was_placeholder = true;
            continue;
        }
        previous_was_placeholder = matches!(&block, ContentBlock::Text(t) if t.text == placeholder);
        result.push(block);
    }
    result
}

// ──────────────────────────────────────────────────────────────
// 2. 单条消息转换
// ──────────────────────────────────────────────────────────────

fn transform_single_message(
    msg: Message,
    model: &Model,
    normalize_tool_call_id: Option<&NormalizeToolCallIdFn>,
    tool_call_id_map: &mut std::collections::HashMap<String, String>,
) -> Message {
    match msg {
        Message::User(_) | Message::BashExecution(_) | Message::Custom(_)
        | Message::BranchSummary(_) | Message::CompactionSummary(_) => msg,

        Message::ToolResult(mut tr) => {
            // 用映射表规范化 tool_call_id
            if let Some(norm_id) = tool_call_id_map.get(&tr.tool_call_id) {
                if norm_id != &tr.tool_call_id {
                    tr.tool_call_id = norm_id.clone();
                }
            }
            Message::ToolResult(tr)
        }

        Message::Assistant(a) => {
            let is_same_model = a.provider == model.provider
                && a.api == model.api
                && a.model == model.id;

            let transformed_content: Vec<AssistantContentBlock> = a
                .content
                .iter()
                .cloned()
                .flat_map(|block| transform_content_block(block, is_same_model, model, &a, normalize_tool_call_id, tool_call_id_map))
                .collect();

            Message::Assistant(AssistantMessage {
                role: a.role,
                content: transformed_content,
                api: a.api,
                provider: a.provider,
                model: a.model,
                response_model: a.response_model,
                response_id: a.response_id,
                usage: a.usage,
                stop_reason: a.stop_reason,
                error_message: a.error_message,
                timestamp: a.timestamp,
            })
        }
    }
}

fn transform_content_block(
    block: AssistantContentBlock,
    is_same_model: bool,
    _model: &Model,
    assistant: &AssistantMessage,
    normalize_tool_call_id: Option<&NormalizeToolCallIdFn>,
    tool_call_id_map: &mut std::collections::HashMap<String, String>,
) -> Vec<AssistantContentBlock> {
    match block {
        AssistantContentBlock::Thinking(th) => {
            // redacted thinking 跨模型丢弃
            if th.redacted == Some(true) {
                return if is_same_model { vec![AssistantContentBlock::Thinking(th)] } else { vec![] };
            }
            // 同模型且带 signature：保留（replay 需要）
            if is_same_model && th.thinking_signature.is_some() {
                return vec![AssistantContentBlock::Thinking(th)];
            }
            // 空 thinking：丢弃
            if th.thinking.trim().is_empty() {
                return vec![];
            }
            // 同模型：保留
            if is_same_model {
                return vec![AssistantContentBlock::Thinking(th)];
            }
            // 跨模型：转纯文本
            vec![AssistantContentBlock::Text(TextContent {
                text: th.thinking,
                text_signature: None,
            })]
        }

        AssistantContentBlock::Text(t) => {
            if is_same_model {
                vec![AssistantContentBlock::Text(t)]
            } else {
                // 跨模型：去掉 signature
                vec![AssistantContentBlock::Text(TextContent {
                    text: t.text,
                    text_signature: None,
                })]
            }
        }

        AssistantContentBlock::ToolCall(mut tc) => {
            // 跨模型：删 thought_signature
            if !is_same_model && tc.thought_signature.is_some() {
                tc.thought_signature = None;
            }

            // 跨模型：规范化 tool call ID
            if !is_same_model {
                if let Some(normalize_fn) = normalize_tool_call_id {
                    let normalized_id = normalize_fn(&tc.id, _model, assistant);
                    if normalized_id != tc.id {
                        tool_call_id_map.insert(tc.id.clone(), normalized_id.clone());
                        tc.id = normalized_id;
                    }
                }
            }

            vec![AssistantContentBlock::ToolCall(tc)]
        }
    }
}

// ──────────────────────────────────────────────────────────────
// 3. 孤儿 tool call 补合成 result + 跳过 error/aborted
// ──────────────────────────────────────────────────────────────

fn insert_synthetic_tool_results(messages: Vec<Message>) -> Vec<Message> {
    let mut result: Vec<Message> = Vec::new();
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
    let mut existing_tool_result_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for msg in messages {
        match &msg {
            Message::Assistant(a) => {
                // flush 上一轮的孤儿 tool call
                flush_pending(&mut pending_tool_calls, &mut existing_tool_result_ids, &mut result);

                // 跳过 error/aborted
                if a.stop_reason == StopReason::Error || a.stop_reason == StopReason::Aborted {
                    continue;
                }

                // 收集本轮 tool call
                let tool_calls: Vec<ToolCall> = a.content.iter().filter_map(|b| match b {
                    AssistantContentBlock::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                }).collect();
                if !tool_calls.is_empty() {
                    pending_tool_calls = tool_calls;
                    existing_tool_result_ids.clear();
                }

                result.push(msg);
            }
            Message::ToolResult(tr) => {
                existing_tool_result_ids.insert(tr.tool_call_id.clone());
                result.push(msg);
            }
            Message::User(_) => {
                // user 中断 tool flow
                flush_pending(&mut pending_tool_calls, &mut existing_tool_result_ids, &mut result);
                result.push(msg);
            }
            _ => {
                result.push(msg);
            }
        }
    }

    // 对话末尾的孤儿 tool call
    flush_pending(&mut pending_tool_calls, &mut existing_tool_result_ids, &mut result);

    result
}

fn flush_pending(
    pending: &mut Vec<ToolCall>,
    existing: &mut std::collections::HashSet<String>,
    result: &mut Vec<Message>,
) {
    for tc in pending.drain(..) {
        if !existing.contains(&tc.id) {
            result.push(Message::ToolResult(ToolResultMessage {
                role: "toolResult".into(),
                tool_call_id: tc.id,
                tool_name: tc.name,
                content: vec![ContentBlock::Text(TextContent {
                    text: "No result provided".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: true,
                timestamp: now_ms(),
            }));
        }
    }
    existing.clear();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ──────────────────────────────────────────────────────────────
// Cross-provider handoff helpers
// ──────────────────────────────────────────────────────────────

/// Provider identifier used for cross-provider logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    Anthropic,
    OpenAI,
    Other,
}

impl ProviderKind {
    /// Detect the provider kind from the `provider` field on a model/message.
    pub fn from_provider_str(provider: &str) -> Self {
        let p = provider.to_ascii_lowercase();
        if p.contains("anthropic") {
            Self::Anthropic
        } else if p.contains("openai") {
            Self::OpenAI
        } else {
            Self::Other
        }
    }
}

// ──────────────────────────────────────────────────────────────
// 1. Thinking block degradation (Anthropic → OpenAI)
// ──────────────────────────────────────────────────────────────
//
// OpenAI does not have first-class thinking blocks. When an assistant
// message that originated from Anthropic (which produces `Thinking`
// blocks with optional signatures) is replayed against an OpenAI
// target model, every thinking block must be degraded to a plain text
// block. The signature is stripped because it is meaningless outside
// Anthropic.

/// Degrade a thinking content block to a plain text block.
/// Returns `None` if the thinking content is empty/whitespace (caller
/// should drop the block entirely in that case).
pub fn degrade_thinking_to_text(th: &ThinkingContent) -> Option<TextContent> {
    if th.thinking.trim().is_empty() {
        return None;
    }
    Some(TextContent {
        text: th.thinking.clone(),
        text_signature: None,
    })
}

/// Degrade all thinking blocks in an assistant message's content vector
/// to text blocks, suitable for providers that lack native thinking
/// support (e.g. OpenAI). Non-thinking blocks are left untouched.
pub fn degrade_all_thinking(content: &[AssistantContentBlock]) -> Vec<AssistantContentBlock> {
    content
        .iter()
        .flat_map(|block| match block {
            AssistantContentBlock::Thinking(th) => {
                match degrade_thinking_to_text(th) {
                    Some(text) => vec![AssistantContentBlock::Text(text)],
                    None => vec![], // empty thinking dropped
                }
            }
            // Text blocks: strip signature for cross-provider safety
            AssistantContentBlock::Text(t) => vec![AssistantContentBlock::Text(TextContent {
                text: t.text.clone(),
                text_signature: None,
            })],
            other => vec![other.clone()],
        })
        .collect()
}

// ──────────────────────────────────────────────────────────────
// 2. Tool call ID normalization (Anthropic ↔ OpenAI)
// ──────────────────────────────────────────────────────────────
//
// Anthropic tool call IDs look like `toolu_01ABCdef...`.
// OpenAI tool call IDs look like `call_abc123...` or, in the Responses
// API, `call_xxx|rs_yyy`.
// When transforming messages between providers the IDs need to be
// normalized so that the target provider accepts them and so that
// tool-result messages can be correlated back to the originating tool
// call.

const ANTHROPIC_TOOL_ID_PREFIX: &str = "toolu_";
const OPENAI_TOOL_ID_PREFIX: &str = "call_";

/// Convert an Anthropic-style tool call ID (`toolu_xxx`) to the OpenAI
/// format (`call_xxx`). If the input does not start with `toolu_` it
/// is returned unchanged.
pub fn anthropic_to_openai_tool_call_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix(ANTHROPIC_TOOL_ID_PREFIX) {
        format!("{OPENAI_TOOL_ID_PREFIX}{rest}")
    } else {
        id.to_string()
    }
}

/// Convert an OpenAI-style tool call ID (`call_xxx` or
/// `call_xxx|rs_yyy`) to the Anthropic format (`toolu_xxx`). If the
/// input does not start with `call_` it is returned unchanged. The
/// pipe-separated item-id suffix (Responses API) is dropped.
pub fn openai_to_anthropic_tool_call_id(id: &str) -> String {
    // Drop the Responses-API "|item_id" suffix first.
    let base = id.split('|').next().unwrap_or(id);
    if let Some(rest) = base.strip_prefix(OPENAI_TOOL_ID_PREFIX) {
        format!("{ANTHROPIC_TOOL_ID_PREFIX}{rest}")
    } else {
        base.to_string()
    }
}

/// Generic cross-provider tool-call-id normalizer that picks the
/// correct conversion direction based on the source and target
/// providers embedded in the assistant message and model.
pub fn normalize_tool_call_id_cross_provider(
    id: &str,
    target_model: &Model,
    source: &AssistantMessage,
) -> String {
    let source_kind = ProviderKind::from_provider_str(&source.provider);
    let target_kind = ProviderKind::from_provider_str(&target_model.provider);

    match (source_kind, target_kind) {
        (ProviderKind::Anthropic, ProviderKind::OpenAI) => anthropic_to_openai_tool_call_id(id),
        (ProviderKind::OpenAI, ProviderKind::Anthropic) => openai_to_anthropic_tool_call_id(id),
        _ => default_normalize_tool_call_id(id, target_model, source),
    }
}

// ──────────────────────────────────────────────────────────────
// Default ID normalizer: Anthropic requires ^[a-zA-Z0-9_-]+$ (max 64)
// ──────────────────────────────────────────────────────────────

pub fn default_normalize_tool_call_id(id: &str, _model: &Model, _source: &AssistantMessage) -> String {
    // 如果已经合规，直接返回
    let is_valid = id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if is_valid {
        return id.to_string();
    }

    // OpenAI Responses 格式：{call_id}|{item_id} — 取 call_id 部分
    if let Some((call_id, _)) = id.split_once('|') {
        if !call_id.is_empty() {
            let cleaned: String = call_id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
            if !cleaned.is_empty() {
                return cleaned.chars().take(64).collect();
            }
        }
    }

    // 兜底：hash 成合规 ID
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    id.hash(&mut hasher);
    let hash = hasher.finish();
    format!("call_{hash:x}")
}

// ──────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_model(api: &str, provider: &str, id: &str, input: Vec<String>) -> Model {
        Model {
            id: id.into(), name: id.into(),
            api: api.into(), provider: provider.into(),
            base_url: String::new(),
            reasoning: false, input,
            cost: Cost::default(),
            context_window: 128000, max_tokens: 4096,
            compat: None, headers: None,
        }
    }

    fn make_user_text(text: &str) -> Message {
        Message::User(UserMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text(TextContent { text: text.into(), text_signature: None })],
            timestamp: 0,
            source: MessageSource::Prompt,
        })
    }

    fn make_assistant_text(text: &str, model: &Model) -> Message {
        Message::Assistant(AssistantMessage {
            role: "assistant".into(),
            content: vec![AssistantContentBlock::Text(TextContent { text: text.into(), text_signature: None })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None, response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        })
    }

    #[test]
    fn downgrade_images_for_non_vision_model() {
        let model = make_model("anthropic-messages", "anthropic", "claude-3-haiku", vec!["text".into()]);
        let messages = vec![Message::User(UserMessage {
            role: "user".into(),
            content: vec![
                ContentBlock::Text(TextContent { text: "看这张图".into(), text_signature: None }),
                ContentBlock::Image(ImageContent { data: "abc".into(), mime_type: "image/png".into() }),
            ],
            timestamp: 0,
            source: MessageSource::Prompt,
        })];
        let result = transform_messages(messages, &model, None);
        match &result[0] {
            Message::User(u) => {
                assert_eq!(u.content.len(), 2);
                assert!(matches!(&u.content[0], ContentBlock::Text(t) if t.text == "看这张图"));
                assert!(matches!(&u.content[1], ContentBlock::Text(t) if t.text.contains("image omitted")));
            }
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn keep_images_for_vision_model() {
        let model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into(), "image".into()]);
        let messages = vec![Message::User(UserMessage {
            role: "user".into(),
            content: vec![
                ContentBlock::Text(TextContent { text: "看这张图".into(), text_signature: None }),
                ContentBlock::Image(ImageContent { data: "abc".into(), mime_type: "image/png".into() }),
            ],
            timestamp: 0,
            source: MessageSource::Prompt,
        })];
        let result = transform_messages(messages, &model, None);
        match &result[0] {
            Message::User(u) => assert!(u.content.iter().any(|b| matches!(b, ContentBlock::Image(_)))),
            _ => panic!("expected User"),
        }
    }

    #[test]
    fn thinking_cross_model_becomes_text() {
        let source_model = make_model("openai-completions", "openai", "gpt-5", vec!["text".into()]);
        let target_model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Thinking(ThinkingContent {
                    thinking: "Let me think...".into(),
                    thinking_signature: None,
                    redacted: None,
                }),
                AssistantContentBlock::Text(TextContent { text: "Answer".into(), text_signature: None }),
            ],
            api: source_model.api.clone(),
            provider: source_model.provider.clone(),
            model: source_model.id.clone(),
            response_model: None, response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let messages = vec![Message::Assistant(assistant)];
        let result = transform_messages(messages, &target_model, None);
        match &result[0] {
            Message::Assistant(a) => {
                assert_eq!(a.content.len(), 2);
                // thinking → text
                assert!(matches!(&a.content[0], AssistantContentBlock::Text(t) if t.text == "Let me think..."));
                // 原 text 保留
                assert!(matches!(&a.content[1], AssistantContentBlock::Text(t) if t.text == "Answer"));
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn thinking_same_model_kept() {
        let model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![AssistantContentBlock::Thinking(ThinkingContent {
                thinking: "思考内容".into(),
                thinking_signature: Some("sig_123".into()),
                redacted: None,
            })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None, response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let messages = vec![Message::Assistant(assistant)];
        let result = transform_messages(messages, &model, None);
        match &result[0] {
            Message::Assistant(a) => {
                assert_eq!(a.content.len(), 1);
                assert!(matches!(&a.content[0], AssistantContentBlock::Thinking(_)));
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn redacted_thinking_dropped_cross_model() {
        let source_model = make_model("openai-completions", "openai", "gpt-5", vec!["text".into()]);
        let target_model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Thinking(ThinkingContent {
                    thinking: "encrypted_data".into(),
                    thinking_signature: Some("sig".into()),
                    redacted: Some(true),
                }),
                AssistantContentBlock::Text(TextContent { text: "Final answer".into(), text_signature: None }),
            ],
            api: source_model.api.clone(),
            provider: source_model.provider.clone(),
            model: source_model.id.clone(),
            response_model: None, response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let messages = vec![Message::Assistant(assistant)];
        let result = transform_messages(messages, &target_model, None);
        match &result[0] {
            Message::Assistant(a) => {
                // redacted 应被丢弃，只剩 text
                assert_eq!(a.content.len(), 1);
                assert!(matches!(&a.content[0], AssistantContentBlock::Text(_)));
            }
            _ => panic!("expected Assistant"),
        }
    }

    #[test]
    fn tool_call_id_normalized_cross_provider() {
        let source_model = make_model("openai-responses", "openai", "gpt-5", vec!["text".into()]);
        let target_model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Text(TextContent { text: "查天气".into(), text_signature: None }),
                AssistantContentBlock::ToolCall(ToolCall {
                    call_type: "function".into(),
                    id: "call_abc123|rs_xyz789".into(), // OpenAI Responses 格式
                    name: "get_weather".into(),
                    arguments: serde_json::json!({"location":"北京"}),
                    thought_signature: None,
                }),
            ],
            api: source_model.api.clone(),
            provider: source_model.provider.clone(),
            model: source_model.id.clone(),
            response_model: None, response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        };
        let messages = vec![
            Message::Assistant(assistant),
            Message::ToolResult(ToolResultMessage {
                role: "toolResult".into(),
                tool_call_id: "call_abc123|rs_xyz789".into(),
                tool_name: "get_weather".into(),
                content: vec![ContentBlock::Text(TextContent { text: "晴".into(), text_signature: None })],
                details: None, is_error: false, timestamp: 0,
            }),
        ];
        let normalizer: NormalizeToolCallIdFn = Box::new(|id, _, _| default_normalize_tool_call_id(id, &Model {
            id: String::new(), name: String::new(), api: String::new(), provider: String::new(),
            base_url: String::new(), reasoning: false, input: vec![], cost: Cost::default(),
            context_window: 0, max_tokens: 0, compat: None, headers: None,
        }, &AssistantMessage {
            role: String::new(), content: vec![], api: String::new(), provider: String::new(),
            model: String::new(), response_model: None, response_id: None,
            usage: Usage::default(), stop_reason: StopReason::Stop, error_message: None, timestamp: 0,
        }));
        let result = transform_messages(messages, &target_model, Some(&normalizer));
        // assistant 的 tool_call id 应被规范化
        match &result[0] {
            Message::Assistant(a) => {
                if let Some(AssistantContentBlock::ToolCall(tc)) = a.content.last() {
                    assert_eq!(tc.id, "call_abc123"); // | 后部分被截断
                } else {
                    panic!("expected ToolCall");
                }
            }
            _ => panic!("expected Assistant"),
        }
        // tool result 的 tool_call_id 也应被映射
        match &result[1] {
            Message::ToolResult(tr) => assert_eq!(tr.tool_call_id, "call_abc123"),
            _ => panic!("expected ToolResult"),
        }
    }

    #[test]
    fn orphaned_tool_call_gets_synthetic_result() {
        let model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Text(TextContent { text: "查天气".into(), text_signature: None }),
                AssistantContentBlock::ToolCall(ToolCall {
                    call_type: "function".into(),
                    id: "call_1".into(),
                    name: "get_weather".into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }),
            ],
            api: model.api.clone(), provider: model.provider.clone(), model: model.id.clone(),
            response_model: None, response_id: None, usage: Usage::default(),
            stop_reason: StopReason::ToolUse, error_message: None, timestamp: 0,
        };
        // 没有 tool result，直接接 user
        let messages = vec![
            Message::Assistant(assistant),
            make_user_text("算了不用查了"),
        ];
        let result = transform_messages(messages, &model, None);
        // 应该在 assistant 和 user 之间插入合成 tool result
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], Message::Assistant(_)));
        assert!(matches!(&result[1], Message::ToolResult(tr) if tr.is_error));
        assert!(matches!(&result[2], Message::User(_)));
    }

    #[test]
    fn error_assistant_skipped() {
        let model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let error_assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![AssistantContentBlock::Text(TextContent { text: "partial".into(), text_signature: None })],
            api: model.api.clone(), provider: model.provider.clone(), model: model.id.clone(),
            response_model: None, response_id: None, usage: Usage::default(),
            stop_reason: StopReason::Error, error_message: Some("timeout".into()), timestamp: 0,
        };
        let messages = vec![
            make_user_text("hi"),
            Message::Assistant(error_assistant),
            make_user_text("retry"),
        ];
        let result = transform_messages(messages, &model, None);
        // error assistant 应被跳过
        assert_eq!(result.len(), 2);
        assert!(matches!(&result[0], Message::User(_)));
        assert!(matches!(&result[1], Message::User(_)));
    }

    #[test]
    fn default_normalize_id_already_valid() {
        let id = "call_abc123";
        let model = make_model("anthropic-messages", "anthropic", "claude-3", vec![]);
        let assistant = AssistantMessage {
            role: "assistant".into(), content: vec![], api: String::new(), provider: String::new(),
            model: String::new(), response_model: None, response_id: None,
            usage: Usage::default(), stop_reason: StopReason::Stop, error_message: None, timestamp: 0,
        };
        let result = default_normalize_tool_call_id(id, &model, &assistant);
        assert_eq!(result, id);
    }

    #[test]
    fn default_normalize_id_with_pipe() {
        let model = make_model("anthropic-messages", "anthropic", "claude-3", vec![]);
        let assistant = AssistantMessage {
            role: "assistant".into(), content: vec![], api: String::new(), provider: String::new(),
            model: String::new(), response_model: None, response_id: None,
            usage: Usage::default(), stop_reason: StopReason::Stop, error_message: None, timestamp: 0,
        };
        let result = default_normalize_tool_call_id("call_abc|item_xyz", &model, &assistant);
        assert_eq!(result, "call_abc");
    }

    // ──────────────────────────────────────────────────────────────
    // Cross-provider transformation tests
    // ──────────────────────────────────────────────────────────────

    /// Anthropic assistant with thinking block + signature, replayed against
    /// an OpenAI model. The thinking must become plain text (OpenAI has no
    /// first-class thinking block) and the signature must be stripped.
    #[test]
    fn thinking_anthropic_to_openai_becomes_text() {
        let source_model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let target_model = make_model("openai-completions", "openai", "gpt-5", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Thinking(ThinkingContent {
                    thinking: "Step 1: analyze input".into(),
                    thinking_signature: Some("sig_anthropic_456".into()),
                    redacted: None,
                }),
                AssistantContentBlock::Text(TextContent {
                    text: "Here is the answer".into(),
                    text_signature: Some("text_sig_789".into()),
                }),
            ],
            api: source_model.api.clone(),
            provider: source_model.provider.clone(),
            model: source_model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let messages = vec![Message::Assistant(assistant)];
        let result = transform_messages(messages, &target_model, None);
        match &result[0] {
            Message::Assistant(a) => {
                // Both blocks survive as Text; signature is dropped on cross-model.
                assert_eq!(a.content.len(), 2);
                match &a.content[0] {
                    AssistantContentBlock::Text(t) => {
                        assert_eq!(t.text, "Step 1: analyze input");
                        // Signature stripped across providers
                        assert!(t.text_signature.is_none());
                    }
                    other => panic!("expected Text, got {other:?}"),
                }
                match &a.content[1] {
                    AssistantContentBlock::Text(t) => {
                        assert_eq!(t.text, "Here is the answer");
                        assert!(t.text_signature.is_none());
                    }
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            _ => panic!("expected Assistant"),
        }
    }

    /// Tool call IDs use provider-specific prefixes: Anthropic `toolu_xxx`,
    /// OpenAI `call_xxx`. When normalizing an Anthropic `toolu_` id for an
    /// OpenAI target, the id already satisfies `^[a-zA-Z0-9_-]+$` so it is
    /// kept verbatim, but the normalizer callback still runs.
    #[test]
    fn tool_call_id_anthropic_to_openai_kept_when_valid() {
        let source_model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let target_model = make_model("openai-completions", "openai", "gpt-5", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![AssistantContentBlock::ToolCall(ToolCall {
                call_type: "function".into(),
                id: "toolu_01ABCdef".into(), // Anthropic-style id
                name: "read_file".into(),
                arguments: serde_json::json!({"path": "/tmp/x"}),
                thought_signature: Some("thought_sig".into()),
            })],
            api: source_model.api.clone(),
            provider: source_model.provider.clone(),
            model: source_model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        };
        let messages = vec![
            Message::Assistant(assistant),
            Message::ToolResult(ToolResultMessage {
                role: "toolResult".into(),
                tool_call_id: "toolu_01ABCdef".into(),
                tool_name: "read_file".into(),
                content: vec![ContentBlock::Text(TextContent { text: "content".into(), text_signature: None })],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
        ];
        // Use the default normalizer via a thin closure.
        let normalizer: NormalizeToolCallIdFn = Box::new(|id, _m, _s| {
            default_normalize_tool_call_id(
                id,
                &make_model("openai-completions", "openai", "gpt-5", vec![]),
                &AssistantMessage::new(&make_model("openai-completions", "openai", "gpt-5", vec![])),
            )
        });
        let result = transform_messages(messages, &target_model, Some(&normalizer));
        // assistant tool call id is valid, so unchanged
        match &result[0] {
            Message::Assistant(a) => match a.content.last() {
                Some(AssistantContentBlock::ToolCall(tc)) => {
                    assert_eq!(tc.id, "toolu_01ABCdef");
                    // cross-model: thought_signature stripped
                    assert!(tc.thought_signature.is_none());
                }
                other => panic!("expected ToolCall, got {other:?}"),
            },
            _ => panic!("expected Assistant"),
        }
        // tool result id mapped back to the normalized value
        match &result[1] {
            Message::ToolResult(tr) => assert_eq!(tr.tool_call_id, "toolu_01ABCdef"),
            _ => panic!("expected ToolResult"),
        }
    }

    /// Orphaned tool result: a tool result whose `tool_call_id` does not
    /// correspond to any preceding assistant tool call. It must be preserved
    /// unchanged (it is not dropped, and no synthetic assistant is inserted).
    #[test]
    fn orphaned_tool_result_preserved() {
        let model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let messages = vec![
            make_user_text("run something"),
            Message::ToolResult(ToolResultMessage {
                role: "toolResult".into(),
                tool_call_id: "toolu_orphan_999".into(),
                tool_name: "ghost_tool".into(),
                content: vec![ContentBlock::Text(TextContent {
                    text: "stray result".into(),
                    text_signature: None,
                })],
                details: None,
                is_error: false,
                timestamp: 0,
            }),
            make_assistant_text("ok", &model),
        ];
        let result = transform_messages(messages, &model, None);
        // user, tool result, assistant -> all preserved, length unchanged
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], Message::User(_)));
        match &result[1] {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "toolu_orphan_999");
                assert_eq!(tr.tool_name, "ghost_tool");
                assert!(!tr.is_error);
            }
            _ => panic!("expected ToolResult"),
        }
        assert!(matches!(&result[2], Message::Assistant(_)));
    }

    /// Image block inside a tool result is downgraded to a placeholder text
    /// block when the target model does not support images.
    #[test]
    fn tool_result_image_downgraded_for_non_vision_model() {
        let model = make_model("openai-completions", "openai", "gpt-3.5", vec!["text".into()]);
        let messages = vec![Message::ToolResult(ToolResultMessage {
            role: "toolResult".into(),
            tool_call_id: "call_img_1".into(),
            tool_name: "screenshot".into(),
            content: vec![
                ContentBlock::Text(TextContent { text: "captured".into(), text_signature: None }),
                ContentBlock::Image(ImageContent { data: "pngdata".into(), mime_type: "image/png".into() }),
            ],
            details: None,
            is_error: false,
            timestamp: 0,
        })];
        let result = transform_messages(messages, &model, None);
        match &result[0] {
            Message::ToolResult(tr) => {
                assert_eq!(tr.content.len(), 2);
                assert!(matches!(&tr.content[0], ContentBlock::Text(t) if t.text == "captured"));
                // image replaced by tool-image placeholder text
                match &tr.content[1] {
                    ContentBlock::Text(t) => assert!(t.text.contains("tool image omitted")),
                    other => panic!("expected Text placeholder, got {other:?}"),
                }
            }
            _ => panic!("expected ToolResult"),
        }
    }

    /// Conversation that ends with outstanding tool calls (no trailing
    /// result) must get synthetic error results appended for every pending
    /// tool call.
    #[test]
    fn multiple_orphaned_tool_calls_at_end() {
        let model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Text(TextContent { text: "two calls".into(), text_signature: None }),
                AssistantContentBlock::ToolCall(ToolCall {
                    call_type: "function".into(),
                    id: "call_a".into(),
                    name: "fn_a".into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }),
                AssistantContentBlock::ToolCall(ToolCall {
                    call_type: "function".into(),
                    id: "call_b".into(),
                    name: "fn_b".into(),
                    arguments: serde_json::json!({}),
                    thought_signature: None,
                }),
            ],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: 0,
        };
        let messages = vec![Message::Assistant(assistant)];
        let result = transform_messages(messages, &model, None);
        // assistant + 2 synthetic results
        assert_eq!(result.len(), 3);
        assert!(matches!(&result[0], Message::Assistant(_)));
        let ids: Vec<&str> = result[1..].iter().filter_map(|m| match m {
            Message::ToolResult(tr) => {
                assert!(tr.is_error);
                Some(tr.tool_call_id.as_str())
            }
            _ => None,
        }).collect();
        assert_eq!(ids, vec!["call_a", "call_b"]);
    }

    /// An empty (whitespace-only) thinking block must be dropped regardless
    /// of whether we stay on the same model or cross providers.
    #[test]
    fn empty_thinking_block_dropped() {
        let target_model = make_model("openai-completions", "openai", "gpt-5", vec!["text".into()]);
        let source_model = make_model("anthropic-messages", "anthropic", "claude-3-sonnet", vec!["text".into()]);
        let assistant = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Thinking(ThinkingContent {
                    thinking: "   \n  ".into(), // whitespace only
                    thinking_signature: None,
                    redacted: None,
                }),
                AssistantContentBlock::Text(TextContent { text: "real answer".into(), text_signature: None }),
            ],
            api: source_model.api.clone(),
            provider: source_model.provider.clone(),
            model: source_model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let messages = vec![Message::Assistant(assistant)];
        let result = transform_messages(messages, &target_model, None);
        match &result[0] {
            Message::Assistant(a) => {
                // empty thinking dropped, only the text block remains
                assert_eq!(a.content.len(), 1);
                assert!(matches!(&a.content[0], AssistantContentBlock::Text(t) if t.text == "real answer"));
            }
            _ => panic!("expected Assistant"),
        }
    }

    // ──────────────────────────────────────────────────────────
    // Cross-provider handoff tests
    // ──────────────────────────────────────────────────────────

    // --- Thinking block degradation ---

    #[test]
    fn degrade_thinking_to_text_basic() {
        let th = ThinkingContent {
            thinking: "Let me analyze this step by step.".into(),
            thinking_signature: Some("sig_abc123".into()),
            redacted: None,
        };
        let text = degrade_thinking_to_text(&th).expect("should produce text");
        assert_eq!(text.text, "Let me analyze this step by step.");
        assert!(text.text_signature.is_none(), "signature must be stripped");
    }

    #[test]
    fn degrade_thinking_empty_returns_none() {
        let th = ThinkingContent {
            thinking: "   \n  ".into(),
            thinking_signature: None,
            redacted: None,
        };
        assert!(degrade_thinking_to_text(&th).is_none());
    }

    #[test]
    fn degrade_all_thinking_mixed_blocks() {
        let content = vec![
            AssistantContentBlock::Thinking(ThinkingContent {
                thinking: "internal reasoning".into(),
                thinking_signature: Some("sig".into()),
                redacted: None,
            }),
            AssistantContentBlock::Text(TextContent {
                text: "public answer".into(),
                text_signature: Some("tsig".into()),
            }),
            AssistantContentBlock::Thinking(ThinkingContent {
                thinking: "".into(), // empty → dropped
                thinking_signature: None,
                redacted: None,
            }),
            AssistantContentBlock::ToolCall(ToolCall {
                call_type: "function".into(),
                id: "call_1".into(),
                name: "get_weather".into(),
                arguments: serde_json::json!({"city": "SF"}),
                thought_signature: None,
            }),
        ];

        let result = degrade_all_thinking(&content);
        assert_eq!(result.len(), 3, "empty thinking dropped, rest kept");

        // Thinking → Text
        match &result[0] {
            AssistantContentBlock::Text(t) => {
                assert_eq!(t.text, "internal reasoning");
                assert!(t.text_signature.is_none());
            }
            _ => panic!("expected Text from thinking degradation"),
        }
        // Existing Text signature stripped for cross-provider safety
        match &result[1] {
            AssistantContentBlock::Text(t) => {
                assert_eq!(t.text, "public answer");
                assert!(t.text_signature.is_none());
            }
            _ => panic!("expected Text"),
        }
        // ToolCall preserved
        assert!(matches!(&result[2], AssistantContentBlock::ToolCall(_)));
    }

    #[test]
    fn degrade_all_thinking_preserves_tool_call_intact() {
        let content = vec![AssistantContentBlock::ToolCall(ToolCall {
            call_type: "function".into(),
            id: "toolu_01xyz".into(),
            name: "fn".into(),
            arguments: serde_json::json!({}),
            thought_signature: None,
        })];
        let result = degrade_all_thinking(&content);
        if let AssistantContentBlock::ToolCall(tc) = &result[0] {
            assert_eq!(tc.id, "toolu_01xyz", "tool call id must not be mutated by thinking degradation");
        } else {
            panic!("expected ToolCall");
        }
    }

    // --- Tool call ID normalization ---

    #[test]
    fn anthropic_to_openai_id_conversion() {
        assert_eq!(
            anthropic_to_openai_tool_call_id("toolu_01ABCdef"),
            "call_01ABCdef"
        );
    }

    #[test]
    fn anthropic_to_openai_id_already_openai() {
        // Already starts with call_ — should be unchanged
        assert_eq!(
            anthropic_to_openai_tool_call_id("call_abc123"),
            "call_abc123"
        );
    }

    #[test]
    fn anthropic_to_openai_id_unrecognized_prefix() {
        // Unknown prefix — return as-is
        assert_eq!(
            anthropic_to_openai_tool_call_id("custom_xyz"),
            "custom_xyz"
        );
    }

    #[test]
    fn openai_to_anthropic_id_conversion() {
        assert_eq!(
            openai_to_anthropic_tool_call_id("call_abc123"),
            "toolu_abc123"
        );
    }

    #[test]
    fn openai_to_anthropic_id_with_responses_suffix() {
        // Responses API format: call_xxx|rs_yyy — suffix must be stripped
        assert_eq!(
            openai_to_anthropic_tool_call_id("call_abc123|rs_456"),
            "toolu_abc123"
        );
    }

    #[test]
    fn openai_to_anthropic_id_already_anthropic() {
        assert_eq!(
            openai_to_anthropic_tool_call_id("toolu_01ABCdef"),
            "toolu_01ABCdef"
        );
    }

    #[test]
    fn cross_provider_normalizer_anthropic_to_openai() {
        let source_model = make_model("anthropic-messages", "anthropic", "claude-3", vec!["text".into()]);
        let target_model = make_model("openai-completions", "openai", "gpt-5", vec!["text".into()]);
        let source = AssistantMessage::new(&source_model);
        let result = normalize_tool_call_id_cross_provider("toolu_01XYZ", &target_model, &source);
        assert_eq!(result, "call_01XYZ");
    }

    #[test]
    fn cross_provider_normalizer_openai_to_anthropic() {
        let source_model = make_model("openai-completions", "openai", "gpt-5", vec!["text".into()]);
        let target_model = make_model("anthropic-messages", "anthropic", "claude-3", vec!["text".into()]);
        let source = AssistantMessage::new(&source_model);
        let result = normalize_tool_call_id_cross_provider("call_abc|rs_99", &target_model, &source);
        assert_eq!(result, "toolu_abc");
    }

    #[test]
    fn cross_provider_normalizer_same_provider_uses_default() {
        // Same provider → falls back to default_normalize_tool_call_id
        let source_model = make_model("openai-completions", "openai", "gpt-5", vec!["text".into()]);
        let target_model = make_model("openai-completions", "openai", "gpt-4o", vec!["text".into()]);
        let source = AssistantMessage::new(&source_model);
        let result = normalize_tool_call_id_cross_provider("call_abc", &target_model, &source);
        assert_eq!(result, "call_abc", "same-provider IDs pass through default normalizer unchanged");
    }

    #[test]
    fn provider_kind_detection() {
        assert_eq!(ProviderKind::from_provider_str("anthropic"), ProviderKind::Anthropic);
        assert_eq!(ProviderKind::from_provider_str("Anthropic"), ProviderKind::Anthropic);
        assert_eq!(ProviderKind::from_provider_str("openai"), ProviderKind::OpenAI);
        assert_eq!(ProviderKind::from_provider_str("OpenAI"), ProviderKind::OpenAI);
        assert_eq!(ProviderKind::from_provider_str("custom-provider"), ProviderKind::Other);
    }

    #[test]
    fn roundtrip_tool_call_id() {
        let original = "toolu_01AbCdEf";
        let to_openai = anthropic_to_openai_tool_call_id(original);
        assert_eq!(to_openai, "call_01AbCdEf");
        let back_to_anthropic = openai_to_anthropic_tool_call_id(&to_openai);
        assert_eq!(back_to_anthropic, original, "roundtrip must restore original ID");
    }
}
