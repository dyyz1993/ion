//! Google Generative AI (Gemini) provider
//!
//! 对齐 pi packages/ai/src/providers/google.ts
//! 协议：https://ai.google.dev/api/generate-content
//! 端点：POST /v1beta/models/{model}:streamGenerateContent?alt=sse

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

pub struct GoogleGenerativeAIProvider;

#[async_trait]
impl ApiProvider for GoogleGenerativeAIProvider {
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

impl GoogleGenerativeAIProvider {
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
            "https://generativelanguage.googleapis.com".to_string()
        } else {
            model.base_url.clone()
        };
        let url = format!(
            "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
            base_url.trim_end_matches('/'),
            model.id
        );

        let body = build_request_body(model, context, options)?;
        let body_json = serde_json::to_string(&body).map_err(|e| ProviderError::Provider(e.to_string()))?;

        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| ProviderError::Provider(e.to_string()))?;

        let send_fut = client
            .post(&url)
            .header("x-goog-api-key", &api_key)
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
                        tracing::info!("[stream] google SSE read cancelled by abort");
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
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GeminiToolDeclaration>,
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GeminiPart {
    Text { text: String },
    Thought { text: String, thought: bool },
    FunctionCall { function_call: GeminiFunctionCall },
    FunctionResponse { function_response: GeminiFunctionResponse },
    InlineData { inline_data: GeminiInlineData },
}

#[derive(Serialize)]
struct GeminiInlineData {
    mime_type: String,
    data: String, // base64
}

#[derive(Serialize)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Serialize)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
}

#[derive(Serialize)]
struct GeminiToolDeclaration {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Serialize)]
struct GenerationConfig {
    max_output_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_config: Option<ThinkingConfig>,
}

#[derive(Serialize)]
struct ThinkingConfig {
    include_thoughts: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking_budget: Option<i32>,
}

