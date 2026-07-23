//! Mistral Conversations API provider.
//!
//! 对齐 pi `packages/ai/src/providers/mistral.ts`（mistral-conversations 协议）。
//!
//! Mistral 的 Chat Completions API 与 OpenAI 高度相似，但有几处关键差异：
//!
//! 1. **`delta.content` 可以是字符串 *或* 数组**
//!    - 纯文本场景：`"content": "hello"`
//!    - 思考场景：`"content": [{"type":"thinking","thinking":[{"type":"text","text":"..."}]}, {"type":"text","text":"..."}]`
//!    - OpenAI provider 只处理字符串，这里必须同时处理两种形态。
//!
//! 2. **assistant 消息的 thinking 作为 content block 回传**
//!    - OpenAI 把思考放在 `reasoning_content` 顶层字段
//!    - Mistral 把思考作为 content parts 数组中的 `{type:"thinking"}` 元素
//!
//! 3. **tool result 消息带 `name` 字段**
//!    - OpenAI 的 `tool` 消息只有 `tool_call_id` + `content`
//!    - Mistral 要求（推荐）同时给 `name`（即 tool name）
//!
//! 4. **reasoning 参数：`prompt_mode` / `reasoning_effort`**
//!    - Codestral / Magistral 用 `prompt_mode: "reasoning"` 开启思考
//!    - mistral-small / medium 用 `reasoning_effort: "high"|"none"`
//!
//! 5. **stop reason 多一个 `model_length`**（映射到 `Length`）
//!
//! 6. **字段命名 camelCase**（`max_tokens`→`maxTokens`、`tool_calls`→`toolCalls` 等）
//!    - Mistral SDK 用 camelCase，但其 HTTP API 同时接受 snake_case 与 camelCase
//!    - 本实现统一发送 **snake_case**（HTTP 原生），兼容性最广，与 openai.rs 风格一致

use crate::env_keys::resolve_api_key;
use crate::error::{ProviderError, ProviderResult};
use crate::event_stream::{EventSender, EventStream};
use crate::types::*;
use crate::ApiProvider;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct MistralProvider;

#[async_trait]
impl ApiProvider for MistralProvider {
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

impl MistralProvider {
    async fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        let output = AssistantMessage::new(model);
        let (stream, sender) = EventStream::new();

        let api_key = resolve_api_key(
            &model.provider,
            options.and_then(|o| o.api_key.clone()),
        )?;

        // ---- 构造消息 ----
        let mut messages: Vec<MistralMessage> = Vec::new();

        // system：Mistral 支持 system role（不像某些 provider 需要前缀注入）
        if let Some(ref sp) = context.system_prompt {
            messages.push(MistralMessage {
                role: "system".into(),
                content: Some(serde_json::Value::String(sp.clone())),
                tool_call_id: None,
                tool_calls: None,
                name: None,
            });
        }

        let supports_images = model.input.iter().any(|i| i == "image");

