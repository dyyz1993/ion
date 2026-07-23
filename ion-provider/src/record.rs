//! RecordingProvider — wraps a real provider, taps Done, writes trace + meta.

use crate::error::ProviderResult;
use crate::event_stream::EventStream;
use crate::registry::ApiProvider;
use crate::types::{AssistantContentBlock, AssistantMessage, Context, Model, StreamOptions, TextContent, ThinkingContent};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default, Serialize, Deserialize)]
struct RecordingMeta {
    schema_version: u32,
    id: String,
    model: String,
    provider: String,
    created_at: i64,
    response_count: u32,
    tool_call_count: u32,
    tool_calls: Vec<ToolCallSummary>,
}

#[derive(Serialize, Deserialize)]
struct ToolCallSummary {
    name: String,
    input_summary: String,
}

/// Wraps a real provider, recording each Done message to trace_path.
pub struct RecordingProvider {
    inner: Box<dyn ApiProvider>,
    trace_path: PathBuf,
    meta_path: PathBuf,
    meta: Arc<Mutex<RecordingMeta>>,
}

impl RecordingProvider {
    pub fn new(inner: Box<dyn ApiProvider>, trace_path: PathBuf, meta_path: PathBuf) -> Self {
        let meta = RecordingMeta {
            schema_version: 1,
            created_at: now_ms(),
            ..Default::default()
        };
        Self { inner, trace_path, meta_path, meta: Arc::new(Mutex::new(meta)) }
    }
}

#[async_trait]
impl ApiProvider for RecordingProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        // Update meta with model info (first call)
        {
            let mut m = self.meta.lock().unwrap();
            if m.model.is_empty() {
                m.model = model.id.clone();
                m.provider = model.provider.clone();
            }
        }
        // Compute request_hash (Phase 1: record only, don't enforce)
        let req_hash = request_hash(context, model);

        let inner_stream = self.inner.stream(model, context, options, cancel).await?;
        let trace_path = self.trace_path.clone();
        let meta_path = self.meta_path.clone();
        let meta_arc = self.meta.clone();

        Ok(EventStream::forward_with_done_tap(inner_stream, move |msg| {
            write_trace_line(&trace_path, msg, &req_hash);
            update_meta(&meta_arc, &meta_path, msg);
        }))
    }
}

fn request_hash(context: &Context, model: &Model) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    context.system_prompt.hash(&mut h);
    context.messages.len().hash(&mut h);
    model.id.hash(&mut h);
    model.api.hash(&mut h);
    format!("{:016x}", h.finish())
}

fn write_trace_line(trace_path: &Path, msg: &AssistantMessage, req_hash: &str) {
    if let Some(parent) = trace_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let line = serialize_response(msg, req_hash);
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(trace_path) {
        let _ = writeln!(f, "{}", line);
        let _ = std::fs::set_permissions(trace_path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    }
}

fn update_meta(meta_arc: &Arc<Mutex<RecordingMeta>>, meta_path: &Path, msg: &AssistantMessage) {
    let mut m = meta_arc.lock().unwrap();
    m.response_count += 1;
    for block in &msg.content {
        if let AssistantContentBlock::ToolCall(tc) = block {
            m.tool_call_count += 1;
            m.tool_calls.push(ToolCallSummary {
                name: tc.name.clone(),
                input_summary: serde_json::to_string(&tc.arguments).unwrap_or_default(),
            });
        }
    }
    if let Ok(content) = serde_json::to_string_pretty(&*m) {
        let tmp = meta_path.with_extension("json.tmp");
        if std::fs::write(&tmp, &content).is_ok() {
            let _ = std::fs::rename(&tmp, meta_path);
            let _ = std::fs::set_permissions(meta_path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
        }
    }
}

fn serialize_response(msg: &AssistantMessage, req_hash: &str) -> String {
    let mut text_parts = Vec::new();
    let mut thinking_parts = Vec::new();
    let mut tool_calls = Vec::new();
    for block in &msg.content {
        match block {
            AssistantContentBlock::Text(TextContent { text, .. }) => text_parts.push(text.clone()),
            AssistantContentBlock::Thinking(ThinkingContent { thinking, .. }) => thinking_parts.push(thinking.clone()),
            AssistantContentBlock::ToolCall(tc) => {
                tool_calls.push(serde_json::json!({"name": tc.name, "input": tc.arguments}));
            }
        }
    }
    let mut obj = serde_json::json!({});
    if !thinking_parts.is_empty() {
        obj["thinking"] = serde_json::Value::String(thinking_parts.join("\n"));
    }
    if !text_parts.is_empty() {
        obj["text"] = serde_json::Value::String(text_parts.join("\n"));
    }
    if let Some(first_tc) = tool_calls.into_iter().next() {
        obj["tool_call"] = first_tc;
    }
    // 总是记录 stop_reason（replay 需要它来决定是否执行 tool_call）
    let stop_str = match msg.stop_reason {
        crate::types::StopReason::ToolUse => "tool_use",
        crate::types::StopReason::Stop => "stop",
        crate::types::StopReason::Length => "length",
        crate::types::StopReason::Error => "error",
        crate::types::StopReason::Aborted => "aborted",
        _ => "stop",
    };
    obj["stop_reason"] = serde_json::Value::String(stop_str.into());
    if matches!(msg.stop_reason, crate::types::StopReason::Error) {
        if let Some(em) = &msg.error_message {
            obj["error_message"] = serde_json::Value::String(em.clone());
        }
    }
    obj["request_hash"] = serde_json::Value::String(req_hash.to_string());
    obj.to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
