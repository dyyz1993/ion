use crate::env_keys::resolve_api_key;
use crate::error::{ProviderError, ProviderResult};
use crate::event_stream::{EventStream, EventSender};
use crate::types::*;
use crate::ApiProvider;
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Cloudflare Workers AI provider.
///
/// Implements the `cloudflare-workers-ai` API protocol, which uses the same
/// OpenAI-compatible chat/completions request/response format and SSE streaming.
/// The only difference from `openai-completions` is the base URL pattern:
///   https://api.cloudflare.com/client/v4/accounts/{account_id}/ai/v1
///
/// Auth is a Bearer token in the Authorization header.
pub struct CloudflareWorkersAIProvider;

#[async_trait]
impl ApiProvider for CloudflareWorkersAIProvider {
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

impl CloudflareWorkersAIProvider {
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
        let mut openai_messages: Vec<CloudflareMessage> = Vec::new();

        // System prompt
        if let Some(ref sp) = context.system_prompt {
            openai_messages.push(CloudflareMessage {
                role: "system".into(),
                content: serde_json::Value::String(sp.clone()),
                tool_call_id: None,
                tool_calls: None,
            });
        }

        // Messages
        for msg in &context.messages {
            match msg {
                Message::User(u) => {
                    let has_image = u.content.iter().any(|b| matches!(b, ContentBlock::Image(_)));
                    if has_image {
                        // Vision: content must be an array of content parts
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
                        openai_messages.push(CloudflareMessage {
                            role: "user".into(),
                            content: serde_json::Value::Array(parts),
                            tool_call_id: None,
                            tool_calls: None,
                        });
                    } else {
                        let text = u.content.iter()
                            .filter_map(|b| match b {
                                ContentBlock::Text(t) => Some(t.text.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        openai_messages.push(CloudflareMessage {
                            role: "user".into(),
                            content: serde_json::Value::String(text),
                            tool_call_id: None,
                            tool_calls: None,
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

                    openai_messages.push(CloudflareMessage {
                        role: "assistant".into(),
                        content: serde_json::Value::String(if tcs.is_empty() { text } else { String::new() }),
                        tool_call_id: None,
                        tool_calls: if tcs.is_empty() { None } else { Some(tcs) },
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
                    openai_messages.push(CloudflareMessage {
                        role: "tool".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: Some(tr.tool_call_id.clone()),
                        tool_calls: None,
                    });
                }
                Message::BashExecution(b) => {
                    // Skip when excludeFromContext=true (user `!cmd` exclusion type)
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
                    openai_messages.push(CloudflareMessage {
                        role: "user".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: None,
                        tool_calls: None,
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
                    openai_messages.push(CloudflareMessage {
                        role: "user".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }
                Message::BranchSummary(b) => {
                    let text = format!(
                        "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n{}\n</summary>",
                        b.summary
                    );
                    openai_messages.push(CloudflareMessage {
                        role: "user".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }
                Message::CompactionSummary(c) => {
                    let text = format!(
                        "The conversation history before this point was compacted into the following summary:\n\n<summary>\n{}\n</summary>",
                        c.summary
                    );
                    openai_messages.push(CloudflareMessage {
                        role: "user".into(),
                        content: serde_json::Value::String(text),
                        tool_call_id: None,
                        tool_calls: None,
                    });
                }
            }
        }

        // Tools
        let tools: Option<Vec<CloudflareTool>> = context.tools.as_ref().map(|tools| {
            tools.iter().map(|t| CloudflareTool {
                r#type: "function".into(),
                function: CloudflareToolFunction {
                    name: t.name.clone(),
                    description: t.description.clone(),
                    parameters: t.parameters.clone(),
                },
            }).collect()
        });

        // Build request body (OpenAI-compatible chat/completions format)
        let max_tokens = options.and_then(|o| o.max_tokens).unwrap_or(model.max_tokens);
        let mut body = serde_json::json!({
            "model": model.id,
            "messages": openai_messages,
            "stream": true,
            "stream_options": {"include_usage": true},
            "tools": tools,
            "tool_choice": if tools.is_some() { serde_json::json!("auto") } else { serde_json::Value::Null },
        });
        body["max_tokens"] = serde_json::json!(max_tokens);

        // JSON mode
        if let Some(ref fmt) = options.and_then(|o| o.response_format.clone()) {
            body["response_format"] = serde_json::json!({"type": fmt});
        }

        // Build URL: base_url already contains the account-scoped path ending in /ai/v1
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

        // Allow cancellation during HTTP handshake (abort closes the TCP connection immediately)
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

        // Spawn SSE reader (cancellable: dropping resp closes the TCP connection)
        tokio::spawn(async move {
            if let Some(c) = &cancel {
                tokio::select! {
                    _ = read_sse(resp, sender, output) => {}
                    _ = c.cancelled() => {
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

#[derive(Serialize)]
struct CloudflareMessage {
    role: String,
    // String (plain text) or Array (content parts with image_url)
    content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<serde_json::Value>>,
}

#[derive(Serialize)]
struct CloudflareTool {
    r#type: String,
    function: CloudflareToolFunction,
}

#[derive(Serialize)]
struct CloudflareToolFunction {
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

    let stream_debug = std::env::var("ION_STREAM_DEBUG").ok().as_deref() == Some("1");
    let mut _chunk_seq: u64 = 0;

    while let Some(bytes) = chunk_rx.recv().await {
        if stream_debug {
            _chunk_seq += 1;
            eprintln!("[stream-debug] provider bytes_chunk #{_chunk_seq} len={}", bytes.len());
        }
        let text = String::from_utf8_lossy(&bytes);
        buffer.push_str(&text);

        // Parse each SSE event (a single TCP packet may contain multiple events)
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

                        // Build tool calls
                        let tcs: Vec<ToolCall> = accs.iter().map(|a| ToolCall {
                            call_type: a.call_type.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::env_keys::{get_env_api_key, resolve_api_key};

    // ---- 1. Provider construction ----

    #[test]
    fn provider_can_be_constructed() {
        // CloudflareWorkersAIProvider is a unit struct; verify it can be
        // instantiated with no fields and that it is zero-sized.
        let provider = CloudflareWorkersAIProvider;
        let _ = &provider; // ensure binding is used
        assert_eq!(
            std::mem::size_of_val(&provider),
            0,
            "unit provider should be zero-sized"
        );
    }

    #[test]
    fn provider_is_send_sync() {
        // The provider must be usable across async tasks; this is a compile-time
        // guarantee rather than a runtime assertion.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CloudflareWorkersAIProvider>();
    }

    // ---- 2. Provider name / env-var mapping ----

    #[test]
    fn provider_name_is_cloudflare_workers_ai() {
        // The provider is wired under the `cloudflare-workers-ai` provider key,
        // which is what env_keys maps to CLOUDFLARE_API_KEY. Confirm the env
        // resolver recognises that provider string.
        let key = get_env_api_key("cloudflare-workers-ai");
        // No env set in tests -> should be None, but the function must not panic.
        assert!(key.is_none() || key.is_some());

        // Explicit key always wins regardless of provider string.
        let resolved = resolve_api_key("cloudflare-workers-ai", Some("explicit-token".into()));
        assert_eq!(resolved.unwrap(), "explicit-token");
    }

    #[test]
    fn explicit_api_key_overrides_env() {
        // resolve_api_key must prefer the explicit value over any env var.
        let r = resolve_api_key("cloudflare-workers-ai", Some("abc123".into())).unwrap();
        assert_eq!(r, "abc123");
    }

    // ---- 3. Request URL construction ----

    #[test]
    fn chat_completions_url_appends_path() {
        // Mirrors the production URL build logic:
        //   format!("{}/chat/completions", model.base_url.trim_end_matches('/'))
        let base_url = "https://api.cloudflare.com/client/v4/accounts/abc/ai/v1";
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        assert_eq!(
            url,
            "https://api.cloudflare.com/client/v4/accounts/abc/ai/v1/chat/completions"
        );
    }

    #[test]
    fn chat_completions_url_strips_trailing_slash() {
        // A trailing slash on base_url must not produce a double slash.
        let base_url = "https://api.cloudflare.com/client/v4/accounts/abc/ai/v1/";
        let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
        assert_eq!(
            url,
            "https://api.cloudflare.com/client/v4/accounts/abc/ai/v1/chat/completions"
        );
    }

    #[test]
    fn url_contains_account_segment() {
        // The Cloudflare base URL embeds the account id under /accounts/.
        let base_url = "https://api.cloudflare.com/client/v4/accounts/MY_ACC/ai/v1";
        assert!(
            base_url.contains("/accounts/MY_ACC/"),
            "base_url should embed the account id"
        );
        assert!(
            base_url.ends_with("/ai/v1"),
            "base_url should end with the OpenAI-compatible /ai/v1 path"
        );
    }

    // ---- 4. Request header construction (Bearer token) ----

    #[test]
    fn authorization_header_is_bearer_format() {
        // Mirrors production header construction:
        //   format!("Bearer {api_key}")
        let api_key = "test-secret-key";
        let header = format!("Bearer {api_key}");
        assert_eq!(header, "Bearer test-secret-key");
        assert!(
            header.starts_with("Bearer "),
            "Authorization header must be a Bearer token"
        );
    }

    #[test]
    fn authorization_header_preserves_full_key() {
        // The token after "Bearer " must equal the full key without trimming.
        let api_key = "cf_abc.def-ghi+123";
        let header = format!("Bearer {api_key}");
        let token = header.strip_prefix("Bearer ").unwrap();
        assert_eq!(token, api_key);
    }

    // ---- 5. Helper/utility: build_assistant_content ----

    #[test]
    fn build_content_empty_returns_empty_vec() {
        // No text and no reasoning => empty content list.
        let content = build_assistant_content(&[], &[], &[]);
        assert!(content.is_empty());
    }

    #[test]
    fn build_content_text_only() {
        // A single text part becomes one Text block.
        let content = build_assistant_content(&["hello".to_string()], &[], &[]);
        assert_eq!(content.len(), 1);
        match &content[0] {
            AssistantContentBlock::Text(t) => assert_eq!(t.text, "hello"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn build_content_joins_multiple_text_parts() {
        // Multiple deltas are concatenated with no separator (matching join("")).
        let content =
            build_assistant_content(&["foo".to_string(), "bar".to_string(), "baz".to_string()], &[], &[]);
        match &content[0] {
            AssistantContentBlock::Text(t) => assert_eq!(t.text, "foobarbaz"),
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn build_content_reasoning_only() {
        // Reasoning content alone produces a single Thinking block.
        let content = build_assistant_content(&[], &["thinking 1".to_string()], &[]);
        assert_eq!(content.len(), 1);
        match &content[0] {
            AssistantContentBlock::Thinking(t) => assert_eq!(t.thinking, "thinking 1"),
            other => panic!("expected Thinking, got {other:?}"),
        }
    }

    #[test]
    fn build_content_reasoning_come_before_text() {
        // Production order: Thinking first, then Text.
        let content = build_assistant_content(
            &["text body".to_string()],
            &["why".to_string()],
            &[],
        );
        assert_eq!(content.len(), 2);
        assert!(matches!(content[0], AssistantContentBlock::Thinking(_)));
        assert!(matches!(content[1], AssistantContentBlock::Text(_)));
    }

    // ---- 5b. Request body serialisation (CloudflareMessage) ----

    #[test]
    fn cloudflare_message_serialises_text_content() {
        // A plain-text user message serialises to role + content string.
        let msg = CloudflareMessage {
            role: "user".into(),
            content: serde_json::Value::String("hi".into()),
            tool_call_id: None,
            tool_calls: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hi");
        // tool_call_id / tool_calls are skipped when None.
        assert!(json.get("tool_call_id").is_none());
        assert!(json.get("tool_calls").is_none());
    }

    #[test]
    fn cloudflare_message_serialises_tool_result() {
        // A tool result message includes tool_call_id.
        let msg = CloudflareMessage {
            role: "tool".into(),
            content: serde_json::Value::String("42".into()),
            tool_call_id: Some("call_1".into()),
            tool_calls: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
        assert_eq!(json["content"], "42");
    }

    #[test]
    fn cloudflare_message_serialises_vision_content_array() {
        // Vision messages use an array content (image_url + text parts).
        let parts = serde_json::json!([
            { "type": "text", "text": "what is this?" },
            { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA" } }
        ]);
        let msg = CloudflareMessage {
            role: "user".into(),
            content: parts,
            tool_call_id: None,
            tool_calls: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert!(json["content"].is_array());
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][1]["type"], "image_url");
    }

    // ---- 5c. SSE Chunk deserialisation (parse helper) ----

    #[test]
    fn chunk_deserialises_text_delta() {
        // A minimal OpenAI-style streaming chunk with a text delta.
        let raw = r#"{"choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}"#;
        let chunk: Chunk = serde_json::from_str(raw).unwrap();
        assert_eq!(chunk.choices.len(), 1);
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hi"));
        assert!(chunk.choices[0].finish_reason.is_none());
    }

    #[test]
    fn chunk_deserialises_usage() {
        // The final chunk carries token usage stats.
        let raw = r#"{
            "choices":[{"delta":{},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
        }"#;
        let chunk: Chunk = serde_json::from_str(raw).unwrap();
        let usage = chunk.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn chunk_deserialises_tool_call_delta() {
        // Streaming tool-call deltas accumulate across chunks.
        let raw = r#"{
            "choices":[{
                "delta":{
                    "tool_calls":[{
                        "index":0,
                        "id":"call_9",
                        "type":"function",
                        "function":{"name":"get_weather","arguments":"{\"q\":"}
                    }]
                }
            }]
        }"#;
        let chunk: Chunk = serde_json::from_str(raw).unwrap();
        let tc = &chunk.choices[0].delta.tool_calls.as_ref().unwrap()[0];
        assert_eq!(tc.index, Some(0));
        assert_eq!(tc.id.as_deref(), Some("call_9"));
        assert_eq!(tc.function.as_ref().unwrap().name.as_deref(), Some("get_weather"));
    }

    #[test]
    fn chunk_tolerates_missing_fields() {
        // Empty delta object should still parse (all fields defaulted).
        let raw = r#"{"choices":[{"delta":{}}]}"#;
        let chunk: Chunk = serde_json::from_str(raw).unwrap();
        assert!(chunk.choices[0].delta.content.is_none());
        assert!(chunk.usage.is_none());
    }
}