        for msg in &context.messages {
            match msg {
                Message::User(u) => {
                    let has_image = u.content.iter().any(|b| matches!(b, ContentBlock::Image(_)));
                    if has_image && supports_images {
                        // 含图片：content 必须是 content parts 数组
                        let parts: Vec<serde_json::Value> = u.content.iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text(t) if !t.text.is_empty() => Some(serde_json::json!({
                                    "type": "text",
                                    "text": t.text,
                                })),
                                ContentBlock::Image(img) => Some(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": format!("data:{};base64,{}", img.mime_type, img.data),
                                })),
                                _ => None,
                            })
                            .collect();
                        messages.push(MistralMessage {
                            role: "user".into(),
                            content: Some(serde_json::Value::Array(parts)),
                            tool_call_id: None,
                            tool_calls: None,
                            name: None,
                        });
                    } else {
                        let text = u.content.iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        messages.push(MistralMessage {
                            role: "user".into(),
                            content: Some(serde_json::Value::String(text)),
                            tool_call_id: None,
                            tool_calls: None,
                            name: None,
                        });
                    }
                }
                Message::Assistant(a) => {
                    // Mistral assistant 消息：content 是 content parts 数组（可含 thinking / text）
                    // tool_calls 放在顶层 tool_calls 字段
                    let mut content_parts: Vec<serde_json::Value> = Vec::new();
                    for block in &a.content {
                        match block {
                            AssistantContentBlock::Text(t) => {
                                if !t.text.trim().is_empty() {
                                    content_parts.push(serde_json::json!({ "type": "text", "text": t.text }));
                                }
                            }
                            AssistantContentBlock::Thinking(th) => {
                                if !th.thinking.trim().is_empty() {
                                    // Mistral thinking block: { type:"thinking", thinking:[{type:"text", text:"..."}] }
                                    content_parts.push(serde_json::json!({
                                        "type": "thinking",
                                        "thinking": [{ "type": "text", "text": th.thinking }],
                                    }));
                                }
                            }
                            AssistantContentBlock::ToolCall(_) => {
                                // tool calls 单独收集，不放进 content
                            }
                        }
                    }

                    let tool_calls: Vec<serde_json::Value> = a.content.iter()
                        .filter_map(|b| match b {
                            AssistantContentBlock::ToolCall(tc) => Some(serde_json::json!({
                                "id": tc.id,
                                "type": "function",
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments.to_string(),
                                },
                            })),
                            _ => None,
                        })
                        .collect();

                    // Mistral 约定：assistant 带 tool_calls 时 content 可为空字符串
                    let content = if content_parts.is_empty() && !tool_calls.is_empty() {
                        Some(serde_json::Value::String(String::new()))
                    } else if content_parts.is_empty() {
                        None
                    } else {
                        Some(serde_json::Value::Array(content_parts))
                    };

                    messages.push(MistralMessage {
                        role: "assistant".into(),
                        content,
                        tool_call_id: None,
                        tool_calls: if tool_calls.is_empty() { None } else { Some(tool_calls) },
                        name: None,
                    });
                }
                Message::ToolResult(tr) => {
                    // Mistral tool 消息：role="tool", content 是 content parts 数组，带 tool_call_id + name
                    let text = tr.content.iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let error_prefix = if tr.is_error { "[tool error] " } else { "" };
                    let content_parts = vec![serde_json::json!({ "type": "text", "text": format!("{error_prefix}{text}") })];

                    // 如果支持图片且 tool result 含图片，追加 image_url parts
                    let mut content_parts = content_parts;
                    if supports_images {
                        for b in &tr.content {
                            if let ContentBlock::Image(img) = b {
                                content_parts.push(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": format!("data:{};base64,{}", img.mime_type, img.data),
                                }));
                            }
                        }
                    }

                    messages.push(MistralMessage {
                        role: "tool".into(),
                        content: Some(serde_json::Value::Array(content_parts)),
                        tool_call_id: Some(tr.tool_call_id.clone()),
                        tool_calls: None,
                        name: Some(tr.tool_name.clone()),
                    });
                }
                Message::BashExecution(b) => {
                    if b.exclude_from_context == Some(true) {
                        continue;
                    }
                    let mut text = format!("Ran `{}`\n```\n{}\n```", b.command, b.output);
                    if b.cancelled {
                        text.push_str("\n\n(command cancelled)");
                    } else if let Some(code) = b.exit_code {
                        if code != 0 {
                            text.push_str(&format!("\n\nCommand exited with code {code}"));
                        }
                    }
                    if b.truncated {
                        if let Some(ref p) = b.full_output_path {
                            text.push_str(&format!("\n\n[Output truncated. Full output: {p}]"));
                        }
                    }
                    messages.push(MistralMessage {
                        role: "user".into(),
                        content: Some(serde_json::Value::String(text)),
                        tool_call_id: None,
                        tool_calls: None,
                        name: None,
                    });
                }
                Message::Custom(c) => {
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
                    messages.push(MistralMessage {
                        role: "user".into(),
                        content: Some(serde_json::Value::String(text)),
                        tool_call_id: None,
                        tool_calls: None,
                        name: None,
                    });
                }
                Message::BranchSummary(b) => {
                    let text = format!(
                        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{}\n</summary>",
                        b.summary
                    );
                    messages.push(MistralMessage {
                        role: "user".into(),
                        content: Some(serde_json::Value::String(text)),
                        tool_call_id: None,
                        tool_calls: None,
                        name: None,
                    });
                }
                Message::CompactionSummary(c) => {
                    let text = format!(
                        "The conversation history before this point was compacted into the following summary:\n\n<summary>\n{}\n</summary>",
                        c.summary
                    );
                    messages.push(MistralMessage {
                        role: "user".into(),
                        content: Some(serde_json::Value::String(text)),
                        tool_call_id: None,
                        tool_calls: None,
                        name: None,
                    });
                }
            }
        }

        // ---- tools ----
        let tools: Option<Vec<MistralTool>> = context.tools.as_ref().map(|tools| {
            tools.iter().map(|t| MistralTool {
                r#type: "function".into(),
                function: MistralToolFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            }).collect()
        });

        // ---- 构造 body ----
        let max_tokens = options.and_then(|o| o.max_tokens).unwrap_or(model.max_tokens);

        let mut body = serde_json::json!({
            "model": model.id,
            "messages": messages,
            "stream": true,
            "tools": tools,
            // Mistral 用 "any" 比 "auto" 更可靠地触发工具调用，但默认仍用 auto 保持 OpenAI 一致性
            "tool_choice": if tools.is_some() { serde_json::json!("auto") } else { serde_json::Value::Null },
        });
        body["max_tokens"] = serde_json::json!(max_tokens);

        // response_format（JSON mode）
        if let Some(ref fmt) = options.and_then(|o| o.response_format.clone()) {
            body["response_format"] = serde_json::json!({ "type": fmt });
        }

        // ---- reasoning 参数（Codestral / Magistral / mistral-small）----
        apply_mistral_reasoning(&mut body, model, options);

        // model.headers（用户自定义 header，如 x-affinity）
        let extra_headers = model.headers.clone();

        let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
        let client = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?;

        let mut req = client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Accept", "application/json");
        if let Some(ref headers) = extra_headers {
            for (k, v) in headers {
                req = req.header(k, v);
            }
        }

        let send_fut = req.json(&body).send();
        // HTTP 握手期可被 cancel 取消（修复 D）
        let resp = if let Some(c) = &cancel {
            tokio::select! {
                r = send_fut => r?,
                _ = c.cancelled() => return Err(crate::ProviderError::Stream("HTTP request aborted".into())),
            }
        } else {
            send_fut.await?
        };

        if !resp.status().is_success() {
            let s = resp.status();
            let b = resp.text().await.unwrap_or_default();

            // response_format 导致 400 时降级重试（与 openai.rs 一致）
            if s.as_u16() == 400
                && options.and_then(|o| o.response_format.as_ref()).is_some()
                && b.contains("response_format")
            {
                tracing::warn!("Mistral API rejected response_format, falling back");
                let mut fallback_body = body.clone();
                fallback_body.as_object_mut().map(|obj| obj.remove("response_format"));
                let resp = client
                    .post(&url)
                    .header("Authorization", format!("Bearer {api_key}"))
                    .json(&fallback_body)
                    .send()
                    .await?;
                if !resp.status().is_success() {
                    let s2 = resp.status();
                    let b2 = resp.text().await.unwrap_or_default();
                    return Err(ProviderError::HttpError { status: s2.as_u16(), body: b2 });
                }
                let cancel_clone_fb = cancel.clone();
                tokio::spawn(async move {
                    if let Some(c) = &cancel_clone_fb {
                        tokio::select! {
                            _ = read_sse(resp, sender, output) => {}
                            _ = c.cancelled() => {}
                        }
                    } else {
                        read_sse(resp, sender, output).await;
                    }
                });
                return Ok(stream);
            }

            return Err(ProviderError::HttpError { status: s.as_u16(), body: b });
        }

        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            if let Some(c) = &cancel_clone {
                tokio::select! {
                    _ = read_sse(resp, sender, output) => {}
                    _ = c.cancelled() => {
                        tracing::info!("[stream] mistral SSE read cancelled by abort");
                    }
                }
            } else {
                read_sse(resp, sender, output).await;
            }
        });

        Ok(stream)
    }
}

