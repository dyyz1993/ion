//! Azure OpenAI Responses API provider
//!
//! Implements the `azure-openai-responses` API protocol. It mirrors the
//! OpenAI Responses streaming protocol but uses Azure-specific
//! authentication and URL conventions.
//!
//! - Base URL pattern:
//!   `https://{resource_name}.openai.azure.com/openai/deployments/{deployment_id}`
//! - Authentication: `api-key` header (Azure convention) instead of
//!   `Authorization: Bearer`.
//! - Query string: appends `?api-version=2024-05-01-preview` (configurable
//!   via the `api_version` field on the provider, or via model.base_url).

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

/// Default Azure OpenAI API version used when none is configured.
const DEFAULT_API_VERSION: &str = "2024-05-01-preview";

pub struct AzureOpenAIResponsesProvider {
    /// Azure API version query parameter, e.g. "2024-05-01-preview".
    /// Defaults to [`DEFAULT_API_VERSION`] when empty.
    api_version: String,
}

impl Default for AzureOpenAIResponsesProvider {
    fn default() -> Self {
        Self {
            api_version: DEFAULT_API_VERSION.to_string(),
        }
    }
}

impl AzureOpenAIResponsesProvider {
    /// Create a provider that uses a custom Azure API version.
    pub fn with_api_version(api_version: impl Into<String>) -> Self {
        let v = api_version.into();
        Self {
            api_version: if v.is_empty() {
                DEFAULT_API_VERSION.to_string()
            } else {
                v
            },
        }
    }
}

#[async_trait]
impl ApiProvider for AzureOpenAIResponsesProvider {
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

impl AzureOpenAIResponsesProvider {
    async fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        let (stream, sender) = EventStream::new();

        // Resolve the Azure API key (uses provider env var, e.g. AZURE_API_KEY).
        let api_key = resolve_api_key(&model.provider, options.and_then(|o| o.api_key.clone()))?;

        // Resolve base URL. If empty, fall back to the canonical Azure pattern
        // constructed from resource_name + deployment_id (both expected in
        // model.base_url or a future dedicated config). For now we rely on
        // model.base_url carrying the full deployment URL.
        let base_url = if model.base_url.is_empty() {
            // Sensible default; real deployments should set model.base_url.
            "https://resource.openai.azure.com/openai/deployments/deployment".to_string()
        } else {
            model.base_url.clone()
        };

        // Azure appends the API version as a query string, not a path segment.
        // If the base URL already carries a query, we merge api-version in.
        let url = build_azure_url(&base_url, &self.api_version);

        let body = build_request_body(model, context, options)?;
        let body_json = serde_json::to_string(&body).map_err(|e| ProviderError::Provider(e.to_string()))?;

        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| ProviderError::Provider(e.to_string()))?;

