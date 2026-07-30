//! FauxProvider — architecture-level LLM mock.
//! Registers as "faux" ApiProvider; FIFO-replays pre-scripted responses.
//! Mirrors pi's @dyyz1993/pi-ai FauxProvider (packages/ai/src/providers/faux.ts).

use crate::error::ProviderResult;
use crate::event_stream::{EventSender, EventStream};
use crate::registry::{ApiProvider, ApiRegistry};
use crate::types::StreamEvent;
use crate::types::{
    AssistantContentBlock, AssistantMessage, Context, Model, StopReason, StreamOptions,
    TextContent, ThinkingContent, ToolCall, Usage,
};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

/// A single pre-scripted response. Consumed FIFO, one per LLM call.
pub enum FauxResponseStep {
    /// Static: return this AssistantMessage verbatim.
    Static(AssistantMessage),
    /// Dynamic: factory inspects the live context and decides what to return.
    /// Mirrors pi's FauxResponseFactory.
    Factory(
        Box<
            dyn Fn(&Context, Option<&StreamOptions>, &FauxState, &Model) -> AssistantMessage
                + Send
                + Sync,
        >,
    ),
}

/// Observable state passed to factory functions.
pub struct FauxState {
    /// 0-based call index (increments per stream() invocation).
    pub call_count: usize,
}

/// The faux provider. Register under "faux" key in ApiRegistry.
pub struct FauxProvider {
    queue: Mutex<VecDeque<FauxResponseStep>>,
    call_count: AtomicUsize,
    /// Last popped static response (for ION_FAUX_REPEAT=1 mode — multi-turn tests).
    /// Only captures Static steps; Factory steps aren't cloneable but they
    /// already produce fresh output each call so repeat isn't needed for them.
    last_response: Mutex<Option<AssistantMessage>>,
}

impl FauxProvider {
    /// Create an empty provider (queue starts empty; use set_responses to fill).
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            call_count: AtomicUsize::new(0),
            last_response: Mutex::new(None),
        }
    }
}

impl FauxProvider {
    /// Replace the entire response queue.
    pub fn set_responses(&self, responses: Vec<FauxResponseStep>) {
        let mut q = self.queue.lock().unwrap();
        q.clear();
        q.extend(responses);
    }

    /// Append responses to the end of the queue.
    pub fn append_responses(&self, responses: Vec<FauxResponseStep>) {
        let mut q = self.queue.lock().unwrap();
        q.extend(responses);
    }

    /// Number of responses remaining in the queue.
    pub fn pending_count(&self) -> usize {
        self.queue.lock().unwrap().len()
    }

    /// Total number of stream() calls served so far.
    pub fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// FIFO pop one step. Returns None if empty.
    ///
    /// If `ION_FAUX_REPEAT=1` is set and the queue is empty, the LAST popped
    /// Static response is returned again (for multi-turn tests that need a
    /// stable reply without pre-queueing N copies).
    fn pop(&self) -> Option<FauxResponseStep> {
        // ION_FAUX_ERROR=1: inject a simulated error response to trigger auto_retry.
        // Returns an AssistantMessage with stop_reason=Error and an error message,
        // so the agent's retry logic sees it as a retryable error.
        if std::env::var("ION_FAUX_ERROR").ok().as_deref() == Some("1") {
            let error_msg = crate::types::AssistantMessage {
                role: "assistant".into(),
                content: vec![],
                api: "faux".into(),
                provider: "faux".into(),
                model: "faux-error".into(),
                response_model: None,
                response_id: None,
                usage: crate::types::Usage::default(),
                stop_reason: crate::types::StopReason::Error,
                error_message: Some("500 Internal Server Error (faux injected)".into()),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
            };
            return Some(FauxResponseStep::Static(error_msg));
        }

        let mut queue = self.queue.lock().unwrap();
        if let Some(step) = queue.pop_front() {
            // Capture last Static response for repeat mode
            if let FauxResponseStep::Static(ref msg) = step {
                *self.last_response.lock().unwrap() = Some(msg.clone());
            }
            Some(step)
        } else if std::env::var("ION_FAUX_REPEAT").ok().as_deref() == Some("1") {
            // Repeat last Static response indefinitely
            self.last_response
                .lock()
                .unwrap()
                .clone()
                .map(FauxResponseStep::Static)
        } else {
            None
        }
    }
}

