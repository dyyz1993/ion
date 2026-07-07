# FauxProvider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement a FauxProvider — an architecture-level LLM mock that registers as the `"faux"` ApiProvider, FIFO-replays pre-scripted responses (static or factory-function) through the real EventStream/agent-loop pipeline, with loud failure on queue exhaustion. This enables fast, deterministic, network-free testing of agent behavior including Session Tree's branch/rollback flows.

**Architecture:** Implement `ApiProvider` trait (single `stream()` method) on a `FauxProvider` struct holding a `Mutex<VecDeque<FauxResponseStep>>` queue + `AtomicUsize` call counter. Register under `"faux"` key in `ApiRegistry`; route to it by setting `Model.api = "faux"`. Streaming splits each content block into token-sized `TextDelta`/`ToolCallDelta` chunks (mirroring pi's `streamWithDeltas`). Zero-invasive: no changes to existing providers, dispatch, or agent loop.

**Tech Stack:** Rust, `ion-provider` crate, `tokio::sync::{mpsc, oneshot}` (existing EventStream), `async_trait`, `std::sync::{Mutex, Arc, atomic::AtomicUsize}`.

**Reference spec:** [docs/design/FAUX_PROVIDER.md](../../design/FAUX_PROVIDER.md)
**Reference (pi):** `packages/ai/src/providers/faux.ts` in `/Users/xuyingzhou/Project/temporary/pi-momo-fork/`

---

## File Structure

| File | Responsibility | Action |
|------|----------------|--------|
| `ion-provider/src/faux.rs` | FauxProvider struct, FauxResponseStep enum, FauxState, impl ApiProvider, streaming chunker, builder functions | **Create** |
| `ion-provider/src/lib.rs` | Export faux module | **Modify** (1 line) |
| `ion-provider/src/error.rs` | Verify `ProviderError::Other` exists for loud-failure; add if missing | **Modify** (if needed) |
| `ion-provider/tests/faux_test.rs` | Integration tests for FauxProvider (P0/P1 from spec §7) | **Create** |

---

## Task 1: Add `ProviderError` variant for faux queue exhaustion

The loud-failure path needs a way to return `"No more faux responses queued"`. Check what `ProviderError` variants exist and use the appropriate one.

**Files:**
- Read: `ion-provider/src/error.rs`
- Modify: `ion-provider/src/error.rs` (only if no suitable variant exists)

- [ ] **Step 1: Read current ProviderError definition**

Run:
```bash
cat ion-provider/src/error.rs
```

Identify which variant fits "no more responses queued". Likely candidates: `Other(String)`, `Stream(String)`, or a generic string-carrying variant.

- [ ] **Step 2: If a string-carrying variant exists (e.g. `Other(String)`), use it — skip to Task 2.**

If NO string-carrying variant exists, add one:

```rust
// In the ProviderError enum
#[error("{0}")]
Other(String),
```

- [ ] **Step 3: Verify it compiles**

Run:
```bash
cargo build -p ion-provider
```
Expected: compiles with no errors.

- [ ] **Step 4: Commit**

```bash
git add ion-provider/src/error.rs
git commit -m "feat(provider): add ProviderError::Other string variant for faux"
```

---

## Task 2: Create `faux.rs` module skeleton with FauxResponseStep + FauxState

**Files:**
- Create: `ion-provider/src/faux.rs`
- Modify: `ion-provider/src/lib.rs:7` (add `pub mod faux;`)

- [ ] **Step 1: Write the failing test — module exists and FauxState has call_count**

Create `ion-provider/tests/faux_test.rs`:

```rust
use ion_provider::faux::{FauxProvider, FauxResponseStep, FauxState};

#[test]
fn faux_provider_can_be_created_empty() {
    let _provider = FauxProvider::new();
}

#[test]
fn faux_state_has_call_count() {
    let state = FauxState { call_count: 0 };
    assert_eq!(state.call_count, 0);
}
```

- [ ] **Step 2: Run test to verify it fails (module doesn't exist yet)**

Run:
```bash
cargo test -p ion-provider --test faux_test
```
Expected: FAIL — `unresolved module faux` or similar.

- [ ] **Step 3: Add module declaration to lib.rs**

Modify `ion-provider/src/lib.rs` — add after line 7 (`pub mod provider;`):

```rust
pub mod faux;
```

- [ ] **Step 4: Create faux.rs with FauxResponseStep + FauxState + empty FauxProvider**

Create `ion-provider/src/faux.rs`:

```rust
//! FauxProvider — architecture-level LLM mock.
//! Registers as "faux" ApiProvider; FIFO-replays pre-scripted responses.
//! Mirrors pi's @dyyz1993/pi-ai FauxProvider (packages/ai/src/providers/faux.ts).

use crate::types::{AssistantMessage, Context, Model, StreamOptions};
use crate::event_stream::EventStream;
use crate::error::ProviderResult;
use async_trait::async_trait;
use std::collections::VecDeque;
use std::sync::{Mutex, atomic::AtomicUsize};

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
}

impl FauxProvider {
    /// Create an empty provider (queue starts empty; use set_responses to fill).
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            call_count: AtomicUsize::new(0),
        }
    }
}
```

- [ ] **Step 5: Run test to verify it passes**

Run:
```bash
cargo test -p ion-provider --test faux_test
```
Expected: PASS (2 tests).

- [ ] **Step 6: Commit**

```bash
git add ion-provider/src/faux.rs ion-provider/src/lib.rs ion-provider/tests/faux_test.rs
git commit -m "feat(provider): add faux module skeleton (FauxProvider, FauxResponseStep, FauxState)"
```

---

## Task 3: Queue management methods (set/append/pending/count)

**Files:**
- Modify: `ion-provider/src/faux.rs` (add methods to `impl FauxProvider`)
- Modify: `ion-provider/tests/faux_test.rs` (add tests)

- [ ] **Step 1: Write failing tests for queue management**

Append to `ion-provider/tests/faux_test.rs`:

```rust
use ion_provider::faux::FauxResponseStep;
use ion_provider::types::{AssistantMessage, Model, StopReason, Usage};

/// Helper: build a minimal static text response step.
fn static_text_step(text: &str) -> FauxResponseStep {
    let msg = AssistantMessage {
        role: "assistant".into(),
        content: vec![ion_provider::types::AssistantContentBlock::Text(
            ion_provider::types::TextContent { text: text.into(), text_signature: None },
        )],
        api: "faux".into(),
        provider: "faux".into(),
        model: "faux-1".into(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: 0,
    };
    FauxResponseStep::Static(msg)
}

#[test]
fn set_responses_replaces_queue() {
    let provider = FauxProvider::new();
    provider.set_responses(vec![static_text_step("a"), static_text_step("b")]);
    assert_eq!(provider.pending_count(), 2);
    provider.set_responses(vec![static_text_step("c")]);
    assert_eq!(provider.pending_count(), 1);
}

#[test]
fn append_responses_appends_to_queue() {
    let provider = FauxProvider::new();
    provider.set_responses(vec![static_text_step("a")]);
    provider.append_responses(vec![static_text_step("b"), static_text_step("c")]);
    assert_eq!(provider.pending_count(), 3);
}

#[test]
fn pending_count_on_empty_queue() {
    let provider = FauxProvider::new();
    assert_eq!(provider.pending_count(), 0);
}
```

- [ ] **Step 2: Run test to verify it fails (methods don't exist)**

Run:
```bash
cargo test -p ion-provider --test faux_test -- set_responses_replaces_queue
```
Expected: FAIL — `no method named set_responses found`.

- [ ] **Step 3: Implement queue management methods**

Add to `impl FauxProvider` in `ion-provider/src/faux.rs`:

```rust
use std::sync::atomic::Ordering;

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
    fn pop(&self) -> Option<FauxResponseStep> {
        self.queue.lock().unwrap().pop_front()
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run:
```bash
cargo test -p ion-provider --test faux_test
```
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add ion-provider/src/faux.rs ion-provider/tests/faux_test.rs
git commit -m "feat(provider): faux queue management (set/append/pending/count)"
```

---

## Task 4: `impl ApiProvider` — stream() with loud failure

The core: implement the trait method. First version returns the full message via `sender.end()` without token-chunking (chunking comes in Task 5). This gets the dispatch working end-to-end.

**Files:**
- Modify: `ion-provider/src/faux.rs`
- Modify: `ion-provider/tests/faux_test.rs`

- [ ] **Step 1: Write failing test — stream() returns the queued message**

Append to `ion-provider/tests/faux_test.rs`:

```rust
use ion_provider::registry::{ApiRegistry, ApiProvider, stream};

fn faux_model() -> Model {
    Model {
        id: "faux-1".into(),
        name: "Faux Test".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: String::new(),
        reasoning: false,
        input: vec!["text".into()],
        cost: ion_provider::types::Cost::default(),
        context_window: 128000,
        max_tokens: 8192,
        compat: None,
        headers: None,
    }
}

/// Register faux under "faux" key in a fresh registry.
fn registry_with_faux(provider: std::sync::Arc<FauxProvider>) -> ApiRegistry {
    let mut reg = ApiRegistry::new();
    reg.register("faux", Box::new(FauxHandle(provider)));
    reg
}

/// Thin wrapper so Box<dyn ApiProvider> can share an Arc<FauxProvider>.
struct FauxHandle(std::sync::Arc<FauxProvider>);

#[async_trait::async_trait]
impl ApiProvider for FauxHandle {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> ProviderResult<EventStream> {
        self.0.stream(model, context, options).await
    }
}

#[tokio::test]
async fn stream_returns_queued_static_message() {
    let provider = std::sync::Arc::new(FauxProvider::new());
    provider.set_responses(vec![static_text_step("hello")]);
    let reg = registry_with_faux(provider.clone());
    let model = faux_model();
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None).await.unwrap();
    // Drain events
    let mut last_message = None;
    while let Some(ev) = es.recv().await {
        if let ion_provider::types::StreamEvent::Done { message, .. } = ev {
            last_message = Some(message);
        }
    }
    let msg = last_message.expect("no Done event");
    assert_eq!(msg.content.len(), 1);
    assert_eq!(provider.call_count(), 1);
    assert_eq!(provider.pending_count(), 0);
}

#[tokio::test]
async fn stream_loud_failure_on_empty_queue() {
    let provider = std::sync::Arc::new(FauxProvider::new());
    // Queue is empty
    let reg = registry_with_faux(provider.clone());
    let model = faux_model();
    let ctx = Context::default();

    let result = stream(&reg, &model, &ctx, None).await;
    assert!(result.is_err(), "empty queue must error loudly");
    let err = result.unwrap_err();
    assert!(format!("{err}").to_lowercase().contains("no more faux responses")
        || format!("{err}").to_lowercase().contains("faux"));
}
```

- [ ] **Step 2: Run test to verify it fails (FauxProvider doesn't impl ApiProvider)**

Run:
```bash
cargo test -p ion-provider --test faux_test -- stream_returns_queued
```
Expected: FAIL — `the trait ApiProvider is not implemented for FauxProvider`.

- [ ] **Step 3: Implement ApiProvider for FauxProvider**

Add to `ion-provider/src/faux.rs`:

```rust
use crate::registry::ApiProvider;

#[async_trait]
impl ApiProvider for FauxProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> ProviderResult<EventStream> {
        // FIFO pop — loud failure on empty queue (mirrors pi).
        let step = self.pop().ok_or_else(|| {
            crate::ProviderError::Other("No more faux responses queued".into())
        })?;
        let count = self.call_count.fetch_add(1, Ordering::SeqCst);
        let state = FauxState { call_count: count };

        // Resolve static vs factory.
        let message = match step {
            FauxResponseStep::Static(msg) => msg,
            FauxResponseStep::Factory(f) => f(context, options, &state, model),
        };

        // Build the EventStream and emit events on a spawned task.
        let (mut event_stream, sender) = EventStream::new();
        tokio::spawn(async move {
            // Start
            sender.push(StreamEvent::Start { partial: message.clone() });
            // (Token-chunking added in Task 5; for now emit Done directly.)
            sender.end(message);
        });

        Ok(event_stream)
    }
}
```

Also add the necessary `use` at the top of faux.rs:
```rust
use crate::types::StreamEvent;
```

- [ ] **Step 4: Run test to verify it passes**

Run:
```bash
cargo test -p ion-provider --test faux_test
```
Expected: PASS (7 tests).

- [ ] **Step 5: Commit**

```bash
git add ion-provider/src/faux.rs ion-provider/tests/faux_test.rs
git commit -m "feat(provider): impl ApiProvider for FauxProvider (FIFO + loud failure)"
```

---

## Task 5: Token-chunked streaming (mirror pi's streamWithDeltas)

Currently `stream()` emits Start then Done immediately. Real providers (and pi's faux) emit per-block events: TextStart → TextDelta×N → TextEnd, etc. This makes subscriber-based assertions (e.g. Session Tree P0.4) work correctly.

**Files:**
- Modify: `ion-provider/src/faux.rs`
- Modify: `ion-provider/tests/faux_test.rs`

- [ ] **Step 1: Write failing test — text deltas are emitted**

Append to `ion-provider/tests/faux_test.rs`:

```rust
#[tokio::test]
async fn stream_emits_text_deltas_for_long_text() {
    let provider = std::sync::Arc::new(FauxProvider::new());
    let long_text = "This is a long enough message to be chunked into multiple deltas.";
    provider.set_responses(vec![static_text_step(long_text)]);
    let reg = registry_with_faux(provider);
    let model = faux_model();
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None).await.unwrap();
    let mut delta_count = 0;
    let mut start_count = 0;
    let mut end_count = 0;
    let mut done_count = 0;
    while let Some(ev) = es.recv().await {
        match ev {
            ion_provider::types::StreamEvent::Start { .. } => start_count += 1,
            ion_provider::types::StreamEvent::TextStart { .. } => {},
            ion_provider::types::StreamEvent::TextDelta { .. } => delta_count += 1,
            ion_provider::types::StreamEvent::TextEnd { .. } => end_count += 1,
            ion_provider::types::StreamEvent::Done { .. } => done_count += 1,
            _ => {}
        }
    }
    assert_eq!(start_count, 1, "exactly one Start event");
    assert_eq!(done_count, 1, "exactly one Done event");
    assert!(delta_count >= 2, "long text must be split into >=2 deltas, got {delta_count}");
    assert_eq!(end_count, 1, "exactly one TextEnd");
}
```

- [ ] **Step 2: Run test to verify it fails (currently 0 deltas)**

Run:
```bash
cargo test -p ion-provider --test faux_test -- stream_emits_text_deltas
```
Expected: FAIL — `long text must be split into >=2 deltas, got 0`.

- [ ] **Step 3: Implement `split_by_token_size` + `faux_stream_blocks`**

Add to `ion-provider/src/faux.rs`:

```rust
use crate::types::{AssistantContentBlock, TextContent, ThinkingContent, ToolCall};

/// Split a string into pseudo-token-sized chunks (min..=max chars each).
/// Mirrors pi's splitStringByTokenSize.
fn split_by_token_size(s: &str, min: usize, max: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    let bytes = s.as_bytes();
    while start < s.len() {
        // Pick a random width in [min, max], clamped to remaining length.
        let width = (min + (rand_width(max - min + 1))).min(s.len() - start).max(1);
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

/// Deterministic pseudo-random in 0..n (avoid pulling in rand crate for tests).
fn rand_width(n: usize) -> usize {
    if n == 0 { return 0; }
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now().hash(&mut h);
    (h.finish() as usize) % n
}

/// Stream each AssistantContentBlock as Start→Delta×N→End events,
/// then call sender.end(message). Mirrors pi streamWithDeltas.
async fn faux_stream_blocks(sender: crate::event_stream::EventSender, message: AssistantMessage) {
    for (idx, block) in message.content.iter().enumerate() {
        match block {
            AssistantContentBlock::Text(TextContent { text, .. }) => {
                sender.push(StreamEvent::TextStart { content_index: idx, partial: message.clone() });
                for chunk in split_by_token_size(text, 3, 8) {
                    sender.push(StreamEvent::TextDelta { content_index: idx, delta: chunk, partial: message.clone() });
                    tokio::task::yield_now().await;
                }
                sender.push(StreamEvent::TextEnd { content_index: idx, content: text.clone(), partial: message.clone() });
            }
            AssistantContentBlock::Thinking(ThinkingContent { thinking, .. }) => {
                sender.push(StreamEvent::ThinkingStart { content_index: idx, partial: message.clone() });
                for chunk in split_by_token_size(thinking, 3, 8) {
                    sender.push(StreamEvent::ThinkingDelta { content_index: idx, delta: chunk, partial: message.clone() });
                    tokio::task::yield_now().await;
                }
                sender.push(StreamEvent::ThinkingEnd { content_index: idx, content: thinking.clone(), partial: message.clone() });
            }
            AssistantContentBlock::ToolCall(tc) => {
                sender.push(StreamEvent::ToolCallStart { content_index: idx, partial: message.clone() });
                let args_str = serde_json::to_string(&tc.arguments).unwrap_or_default();
                for chunk in split_by_token_size(&args_str, 3, 8) {
                    sender.push(StreamEvent::ToolCallDelta { content_index: idx, delta: chunk, partial: message.clone() });
                    tokio::task::yield_now().await;
                }
                sender.push(StreamEvent::ToolCallEnd { content_index: idx, tool_call: tc.clone(), partial: message.clone() });
            }
        }
    }
    sender.end(message);
}
```

- [ ] **Step 4: Wire `faux_stream_blocks` into the `stream()` impl**

Replace the spawned-task body in `impl ApiProvider`:

```rust
        let (mut event_stream, sender) = EventStream::new();
        tokio::spawn(async move {
            sender.push(StreamEvent::Start { partial: message.clone() });
            faux_stream_blocks(sender, message).await;
        });
        Ok(event_stream)
```

(Remove the old `sender.push(Start)` + `sender.end(message)` two-line body.)

- [ ] **Step 5: Run test to verify it passes**

Run:
```bash
cargo test -p ion-provider --test faux_test
```
Expected: PASS (8 tests).

- [ ] **Step 6: Commit**

```bash
git add ion-provider/src/faux.rs ion-provider/tests/faux_test.rs
git commit -m "feat(provider): faux token-chunked streaming (Text/Thinking/ToolCall deltas)"
```

---

## Task 6: Builder functions (faux_text / faux_thinking / faux_tool_call / faux_assistant_message)

Tests currently build AssistantMessage by hand (verbose). Add pi-style builders.

**Files:**
- Modify: `ion-provider/src/faux.rs`
- Modify: `ion-provider/tests/faux_test.rs` (refactor existing tests to use builders + add builder tests)

- [ ] **Step 1: Write failing tests for builders**

Append to `ion-provider/tests/faux_test.rs`:

```rust
use ion_provider::faux::{faux_text, faux_thinking, faux_tool_call, faux_assistant_message, FauxContent, FauxMessageOptions};

#[test]
fn faux_text_builder() {
    let b = faux_text("hi");
    match b {
        AssistantContentBlock::Text(t) => assert_eq!(t.text, "hi"),
        _ => panic!("expected Text"),
    }
}

#[test]
fn faux_thinking_builder() {
    let b = faux_thinking("plan");
    match b {
        AssistantContentBlock::Thinking(t) => assert_eq!(t.thinking, "plan"),
        _ => panic!("expected Thinking"),
    }
}

#[test]
fn faux_tool_call_builder_has_unique_id() {
    let b1 = faux_tool_call("echo", serde_json::json!({"x":1}));
    let b2 = faux_tool_call("echo", serde_json::json!({"x":2}));
    let id1 = match b1 { AssistantContentBlock::ToolCall(t) => t.id, _ => panic!() };
    let id2 = match b2 { AssistantContentBlock::ToolCall(t) => t.id, _ => panic!() };
    assert_ne!(id1, id2, "tool call ids must be unique");
}

#[test]
fn faux_assistant_message_from_text_string() {
    let msg = faux_assistant_message(FauxContent::Text("hello".into()), FauxMessageOptions::default());
    assert_eq!(msg.content.len(), 1);
    assert_eq!(msg.api, "faux");
    assert_eq!(msg.provider, "faux");
    assert_eq!(msg.model, "faux-1");
    assert_eq!(msg.stop_reason, StopReason::Stop);
}

#[test]
fn faux_assistant_message_from_blocks() {
    let msg = faux_assistant_message(
        FauxContent::Many(vec![faux_text("a"), faux_tool_call("t", serde_json::json!({}))]),
        FauxMessageOptions { stop_reason: Some(StopReason::ToolUse), ..Default::default() },
    );
    assert_eq!(msg.content.len(), 2);
    assert_eq!(msg.stop_reason, StopReason::ToolUse);
}
```

- [ ] **Step 2: Run test to verify it fails (builders don't exist)**

Run:
```bash
cargo test -p ion-provider --test faux_test -- faux_text_builder
```
Expected: FAIL — `cannot find function faux_text`.

- [ ] **Step 3: Implement builders**

Add to `ion-provider/src/faux.rs`:

```rust
// ── Builder functions (mirror pi fauxText/fauxThinking/fauxToolCall/fauxAssistantMessage) ──

/// Build a Text content block.
pub fn faux_text(text: &str) -> AssistantContentBlock {
    AssistantContentBlock::Text(TextContent { text: text.into(), text_signature: None })
}

/// Build a Thinking content block.
pub fn faux_thinking(thinking: &str) -> AssistantContentBlock {
    AssistantContentBlock::Thinking(ThinkingContent { thinking: thinking.into(), thinking_signature: None, redacted: None })
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
pub fn faux_assistant_message(content: FauxContent, options: FauxMessageOptions) -> AssistantMessage {
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
```

- [ ] **Step 4: Refactor the existing `static_text_step` test helper to use `faux_assistant_message`**

In `ion-provider/tests/faux_test.rs`, replace the body of `static_text_step`:

```rust
fn static_text_step(text: &str) -> FauxResponseStep {
    FauxResponseStep::Static(faux_assistant_message(
        FauxContent::Text(text.into()),
        FauxMessageOptions::default(),
    ))
}
```

- [ ] **Step 5: Run all tests**

Run:
```bash
cargo test -p ion-provider --test faux_test
```
Expected: PASS (all tests — builders + queue + stream).

- [ ] **Step 6: Commit**

```bash
git add ion-provider/src/faux.rs ion-provider/tests/faux_test.rs
git commit -m "feat(provider): faux builder functions (text/thinking/tool_call/assistant_message)"
```

---

## Task 7: Factory-function responses (FauxResponseStep::Factory)

Verify the factory path works — the key differentiator vs static responses.

**Files:**
- Modify: `ion-provider/tests/faux_test.rs`

- [ ] **Step 1: Write failing test — factory receives context and call_count**

Append to `ion-provider/tests/faux_test.rs`:

```rust
#[tokio::test]
async fn factory_response_receives_context_and_state() {
    let provider = std::sync::Arc::new(FauxProvider::new());

    // Factory inspects context message count and uses call_count for branching.
    let provider_for_factory = provider.clone();
    provider.set_responses(vec![FauxResponseStep::Factory(Box::new(
        move |ctx, _opts, state, _model| {
            let n = ctx.messages.len();
            faux_assistant_message(
                FauxContent::Text(format!("call={} msgs={}", state.call_count, n)),
                FauxMessageOptions::default(),
            )
        },
    ))]);

    let reg = registry_with_faux(provider_for_factory);
    let model = faux_model();
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None).await.unwrap();
    let mut last = None;
    while let Some(ev) = es.recv().await {
        if let ion_provider::types::StreamEvent::Done { message, .. } = ev {
            last = Some(message);
        }
    }
    let msg = last.unwrap();
    let text = match &msg.content[0] {
        AssistantContentBlock::Text(t) => t.text.clone(),
        _ => panic!(),
    };
    assert!(text.starts_with("call=0 msgs="), "factory saw call_count=0; got: {text}");
}

#[tokio::test]
async fn factory_can_branch_on_call_count() {
    let provider = std::sync::Arc::new(FauxProvider::new());
    // Two factory responses; second returns different text based on call_count.
    provider.set_responses(vec![
        FauxResponseStep::Factory(Box::new(|_, _, s, _| {
            faux_assistant_message(
                FauxContent::Text(format!("first-{}", s.call_count)),
                FauxMessageOptions::default(),
            )
        })),
        FauxResponseStep::Factory(Box::new(|_, _, s, _| {
            faux_assistant_message(
                FauxContent::Text(format!("second-{}", s.call_count)),
                FauxMessageOptions::default(),
            )
        })),
    ]);
    let reg = registry_with_faux(provider);
    let model = faux_model();
    let ctx = Context::default();

    let mut texts = vec![];
    for _ in 0..2 {
        let mut es = stream(&reg, &model, &ctx, None).await.unwrap();
        while let Some(ev) = es.recv().await {
            if let ion_provider::types::StreamEvent::Done { message, .. } = ev {
                if let AssistantContentBlock::Text(t) = &message.content[0] {
                    texts.push(t.text.clone());
                }
            }
        }
    }
    assert_eq!(texts, vec!["first-0", "second-1"]);
}
```

- [ ] **Step 2: Run test to verify it passes (factory is already implemented in Task 4)**

Run:
```bash
cargo test -p ion-provider --test faux_test -- factory_
```
Expected: PASS. (If it fails because `Context` doesn't have a `.messages` field, adjust the test to use whatever field exists — check `Context` struct.)

- [ ] **Step 3: If `Context::default()` or `.messages` doesn't compile, check the Context struct**

Run:
```bash
grep -n "pub struct Context" ion-provider/src/types.rs
sed -n '217,240p' ion-provider/src/types.rs
```

Adjust the factory test to use the actual `Context` fields. The factory's contract is: receive the live `Context` so tests can inspect what the agent sent. If `Context` has no `messages`, use whatever it has (e.g. `prompt`, `history`, etc.) — the point is the factory CAN see it.

- [ ] **Step 4: Commit**

```bash
git add ion-provider/tests/faux_test.rs
git commit -m "test(provider): faux factory responses inspect context + branch on call_count"
```

---

## Task 8: Register helper — `register_faux` convenience function

Tests currently hand-roll `FauxHandle` + `register("faux", ...)`. Provide a one-liner.

**Files:**
- Modify: `ion-provider/src/faux.rs`
- Modify: `ion-provider/tests/faux_test.rs`

- [ ] **Step 1: Write failing test — register_faux returns a handle**

Append to `ion-provider/tests/faux_test.rs`:

```rust
use ion_provider::faux::register_faux;

#[tokio::test]
async fn register_faux_one_liner() {
    let mut reg = ApiRegistry::new();
    reg.register_builtins();
    let faux = register_faux(&mut reg);
    faux.set_responses(vec![static_text_step("registered")]);
    assert_eq!(faux.pending_count(), 1);

    let model = faux_model();
    let ctx = Context::default();
    let mut es = stream(&reg, &model, &ctx, None).await.unwrap();
    while es.recv().await.is_some() {}
    assert_eq!(faux.call_count(), 1);
}
```

- [ ] **Step 2: Run to verify it fails (register_faux doesn't exist)**

Run:
```bash
cargo test -p ion-provider --test faux_test -- register_faux_one_liner
```
Expected: FAIL — `cannot find function register_faux`.

- [ ] **Step 3: Implement `register_faux`**

Add to `ion-provider/src/faux.rs`:

```rust
use crate::registry::ApiRegistry;
use std::sync::Arc;

/// Thin wrapper so a single Arc<FauxProvider> can be both registered as ApiProvider
/// AND held by tests for queue management.
struct FauxProviderHandle(Arc<FauxProvider>);

#[async_trait]
impl ApiProvider for FauxProviderHandle {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> ProviderResult<EventStream> {
        self.0.stream(model, context, options).await
    }
}

/// Register a FauxProvider under "faux" and return a handle for queue management.
/// Mirrors pi's registerFauxProvider.
pub fn register_faux(registry: &mut ApiRegistry) -> Arc<FauxProvider> {
    let provider = Arc::new(FauxProvider::new());
    registry.register("faux", Box::new(FauxProviderHandle(provider.clone())));
    provider
}
```

- [ ] **Step 4: Remove the now-duplicated `FauxHandle` from the test file**

In `ion-provider/tests/faux_test.rs`, delete the local `FauxHandle` struct and `registry_with_faux` helper; replace their usages with `register_faux`. Specifically:

- Delete the `struct FauxHandle` block and its `impl ApiProvider`.
- Delete `fn registry_with_faux`.
- Replace `let reg = registry_with_faux(provider.clone());` with:
  ```rust
  let mut reg = ApiRegistry::new();
  reg.register_builtins();
  // provider was created via Arc; register under "faux"
  reg.register("faux", Box::new(FauxProviderHandle(provider.clone())));
  ```
  OR simpler: change all tests to use `register_faux` from the start:
  ```rust
  let mut reg = ApiRegistry::new();
  reg.register_builtins();
  let provider = register_faux(&mut reg);
  provider.set_responses(...);
  ```

(Prefer the latter — refactor tests to use `register_faux` everywhere.)

- [ ] **Step 5: Run all tests**

Run:
```bash
cargo test -p ion-provider --test faux_test
```
Expected: PASS (all).

- [ ] **Step 6: Commit**

```bash
git add ion-provider/src/faux.rs ion-provider/tests/faux_test.rs
git commit -m "feat(provider): register_faux convenience + refactor tests to use it"
```

---

## Task 9: Complete() compatibility + no-API-key verification

Verify the `complete()` free function works through faux, and that no key is ever requested.

**Files:**
- Modify: `ion-provider/tests/faux_test.rs`

- [ ] **Step 1: Write tests**

Append to `ion-provider/tests/faux_test.rs`:

```rust
use ion_provider::registry::complete;

#[tokio::test]
async fn complete_works_through_faux() {
    let mut reg = ApiRegistry::new();
    let faux = register_faux(&mut reg);
    faux.set_responses(vec![static_text_step("via complete")]);
    let model = faux_model();
    let ctx = Context::default();

    let msg = complete(&reg, &model, &ctx, None).await.unwrap();
    match &msg.content[0] {
        AssistantContentBlock::Text(t) => assert_eq!(t.text, "via complete"),
        _ => panic!("expected text"),
    }
}

#[tokio::test]
async fn faux_needs_no_api_key() {
    // Ensure no FAUX_API_KEY / ION_API_KEY / OPENAI_API_KEY env vars are set.
    let keys = ["FAUX_API_KEY", "ION_API_KEY", "OPENAI_API_KEY"];
    let saved: Vec<(String, Option<String>)> = keys.iter()
        .map(|k| (k.to_string(), std::env::var(k).ok())).collect();
    for k in &keys { std::env::remove_var(k); }

    let mut reg = ApiRegistry::new();
    let faux = register_faux(&mut reg);
    faux.set_responses(vec![static_text_step("no key needed")]);
    let model = faux_model();
    let ctx = Context::default();

    let result = stream(&reg, &model, &ctx, None).await;
    assert!(result.is_ok(), "faux must not require an API key");

    // Restore env
    for (k, v) in saved {
        if let Some(val) = v { std::env::set_var(k, val); }
    }
}
```

- [ ] **Step 2: Run tests**

Run:
```bash
cargo test -p ion-provider --test faux_test
```
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add ion-provider/tests/faux_test.rs
git commit -m "test(provider): faux works via complete() and needs no API key"
```

---

## Task 10: Update lib.rs exports + final verification

Ensure all public faux API is exported from the crate root.

**Files:**
- Modify: `ion-provider/src/lib.rs`

- [ ] **Step 1: Add faux re-export to lib.rs**

Modify `ion-provider/src/lib.rs` — after `pub mod faux;` add:

```rust
pub use faux::*;
```

- [ ] **Step 2: Verify full crate compiles + all tests pass**

Run:
```bash
cargo build -p ion-provider
cargo test -p ion-provider
```
Expected: all compile, all pass.

- [ ] **Step 3: Verify the main ion crate still compiles (it depends on ion-provider)**

Run:
```bash
cargo build --bin ion --bin ion-worker 2>&1 | tail -5
```
Expected: no errors (faux is purely additive).

- [ ] **Step 4: Commit**

```bash
git add ion-provider/src/lib.rs
git commit -m "feat(provider): export faux module public API"
```

---

## Task 11: Spec coverage self-check

This task verifies the plan covers all of spec §7 (FauxProvider's own P0/P1 acceptance).

- [ ] **Step 1: Map spec P0/P1 to tasks**

| Spec case | Covered by task | Status |
|-----------|-----------------|--------|
| F-P0.1 basic text replay | Task 4 (stream_returns_queued_static_message) | ✅ |
| F-P0.2 multi-step FIFO | Task 7 (factory_can_branch_on_call_count does 2 calls) + Task 4 call_count | ✅ |
| F-P0.3 tool call replay | Task 6 (faux_tool_call_builder) + Task 5 (ToolCall streaming) | ✅ |
| F-P0.4 factory response | Task 7 (factory_response_receives_context_and_state) | ✅ |
| F-P0.5 streaming events complete | Task 5 (stream_emits_text_deltas_for_long_text) | ✅ |
| F-P0.6 no API key | Task 9 (faux_needs_no_api_key) | ✅ |
| F-P1.1 loud failure on empty | Task 4 (stream_loud_failure_on_empty_queue) | ✅ |
| F-P1.2 call_count increments | Task 7 (factory_can_branch_on_call_count) | ✅ |
| F-P1.3 appendResponses | Task 3 (append_responses_appends_to_queue) | ✅ |
| F-P1.4 error path | **GAP** — no test for stop_reason=Error | Add below |
| F-P1.5 no real provider pollution | Implicit (faux registered separately); add explicit test | Add below |

- [ ] **Step 2: Add F-P1.4 error path test**

Append to `ion-provider/tests/faux_test.rs`:

```rust
#[tokio::test]
async fn faux_emits_error_for_stop_reason_error() {
    let mut reg = ApiRegistry::new();
    let faux = register_faux(&mut reg);
    faux.set_responses(vec![FauxResponseStep::Static(faux_assistant_message(
        FauxContent::Text("".into()),
        FauxMessageOptions { stop_reason: Some(StopReason::Error), error_message: Some("simulated".into()) },
    ))]);
    let model = faux_model();
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None).await.unwrap();
    let mut saw_error = false;
    while let Some(ev) = es.recv().await {
        if let ion_provider::types::StreamEvent::Error { reason, .. } = ev {
            assert_eq!(reason, StopReason::Error);
            saw_error = true;
        }
    }
    assert!(saw_error, "must emit Error event for stop_reason=Error");
}
```

Wait — `faux_stream_blocks` currently calls `sender.end(message)` which emits `Done` even for Error stop_reason. **Fix**: branch in `faux_stream_blocks`:

Modify `faux_stream_blocks` in `ion-provider/src/faux.rs`:

```rust
async fn faux_stream_blocks(sender: crate::event_stream::EventSender, message: AssistantMessage) {
    // ... block streaming unchanged ...
    match message.stop_reason {
        StopReason::Error | StopReason::Aborted => {
            sender.error(message.stop_reason.clone(), message);
        }
        _ => {
            sender.end(message);
        }
    }
}
```

Run: `cargo test -p ion-provider --test faux_test -- faux_emits_error`
Expected: PASS.

- [ ] **Step 3: Add F-P1.5 isolation test**

Append to `ion-provider/tests/faux_test.rs`:

```rust
#[tokio::test]
async fn real_providers_still_dispatch_when_faux_not_routed() {
    // Register faux AND builtins; a model with api="openai-completions" should
    // NOT route to faux even though faux is registered.
    let mut reg = ApiRegistry::new();
    reg.register_builtins();
    let faux = register_faux(&mut reg);
    faux.set_responses(vec![static_text_step("faux reply")]);

    // Use a real-provider model (will fail at network, proving dispatch didn't hit faux).
    let real_model = Model {
        id: "test".into(), name: "T".into(),
        api: "openai-completions".into(), provider: "openai".into(),
        base_url: "http://127.0.0.1:1/invalid".into(),  // unreachable
        reasoning: false, input: vec!["text".into()],
        cost: ion_provider::types::Cost::default(),
        context_window: 128000, max_tokens: 8192,
        compat: None, headers: None,
    };
    let ctx = Context::default();
    let result = stream(&reg, &real_model, &ctx, None).await;
    // Should error (network), NOT return "faux reply".
    assert!(result.is_err(), "real model must not route to faux");
    assert_eq!(faux.call_count(), 0, "faux must not have been called");
}
```

Run: `cargo test -p ion-provider --test faux_test`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add ion-provider/src/faux.rs ion-provider/tests/faux_test.rs
git commit -m "test(provider): faux error path + isolation from real providers"
```

---

## Task 12: Update AGENTS.md docs navigation + final review

**Files:**
- Modify: `AGENTS.md` (add FauxProvider to the design docs navigation table)

- [ ] **Step 1: Add FauxProvider to the design docs table in AGENTS.md**

In `AGENTS.md`, find the "### 设计文档（docs/design/）" table and add a row:

```markdown
| [docs/design/FAUX_PROVIDER.md](./docs/design/FAUX_PROVIDER.md) | FauxProvider 架构级 LLM Mock：FIFO 队列 + 工厂响应 + 流式分块 (设计稿) |
```

- [ ] **Step 2: Run the full test suite one final time**

Run:
```bash
cargo test -p ion-provider --test faux_test -- --nocapture
```
Expected: ALL tests pass (count them — should be 13+).

- [ ] **Step 3: Verify spec §7 coverage by running the acceptance matrix**

Confirm the test list maps cleanly to spec F-P0.1 through F-P1.5:

```bash
cargo test -p ion-provider --test faux_test -- --list 2>&1 | grep ": test"
```

- [ ] **Step 4: Commit**

```bash
git add AGENTS.md
git commit -m "docs: add FauxProvider to design docs navigation"
```

---

## Completion Criteria

All of the following must be true:
- [ ] `cargo build -p ion-provider` succeeds with no warnings
- [ ] `cargo build --bin ion --bin ion-worker` succeeds (no regression)
- [ ] `cargo test -p ion-provider --test faux_test` passes (13+ tests)
- [ ] All spec §7 P0 cases (F-P0.1 through F-P0.6) have passing tests
- [ ] All spec §7 P1 cases (F-P1.1 through F-P1.5) have passing tests
- [ ] FauxProvider is registered via `register_faux(&mut registry)` returning `Arc<FauxProvider>`
- [ ] No API key is required for faux
- [ ] Real providers (openai/anthropic) still dispatch correctly when faux is registered
- [ ] AGENTS.md navigation updated

---

## Notes for the implementer

- **`Context` struct**: if `Context::default()` doesn't compile or `.messages` field doesn't exist, check `ion-provider/src/types.rs:217+` and adjust factory tests accordingly. The factory's contract is "receives the live Context" — the exact field doesn't matter for the infrastructure.
- **`Cost::default()`**: verify it derives Default. If not, use `Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 }`.
- **`rand_width`**: deliberately avoids the `rand` crate to keep dependencies minimal. It uses `DefaultHasher` on system time — non-cryptographic, fine for chunk-size variation.
- **The `FauxProviderHandle` wrapper** (Task 8) is necessary because `register()` takes `Box<dyn ApiProvider>` (owned), but tests need a live `Arc<FauxProvider>` to call `set_responses`. The handle clones the Arc.
- **`sender.end(message)` consumes the sender** — that's why `faux_stream_blocks` takes ownership. The spawned task owns the sender.