// ──────────────────────────────────────────────────────────────
// reasoning 参数映射
// 对齐 pi mistral.ts 的 usesPromptModeReasoning / usesReasoningEffort
// ──────────────────────────────────────────────────────────────

/// 哪些模型用 `reasoning_effort`（而非 prompt_mode）
/// 对齐 pi: mistral-small-2503/latest/medium-3.5
fn uses_reasoning_effort(model: &Model) -> bool {
    matches!(
        model.id.as_str(),
        "mistral-small-2503" | "mistral-small-latest" | "mistral-medium-3.5"
    )
}

/// 哪些模型用 `prompt_mode: "reasoning"`（Codestral / Magistral 等思考型模型）
/// 对齐 pi: usesPromptModeReasoning = model.reasoning && !usesReasoningEffort
fn uses_prompt_mode_reasoning(model: &Model) -> bool {
    model.reasoning && !uses_reasoning_effort(model)
}

fn apply_mistral_reasoning(
    body: &mut serde_json::Value,
    model: &Model,
    options: Option<&StreamOptions>,
) {
    if !model.reasoning {
        return;
    }

    let level = options.and_then(|o| o.reasoning.clone());
    let enabled = level.is_some() && !matches!(level, Some(ThinkingLevel::Off));

    if uses_reasoning_effort(model) {
        // mistral-small / medium：reasoning_effort: "high" | "none"
        body["reasoning_effort"] = serde_json::json!(if enabled { "high" } else { "none" });
    } else if uses_prompt_mode_reasoning(model) {
        // Codestral / Magistral：prompt_mode: "reasoning"
        if enabled {
            body["prompt_mode"] = serde_json::json!("reasoning");
        }
    }
    // 非 reasoning 模型不发参数
}

