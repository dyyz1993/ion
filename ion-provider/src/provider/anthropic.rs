//! Anthropic Messages API provider (修复版)
//!
//! 对齐 pi packages/ai/src/providers/anthropic.ts
//! 协议：https://docs.anthropic.com/en/api/messages-streaming

use crate::env_keys::resolve_api_key;
use crate::error::{ProviderError, ProviderResult};
use crate::event_stream::{EventStream, EventSender};
use crate::types::*;
use crate::ApiProvider;
use async_trait::async_trait;
use futures_util::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct AnthropicMessagesProvider;

#[async_trait]
impl ApiProvider for AnthropicMessagesProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        self.stream_inner(model, context, options, cancel).await
    }
}

impl AnthropicMessagesProvider {
    async fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        let (stream, sender) = EventStream::new();

        let api_key = resolve_api_key(&model.provider, options.and_then(|o| o.api_key.clone()))?;
        let base_url = if model.base_url.is_empty() {
            "https://api.anthropic.com".to_string()
        } else {
            model.base_url.clone()
        };
        let url = format!("{}/v1/messages", base_url.trim_end_matches('/'));

        let body = build_request_body(model, context, options)?;
        let body_json = serde_json::to_string(&body).map_err(|e| ProviderError::Provider(e.to_string()))?;

        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| ProviderError::Provider(e.to_string()))?;

        let send_fut = client
            .post(&url)
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body_json)
            .send();
        // HTTP 握手期可被 cancel 取消（修复 D）
        let resp = if let Some(c) = &cancel {
            tokio::select! {
                r = send_fut => r.map_err(|e| ProviderError::Provider(e.to_string()))?,
                _ = c.cancelled() => return Err(crate::ProviderError::Stream("HTTP request aborted".into())),
            }
        } else {
            send_fut.await.map_err(|e| ProviderError::Provider(e.to_string()))?
        };

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(ProviderError::HttpError { status: status.as_u16(), body: text });
        }

        let model_clone = model.clone();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            if let Some(c) = &cancel_clone {
                tokio::select! {
                    _ = parse_sse_stream(resp, sender, &model_clone) => {}
                    _ = c.cancelled() => {
                        tracing::info!("[stream] anthropic SSE read cancelled by abort");
                    }
                }
            } else {
                parse_sse_stream(resp, sender, &model_clone).await;
            }
        });

        Ok(stream)
    }
}

// ──────────────────────────────────────────────────────────────
// 请求体构造
// ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u64,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    stream: bool,
}

#[derive(Serialize)]
struct AnthropicMessage {
    role: String,
    content: AnthropicMessageContent,
}

#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicMessageContent {
    Text(String),
    Blocks(Vec<AnthropicContentBlock>),
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { source: AnthropicImageSource },
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String, input: serde_json::Value },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(skip_serializing_if = "is_false")]
        is_error: bool,
    },
    #[serde(rename = "thinking")]
    Thinking { thinking: String },
}

#[derive(Serialize)]
struct AnthropicImageSource {
    #[serde(rename = "type")]
    source_type: String,
    media_type: String,
    data: String,
}

#[derive(Serialize)]
struct AnthropicTool {
    name: String,
    description: String,
    input_schema: serde_json::Value,
}

fn is_false(b: &bool) -> bool { !*b }