#[async_trait]
impl ApiProvider for FauxProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        _cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        // ION_FAUX_ERROR=1: return a hard error to trigger agent's auto_retry.
        // This returns Err (not Ok with stop_reason=Error), which is what the
        // retry logic in agent_loop.rs actually checks.
        if std::env::var("ION_FAUX_ERROR").ok().as_deref() == Some("1") {
            // Only inject error on the first call; subsequent calls (after retry) succeed
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count == 0 {
                return Err(crate::ProviderError::Stream(
                    "500 Internal Server Error (faux injected for auto_retry test)".into(),
                ));
            }
        }
        // FIFO pop — loud failure on empty queue (mirrors pi).
        let step = self
            .pop()
            .ok_or_else(|| crate::ProviderError::Stream("No more faux responses queued".into()))?;
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        let state = FauxState { call_count: count };

        // Resolve static vs factory.
        let message = match step {
            FauxResponseStep::Static(msg) => msg,
            FauxResponseStep::Factory(f) => f(context, options, &state, model),
        };

        // Build the EventStream and emit events on a spawned task.
        let (event_stream, sender) = EventStream::new();
        tokio::spawn(async move {
            // Start
            sender.push(StreamEvent::Start {
                partial: message.clone(),
            });
            // Token-chunked block streaming (Text/Thinking/ToolCall deltas).
            faux_stream_blocks(sender, message).await;
        });

        Ok(event_stream)
    }
}

/// Thin wrapper so a single `Arc<FauxProvider>` can be both registered as an
/// `ApiProvider` AND held by tests for queue management.
struct FauxProviderHandle(Arc<FauxProvider>);

#[async_trait]
impl ApiProvider for FauxProviderHandle {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        self.0.stream(model, context, options, cancel).await
    }
}

/// Register a `FauxProvider` under the "faux" key and return a handle for queue
/// management. Mirrors pi's `registerFauxProvider`.
pub fn register_faux(registry: &mut ApiRegistry) -> Arc<FauxProvider> {
    let provider = Arc::new(FauxProvider::new());
    registry.register("faux", Box::new(FauxProviderHandle(provider.clone())));
    provider
}

/// Split a string into pseudo-token-sized chunks (min..=max chars each).
/// Mirrors pi's splitStringByTokenSize.
fn split_by_token_size(s: &str, min: usize, max: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < s.len() {
        // Pick a pseudo-random width in [min, max], clamped to remaining length.
        let remaining = s.len() - start;
        let span = max - min + 1;
        let width = (min + rand_width(span)).min(remaining).max(1);
        // Advance to a char boundary.
        let mut end = start + width;
        while end < s.len() && !s.is_char_boundary(end) {
            end += 1;
        }
        chunks.push(s[start..end].to_string());
        start = end;
    }
    chunks
}

/// Deterministic-ish pseudo-random in 0..n (avoid pulling in rand crate).
fn rand_width(n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    (h.finish() as usize) % n
}

/// Stream each AssistantContentBlock as Start→Delta×N→End events,
/// then call sender.end(message). Mirrors pi streamWithDeltas.
async fn faux_stream_blocks(sender: EventSender, message: AssistantMessage) {
    for (idx, block) in message.content.iter().enumerate() {
        match block {
            AssistantContentBlock::Text(TextContent { text, .. }) => {
                sender.push(StreamEvent::TextStart {
                    content_index: idx,
                    partial: message.clone(),
                });
                for chunk in split_by_token_size(text, 3, 8) {
                    sender.push(StreamEvent::TextDelta {
                        content_index: idx,
                        delta: chunk,
                        partial: message.clone(),
                    });
                    tokio::task::yield_now().await;
                }
                sender.push(StreamEvent::TextEnd {
                    content_index: idx,
                    content: text.clone(),
                    partial: message.clone(),
                });
            }
            AssistantContentBlock::Thinking(ThinkingContent { thinking, .. }) => {
                sender.push(StreamEvent::ThinkingStart {
                    content_index: idx,
                    partial: message.clone(),
                });
                for chunk in split_by_token_size(thinking, 3, 8) {
                    sender.push(StreamEvent::ThinkingDelta {
                        content_index: idx,
                        delta: chunk,
                        partial: message.clone(),
                    });
                    tokio::task::yield_now().await;
                }
                sender.push(StreamEvent::ThinkingEnd {
                    content_index: idx,
                    content: thinking.clone(),
                    partial: message.clone(),
                });
            }
            AssistantContentBlock::ToolCall(tc) => {
                sender.push(StreamEvent::ToolCallStart {
                    content_index: idx,
                    partial: message.clone(),
                });
                let args_str = serde_json::to_string(&tc.arguments).unwrap_or_default();
                for chunk in split_by_token_size(&args_str, 3, 8) {
                    sender.push(StreamEvent::ToolCallDelta {
                        content_index: idx,
                        delta: chunk,
                        partial: message.clone(),
                    });
                    tokio::task::yield_now().await;
                }
                sender.push(StreamEvent::ToolCallEnd {
                    content_index: idx,
                    tool_call: tc.clone(),
                    partial: message.clone(),
                });
            }
        }
    }
    // F-P1.4: error/aborted stop reasons emit an Error terminal event instead of Done.
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            sender.error(message.stop_reason.clone(), message);
        }
        _ => {
            sender.end(message);
        }
    }
}

// ── Script loading (JSONL → Vec<FauxResponseStep>) ──

use std::path::Path;