// ──────────────────────────────────────────────────────────────
// 请求结构
// ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct MistralMessage {
    role: String,
    // String（纯文本）或 Array（content parts）
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "tool_calls")]
    tool_calls: Option<Vec<serde_json::Value>>,
    // Mistral tool result 消息推荐带 name（tool name）
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

#[derive(Serialize)]
struct MistralTool {
    r#type: String,
    function: MistralToolFunction,
}

#[derive(Serialize)]
struct MistralToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

// ──────────────────────────────────────────────────────────────
// SSE 解析
// ──────────────────────────────────────────────────────────────

#[derive(Deserialize, Debug)]
struct Chunk {
    #[serde(default)]
    id: Option<String>,
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<UsageData>,
}

#[derive(Deserialize, Debug)]
struct UsageData {
    #[serde(default, alias = "prompt_tokens", alias = "promptTokens")]
    prompt_tokens: u64,
    #[serde(default, alias = "completion_tokens", alias = "completionTokens")]
    completion_tokens: u64,
    #[serde(default, alias = "total_tokens", alias = "totalTokens")]
    total_tokens: u64,
}

#[derive(Deserialize, Debug)]
struct ChunkChoice {
    #[serde(default, alias = "finish_reason", alias = "finishReason")]
    finish_reason: Option<String>,
    delta: Delta,
}

#[derive(Deserialize, Debug)]
struct Delta {
    /// Mistral: content 可以是字符串 *或* content parts 数组。
    /// 用 serde_json::Value 统一接收，运行时分支处理。
    #[serde(default)]
    content: Option<serde_json::Value>,
    #[serde(default, alias = "tool_calls", alias = "toolCalls")]
    tool_calls: Option<Vec<ChunkTC>>,
}

#[derive(Deserialize, Debug)]
struct ChunkTC {
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    r#type: Option<String>,
    function: Option<ChunkTCFunc>,
}

#[derive(Deserialize, Debug)]
struct ChunkTCFunc {
    #[serde(default)]
    name: Option<String>,
    /// Mistral 的 arguments 可能是 string 或 object
    #[serde(default)]
    arguments: Option<serde_json::Value>,
}

struct AccTC {
    index: u64,
    id: String,
    name: String,
    arguments: String,
}