fn build_request_body(
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
) -> ProviderResult<GeminiRequest> {
    let max_output_tokens = options
        .and_then(|o| o.max_tokens)
        .or(Some(model.max_tokens))
        .unwrap_or(8192);

    let system_instruction = context.system_prompt.as_ref().map(|s| GeminiContent {
        role: "user".into(),
        parts: vec![GeminiPart::Text { text: s.clone() }],
    });

    let mut contents: Vec<GeminiContent> = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::User(u) => {
                let parts: Vec<GeminiPart> = u.content.iter().filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(GeminiPart::Text { text: t.text.clone() }),
                    ContentBlock::Image(img) => Some(GeminiPart::InlineData {
                        inline_data: GeminiInlineData {
                            mime_type: img.mime_type.clone(),
                            data: img.data.clone(),
                        }
                    }),
                }).collect();
                if !parts.is_empty() {
                    contents.push(GeminiContent { role: "user".into(), parts });
                }
            }
            Message::Assistant(a) => {
                let mut parts: Vec<GeminiPart> = Vec::new();
                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(t) => {
                            parts.push(GeminiPart::Text { text: t.text.clone() });
                        }
                        AssistantContentBlock::Thinking(th) => {
                            if !th.thinking.is_empty() {
                                parts.push(GeminiPart::Thought {
                                    text: th.thinking.clone(),
                                    thought: true,
                                });
                            }
                        }
                        AssistantContentBlock::ToolCall(tc) => {
                            parts.push(GeminiPart::FunctionCall {
                                function_call: GeminiFunctionCall {
                                    name: tc.name.clone(),
                                    args: tc.arguments.clone(),
                                    id: Some(tc.id.clone()),
                                },
                            });
                        }
                    }
                }
                if !parts.is_empty() {
                    contents.push(GeminiContent { role: "model".into(), parts });
                }
            }
            Message::ToolResult(tr) => {
                let text = tr.content.iter().filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                }).collect::<Vec<_>>().join("\n");
                contents.push(GeminiContent {
                    role: "user".into(),
                    parts: vec![GeminiPart::FunctionResponse {
                        function_response: GeminiFunctionResponse {
                            name: tr.tool_name.clone(),
                            response: serde_json::json!({ "output": text }),
                            id: Some(tr.tool_call_id.clone()),
                        },
                    }],
                });
            }
            Message::BashExecution(b) => {
                if b.exclude_from_context == Some(true) { continue; }
                let text = format!("$ {}\n{}", b.command, b.output);
                contents.push(GeminiContent {
                    role: "user".into(),
                    parts: vec![GeminiPart::Text { text }],
                });
            }
            Message::Custom(c) => {
                let has_image = match &c.content {
                    CustomContent::Text(_) => false,
                    CustomContent::Blocks(blocks) => blocks.iter().any(|b| matches!(b, ContentBlock::Image(_))),
                };
                if has_image {
                    // When an image is present, emit parts array with inline_data
                    let parts: Vec<GeminiPart> = match &c.content {
                        CustomContent::Blocks(blocks) => blocks.iter().filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(GeminiPart::Text { text: t.text.clone() }),
                            ContentBlock::Image(img) => Some(GeminiPart::InlineData {
                                inline_data: GeminiInlineData {
                                    mime_type: img.mime_type.clone(),
                                    data: img.data.clone(),
                                }
                            }),
                        }).collect(),
                        _ => vec![],
                    };
                    if !parts.is_empty() {
                        contents.push(GeminiContent { role: "user".into(), parts });
                    }
                } else {
                    // Text-only: join into a single string
                    let text = match &c.content {
                        CustomContent::Text(s) => s.clone(),
                        CustomContent::Blocks(blocks) => blocks.iter().filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        }).collect::<Vec<_>>().join("\n"),
                    };
                    contents.push(GeminiContent {
                        role: "user".into(),
                        parts: vec![GeminiPart::Text { text }],
                    });
                }
            }
            Message::BranchSummary(bs) => {
                contents.push(GeminiContent {
                    role: "user".into(),
                    parts: vec![GeminiPart::Text { text: format!("[Branch summary]\n{}", bs.summary) }],
                });
            }
            Message::CompactionSummary(cs) => {
                contents.push(GeminiContent {
                    role: "user".into(),
                    parts: vec![GeminiPart::Text { text: format!("[Compaction summary]\n{}", cs.summary) }],
                });
            }
        }
    }

    // Tools
    let tools: Vec<GeminiToolDeclaration> = if let Some(tool_defs) = context.tools.as_ref() {
        if tool_defs.is_empty() {
            vec![]
        } else {
            let decls: Vec<GeminiFunctionDeclaration> = tool_defs.iter().map(|t| {
                GeminiFunctionDeclaration {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                }
            }).collect();
            vec![GeminiToolDeclaration { function_declarations: decls }]
        }
    } else {
        vec![]
    };

    // Thinking config
    let thinking_config = if model.reasoning {
        let level = options.and_then(|o| o.reasoning.clone());
        let budget = match level {
            Some(ThinkingLevel::Off) => Some(0),
            Some(ThinkingLevel::Minimal) => Some(1024),
            Some(ThinkingLevel::Low) => Some(4096),
            Some(ThinkingLevel::Medium) => Some(8192),
            Some(ThinkingLevel::High) => Some(24576),
            Some(ThinkingLevel::XHigh) => Some(32768),
            None => None, // dynamic
        };
        // Off → disabled (budget=0), else include thoughts
        if matches!(level, Some(ThinkingLevel::Off)) {
            Some(ThinkingConfig { include_thoughts: false, thinking_budget: Some(0) })
        } else {
            Some(ThinkingConfig { include_thoughts: true, thinking_budget: budget })
        }
    } else {
        None
    };

    Ok(GeminiRequest {
        contents,
        system_instruction,
        tools,
        generation_config: GenerationConfig {
            max_output_tokens,
            temperature: None,
            thinking_config,
        },
    })
}