        let send_fut = client
            .post(&url)
            // Azure uses the `api-key` header instead of `Authorization: Bearer`.
            .header("api-key", &api_key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(body_json)
            .send();
        // HTTP handshake can be cancelled via the cancellation token.
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
                        tracing::info!("[stream] azure-openai-responses SSE read cancelled by abort");
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
// URL construction
// ──────────────────────────────────────────────────────────────

/// Build the final Azure request URL by merging `api-version` into the
/// existing query string (if any).
fn build_azure_url(base_url: &str, api_version: &str) -> String {
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.contains('?') {
        // Already has a query string; append api-version unless present.
        if trimmed.contains("api-version=") {
            format!("{trimmed}")
        } else {
            format!("{trimmed}&api-version={api_version}")
        }
    } else {
        format!("{trimmed}?api-version={api_version}")
    }
}

// ──────────────────────────────────────────────────────────────
// Request body construction
// ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct ResponsesRequest {
    model: String,
    input: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<ReasoningConfig>,
    max_output_tokens: u64,
    stream: bool,
}

#[derive(Serialize)]
struct ReasoningConfig {
    effort: String,
    summary: String,
}

fn build_request_body(
    model: &Model,
    context: &Context,
    options: Option<&StreamOptions>,
) -> ProviderResult<ResponsesRequest> {
    let max_output_tokens = options
        .and_then(|o| o.max_tokens)
        .or(Some(model.max_tokens))
        .unwrap_or(4096);

    let instructions = context.system_prompt.clone();

    // Convert messages to Responses input format (identical to OpenAI Responses).
    let mut input: Vec<serde_json::Value> = Vec::new();

    for msg in &context.messages {
        match msg {
            Message::User(u) => {
                let text = u.content.iter().filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                }).collect::<Vec<_>>().join("\n");

                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": text }],
                }));
            }
            Message::Assistant(a) => {
                for block in &a.content {
                    match block {
                        AssistantContentBlock::Text(t) => {
                            input.push(serde_json::json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{ "type": "output_text", "text": t.text }],
                            }));
                        }
                        AssistantContentBlock::Thinking(th) => {
                            // Replay reasoning items.
                            if let Some(ref sig) = th.thinking_signature {
                                input.push(serde_json::json!({
                                    "type": "reasoning",
                                    "id": sig,
                                    "summary": [{ "type": "summary_text", "text": th.thinking }],
                                }));
                            } else if !th.thinking.is_empty() {
                                input.push(serde_json::json!({
                                    "type": "reasoning",
                                    "summary": [{ "type": "summary_text", "text": th.thinking }],
                                }));
                            }
                        }
                        AssistantContentBlock::ToolCall(tc) => {
                            // Tool call id format: "{call_id}|{item_id}".
                            let (call_id, item_id) = match tc.id.split_once('|') {
                                Some((c, i)) => (c.to_string(), Some(i.to_string())),
                                None => (tc.id.clone(), None),
                            };
                            let mut obj = serde_json::json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": tc.name,
                                "arguments": tc.arguments.to_string(),
                            });
                            if let Some(id) = item_id {
                                obj["id"] = serde_json::Value::String(id);
                            }
                            input.push(obj);
                        }
                    }
                }
            }
            Message::ToolResult(tr) => {
                let text = tr.content.iter().filter_map(|b| match b {
                    ContentBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                }).collect::<Vec<_>>().join("\n");
                input.push(serde_json::json!({
                    "type": "function_call_output",
                    "call_id": tr.tool_call_id,
                    "output": text,
                }));
            }
            Message::BashExecution(b) => {
                if b.exclude_from_context == Some(true) { continue; }
                let text = format!("$ {}\n{}", b.command, b.output);
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": text }],
                }));
            }
            Message::Custom(c) => {
                let text = match &c.content {
                    CustomContent::Text(s) => s.clone(),
                    CustomContent::Blocks(blocks) => blocks.iter().filter_map(|b| match b {
                        ContentBlock::Text(t) => Some(t.text.clone()),
                        _ => None,
                    }).collect::<Vec<_>>().join("\n"),
                };
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": text }],
                }));
            }
            Message::BranchSummary(bs) => {
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": format!("[Branch summary]\n{}", bs.summary) }],
                }));
            }
            Message::CompactionSummary(cs) => {
                input.push(serde_json::json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": format!("[Compaction summary]\n{}", cs.summary) }],
                }));
            }
        }
    }

    // Tools
    let tools: Vec<serde_json::Value> = context.tools
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .map(|t| serde_json::json!({
            "type": "function",
            "name": t.name,
            "description": t.description,
            "parameters": t.parameters,
        }))
        .collect();

    // Reasoning: respect explicit Off (disable), else use level or default medium.
    let user_level = options.and_then(|o| o.reasoning.clone());
    let user_explicit_off = matches!(user_level, Some(ThinkingLevel::Off));
    let reasoning = if user_explicit_off {
        None
    } else {
        let effort = match user_level {
            Some(ThinkingLevel::Minimal) => "minimal",
            Some(ThinkingLevel::Low) => "low",
            Some(ThinkingLevel::Medium) => "medium",
            Some(ThinkingLevel::High) => "high",
            Some(ThinkingLevel::XHigh) => "xhigh",
            None if model.reasoning => "medium",
            None => "",
            Some(ThinkingLevel::Off) => unreachable!(),
        };
        if effort.is_empty() {
            None
        } else {
            Some(ReasoningConfig {
                effort: effort.into(),
                summary: "auto".into(),
            })
        }
    };

    Ok(ResponsesRequest {
        model: model.id.clone(),
        input,
        instructions,
        tools,
        reasoning,
        max_output_tokens,
        stream: true,
    })
}