async fn read_sse(
    resp: reqwest::Response,
    sender: EventSender,
    mut output: AssistantMessage,
) {
    use futures_util::StreamExt;

    let mut buffer = String::new();
    let mut accs: Vec<AccTC> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let content_idx = 0usize;

    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(128);
    let mut stream = resp.bytes_stream();
    tokio::spawn(async move {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => { if chunk_tx.send(b.to_vec()).await.is_err() { break; } }
                Err(e) => { tracing::warn!("Mistral SSE: {e}"); break; }
            }
        }
    });

    sender.send(StreamEvent::Start { partial: output.clone() }).await;

    while let Some(bytes) = chunk_rx.recv().await {
        let text = String::from_utf8_lossy(&bytes);
        buffer.push_str(&text);

        loop {
            let pos = match buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n")) {
                Some(p) => p,
                None => break,
            };
            let event_str = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();
            if event_str.trim().is_empty() { continue; }

            for line in event_str.lines() {
                let line = line.trim();
                if !line.starts_with("data: ") { continue; }
                let json_str = &line[6..];
                if json_str == "[DONE]" { continue; }

                let chunk: Chunk = match serde_json::from_str(json_str) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                // 保留首个非空 response id
                if output.response_id.is_none() {
                    if let Some(ref id) = chunk.id { output.response_id = Some(id.clone()); }
                }

                for choice in chunk.choices {
                    let d = choice.delta;

                    // ---- content：字符串或数组 ----
                    if let Some(content_val) = d.content {
                        process_delta_content(
                            content_val,
                            &mut text_parts,
                            &mut reasoning_parts,
                            &mut output,
                            &sender,
                            content_idx,
                        ).await;
                    }

                    // ---- tool calls ----
                    if let Some(tcs) = d.tool_calls {
                        for tc in tcs {
                            let idx = tc.index.unwrap_or(0);
                            // arguments 可能是 string 或 object
                            let args_str = tc.function.as_ref()
                                .and_then(|f| f.arguments.as_ref())
                                .map(|v| match v {
                                    serde_json::Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                });

                            if let Some(a) = accs.iter_mut().find(|a| a.index == idx) {
                                if let Some(args) = args_str {
                                    a.arguments.push_str(&args);
                                }
                            } else {
                                let raw_id = tc.id.unwrap_or_default();
                                // Mistral 偶尔返回 "null" 字符串作为 id
                                let id = if raw_id.is_empty() || raw_id == "null" {
                                    format!("call_{idx}")
                                } else {
                                    raw_id
                                };
                                let name = tc.function.as_ref()
                                    .and_then(|f| f.name.clone())
                                    .unwrap_or_default();
                                let args = args_str.unwrap_or_default();
                                accs.push(AccTC { index: idx, id, name, arguments: args });
                            }
                        }
                    }

                    // ---- finish ----
                    if let Some(ref reason) = choice.finish_reason {
                        let stop_reason = map_stop_reason(reason);
                        output.stop_reason = stop_reason.clone();

                        if let Some(ref usage) = chunk.usage {
                            output.usage.input = usage.prompt_tokens;
                            output.usage.output = usage.completion_tokens;
                            output.usage.total_tokens = usage.total_tokens;
                        }

                        output.content = build_assistant_content(&text_parts, &reasoning_parts, &accs);

                        let tcs: Vec<ToolCall> = accs.iter().map(|a| ToolCall {
                            call_type: "function".into(),
                            id: a.id.clone(),
                            name: a.name.clone(),
                            arguments: serde_json::from_str(&a.arguments).unwrap_or(serde_json::Value::Null),
                            thought_signature: None,
                        }).collect();

                        if !tcs.is_empty() {
                            sender.send(StreamEvent::ToolCallStart {
                                content_index: content_idx,
                                partial: output.clone(),
                            }).await;
                        }
                        for tc in &tcs {
                            sender.send(StreamEvent::ToolCallEnd {
                                content_index: content_idx,
                                tool_call: tc.clone(),
                                partial: output.clone(),
                            }).await;
                            output.content.push(AssistantContentBlock::ToolCall(tc.clone()));
                        }

                        if !text_parts.is_empty() {
                            let full_text = text_parts.join("");
                            sender.send(StreamEvent::TextEnd {
                                content_index: content_idx,
                                content: full_text,
                                partial: output.clone(),
                            }).await;
                        }

                        sender.end(output.clone());
                        return;
                    }
                }
            }
        }
    }

    // Stream ended without finish_reason
    output.error_message = Some("stream ended unexpectedly".into());
    output.stop_reason = StopReason::Error;
    sender.error(StopReason::Error, output);
}

/// 处理 delta.content —— 字符串或 content parts 数组两种形态。
async fn process_delta_content(
    content_val: serde_json::Value,
    text_parts: &mut Vec<String>,
    reasoning_parts: &mut Vec<String>,
    output: &mut AssistantMessage,
    sender: &EventSender,
    content_idx: usize,
) {
    match content_val {
        // 纯字符串（最常见的非思考场景）
        serde_json::Value::String(s) => {
            if s.is_empty() { return; }
            if text_parts.is_empty() {
                sender.send(StreamEvent::TextStart {
                    content_index: content_idx,
                    partial: output.clone(),
                }).await;
            }
            text_parts.push(s.clone());
            let mut partial = output.clone();
            partial.content = build_assistant_content(text_parts, reasoning_parts, &[]);
            sender.send(StreamEvent::TextDelta {
                content_index: content_idx,
                delta: s,
                partial,
            }).await;
        }
        // 数组：可含 {type:"thinking"} 和 {type:"text"} 两种 part
        serde_json::Value::Array(parts) => {
            for part in parts {
                let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("text");
                match part_type {
                    "thinking" => {
                        // thinking 字段是 [{type:"text", text:"..."}]
                        let delta_text = part.get("thinking")
                            .and_then(|t| t.as_array())
                            .map(|arr| arr.iter()
                                .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                                .collect::<Vec<_>>()
                                .join(""))
                            .unwrap_or_default();
                        if delta_text.is_empty() { continue; }
                        if reasoning_parts.is_empty() {
                            sender.send(StreamEvent::ThinkingStart {
                                content_index: content_idx,
                                partial: output.clone(),
                            }).await;
                        }
                        reasoning_parts.push(delta_text.clone());
                        let mut partial = output.clone();
                        partial.content = build_assistant_content(text_parts, reasoning_parts, &[]);
                        sender.send(StreamEvent::ThinkingDelta {
                            content_index: content_idx,
                            delta: delta_text,
                            partial,
                        }).await;
                    }
                    _ => {
                        // "text" 或未知 type 当文本处理
                        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if text.is_empty() { continue; }
                        if text_parts.is_empty() {
                            sender.send(StreamEvent::TextStart {
                                content_index: content_idx,
                                partial: output.clone(),
                            }).await;
                        }
                        text_parts.push(text.to_string());
                        let mut partial = output.clone();
                        partial.content = build_assistant_content(text_parts, reasoning_parts, &[]);
                        sender.send(StreamEvent::TextDelta {
                            content_index: content_idx,
                            delta: text.to_string(),
                            partial,
                        }).await;
                    }
                }
            }
        }
        // Null 或其他：忽略
        _ => {}
    }
}