fn build_request_body(
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
) -> ProviderResult<AnthropicRequest> {
    let max_tokens = options
        .and_then(|o| o.max_tokens)
        .or(Some(model.max_tokens))
        .unwrap_or(4096);

    let system = context.system_prompt.clone();
    let mut messages: Vec<AnthropicMessage> = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::User(u) => {
                let content = convert_user_message(u);
                messages.push(AnthropicMessage { role: "user".into(), content });
            }
            Message::Assistant(a) => {
                let blocks = convert_assistant_message(a);
                if !blocks.is_empty() {
                    messages.push(AnthropicMessage {
                        role: "assistant".into(),
                        content: AnthropicMessageContent::Blocks(blocks),
                    });
                }
            }
            Message::ToolResult(tr) => {
                let content_text = tr.content.iter().filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                }).collect::<Vec<_>>().join("\n");
                let block = AnthropicContentBlock::ToolResult {
                    tool_use_id: tr.tool_call_id.clone(),
                    content: content_text,
                    is_error: tr.is_error,
                };
                messages.push(AnthropicMessage {
                    role: "user".into(),
                    content: AnthropicMessageContent::Blocks(vec![block]),
                });
            }
            Message::BashExecution(b) => {
                messages.push(AnthropicMessage {
                    role: "user".into(),
                    content: AnthropicMessageContent::Text(format!("$ {}\n{}", b.command, b.output)),
                });
            }
            Message::Custom(c) => {
                let has_image = match &c.content {
                    CustomContent::Text(_) => false,
                    CustomContent::Blocks(blocks) => blocks.iter().any(|b| matches!(b, ContentBlock::Image(_))),
                };
                if has_image {
                    // When an image is present, emit content blocks array
                    let blocks: Vec<AnthropicContentBlock> = match &c.content {
                        CustomContent::Blocks(blocks) => blocks.iter().filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(AnthropicContentBlock::Text { text: t.text.clone() }),
                            ContentBlock::Image(img) => Some(AnthropicContentBlock::Image {
                                source: AnthropicImageSource {
                                    source_type: "base64".into(),
                                    media_type: img.mime_type.clone(),
                                    data: img.data.clone(),
                                },
                            }),
                        }).collect(),
                        _ => vec![],
                    };
                    messages.push(AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicMessageContent::Blocks(blocks),
                    });
                } else {
                    // Text-only: join into a single string
                    let text = match &c.content {
                        CustomContent::Text(s) => s.clone(),
                        CustomContent::Blocks(blocks) => blocks.iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    messages.push(AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicMessageContent::Text(text),
                    });
                }
            }
            Message::BranchSummary(bs) => {
                messages.push(AnthropicMessage {
                    role: "assistant".into(),
                    content: AnthropicMessageContent::Text(bs.summary.clone()),
                });
            }
            Message::CompactionSummary(cs) => {
                messages.push(AnthropicMessage {
                    role: "assistant".into(),
                    content: AnthropicMessageContent::Text(cs.summary.clone()),
                });
            }
        }
    }

    let tools: Vec<AnthropicTool> = context.tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|td| AnthropicTool {
            name: td.name.clone(),
            description: td.description.clone(),
            input_schema: td.parameters.clone(),
        })
        .collect();

    Ok(AnthropicRequest {
        model: model.id.clone(),
        max_tokens,
        messages,
        system,
        tools,
        stream: true,
    })
}

fn convert_user_message(u: &UserMessage) -> AnthropicMessageContent {
    let has_image = u.content.iter().any(|b| matches!(b, ContentBlock::Image(_)));
    if !has_image {
        let text = u.content.iter().filter_map(|b| match b {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        }).collect::<Vec<_>>().join("\n");
        AnthropicMessageContent::Text(text)
    } else {
        let blocks: Vec<AnthropicContentBlock> = u.content.iter().map(|b| match b {
            ContentBlock::Text(t) => AnthropicContentBlock::Text { text: t.text.clone() },
            ContentBlock::Image(img) => AnthropicContentBlock::Image {
                source: AnthropicImageSource {
                    source_type: "base64".into(),
                    media_type: img.mime_type.clone(),
                    data: img.data.clone(),
                },
            },
        }).collect();
        AnthropicMessageContent::Blocks(blocks)
    }
}

fn convert_assistant_message(a: &AssistantMessage) -> Vec<AnthropicContentBlock> {
    a.content.iter().filter_map(|b| match b {
        AssistantContentBlock::Text(t) => Some(AnthropicContentBlock::Text { text: t.text.clone() }),
        AssistantContentBlock::Thinking(th) => Some(AnthropicContentBlock::Thinking { thinking: th.thinking.clone() }),
        AssistantContentBlock::ToolCall(tc) => Some(AnthropicContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input: tc.arguments.clone(),
        }),
    }).collect()
}

// ──────────────────────────────────────────────────────────────
// SSE 流解析
// ──────────────────────────────────────────────────────────────