// ──────────────────────────────────────────────────────────────
// SSE stream parsing (same format as OpenAI Responses)
// ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct ResponsesSSEEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    response: Option<serde_json::Value>,
    #[serde(default)]
    item: Option<serde_json::Value>,
    #[serde(default)]
    delta: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    part: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    output_index: Option<u32>,
    #[serde(default)]
    #[allow(dead_code)]
    content_index: Option<u32>,
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

    // Current block being built, tracked by content_index.
    let mut current_block_type: Option<BlockKind> = None;
    let mut current_text = String::new();
    let mut current_thinking = String::new();
    let mut current_tool_partial_json = String::new();
    let mut current_tool_id: Option<String> = None;
    let mut current_tool_name: Option<String> = None;
    let mut current_tool_item_id: Option<String> = None;

    let _ = sender.send(StreamEvent::Start { partial: output.clone() }).await;

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
                if data_str.trim() == "[DONE]" { continue; }

                let event: ResponsesSSEEvent = match serde_json::from_str(&data_str) {
                    Ok(e) => e,
                    Err(_) => continue,
                };

                match event.event_type.as_str() {
                    "response.created" => {
                        if let Some(resp) = event.response {
                            if let Some(id) = resp.get("id").and_then(|v| v.as_str()) {
                                output.response_id = Some(id.to_string());
                            }
                        }
                    }
                    "response.output_item.added" => {
                        if let Some(item) = &event.item {
                            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            let content_index = output.content.len();
                            match item_type {
                                "reasoning" => {
                                    output.content.push(AssistantContentBlock::Thinking(ThinkingContent {
                                        thinking: String::new(),
                                        thinking_signature: None,
                                        redacted: None,
                                    }));
                                    current_block_type = Some(BlockKind::Thinking);
                                    current_thinking.clear();
                                    let _ = sender.send(StreamEvent::ThinkingStart {
                                        content_index,
                                        partial: output.clone(),
                                    }).await;
                                }
                                "message" => {
                                    output.content.push(AssistantContentBlock::Text(TextContent {
                                        text: String::new(),
                                        text_signature: None,
                                    }));
                                    current_block_type = Some(BlockKind::Text);
                                    current_text.clear();
                                    let _ = sender.send(StreamEvent::TextStart {
                                        content_index,
                                        partial: output.clone(),
                                    }).await;
                                }
                                "function_call" => {
                                    current_tool_id = item.get("call_id").and_then(|v| v.as_str()).map(String::from);
                                    current_tool_name = item.get("name").and_then(|v| v.as_str()).map(String::from);
                                    current_tool_item_id = item.get("id").and_then(|v| v.as_str()).map(String::from);
                                    current_tool_partial_json.clear();
                                    output.content.push(AssistantContentBlock::ToolCall(ToolCall {
                                        call_type: "function".into(),
                                        id: current_tool_id.clone().unwrap_or_default(),
                                        name: current_tool_name.clone().unwrap_or_default(),
                                        arguments: serde_json::json!({}),
                                        thought_signature: None,
                                    }));
                                    current_block_type = Some(BlockKind::ToolCall);
                                    let _ = sender.send(StreamEvent::ToolCallStart {
                                        content_index,
                                        partial: output.clone(),
                                    }).await;
                                }
                                _ => {}
                            }
                        }
                    }
                    "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                        if matches!(current_block_type, Some(BlockKind::Thinking)) {
                            if let Some(ref delta) = event.delta {
                                current_thinking.push_str(delta);
                                if let Some(AssistantContentBlock::Thinking(th)) = output.content.last_mut() {
                                    th.thinking.push_str(delta);
                                }
                                let content_index = output.content.len().saturating_sub(1);
                                let _ = sender.send(StreamEvent::ThinkingDelta {
                                    content_index,
                                    delta: delta.clone(),
                                    partial: output.clone(),
                                }).await;
                            }
                        }
                    }
                    "response.output_text.delta" => {
                        if matches!(current_block_type, Some(BlockKind::Text)) {
                            if let Some(ref delta) = event.delta {
                                current_text.push_str(delta);
                                if let Some(AssistantContentBlock::Text(t)) = output.content.last_mut() {
                                    t.text.push_str(delta);
                                }
                                let content_index = output.content.len().saturating_sub(1);
                                let _ = sender.send(StreamEvent::TextDelta {
                                    content_index,
                                    delta: delta.clone(),
                                    partial: output.clone(),
                                }).await;
                            }
                        }
                    }
                    "response.function_call_arguments.delta" => {
                        if matches!(current_block_type, Some(BlockKind::ToolCall)) {
                            if let Some(ref delta) = event.delta {
                                current_tool_partial_json.push_str(delta);
                                let parsed = parse_json_repair(&current_tool_partial_json);
                                if let Some(AssistantContentBlock::ToolCall(tc)) = output.content.last_mut() {
                                    tc.arguments = parsed;
                                }
                                let content_index = output.content.len().saturating_sub(1);
                                let _ = sender.send(StreamEvent::ToolCallDelta {
                                    content_index,
                                    delta: delta.clone(),
                                    partial: output.clone(),
                                }).await;
                            }
                        }
                    }
                    "response.function_call_arguments.done" => {
                        if matches!(current_block_type, Some(BlockKind::ToolCall)) {
                            if let Some(args) = event.arguments {
                                let parsed = parse_json_repair(&args);
                                if let Some(AssistantContentBlock::ToolCall(tc)) = output.content.last_mut() {
                                    tc.arguments = parsed;
                                }
                            }
                        }
                    }
                    "response.output_item.done" => {
                        let content_index = output.content.len().saturating_sub(1);
                        match current_block_type.take() {
                            Some(BlockKind::Text) => {
                                if let Some(AssistantContentBlock::Text(t)) = output.content.last() {
                                    let _ = sender.send(StreamEvent::TextEnd {
                                        content_index,
                                        content: t.text.clone(),
                                        partial: output.clone(),
                                    }).await;
                                }
                            }
                            Some(BlockKind::Thinking) => {
                                // Capture reasoning signature (full item JSON) for replay.
                                if let Some(item) = &event.item {
                                    let sig = serde_json::to_string(item).unwrap_or_default();
                                    if let Some(AssistantContentBlock::Thinking(th)) = output.content.last_mut() {
                                        th.thinking_signature = Some(sig);
                                    }
                                }
                                if let Some(AssistantContentBlock::Thinking(th)) = output.content.last() {
                                    let _ = sender.send(StreamEvent::ThinkingEnd {
                                        content_index,
                                        content: th.thinking.clone(),
                                        partial: output.clone(),
                                    }).await;
                                }
                            }
                            Some(BlockKind::ToolCall) => {
                                // Finalize tool call id: "{call_id}|{item_id}" for replay.
                                let final_id = match (&current_tool_id, &current_tool_item_id) {
                                    (Some(c), Some(i)) => format!("{c}|{i}"),
                                    (Some(c), None) => c.clone(),
                                    _ => String::new(),
                                };
                                if let Some(AssistantContentBlock::ToolCall(tc)) = output.content.last_mut() {
                                    tc.id = final_id;
                                }
                                if let Some(AssistantContentBlock::ToolCall(tc)) = output.content.last() {
                                    let _ = sender.send(StreamEvent::ToolCallEnd {
                                        content_index,
                                        tool_call: tc.clone(),
                                        partial: output.clone(),
                                    }).await;
                                }
                                current_tool_id = None;
                                current_tool_name = None;
                                current_tool_item_id = None;
                                current_tool_partial_json.clear();
                            }
                            None => {}
                        }
                    }
                    "response.completed" => {
                        if let Some(resp) = &event.response {
                            // stop_reason
                            if let Some(status) = resp.get("status").and_then(|v| v.as_str()) {
                                stop_reason = match status {
                                    "completed" => StopReason::Stop,
                                    "incomplete" => StopReason::Length,
                                    "failed" | "incompatible" | "cancelled" => StopReason::Error,
                                    _ => StopReason::Stop,
                                };
                            }
                            // usage
                            if let Some(usage) = resp.get("usage") {
                                let input = usage.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                let out = usage.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
                                let total = usage.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(input + out);
                                output.usage.input = input;
                                output.usage.output = out;
                                output.usage.total_tokens = total;
                            }
                        }
                    }
                    "error" => {
                        let msg = event.item
                            .as_ref()
                            .and_then(|v| v.get("message"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("Azure OpenAI Responses API error")
                            .to_string();
                        return Err(ProviderError::Provider(msg));
                    }
                    _ => {}
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

#[derive(Clone, Copy)]
enum BlockKind {
    Text,
    Thinking,
    ToolCall,
}

/// Tolerant JSON parser that repairs common truncation artifacts.
fn parse_json_repair(s: &str) -> serde_json::Value {
    if s.is_empty() { return serde_json::json!({}); }
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) { return v; }
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
// Tests
// ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_model() -> Model {
        Model {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            api: "azure-openai-responses".into(),
            provider: "azure".into(),
            base_url: "https://myresource.openai.azure.com/openai/deployments/my-deploy".into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: Cost::default(),
            context_window: 200000,
            max_tokens: 16384,
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
        assert_eq!(body.model, "gpt-5");
        assert_eq!(body.instructions, Some("You are helpful".into()));
        assert_eq!(body.input.len(), 1);
        assert!(body.stream);
        assert!(body.reasoning.is_some());
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
    }

    #[test]
    fn build_request_body_reasoning_off() {
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
        assert!(body.reasoning.is_none());
    }

    #[test]
    fn url_appends_api_version() {
        let url = build_azure_url(
            "https://myresource.openai.azure.com/openai/deployments/my-deploy",
            "2024-05-01-preview",
        );
        assert_eq!(
            url,
            "https://myresource.openai.azure.com/openai/deployments/my-deploy?api-version=2024-05-01-preview"
        );
    }

    #[test]
    fn url_merges_into_existing_query() {
        let url = build_azure_url(
            "https://myresource.openai.azure.com/openai/deployments/my-deploy?foo=bar",
            "2024-05-01-preview",
        );
        assert_eq!(
            url,
            "https://myresource.openai.azure.com/openai/deployments/my-deploy?foo=bar&api-version=2024-05-01-preview"
        );
    }

    #[test]
    fn url_does_not_duplicate_api_version() {
        let url = build_azure_url(
            "https://myresource.openai.azure.com/openai/deployments/my-deploy?api-version=2023-01-01",
            "2024-05-01-preview",
        );
        // Existing api-version is preserved (not overwritten).
        assert_eq!(
            url,
            "https://myresource.openai.azure.com/openai/deployments/my-deploy?api-version=2023-01-01"
        );
    }

    #[test]
    fn default_api_version_is_used() {
        let p = AzureOpenAIResponsesProvider::default();
        assert_eq!(p.api_version, DEFAULT_API_VERSION);
    }

    #[test]
    fn json_repair_partial() {
        let v = parse_json_repair(r#"{"name":"test"#);
        assert_eq!(v["name"], "test");
    }
}