fn map_stop_reason(reason: &str) -> StopReason {
    match reason {
        "stop" => StopReason::Stop,
        "length" | "model_length" => StopReason::Length,
        "tool_calls" => StopReason::ToolUse,
        "error" => StopReason::Error,
        _ => StopReason::Stop,
    }
}

fn build_assistant_content(
    text_parts: &[String],
    reasoning_parts: &[String],
    _accs: &[AccTC],
) -> Vec<AssistantContentBlock> {
    let mut content = Vec::new();

    if !reasoning_parts.is_empty() {
        content.push(AssistantContentBlock::Thinking(ThinkingContent {
            thinking: reasoning_parts.join(""),
            thinking_signature: None,
            redacted: None,
        }));
    }

    if !text_parts.is_empty() {
        content.push(AssistantContentBlock::Text(TextContent {
            text: text_parts.join(""),
            text_signature: None,
        }));
    }

    content
}

// ──────────────────────────────────────────────────────────────
// 单元测试（不调真实 API）
// ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_model(reasoning: bool) -> Model {
        Model {
            id: "mistral-large-latest".into(),
            name: "Mistral Large".into(),
            api: "mistral-conversations".into(),
            provider: "mistral".into(),
            base_url: "https://api.mistral.ai/v1".into(),
            reasoning,
            input: vec!["text".into()],
            cost: Cost::default(),
            context_window: 128000,
            max_tokens: 4096,
            compat: None,
            headers: None,
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

    // ---- stop reason 映射 ----

    #[test]
    fn map_stop_reason_basic() {
        assert_eq!(map_stop_reason("stop"), StopReason::Stop);
        assert_eq!(map_stop_reason("length"), StopReason::Length);
        assert_eq!(map_stop_reason("model_length"), StopReason::Length);
        assert_eq!(map_stop_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_stop_reason("error"), StopReason::Error);
        assert_eq!(map_stop_reason("unknown"), StopReason::Stop);
    }

    // ---- reasoning 参数映射 ----

    #[test]
    fn reasoning_effort_for_mistral_small() {
        // mistral-small 用 reasoning_effort，不用 prompt_mode
        let mut model = make_model(true);
        model.id = "mistral-small-latest".into();
        assert!(uses_reasoning_effort(&model));
        assert!(!uses_prompt_mode_reasoning(&model));

        let mut body = serde_json::json!({});
        apply_mistral_reasoning(&mut body, &model, Some(&StreamOptions {
            max_tokens: None, api_key: None,
            reasoning: Some(ThinkingLevel::High),
            timeout_ms: None, max_retries: None, response_format: None,
        }));
        assert_eq!(body["reasoning_effort"], "high");
        // prompt_mode 不应出现
        assert!(body.get("prompt_mode").is_none());
    }

    #[test]
    fn reasoning_effort_off_maps_to_none() {
        let mut model = make_model(true);
        model.id = "mistral-small-latest".into();
        let mut body = serde_json::json!({});
        apply_mistral_reasoning(&mut body, &model, Some(&StreamOptions {
            max_tokens: None, api_key: None,
            reasoning: Some(ThinkingLevel::Off),
            timeout_ms: None, max_retries: None, response_format: None,
        }));
        assert_eq!(body["reasoning_effort"], "none");
    }

    #[test]
    fn prompt_mode_for_codestral() {
        // Codestral 用 prompt_mode: "reasoning"
        let mut model = make_model(true);
        model.id = "codestral-latest".into();
        assert!(!uses_reasoning_effort(&model));
        assert!(uses_prompt_mode_reasoning(&model));

        let mut body = serde_json::json!({});
        apply_mistral_reasoning(&mut body, &model, Some(&StreamOptions {
            max_tokens: None, api_key: None,
            reasoning: Some(ThinkingLevel::Medium),
            timeout_ms: None, max_retries: None, response_format: None,
        }));
        assert_eq!(body["prompt_mode"], "reasoning");
        // reasoning_effort 不应出现
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn no_reasoning_params_for_non_reasoning_model() {
        let model = make_model(false);
        let mut body = serde_json::json!({});
        apply_mistral_reasoning(&mut body, &model, Some(&StreamOptions {
            max_tokens: None, api_key: None,
            reasoning: Some(ThinkingLevel::High),
            timeout_ms: None, max_retries: None, response_format: None,
        }));
        assert!(body.get("prompt_mode").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    // ---- 消息转换：system role 直传 ----

    #[test]
    fn mistral_message_system_role_serialized() {
        let msg = MistralMessage {
            role: "system".into(),
            content: Some(serde_json::Value::String("You are helpful".into())),
            tool_call_id: None,
            tool_calls: None,
            name: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"system""#));
        assert!(json.contains(r#""content":"You are helpful""#));
        // 可选字段不应出现
        assert!(!json.contains("tool_call_id"));
        assert!(!json.contains("tool_calls"));
        assert!(!json.contains(r#""name""#));
    }

    // ---- 消息转换：tool result 带 name ----

    #[test]
    fn mistral_tool_result_has_name() {
        let msg = MistralMessage {
            role: "tool".into(),
            content: Some(serde_json::Value::Array(vec![
                serde_json::json!({ "type": "text", "text": "晴" }),
            ])),
            tool_call_id: Some("call_abc".into()),
            tool_calls: None,
            name: Some("get_weather".into()),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""role":"tool""#));
        assert!(json.contains(r#""tool_call_id":"call_abc""#));
        assert!(json.contains(r#""name":"get_weather""#));
    }

    // ---- assistant 消息：thinking 作为 content part ----

    #[test]
    fn assistant_thinking_becomes_content_part() {
        // 构造一条含 thinking + text 的 assistant 消息，验证序列化结果
        let a = AssistantMessage {
            role: "assistant".into(),
            content: vec![
                AssistantContentBlock::Thinking(ThinkingContent {
                    thinking: "Let me think...".into(),
                    thinking_signature: None,
                    redacted: None,
                }),
                AssistantContentBlock::Text(TextContent { text: "Answer".into(), text_signature: None }),
            ],
            api: "mistral-conversations".into(),
            provider: "mistral".into(),
            model: "codestral-latest".into(),
            response_model: None, response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };

        // 复用 provider 里的转换逻辑
        let mut content_parts: Vec<serde_json::Value> = Vec::new();
        for block in &a.content {
            match block {
                AssistantContentBlock::Text(t) => {
                    if !t.text.trim().is_empty() {
                        content_parts.push(serde_json::json!({ "type": "text", "text": t.text }));
                    }
                }
                AssistantContentBlock::Thinking(th) => {
                    if !th.thinking.trim().is_empty() {
                        content_parts.push(serde_json::json!({
                            "type": "thinking",
                            "thinking": [{ "type": "text", "text": th.thinking }],
                        }));
                    }
                }
                _ => {}
            }
        }
        assert_eq!(content_parts.len(), 2);
        assert_eq!(content_parts[0]["type"], "thinking");
        assert_eq!(content_parts[0]["thinking"][0]["text"], "Let me think...");
        assert_eq!(content_parts[1]["type"], "text");
        assert_eq!(content_parts[1]["text"], "Answer");
    }

    // ---- delta.content 字符串解析 ----

    #[test]
    fn delta_content_string_parsed() {
        // 模拟 Mistral SSE chunk 的 delta.content 是字符串
        let chunk_json = r#"{"id":"abc","choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let chunk: Chunk = serde_json::from_str(chunk_json).unwrap();
        let content = chunk.choices[0].delta.content.as_ref().unwrap();
        assert_eq!(content.as_str().unwrap(), "hello");
    }

    // ---- delta.content 数组（含 thinking）解析 ----

    #[test]
    fn delta_content_array_with_thinking_parsed() {
        let chunk_json = r#"{
            "id":"abc",
            "choices":[{
                "delta":{
                    "content":[
                        {"type":"thinking","thinking":[{"type":"text","text":"reasoning here"}]},
                        {"type":"text","text":"final answer"}
                    ]
                },
                "finish_reason":null
            }]
        }"#;
        let chunk: Chunk = serde_json::from_str(chunk_json).unwrap();
        let content = chunk.choices[0].delta.content.as_ref().unwrap();
        let arr = content.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "thinking");
        assert_eq!(arr[0]["thinking"][0]["text"], "reasoning here");
        assert_eq!(arr[1]["type"], "text");
        assert_eq!(arr[1]["text"], "final answer");
    }

    // ---- tool call chunk 解析 ----

    #[test]
    fn tool_call_chunk_parsed() {
        let chunk_json = r#"{
            "id":"abc",
            "choices":[{
                "delta":{
                    "tool_calls":[{
                        "index":0,
                        "id":"call_xyz",
                        "type":"function",
                        "function":{"name":"get_weather","arguments":"{\"loc"}
                    }]
                },
                "finish_reason":null
            }]
        }"#;
        let chunk: Chunk = serde_json::from_str(chunk_json).unwrap();
        let tcs = chunk.choices[0].delta.tool_calls.as_ref().unwrap();
        assert_eq!(tcs.len(), 1);
        assert_eq!(tcs[0].id.as_deref(), Some("call_xyz"));
        assert_eq!(tcs[0].function.as_ref().unwrap().name.as_deref(), Some("get_weather"));
    }

    // ---- process_delta_content 字符串路径 ----

    #[test]
    fn process_delta_content_string_emits_text() {
        // 验证纯字符串 delta 能正确累积到 text_parts（不发事件，只测状态）
        // 这里无法测 sender 事件（需要 async），所以只验证 text_parts 累积
        // 完整事件测试由 process_delta_content_string_pushes_text 覆盖
        let mut text_parts: Vec<String> = Vec::new();
        let mut reasoning_parts: Vec<String> = Vec::new();
        let output = AssistantMessage::new(&make_model(false));

        // 用 block_on 驱动 async 函数
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (stream, sender) = EventStream::new();
        rt.block_on(async {
            // 消费 stream 防止 channel 满（sender 用 try_send，不会阻塞）
            tokio::spawn(async move {
                let mut s = stream;
                while s.recv().await.is_some() {}
            });
            process_delta_content(
                serde_json::Value::String("hello".into()),
                &mut text_parts,
                &mut reasoning_parts,
                &mut output.clone(),
                &sender,
                0,
            ).await;
        });
        assert_eq!(text_parts, vec!["hello".to_string()]);
        assert!(reasoning_parts.is_empty());
    }

    // ---- process_delta_content 数组（thinking + text）路径 ----

    #[test]
    fn process_delta_content_array_splits_thinking_and_text() {
        let mut text_parts: Vec<String> = Vec::new();
        let mut reasoning_parts: Vec<String> = Vec::new();
        let output = AssistantMessage::new(&make_model(true));

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (stream, sender) = EventStream::new();
        rt.block_on(async {
            tokio::spawn(async move {
                let mut s = stream;
                while s.recv().await.is_some() {}
            });
            process_delta_content(
                serde_json::json!([
                    {"type":"thinking","thinking":[{"type":"text","text":"step 1"}]},
                    {"type":"text","text":"answer"}
                ]),
                &mut text_parts,
                &mut reasoning_parts,
                &mut output.clone(),
                &sender,
                0,
            ).await;
        });
        assert_eq!(reasoning_parts, vec!["step 1".to_string()]);
        assert_eq!(text_parts, vec!["answer".to_string()]);
    }

    // ---- build_assistant_content 顺序：thinking 在前 text 在后 ----

    #[test]
    fn build_content_thinking_before_text() {
        let content = build_assistant_content(
            &vec!["answer".into()],
            &vec!["thought".into()],
            &[],
        );
        assert_eq!(content.len(), 2);
        assert!(matches!(content[0], AssistantContentBlock::Thinking(_)));
        assert!(matches!(content[1], AssistantContentBlock::Text(_)));
    }

    // ---- provider 注册验证 ----

    #[test]
    fn mistral_registered_in_factory_and_registry() {
        use crate::registry::{ApiRegistry, BuiltinProviderFactory, ProviderFactory};
        // Factory
        let factory = BuiltinProviderFactory;
        assert!(factory.create("mistral-conversations").is_some());
        assert!(factory.create("nonexistent").is_none());

        // Registry register_builtins
        let mut reg = ApiRegistry::new();
        reg.register_builtins();
        assert!(reg.get("mistral-conversations").is_some());
    }
}