// ──────────────────────────────────────────────────────────────
// SSE 流解析
// ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiStreamChunk {
    #[serde(default)]
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: Option<GeminiContentResp>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiContentResp {
    #[serde(default)]
    parts: Vec<GeminiPartResp>,
    #[serde(default)]
    #[allow(dead_code)]
    role: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GeminiPartResp {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(default)]
    thought_signature: Option<String>,
    #[serde(default)]
    function_call: Option<GeminiFunctionCallResp>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GeminiFunctionCallResp {
    name: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: u64,
    #[serde(default)]
    candidates_token_count: u64,
    #[serde(default)]
    total_token_count: u64,
}

async fn parse_sse_stream(
    resp: reqwest::Response,
    sender: EventSender,
    model: &Model,
) {
    let mut output = AssistantMessage::new(model);
    let mut byte_stream = resp.bytes_stream();
    let mut buffer = String::new();
    let mut stop_reason = StopReason::Stop;

    // Current block being built
    let mut current_block: Option<GeminiBlockKind> = None;
    let mut current_text = String::new();
    let mut current_thinking = String::new();
    let mut current_thought_signature: Option<String> = None;

    let _ = sender.push(StreamEvent::Start { partial: output.clone() });

    let parse_result: ProviderResult<()> = async {
        while let Some(chunk_result) = byte_stream.next().await {
            let chunk = chunk_result.map_err(|e| ProviderError::Stream(e.to_string()))?;
            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(pos) = buffer.find("\n\n") {
                let event_str = buffer[..pos].to_string();
                buffer = buffer[pos + 2..].to_string();

                // Extract data: lines
                let mut data_str = String::new();
                for line in event_str.lines() {
                    if let Some(rest) = line.strip_prefix("data: ") {
                        data_str.push_str(rest);
                    } else if line.starts_with("data:") {
                        data_str.push_str(&line[5..]);
                    }
                }
                if data_str.is_empty() { continue; }

                let chunk_data: GeminiStreamChunk = match serde_json::from_str(&data_str) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                // Usage
                if let Some(usage) = chunk_data.usage_metadata {
                    output.usage.input = usage.prompt_token_count;
                    output.usage.output = usage.candidates_token_count;
                    output.usage.total_tokens = usage.total_token_count;
                }

                for candidate in chunk_data.candidates {
                    // finish_reason
                    if let Some(ref reason) = candidate.finish_reason {
                        stop_reason = match reason.as_str() {
                            "STOP" => StopReason::Stop,
                            "MAX_TOKENS" => StopReason::Length,
                            "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => StopReason::Stop,
                            _ => StopReason::Stop,
                        };
                    }

                    if let Some(content) = candidate.content {
                        for part in content.parts {
                            let is_thought = part.thought.unwrap_or(false);

                            // Function call
                            if let Some(fc) = part.function_call {
                                // Close current text/thinking block if any
                                close_current_block(&mut current_block, &mut output, &current_text, &current_thinking, &current_thought_signature, &sender);

                                let id = fc.id.unwrap_or_else(|| format!("call_{}", output.content.len()));
                                let args = fc.args.unwrap_or(serde_json::json!({}));
                                let tool_call = ToolCall {
                                    call_type: "function".into(),
                                    id: id.clone(),
                                    name: fc.name.clone(),
                                    arguments: args,
                                    thought_signature: part.thought_signature.clone(),
                                };
                                let content_index = output.content.len();
                                output.content.push(AssistantContentBlock::ToolCall(tool_call.clone()));
                                let _ = sender.send(StreamEvent::ToolCallStart {
                                    content_index,
                                    partial: output.clone(),
                                }).await;
                                let _ = sender.send(StreamEvent::ToolCallEnd {
                                    content_index,
                                    tool_call,
                                    partial: output.clone(),
                                }).await;
                                continue;
                            }

                            // Text or thought
                            if let Some(text) = part.text {
                                if is_thought {
                                    // Switch to thinking block
                                    if !matches!(current_block, Some(GeminiBlockKind::Thinking)) {
                                        close_current_block(&mut current_block, &mut output, &current_text, &current_thinking, &current_thought_signature, &sender);
                                        current_thinking.clear();
                                        current_thought_signature = None;
                                        output.content.push(AssistantContentBlock::Thinking(ThinkingContent {
                                            thinking: String::new(),
                                            thinking_signature: None,
                                            redacted: None,
                                        }));
                                        current_block = Some(GeminiBlockKind::Thinking);
                                        let content_index = output.content.len() - 1;
                                        let _ = sender.send(StreamEvent::ThinkingStart {
                                            content_index,
                                            partial: output.clone(),
                                        }).await;
                                    }
                                    current_thinking.push_str(&text);
                                    if let Some(sig) = part.thought_signature.as_ref() {
                                        current_thought_signature = Some(sig.clone());
                                    }
                                    if let Some(AssistantContentBlock::Thinking(th)) = output.content.last_mut() {
                                        th.thinking.push_str(&text);
                                        if let Some(sig) = current_thought_signature.as_ref() {
                                            th.thinking_signature = Some(sig.clone());
                                        }
                                    }
                                    let content_index = output.content.len() - 1;
                                    let _ = sender.send(StreamEvent::ThinkingDelta {
                                        content_index,
                                        delta: text,
                                        partial: output.clone(),
                                    }).await;
                                } else {
                                    // Text block
                                    if !matches!(current_block, Some(GeminiBlockKind::Text)) {
                                        close_current_block(&mut current_block, &mut output, &current_text, &current_thinking, &current_thought_signature, &sender);
                                        current_text.clear();
                                        output.content.push(AssistantContentBlock::Text(TextContent {
                                            text: String::new(),
                                            text_signature: None,
                                        }));
                                        current_block = Some(GeminiBlockKind::Text);
                                        let content_index = output.content.len() - 1;
                                        let _ = sender.send(StreamEvent::TextStart {
                                            content_index,
                                            partial: output.clone(),
                                        }).await;
                                    }
                                    current_text.push_str(&text);
                                    if let Some(AssistantContentBlock::Text(t)) = output.content.last_mut() {
                                        t.text.push_str(&text);
                                    }
                                    let content_index = output.content.len() - 1;
                                    let _ = sender.send(StreamEvent::TextDelta {
                                        content_index,
                                        delta: text,
                                        partial: output.clone(),
                                    }).await;
                                }
                            }
                        }
                    }
                }
            }
        }
        // Close any remaining block before returning
        close_current_block(&mut current_block, &mut output, &current_text, &current_thinking, &current_thought_signature, &sender);
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

#[derive(Clone, Copy)]
enum GeminiBlockKind {
    Text,
    Thinking,
}

fn close_current_block(
    current_block: &mut Option<GeminiBlockKind>,
    output: &mut AssistantMessage,
    current_text: &String,
    current_thinking: &String,
    current_thought_signature: &Option<String>,
    sender: &EventSender,
) {
    if let Some(kind) = current_block.take() {
        let content_index = output.content.len().saturating_sub(1);
        match kind {
            GeminiBlockKind::Text => {
                if let Some(AssistantContentBlock::Text(t)) = output.content.last() {
                    let _ = sender.push(StreamEvent::TextEnd {
                        content_index,
                        content: t.text.clone(),
                        partial: output.clone(),
                    });
                }
            }
            GeminiBlockKind::Thinking => {
                if let Some(AssistantContentBlock::Thinking(th)) = output.content.last_mut() {
                    if let Some(sig) = current_thought_signature.as_ref() {
                        th.thinking_signature = Some(sig.clone());
                    }
                }
                if let Some(AssistantContentBlock::Thinking(th)) = output.content.last() {
                    let _ = sender.push(StreamEvent::ThinkingEnd {
                        content_index,
                        content: th.thinking.clone(),
                        partial: output.clone(),
                    });
                }
            }
        }
    }
    let _ = current_text;
    let _ = current_thinking;
}

// ──────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_model() -> Model {
        Model {
            id: "gemini-2.5-pro".into(),
            name: "Gemini 2.5 Pro".into(),
            api: "google-generative-ai".into(),
            provider: "google".into(),
            base_url: "".into(),
            reasoning: true,
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
        assert_eq!(body.contents.len(), 1);
        assert!(body.system_instruction.is_some());
        assert!(body.generation_config.thinking_config.is_some());
        assert!(body.generation_config.thinking_config.as_ref().unwrap().include_thoughts);
    }

    #[test]
    fn build_request_body_thinking_off() {
        let model = make_test_model();
        let ctx = Context::new(None, vec![]);
        let body = build_request_body(&model, &ctx, Some(&StreamOptions {
            max_tokens: None,
            api_key: None,
            reasoning: Some(ThinkingLevel::Off),
            timeout_ms: None,
            max_retries: None,
            response_format: None,
        })).unwrap();
        let tc = body.generation_config.thinking_config.unwrap();
        assert!(!tc.include_thoughts);
        assert_eq!(tc.thinking_budget, Some(0));
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
        assert_eq!(body.tools[0].function_declarations.len(), 1);
    }

    #[test]
    fn parse_sse_data_basic() {
        let data = r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}],"role":"model"},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":10,"candidatesTokenCount":5,"totalTokenCount":15}}"#;
        let chunk: GeminiStreamChunk = serde_json::from_str(data).unwrap();
        assert_eq!(chunk.candidates.len(), 1);
        assert!(chunk.usage_metadata.is_some());
        let u = chunk.usage_metadata.unwrap();
        assert_eq!(u.prompt_token_count, 10);
    }
}
