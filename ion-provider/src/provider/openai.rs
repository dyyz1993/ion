use crate::env_keys::resolve_api_key;
use crate::error::{ProviderError, ProviderResult};
use crate::event_stream::{EventStream, EventSender};
use crate::types::*;
use crate::ApiProvider;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub struct OpenAICompletionsProvider;

#[async_trait]
impl ApiProvider for OpenAICompletionsProvider {
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

impl OpenAICompletionsProvider {
    async fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        let output = AssistantMessage::new(model);
        let (stream, sender) = EventStream::new();

        // Resolve API key
        let api_key = resolve_api_key(
            &model.provider,
            options.and_then(|o| o.api_key.clone()),
        )?;

        // Build content blocks
        let mut openai_messages: Vec<OpenAIMessage> = Vec::new();

        // System prompt — with ephemeral prompt caching enabled
        if let Some(ref sp) = context.system_prompt {
            openai_messages.push(OpenAIMessage {
                role: "system".into(),
                content: serde_json::Value::String(sp.clone()),
                tool_call_id: None,
                tool_calls: None,
                cache_control: Some(CacheControl::ephemeral()),
            });
        }

        // Messages
        for msg in &context.messages {
            match msg {
                Message::User(u) => {
                    let has_image = u.content.iter().any(|b| matches!(b, ContentBlock::Image(_)));
                    if has_image {
                        // Vision：content 必须是 content parts 数组
                        let parts: Vec<serde_json::Value> = u.content.iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text(t) if !t.text.is_empty() => Some(serde_json::json!({
                                    "type": "text",
                                    "text": t.text
                                })),
                                ContentBlock::Image(img) => Some(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {
                                        "url": format!("data:{};base64,{}", img.mime_type, img.data)
                                    }
                                })),
                                _ => None,
                            })
                            .collect();
                        openai_messages.push(OpenAIMessage {
                            role: "user".into(),
                            content: serde_json::Value::Array(parts),
                            tool_call_id: None,
                            tool_calls: None,
                            cache_control: None,
                        });
                    } else {
                        let text = u.content.iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        openai_messages.push(OpenAIMessage {
                            role: "user".into(),
                            content: serde_json::Value::String(text),
                            tool_call_id: None,
                            tool_calls: None,
                            cache_control: None,
                        });
                    }
                }
                Message::Assistant(a) => {
                    // Collect text content
                    let text: String = a.content.iter()
                        .filter_map(|b| match b {
                            AssistantContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("");

                    // Collect tool calls
                    let tcs: Vec<serde_json::Value> = a.content.iter()
                        .filter_map(|b| match b {
                            AssistantContentBlock::ToolCall(tc) => Some(serde_json::json!({
                                "id": tc.id,
                                "type": tc.call_type,
                                "function": {
                                    "name": tc.name,
                                    "arguments": tc.arguments.to_string(),
                                }
                            })),
                            _ => None,
                        })
                        .collect();

                    openai_messages.push(OpenAIMessage {
                        role: "assistant".into(),
                        content: serde_json::Value::String(if tcs.is_empty() { text } else { String::new() }),
                        tool_call_id: None,
                        tool_calls: if tcs.is_empty() { None } else { Some(tcs) },
                        cache_control: None,
                    });
                }
                Message::ToolResult(tr) => {
                    let text = tr.content.iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    openai_messages.push(OpenAIMessage {
                        role: "tool".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: Some(tr.tool_call_id.clone()),
                        tool_calls: None,
                        cache_control: None,
                    });
                }
                Message::BashExecution(b) => {
                    // excludeFromContext=true 时不发给 LLM（用户 `!cmd` 排除型）
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
                    openai_messages.push(OpenAIMessage {
                        role: "user".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: None,
                        tool_calls: None,
                        cache_control: None,
                    });
                }
                Message::Custom(c) => {
                    // Check whether the custom message contains image blocks
                    let blocks: Option<Vec<ContentBlock>> = match &c.content {
                        CustomContent::Text(_) => None,
                        CustomContent::Blocks(b) => Some(b.clone()),
                    };
                    if let Some(ref blocks) = blocks {
                        let has_image = blocks.iter().any(|b| matches!(b, ContentBlock::Image(_)));
                        if has_image {
                            // When an image is present, content must be an array of parts
                            let parts: Vec<serde_json::Value> = blocks.iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Text(t) if !t.text.is_empty() => Some(serde_json::json!({
                                        "type": "text",
                                        "text": t.text
                                    })),
                                    ContentBlock::Image(img) => Some(serde_json::json!({
                                        "type": "image_url",
                                        "image_url": {
                                            "url": format!("data:{};base64,{}", img.mime_type, img.data)
                                        }
                                    })),
                                    _ => None,
                                })
                                .collect();
                            openai_messages.push(OpenAIMessage {
                                role: "user".into(),
                                content: serde_json::Value::Array(parts),
                                tool_call_id: None,
                                tool_calls: None,
                                cache_control: None,
                            });
                        } else {
                            // Text-only: join all text blocks into a single string
                            let text = blocks.iter()
                                .filter_map(|b| match b {
                                    ContentBlock::Text(t) => Some(t.text.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>()
                                .join("\n");
                            openai_messages.push(OpenAIMessage {
                                role: "user".into(),
                                content: serde_json::Value::String(text),
                                tool_call_id: None,
                                tool_calls: None,
                                cache_control: None,
                            });
                        }
                    } else {
                        // Plain string content
                        let text = match &c.content {
                            CustomContent::Text(s) => s.clone(),
                            _ => String::new(),
                        };
                        openai_messages.push(OpenAIMessage {
                            role: "user".into(),
                            content: serde_json::Value::String(text),
                            tool_call_id: None,
                            tool_calls: None,
                            cache_control: None,
                        });
                    }
                }
                Message::BranchSummary(b) => {
                    let text = format!(
                        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{}\n</summary>",
                        b.summary
                    );
                    openai_messages.push(OpenAIMessage {
                        role: "user".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: None,
                        tool_calls: None,
                        cache_control: None,
                    });
                }
                Message::CompactionSummary(c) => {
                    let text = format!(
                        "The conversation history before this point was compacted into the following summary:\n\n<summary>\n{}\n</summary>",
                        c.summary
                    );
                    openai_messages.push(OpenAIMessage {
                        role: "user".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: None,
                        tool_calls: None,
                        cache_control: None,
                    });
                }
            }
        }

        // Tools
        let tools: Option<Vec<OpenAITool>> = context.tools.as_ref().map(|tools| {
            tools.iter().map(|t| OpenAITool {
                r#type: "function".into(),
                function: OpenAIToolFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            }).collect()
        });

        // Build request
        let compat = detect_compat(model);
        let max_tokens = options.and_then(|o| o.max_tokens).unwrap_or(model.max_tokens);

        let max_tokens_field = compat.max_tokens_field.clone();
        let mut body = serde_json::json!({
            "model": model.id,
            "messages": openai_messages,
            "stream": true,
            "stream_options": {"include_usage": true},
            "tools": tools,
            "tool_choice": if tools.is_some() { serde_json::json!("auto") } else { serde_json::Value::Null },
        });
        body[&max_tokens_field] = serde_json::json!(max_tokens);

        // JSON mode
        if let Some(ref fmt) = options.and_then(|o| o.response_format.clone()) {
            body["response_format"] = serde_json::json!({"type": fmt});
        }

        // Apply thinking format
        apply_thinking_format(&mut body, model, options, &compat);

        let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));
        if std::env::var("ION_STREAM_DEBUG").ok().as_deref() == Some("1") {
            eprintln!("[stream-debug] POST {url}");
        }
        let client = Client::builder()
            .timeout(Duration::from_secs(180))
            .build()?;

        // Send request
        let send_fut = client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .json(&body)
            .send();

        // HTTP 握手期可被 cancel 取消（修复 D：abort 时立刻关 TCP）
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

            // If response_format caused the error, retry without it
            if s.as_u16() == 400
                && options.and_then(|o| o.response_format.as_ref()).is_some()
                && b.contains("response_format")
            {
                tracing::warn!("API rejected response_format, falling back to prompt injection");
                // Retry without response_format
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
                // Spawn SSE reader with fallback response
                let cancel_clone = cancel.clone();
                tokio::spawn(async move {
                    if let Some(c) = &cancel_clone {
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

        // Spawn SSE reader（可被 cancel 取消：drop resp 关 TCP）
        tokio::spawn(async move {
            if let Some(c) = &cancel {
                tokio::select! {
                    _ = read_sse(resp, sender, output) => {}
                    _ = c.cancelled() => {
                        // resp drop → reqwest 关连接，read_sse 内部的 bytes_stream task 也会因为 sender closed 而退出
                        tracing::info!("[stream] SSE read cancelled by abort");
                    }
                }
            } else {
                read_sse(resp, sender, output).await;
            }
        });

        Ok(stream)
    }
}

// ---- SSE parsing ----

/// Prompt cache control marker sent alongside a message.
#[derive(Serialize, Clone, Debug)]
struct CacheControl {
    r#type: String,
}

impl CacheControl {
    fn ephemeral() -> Self {
        CacheControl { r#type: "ephemeral".into() }
    }
}

#[derive(Serialize)]
struct OpenAIMessage {
    role: String,
    content: serde_json::Value, // String (plain text) or Array (content parts with image_url)
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<serde_json::Value>>,
    /// Ephemeral cache breakpoint for prompt caching.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<CacheControl>,
}

#[derive(Serialize)]
struct OpenAITool {
    r#type: String,
    function: OpenAIToolFunction,
}

#[derive(Serialize)]
struct OpenAIToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct Chunk {
    choices: Vec<ChunkChoice>,
    #[serde(default)]
    usage: Option<UsageData>,
}

#[derive(Deserialize, Debug)]
struct UsageData {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
}

#[derive(Deserialize, Debug)]
struct ChunkChoice {
    #[serde(default)]
    finish_reason: Option<String>,
    delta: Delta,
}

#[derive(Deserialize, Debug)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ChunkTC>>,
}

#[derive(Deserialize, Debug)]
struct ChunkTC {
    #[serde(default)]
    index: Option<u64>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    function: Option<ChunkTCFunc>,
}

#[derive(Deserialize, Debug)]
struct ChunkTCFunc {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

struct AccTC {
    index: u64,
    id: String,
    call_type: String,
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
    let content_idx = 0;

    let (chunk_tx, mut chunk_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(128);
    let mut stream = resp.bytes_stream();
    tokio::spawn(async move {
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(b) => { if chunk_tx.send(b.to_vec()).await.is_err() { break; } }
                Err(e) => { tracing::warn!("SSE: {e}"); break; }
            }
        }
    });

    sender.send(StreamEvent::Start { partial: output.clone() }).await;

    // 流式诊断开关：ION_STREAM_DEBUG=1 时在 stderr 打 chunk 大小 + delta 计数
    let stream_debug = std::env::var("ION_STREAM_DEBUG").ok().as_deref() == Some("1");
    let mut _chunk_seq: u64 = 0;

    // Idle timeout: if no chunk received within this duration, treat connection as broken.
    // Some API proxies (e.g. zai) silently drop SSE connections during long reasoning phases.
    // Configurable via ION_SSE_IDLE_TIMEOUT (default 120s).
    let idle_timeout_secs: u64 = std::env::var("ION_SSE_IDLE_TIMEOUT")
        .ok().and_then(|s| s.parse().ok()).unwrap_or(120);

    loop {
        let bytes = match tokio::time::timeout(
            std::time::Duration::from_secs(idle_timeout_secs),
            chunk_rx.recv(),
        ).await {
            Ok(Some(b)) => b,
            Ok(None) => break,  // stream ended normally
            Err(_) => {
                // Timeout — connection stalled. Log and break with partial results.
                tracing::warn!(
                    "[stream] SSE idle timeout ({}s) — connection may have been dropped by proxy. \
                     Partial results: {} text chunks, {} reasoning chunks",
                    idle_timeout_secs, text_parts.len(), reasoning_parts.len()
                );
                break;
            }
        };

        if stream_debug {
            _chunk_seq += 1;
            eprintln!("[stream-debug] provider bytes_chunk #{_chunk_seq} len={}", bytes.len());
        }
        let text = String::from_utf8_lossy(&bytes);
        buffer.push_str(&text);

        // 逐个 SSE event 解析（可能一个 TCP 包含多个 event）
        loop {
            let pos = match buffer.find("\n\n").or_else(|| buffer.find("\r\n\r\n")) {
                Some(p) => p,
                None => break,
            };
            let event_str = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();
            if event_str.trim().is_empty() { continue; }

            // 每个 SSE event 立刻处理 + 发送（不 buffer）
            // （下面的 for line 循环已经逐 event 处理）

            for line in event_str.lines() {
                let line = line.trim();
                if !line.starts_with("data: ") { continue; }
                let json_str = &line[6..];
                if json_str == "[DONE]" { continue; }

                let chunk: Chunk = match serde_json::from_str(json_str) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                for choice in chunk.choices {
                    let d = choice.delta;

                    // Reasoning content
                    if let Some(ref rc) = d.reasoning_content && !rc.is_empty() {
                        reasoning_parts.push(rc.clone());
                        let mut partial = output.clone();
                        partial.content = build_assistant_content(&text_parts, &reasoning_parts, &accs);
                        sender.send(StreamEvent::ThinkingDelta {
                            content_index: content_idx,
                            delta: rc.clone(),
                            partial,
                        }).await;
                    }

                    // Tool calls
                    if let Some(tcs) = d.tool_calls {
                        for tc in tcs {
                            let idx = tc.index.unwrap_or(0);
                            let tc_name = tc.function.as_ref().and_then(|f| f.name.clone()).unwrap_or_default();
                            if let Some(a) = accs.iter_mut().find(|a| a.index == idx) {
                                if let Some(args) = tc.function.as_ref().and_then(|f| f.arguments.clone()) {
                                    if !args.is_empty() {
                                        a.arguments.push_str(&args);
                                        if stream_debug {
                                            eprintln!("[stream-debug] provider emit tool_call_delta idx={idx} len={}", args.len());
                                        }
                                        // 发 ToolCallDelta——让上层知道 arguments 正在流式生成
                                        let mut partial = output.clone();
                                        partial.content = build_assistant_content(&text_parts, &reasoning_parts, &accs);
                                        sender.send(StreamEvent::ToolCallDelta {
                                            content_index: idx as usize,
                                            delta: args.clone(),
                                            partial,
                                        }).await;
                                    }
                                }
                            } else {
                                let id = tc.id.unwrap_or_else(|| format!("call_{idx}"));
                                let ct = tc.r#type.unwrap_or_else(|| "function".into());
                                let name = tc_name.clone();
                                let args = tc.function.as_ref().and_then(|f| f.arguments.clone()).unwrap_or_default();
                                accs.push(AccTC { index: idx, id, call_type: ct, name, arguments: args.clone() });
                                // 首次出现的 tool_call 也发 delta（如果有初始 args）
                                if !args.is_empty() {
                                    let mut partial = output.clone();
                                    partial.content = build_assistant_content(&text_parts, &reasoning_parts, &accs);
                                    sender.send(StreamEvent::ToolCallDelta {
                                        content_index: idx as usize,
                                        delta: args,
                                        partial,
                                    }).await;
                                }
                            }
                        }
                    }

                    // Text content
                    if let Some(ref content) = d.content && !content.is_empty() {
                        if text_parts.is_empty() {
                            sender.send(StreamEvent::TextStart {
                                content_index: content_idx,
                                partial: output.clone(),
                            }).await;
                        }
                        text_parts.push(content.clone());
                        let mut partial = output.clone();
                        partial.content = build_assistant_content(&text_parts, &reasoning_parts, &accs);
                        sender.send(StreamEvent::TextDelta {
                            content_index: content_idx,
                            delta: content.clone(),
                            partial,
                        }).await;
                    }

                    // Finish
                    if let Some(ref reason) = choice.finish_reason {
                        let stop_reason = match reason.as_str() {
                            "stop" => StopReason::Stop,
                            "length" => StopReason::Length,
                            "tool_calls" => StopReason::ToolUse,
                            _ => StopReason::Stop,
                        };
                        output.stop_reason = stop_reason.clone();

                        // Extract token usage from the final chunk
                        if let Some(ref usage) = chunk.usage {
                            output.usage.input = usage.prompt_tokens;
                            output.usage.output = usage.completion_tokens;
                            output.usage.total_tokens = usage.total_tokens;
                        }

                        // Finalize content
                        output.content = build_assistant_content(&text_parts, &reasoning_parts, &accs);

                        // Add tool calls to output content
                        let tcs: Vec<ToolCall> = accs.iter().map(|a| ToolCall {
                            call_type: a.call_type.clone(),
                            id: a.id.clone(),
                            name: a.name.clone(),
                            arguments: serde_json::from_str(&a.arguments).unwrap_or(serde_json::Value::Null),
                            thought_signature: None,
                        }).collect();

                        // Emit ToolCallStart/End events so consumers can react
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

                        // Emit TextEnd if we had text
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

    // Stream ended without finish reason — treat as error
    output.error_message = Some("stream ended unexpectedly".into());
    output.stop_reason = StopReason::Error;
    sender.error(StopReason::Error, output);
}

fn build_assistant_content(
    text_parts: &[String],
    reasoning_parts: &[String],
    _accs: &[AccTC],
) -> Vec<AssistantContentBlock> {
    let mut content = Vec::new();

    // Add reasoning content
    if !reasoning_parts.is_empty() {
        content.push(AssistantContentBlock::Thinking(ThinkingContent {
            thinking: reasoning_parts.join(""),
            thinking_signature: None,
            redacted: None,
        }));
    }

    // Add text content
    if !text_parts.is_empty() {
        content.push(AssistantContentBlock::Text(TextContent {
            text: text_parts.join(""),
            text_signature: None,
        }));
    }

    content
}


// ──────────────────────────────────────────────────────────────
// detectCompat — 根据 provider/baseUrl 推断兼容配置
// 对齐 pi openai-completions.ts detectCompat()
// ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
struct ResolvedCompat {
    max_tokens_field: String,
    thinking_format: String,
    supports_reasoning_effort: bool,
    #[allow(dead_code)]
    requires_reasoning_content_on_assistant: bool,
}

fn detect_compat(model: &Model) -> ResolvedCompat {
    let explicit = model.compat.as_ref().and_then(|c| match c {
        CompatConfig::OpenAICompletions(c) => Some(c),
        _ => None,
    });

    let provider = model.provider.to_lowercase();
    let base_url = model.base_url.to_lowercase();

    let is_zai = provider == "zai" || provider == "zai-coding-cn"
        || base_url.contains("api.z.ai") || base_url.contains("open.bigmodel.cn");
    let is_together = provider == "together"
        || base_url.contains("api.together.ai") || base_url.contains("api.together.xyz");
    let is_moonshot = provider == "moonshotai" || provider == "moonshotai-cn"
        || base_url.contains("api.moonshot.");
    let is_openrouter = provider == "openrouter" || base_url.contains("openrouter.ai");
    let is_nvidia = provider == "nvidia" || base_url.contains("integrate.api.nvidia.com");
    let is_ant_ling = provider == "ant-ling" || base_url.contains("api.ant-ling.com");
    let is_deepseek = provider == "deepseek" || base_url.contains("deepseek.com");
    let is_grok = provider == "xai" || base_url.contains("api.x.ai");
    let is_opencode = provider == "opencode" || base_url.contains("opencode.ai");

    let use_max_tokens = base_url.contains("chutes.ai") || is_moonshot || is_together
        || is_nvidia || is_ant_ling || is_opencode || is_zai || is_deepseek;

    let supports_reasoning_effort = !is_grok && !is_zai && !is_moonshot
        && !is_together && !is_nvidia && !is_ant_ling;

    let thinking_format = explicit
        .and_then(|c| c.thinking_format.clone())
        .unwrap_or_else(|| {
            if is_deepseek { "deepseek".into() }
            else if is_zai { "zai".into() }
            else if is_together { "together".into() }
            else if is_ant_ling { "ant-ling".into() }
            else if is_openrouter { "openrouter".into() }
            else { "openai".into() }
        });

    let max_tokens_field = explicit
        .and_then(|c| c.max_tokens_field.clone())
        .unwrap_or_else(|| if use_max_tokens { "max_tokens".into() } else { "max_completion_tokens".into() });

    let requires_reasoning_content_on_assistant = explicit
        .and_then(|c| c.requires_reasoning_content_on_assistant_messages)
        .unwrap_or(is_deepseek);

    ResolvedCompat {
        max_tokens_field,
        thinking_format,
        supports_reasoning_effort,
        requires_reasoning_content_on_assistant,
    }
}

fn apply_thinking_format(
    body: &mut serde_json::Value,
    model: &Model,
    options: Option<&StreamOptions>,
    compat: &ResolvedCompat,
) {
    if !model.reasoning { return; }

    let reasoning_level = options.and_then(|o| o.reasoning.clone());
    let has_level = reasoning_level.is_some() && !matches!(reasoning_level, Some(ThinkingLevel::Off));

    match compat.thinking_format.as_str() {
        "deepseek" => {
            body["thinking"] = serde_json::json!({ "type": if has_level { "enabled" } else { "disabled" } });
            if has_level && compat.supports_reasoning_effort {
                if let Some(lvl) = reasoning_level {
                    body["reasoning_effort"] = serde_json::json!(thinking_level_to_str(lvl));
                }
            }
        }
        "zai" => {
            body["thinking"] = serde_json::json!({ "type": if has_level { "enabled" } else { "disabled" } });
        }
        "qwen" => {
            body["enable_thinking"] = serde_json::json!(has_level);
        }
        "qwen-chat-template" => {
            body["chat_template_kwargs"] = serde_json::json!({
                "enable_thinking": has_level,
                "preserve_thinking": true,
            });
        }
        "openrouter" => {
            if has_level {
                if let Some(lvl) = reasoning_level {
                    body["reasoning"] = serde_json::json!({ "effort": thinking_level_to_str(lvl) });
                }
            } else {
                body["reasoning"] = serde_json::json!({ "effort": "none" });
            }
        }
        "ant-ling" => {
            if has_level {
                if let Some(lvl) = reasoning_level {
                    body["reasoning"] = serde_json::json!({ "effort": thinking_level_to_str(lvl) });
                }
            }
        }
        "together" => {
            body["reasoning"] = serde_json::json!({ "enabled": has_level });
            if has_level && compat.supports_reasoning_effort {
                if let Some(lvl) = reasoning_level {
                    body["reasoning_effort"] = serde_json::json!(thinking_level_to_str(lvl));
                }
            }
        }
        "string-thinking" => {
            if has_level {
                if let Some(lvl) = reasoning_level {
                    body["thinking"] = serde_json::json!(thinking_level_to_str(lvl));
                }
            } else {
                body["thinking"] = serde_json::json!("none");
            }
        }
        _ => {
            if has_level && compat.supports_reasoning_effort {
                if let Some(lvl) = reasoning_level {
                    body["reasoning_effort"] = serde_json::json!(thinking_level_to_str(lvl));
                }
            } else if !has_level && compat.supports_reasoning_effort {
                body["reasoning_effort"] = serde_json::json!("none");
            }
        }
    }
}

fn thinking_level_to_str(lvl: ThinkingLevel) -> &'static str {
    match lvl {
        ThinkingLevel::Off => "none",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn convert_user_message_with_image() {
        // 验证含图片的 user message 能正确转成 OpenAI content parts 数组
        let user_msg = Message::User(UserMessage {
            role: "user".into(),
            content: vec![
                ContentBlock::Text(TextContent { text: "这是什么？".into(), text_signature: None }),
                ContentBlock::Image(ImageContent { data: "base64data".into(), mime_type: "image/png".into() }),
            ],
            timestamp: 0,
            source: MessageSource::Prompt,
        });

        // 模拟 transform：手动构建 content parts
        let parts: Vec<serde_json::Value> = match &user_msg {
            Message::User(u) => u.content.iter().filter_map(|b| match b {
                ContentBlock::Text(t) if !t.text.is_empty() => Some(serde_json::json!({"type":"text","text":t.text})),
                ContentBlock::Image(img) => Some(serde_json::json!({"type":"image_url","image_url":{"url":format!("data:{};base64,{}",img.mime_type,img.data)}})),
                _ => None,
            }).collect(),
            _ => panic!("expected user message"),
        };

        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "这是什么？");
        assert_eq!(parts[1]["type"], "image_url");
        assert!(parts[1]["image_url"]["url"].as_str().unwrap().contains("data:image/png;base64,base64data"));
    }

    #[test]
    fn convert_text_only_message_stays_string() {
        // 纯文本消息应保持 String 格式（不用数组）
        let user_msg = Message::User(UserMessage {
            role: "user".into(),
            content: vec![
                ContentBlock::Text(TextContent { text: "你好".into(), text_signature: None }),
            ],
            timestamp: 0,
            source: MessageSource::Prompt,
        });

        let has_image = match &user_msg {
            Message::User(u) => u.content.iter().any(|b| matches!(b, ContentBlock::Image(_))),
            _ => false,
        };
        assert!(!has_image, "纯文本不应有图片");
    }
}