/// Load a JSONL script file into a Vec<FauxResponseStep>.
/// Each non-empty, non-comment line is one response. Supported line formats:
///   {"text":"..."}                           → static text
///   {"tool_call":{"name":"x","input":{...}}} → static tool call
///   {"thinking":"...","text":"..."}          → static thinking + text
///   {"error":"..."}                          → static error
pub fn load_script(path: &Path) -> ProviderResult<Vec<FauxResponseStep>> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        crate::ProviderError::Stream(format!(
            "failed to read faux script {}: {}",
            path.display(),
            e
        ))
    })?;
    let mut steps = Vec::new();
    for (lineno, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue; // skip blank lines and comments
        }
        let v: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            crate::ProviderError::Stream(format!(
                "faux script line {}: parse error: {}",
                lineno + 1,
                e
            ))
        })?;
        let step = parse_script_line(&v).ok_or_else(|| {
            crate::ProviderError::Stream(format!(
                "faux script line {}: unrecognized format",
                lineno + 1
            ))
        })?;
        steps.push(step);
    }
    if steps.is_empty() {
        return Err(crate::ProviderError::Stream("faux script is empty".into()));
    }
    Ok(steps)
}

fn parse_script_line(v: &serde_json::Value) -> Option<FauxResponseStep> {
    // {"text":"..."}
    if let Some(text) = v.get("text").and_then(|t| t.as_str()) {
        let mut blocks = vec![faux_text(text)];
        if let Some(thinking) = v.get("thinking").and_then(|t| t.as_str()) {
            blocks.insert(0, faux_thinking(thinking));
        }
        let stop_reason = v
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .and_then(|s| match s {
                "toolUse" | "tool_use" => Some(StopReason::ToolUse),
                "error" => Some(StopReason::Error),
                "length" => Some(StopReason::Length),
                _ => None,
            });
        let error_message = v
            .get("error_message")
            .and_then(|s| s.as_str())
            .map(String::from);
        return Some(FauxResponseStep::Static(faux_assistant_message(
            FauxContent::Many(blocks),
            FauxMessageOptions {
                stop_reason,
                error_message,
            },
        )));
    }
    // {"tool_call":{"name":"x","input":{...}}}
    if let Some(tc) = v.get("tool_call") {
        let name = tc.get("name")?.as_str()?;
        let input = tc
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Object(Default::default()));
        let block = faux_tool_call(name, input);
        return Some(FauxResponseStep::Static(faux_assistant_message(
            FauxContent::Single(block),
            FauxMessageOptions {
                stop_reason: Some(StopReason::ToolUse),
                error_message: None,
            },
        )));
    }
    // {"error":"..."}
    if let Some(err) = v.get("error").and_then(|s| s.as_str()) {
        return Some(FauxResponseStep::Static(faux_assistant_message(
            FauxContent::Text(String::new()),
            FauxMessageOptions {
                stop_reason: Some(StopReason::Error),
                error_message: Some(err.into()),
            },
        )));
    }
    None
}

// ── Builder functions (mirror pi fauxText/fauxThinking/fauxToolCall/fauxAssistantMessage) ──

/// Build a Text content block.
pub fn faux_text(text: &str) -> AssistantContentBlock {
    AssistantContentBlock::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

/// Build a Thinking content block.
pub fn faux_thinking(thinking: &str) -> AssistantContentBlock {
    AssistantContentBlock::Thinking(ThinkingContent {
        thinking: thinking.into(),
        thinking_signature: None,
        redacted: None,
    })
}

/// Build a ToolCall content block with an auto-generated unique id.
pub fn faux_tool_call(name: &str, arguments: serde_json::Value) -> AssistantContentBlock {
    AssistantContentBlock::ToolCall(ToolCall {
        call_type: "function".into(),
        id: format!("call_{}", faux_id()),
        name: name.into(),
        arguments,
        thought_signature: None,
    })
}

/// Content input for faux_assistant_message: string, single block, or many blocks.
pub enum FauxContent {
    Text(String),
    Single(AssistantContentBlock),
    Many(Vec<AssistantContentBlock>),
}

/// Options for faux_assistant_message.
#[derive(Default)]
pub struct FauxMessageOptions {
    pub stop_reason: Option<StopReason>,
    pub error_message: Option<String>,
}

/// Build a full AssistantMessage stamped as faux.
pub fn faux_assistant_message(
    content: FauxContent,
    options: FauxMessageOptions,
) -> AssistantMessage {
    let blocks = match content {
        FauxContent::Text(s) => vec![faux_text(&s)],
        FauxContent::Single(b) => vec![b],
        FauxContent::Many(v) => v,
    };
    AssistantMessage {
        role: "assistant".into(),
        content: blocks,
        api: "faux".into(),
        provider: "faux".into(),
        model: "faux-1".into(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: options.stop_reason.unwrap_or(StopReason::Stop),
        error_message: options.error_message,
        timestamp: 0,
    }
}

/// Generate a short unique id (timestamp + counter).
fn faux_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{ts:x}{n:x}")
}