async fn parse_sse_stream(
    resp: reqwest::Response,
    sender: EventSender,
    model: &Model,
) {
    let mut output = AssistantMessage::new(model);
    let mut byte_stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut content_index: usize = 0;
    let mut current_block: Option<BlockState> = None;
    let mut stop_reason = StopReason::Stop;

    // Start 事件
    let _ = sender.send(StreamEvent::Start { partial: output.clone() }).await;

    let parse_result: ProviderResult<()> = async {
        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => { println!("[anthropic-debug] chunk error: {e}"); return Err(ProviderError::Stream(e.to_string())); }
            };
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);
            let _find_result = buffer.find("\n\n");

            while let Some(pos) = buffer.find("\n\n") {
                let event_str = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                if let Some((event_type, data)) = parse_sse_event(&event_str) {
                    match event_type.as_str() {
                        "message_start" => {}
                        "content_block_start" => {
                            if let Ok(block_start) = serde_json::from_str::<ContentBlockStart>(&data) {
                                let block = block_start.content_block;
                                match block.block_type.as_str() {
                                    "text" => {
                                        let _ = sender.send(StreamEvent::TextStart {
                                            content_index,
                                            partial: output.clone(),
                                        }).await;
                                        current_block = Some(BlockState::Text { text: String::new() });
                                    }
                                    "tool_use" => {
                                        let id = block.id.unwrap_or_default();
                                        let name = block.name.unwrap_or_default();
                                        let _ = sender.send(StreamEvent::ToolCallStart {
                                            content_index,
                                            partial: output.clone(),
                                        }).await;
                                        current_block = Some(BlockState::ToolUse {
                                            id,
                                            name,
                                            partial_json: String::new(),
                                        });
                                    }
                                    "thinking" => {
                                        let _ = sender.send(StreamEvent::ThinkingStart {
                                            content_index,
                                            partial: output.clone(),
                                        }).await;
                                        current_block = Some(BlockState::Thinking {
                                            thinking: String::new(),
                                            signature: None,
                                        });
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Ok(delta) = serde_json::from_str::<ContentBlockDelta>(&data) {
                                match &delta.delta {
                                    DeltaPayload::TextDelta { text } => {
                                        if let Some(BlockState::Text { text: t }) = &mut current_block {
                                            t.push_str(text);
                                            if let Some(AssistantContentBlock::Text(tc)) = output.content.last_mut() {
                                                tc.text.push_str(text);
                                            }
                                        }
                                        let _ = sender.send(StreamEvent::TextDelta {
                                            content_index,
                                            delta: text.clone(),
                                            partial: output.clone(),
                                        }).await;
                                    }
                                    DeltaPayload::InputJsonDelta { partial_json } => {
                                        if let Some(BlockState::ToolUse { partial_json: pj, .. }) = &mut current_block {
                                            pj.push_str(partial_json);
                                        }
                                        let _ = sender.send(StreamEvent::ToolCallDelta {
                                            content_index,
                                            delta: partial_json.clone(),
                                            partial: output.clone(),
                                        }).await;
                                    }
                                    DeltaPayload::ThinkingDelta { thinking } => {
                                        if let Some(BlockState::Thinking { thinking: t, .. }) = &mut current_block {
                                            t.push_str(thinking);
                                            if let Some(AssistantContentBlock::Thinking(tc)) = output.content.last_mut() {
                                                tc.thinking.push_str(thinking);
                                            }
                                        }
                                        let _ = sender.send(StreamEvent::ThinkingDelta {
                                            content_index,
                                            delta: thinking.clone(),
                                            partial: output.clone(),
                                        }).await;
                                    }
                                    DeltaPayload::SignatureDelta { signature } => {
                                        if let Some(BlockState::Thinking { signature: s, .. }) = &mut current_block {
                                            *s = Some(signature.clone());
                                            if let Some(AssistantContentBlock::Thinking(tc)) = output.content.last_mut() {
                                                tc.thinking_signature = Some(signature.clone());
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                        "content_block_stop" => {
                            match current_block.take() {
                                Some(BlockState::Text { text }) => {
                                    output.content.push(AssistantContentBlock::Text(TextContent {
                                        text: text.clone(),
                                        text_signature: None,
                                    }));
                                    let _ = sender.send(StreamEvent::TextEnd {
                                        content_index,
                                        content: text,
                                        partial: output.clone(),
                                    }).await;
                                    content_index += 1;
                                }
                                Some(BlockState::ToolUse { id, name, partial_json }) => {
                                    let arguments = parse_json_repair(&partial_json);
                                    let tool_call = ToolCall {
                                        call_type: "function".into(),
                                        id: id.clone(),
                                        name: name.clone(),
                                        arguments: arguments.clone(),
                                        thought_signature: None,
                                    };
                                    output.content.push(AssistantContentBlock::ToolCall(tool_call.clone()));
                                    let _ = sender.send(StreamEvent::ToolCallEnd {
                                        content_index,
                                        tool_call,
                                        partial: output.clone(),
                                    }).await;
                                    content_index += 1;
                                }
                                Some(BlockState::Thinking { thinking, signature }) => {
                                    output.content.push(AssistantContentBlock::Thinking(ThinkingContent {
                                        thinking: thinking.clone(),
                                        thinking_signature: signature,
                                        redacted: None,
                                    }));
                                    let _ = sender.send(StreamEvent::ThinkingEnd {
                                        content_index,
                                        content: thinking,
                                        partial: output.clone(),
                                    }).await;
                                    content_index += 1;
                                }
                                None => {}
                            }
                        }
                        "message_delta" => {
                            if let Ok(msg_delta) = serde_json::from_str::<MessageDelta>(&data) {
                                if let Some(sr) = msg_delta.delta.stop_reason {
                                    stop_reason = match sr.as_str() {
                                        "end_turn" | "stop_sequence" => StopReason::Stop,
                                        "max_tokens" => StopReason::Length,
                                        "tool_use" => StopReason::ToolUse,
                                        _ => StopReason::Stop,
                                    };
                                }
                                if let Some(usage) = msg_delta.usage {
                                    output.usage.output = usage.output_tokens;
                                }
                            }
                        }
                        "message_stop" => {}
                        "error" => {
                            if let Ok(err) = serde_json::from_str::<AnthropicError>(&data) {
                                return Err(ProviderError::Provider(err.error.message));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }.await;

    match parse_result {
        Ok(()) => {
            output.stop_reason = stop_reason;
            sender.end(output);
        }
        Err(e) => {
            output.error_message = Some(e.to_string());
            output.stop_reason = StopReason::Error;
            sender.error(StopReason::Error, output);
        }
    }
}

enum BlockState {
    Text { text: String },
    ToolUse { id: String, name: String, partial_json: String },
    Thinking { thinking: String, signature: Option<String> },
}

fn parse_sse_event(event_str: &str) -> Option<(String, String)> {
    let mut event_type = String::new();
    let mut data = String::new();

    for line in event_str.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data: ") {
            data.push_str(rest);
        } else if line.starts_with("data:") {
            data.push_str(&line[5..]);
        }
    }

    if event_type.is_empty() && data.is_empty() {
        return None;
    }
    Some((event_type, data))
}

/// JSON 容错解析：partial JSON 也能解析出部分结果
fn parse_json_repair(s: &str) -> serde_json::Value {
    if s.is_empty() {
        return serde_json::json!({});
    }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
        return v;
    }
    let mut repaired = s.to_string();
    let mut open_braces = 0i32;
    let mut open_brackets = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for c in repaired.chars() {
        if escape { escape = false; continue; }
        if c == '\\' { escape = true; continue; }
        if c == '"' { in_string = !in_string; continue; }
        if in_string { continue; }
        match c {
            '{' => open_braces += 1,
            '}' => open_braces -= 1,
            '[' => open_brackets += 1,
            ']' => open_brackets -= 1,
            _ => {}
        }
    }
    if in_string { repaired.push('"'); }
    for _ in 0..open_brackets.max(0) { repaired.push(']'); }
    for _ in 0..open_braces.max(0) { repaired.push('}'); }
    serde_json::from_str(&repaired).unwrap_or(serde_json::json!({}))
}

// ──────────────────────────────────────────────────────────────
// SSE 事件类型反序列化
// ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ContentBlockStart {
    #[allow(dead_code)]
    index: u32,
    content_block: ContentBlockInfo,
}

#[derive(Deserialize)]
struct ContentBlockInfo {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    text: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    thinking: Option<String>,
}

#[derive(Deserialize)]
struct ContentBlockDelta {
    #[allow(dead_code)]
    index: u32,
    delta: DeltaPayload,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum DeltaPayload {
    #[serde(rename = "text_delta")]
    TextDelta { text: String },
    #[serde(rename = "input_json_delta")]
    InputJsonDelta { partial_json: String },
    #[serde(rename = "thinking_delta")]
    ThinkingDelta { thinking: String },
    #[serde(rename = "signature_delta")]
    SignatureDelta { signature: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageDelta {
    delta: MessageDeltaInner,
    #[serde(default)]
    usage: Option<MessageDeltaUsage>,
}

#[derive(Deserialize)]
struct MessageDeltaInner {
    #[serde(default)]
    stop_reason: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    stop_sequence: Option<String>,
}

#[derive(Deserialize)]
struct MessageDeltaUsage {
    output_tokens: u64,
}

#[derive(Deserialize)]
struct AnthropicError {
    error: AnthropicErrorDetail,
}

#[derive(Deserialize)]
struct AnthropicErrorDetail {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    error_type: String,
    message: String,
}

// ──────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_event_basic() {
        let event = "event: message_start\ndata: {\"type\":\"message_start\"}";
        let (et, data) = parse_sse_event(event).unwrap();
        assert_eq!(et, "message_start");
        assert!(data.contains("message_start"));
    }

    #[test]
    fn parse_sse_event_multiline_data() {
        let event = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}";
        let (et, data) = parse_sse_event(event).unwrap();
        assert_eq!(et, "content_block_delta");
        let delta: ContentBlockDelta = serde_json::from_str(&data).unwrap();
        match delta.delta {
            DeltaPayload::TextDelta { text } => assert_eq!(text, "Hello"),
            _ => panic!("expected TextDelta"),
        }
    }

    #[test]
    fn json_repair_complete() {
        let v = parse_json_repair(r#"{"key":"value"}"#);
        assert_eq!(v["key"], "value");
    }

    #[test]
    fn json_repair_truncated_object() {
        let v = parse_json_repair(r#"{"location":"北京"#);
        assert_eq!(v["location"], "北京");
    }

    #[test]
    fn json_repair_truncated_with_brackets() {
        let v = parse_json_repair(r#"{"list":[1,2"#);
        assert_eq!(v["list"][0], 1);
        assert_eq!(v["list"][1], 2);
    }

    #[test]
    fn json_repair_empty_string() {
        let v = parse_json_repair("");
        assert_eq!(v, serde_json::json!({}));
    }

    fn make_test_model() -> Model {
        Model {
            id: "claude-3-5-sonnet-20241022".into(),
            name: "Claude 3.5 Sonnet".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "".into(),
            reasoning: false,
            input: vec!["text".into()],
            cost: Cost::default(),
            context_window: 200000,
            max_tokens: 8192,
            compat: None,
            headers: None,
        }
    }

    #[test]
    fn build_request_body_basic() {
        let model = make_test_model();
        let ctx = Context::new(
            Some("You are helpful".into()),
            vec![Message::User(UserMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text(TextContent { text: "Hello".into(), text_signature: None })],
                timestamp: 0,
                source: MessageSource::Prompt,
            })],
        );
        let body = build_request_body(&model, &ctx, None).unwrap();
        assert_eq!(body.model, "claude-3-5-sonnet-20241022");
        assert_eq!(body.system, Some("You are helpful".into()));
        assert_eq!(body.messages.len(), 1);
        assert!(body.stream);
    }

    #[test]
    fn build_request_body_with_tools() {
        let model = make_test_model();
        let ctx = Context {
            system_prompt: None,
            messages: vec![],
            tools: Some(vec![ToolDef {
                name: "get_weather".into(),
                description: "Get weather".into(),
                parameters: serde_json::json!({"type":"object","properties":{"location":{"type":"string"}}}),
            }]),
        };
        let body = build_request_body(&model, &ctx, None).unwrap();
        assert_eq!(body.tools.len(), 1);
        assert_eq!(body.tools[0].name, "get_weather");
    }

    #[test]
    fn convert_user_message_with_image() {
        let u = UserMessage {
            role: "user".into(),
            content: vec![
                ContentBlock::Text(TextContent { text: "What's this?".into(), text_signature: None }),
                ContentBlock::Image(ImageContent { data: "base64data".into(), mime_type: "image/png".into() }),
            ],
            timestamp: 0,
            source: MessageSource::Prompt,
        };
        let content = convert_user_message(&u);
        match content {
            AnthropicMessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                assert!(matches!(blocks[0], AnthropicContentBlock::Text { .. }));
                assert!(matches!(blocks[1], AnthropicContentBlock::Image { .. }));
            }
            _ => panic!("expected Blocks"),
        }
    }
}
