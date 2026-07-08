# Record/Replay Phase 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement LLM decision record/replay — `ION_RECORD=<id>` records real LLM responses to disk; `--model replay/<id>` replays them without network. Includes safety hardening (path traversal prevention, file perms, concurrency lock, request_hash logging, SecuredRuntime passthrough).

**Architecture:** RecordingProvider wraps a real provider via ProviderFactory, taps the EventStream to capture each Done message into `~/.ion/recordings/<id>/trace.jsonl`. ReplayProvider is a thin shell that loads the trace via `load_script` and delegates to FauxProvider. Both register as standard ApiProviders. Zero changes to agent loop or existing providers.

**Tech Stack:** Rust, ion-provider crate, tokio, async_trait, `regex` (for ID validation), std::fs permission APIs.

**Reference spec:** [docs/design/RECORD_REPLAY.md](../../design/RECORD_REPLAY.md) (revised with review feedback)

**Hard boundary:** This is LLM *decision* replay, not full environment replay. Tools execute for real during replay; only model responses are mocked. Replay must pass through SecuredRuntime — cannot bypass permissions.

---

## File Structure

| File | Responsibility | Action |
|------|----------------|--------|
| `ion-provider/src/event_stream.rs` | Add `forward_with_done_tap` public utility | Modify |
| `ion-provider/src/registry.rs` | Add `ProviderFactory` trait + `BuiltinProviderFactory` | Modify |
| `ion-provider/src/record.rs` | RecordingProvider (wraps real, taps Done, writes trace/meta) | Create |
| `ion-provider/src/replay.rs` | ReplayProvider (loads trace, delegates to FauxProvider) + ID validation + recordings_dir | Create |
| `ion-provider/src/lib.rs` | Export record + replay modules | Modify |
| `ion-provider/Cargo.toml` | Add `regex` dependency | Modify |
| `ion-provider/tests/record_replay_test.rs` | Integration tests | Create |
| `ion/src/paths.rs` | `recordings_dir()` helper (or put in ion-provider) | Modify/verify |
| `ion/src/bin/ion.rs` | Wire recording in `build_registry_and_model` + `ion recordings list` subcommand | Modify |
| `ion/src/bin/ion_worker.rs` | Wire recording (env var) + propagate | Modify |
| `ion/src/worker_registry.rs` | Propagate `ION_RECORD` / `ION_RECORD_OVERWRITE` to subprocess | Modify |
| `tests/record_replay_ci.sh` | 3-scenario bash E2E | Create |

---

## Task 1: `EventStream::forward_with_done_tap` public utility

The foundation for RecordingProvider. Also reusable for debug/observability later.

**Files:**
- Modify: `ion-provider/src/event_stream.rs`
- Create test in: `ion-provider/tests/record_replay_test.rs`

- [ ] **Step 1: Write failing test — tap captures Done message**

Create `ion-provider/tests/record_replay_test.rs`:
```rust
use ion_provider::event_stream::EventStream;
use ion_provider::types::{StreamEvent, AssistantMessage, StopReason, Usage};

fn faux_done_message(text: &str) -> AssistantMessage {
    let mut msg = AssistantMessage::new(&ion_provider::types::Model {
        id: "test".into(), name: "T".into(), api: "x".into(), provider: "x".into(),
        base_url: String::new(), reasoning: false, input: vec![],
        cost: ion_provider::types::Cost::default(),
        context_window: 1, max_tokens: 1, compat: None, headers: None,
    });
    msg.content = vec![ion_provider::types::AssistantContentBlock::Text(
        ion_provider::types::TextContent { text: text.into(), text_signature: None }
    )];
    msg.stop_reason = StopReason::Stop;
    msg
}

#[tokio::test]
async fn forward_with_done_tap_captures_final_message() {
    // Build a source stream that emits Start + Done
    let (src_stream, src_sender) = EventStream::new();
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let captured_clone = captured.clone();
    let tapped = EventStream::forward_with_done_tap(src_stream, move |msg| {
        if let Some(ion_provider::types::AssistantContentBlock::Text(t)) = msg.content.first() {
            *captured_clone.lock().unwrap() = Some(t.text.clone());
        }
    });

    // Drive the source: emit events
    src_sender.push(StreamEvent::Start { partial: faux_done_message("hello") });
    src_sender.end(faux_done_message("hello"));

    // Consume the tapped stream
    let result = tapped.result().await.unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop);
    // Wait a tick for the spawn task to run on_done
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;
    assert_eq!(*captured.lock().unwrap(), Some("hello".to_string()));
}
```

- [ ] **Step 2: Run to verify it fails (method doesn't exist)**

```bash
cd ion-provider && cargo test --test record_replay_test
```
Expected: FAIL — `no function named forward_with_done_tap`.

- [ ] **Step 3: Implement `forward_with_done_tap`**

Add to `ion-provider/src/event_stream.rs` (in `impl EventStream`):
```rust
/// Forward events from `inner` to a new EventStream, tapping the final Done/Error message.
/// `on_done` is called once with the final AssistantMessage (for recording).
/// Correctly completes the result oneshot — do NOT drop the returned stream early.
pub fn forward_with_done_tap<F>(
    mut inner: EventStream,
    on_done: F,
) -> EventStream
where
    F: FnOnce(&AssistantMessage) + Send + 'static,
{
    let (tap_stream, tap_sender) = EventStream::new();
    tokio::spawn(async move {
        let mut final_msg: Option<AssistantMessage> = None;
        while let Some(ev) = inner.recv().await {
            match &ev {
                StreamEvent::Done { message, .. } => {
                    final_msg = Some(message.clone());
                }
                StreamEvent::Error { message, .. } => {
                    final_msg = Some(message.clone());
                }
                _ => {}
            }
            tap_sender.push(ev);
        }
        // inner ended; complete the tap's oneshot
        match final_msg {
            Some(msg) => {
                if matches!(msg.stop_reason, StopReason::Error | StopReason::Aborted) {
                    tap_sender.error(msg.stop_reason.clone(), msg);
                } else {
                    tap_sender.end(msg);
                }
            }
            None => {
                // inner ended without Done/Error — tap_sender drops, result() will error
                // "stream ended without result"
            }
        }
        // Call on_done AFTER completing the stream so recording doesn't block the consumer.
        // But we need the message — re-extract from final_msg... wait, it was moved.
        // Fix: clone before end/error.
    });
    tap_stream
}
```

**IMPORTANT bug in the above:** `final_msg` is moved into `end(msg)`/`error(msg)`, so `on_done` can't use it after. Fix by cloning before consuming:

```rust
        match final_msg {
            Some(msg) => {
                on_done(&msg);  // call BEFORE consuming
                if matches!(msg.stop_reason, StopReason::Error | StopReason::Aborted) {
                    tap_sender.error(msg.stop_reason.clone(), msg);
                } else {
                    tap_sender.end(msg);
                }
            }
            None => {}
        }
```

Use the fixed version (on_done called before end/error consumes msg).

- [ ] **Step 4: Run test, verify pass**

```bash
cd ion-provider && cargo test --test record_replay_test
```
Expected: PASS (1 test).

- [ ] **Step 5: Commit**

```bash
cd ion-provider
git add src/event_stream.rs tests/record_replay_test.rs
git commit -m "feat(provider): EventStream::forward_with_done_tap public utility"
```

---

## Task 2: ProviderFactory trait

Replaces boxed_clone. Lets RecordingProvider wrap a real provider without cloning.

**Files:**
- Modify: `ion-provider/src/registry.rs`
- Modify: `ion-provider/tests/record_replay_test.rs`

- [ ] **Step 1: Write failing test — factory creates each builtin**

Append to `ion-provider/tests/record_replay_test.rs`:
```rust
use ion_provider::registry::{ProviderFactory, BuiltinProviderFactory};

#[test]
fn builtin_factory_creates_all_known_apis() {
    let f = BuiltinProviderFactory;
    assert!(f.create("openai-completions").is_some());
    assert!(f.create("anthropic-messages").is_some());
    assert!(f.create("openai-responses").is_some());
    assert!(f.create("google-generative-ai").is_some());
    assert!(f.create("nonexistent").is_none());
}
```

- [ ] **Step 2: Run to verify fail**

```bash
cd ion-provider && cargo test --test record_replay_test -- builtin_factory
```
Expected: FAIL — `cannot find type ProviderFactory`.

- [ ] **Step 3: Implement ProviderFactory + BuiltinProviderFactory**

Add to `ion-provider/src/registry.rs` (after the ApiProvider trait, before ApiRegistry):
```rust
/// Factory for creating fresh provider instances by API name.
/// Used by RecordingProvider to wrap a real provider without cloning.
pub trait ProviderFactory: Send + Sync {
    fn create(&self, api: &str) -> Option<Box<dyn ApiProvider>>;
}

/// Built-in provider factory — knows how to construct each builtin.
pub struct BuiltinProviderFactory;

impl ProviderFactory for BuiltinProviderFactory {
    fn create(&self, api: &str) -> Option<Box<dyn ApiProvider>> {
        match api {
            "openai-completions" => Some(Box::new(super::openai::OpenAICompletionsProvider)),
            "anthropic-messages" => Some(Box::new(super::anthropic::AnthropicMessagesProvider)),
            "openai-responses" => Some(Box::new(super::openai_responses::OpenAIResponsesProvider)),
            "google-generative-ai" => Some(Box::new(super::google::GoogleGenerativeAIProvider)),
            _ => None,
        }
    }
}
```

- [ ] **Step 4: Run, verify pass**

```bash
cd ion-provider && cargo test --test record_replay_test
```
Expected: PASS (2 tests).

- [ ] **Step 5: Commit**

```bash
cd ion-provider
git add src/registry.rs tests/record_replay_test.rs
git commit -m "feat(provider): ProviderFactory trait + BuiltinProviderFactory"
```

---

## Task 3: `recordings_dir()` + recording ID validation

Path helper and the security-critical ID validator (path traversal prevention).

**Files:**
- Create: `ion-provider/src/replay.rs` (start the module)
- Modify: `ion-provider/Cargo.toml` (add regex dep)
- Modify: `ion-provider/src/lib.rs` (add `pub mod replay;`)
- Modify: `ion-provider/tests/record_replay_test.rs`

- [ ] **Step 1: Add `regex` to Cargo.toml**

In `ion-provider/Cargo.toml`, under `[dependencies]`:
```toml
regex = "1"
```

- [ ] **Step 2: Write failing tests for ID validation**

Append to tests:
```rust
use ion_provider::replay::{validate_recording_id, recordings_dir};

#[test]
fn validate_recording_id_accepts_safe_ids() {
    assert!(validate_recording_id("fix-bug").is_ok());
    assert!(validate_recording_id("test_2026-07-08.v1").is_ok());
    assert!(validate_recording_id("a").is_ok());
    assert!(validate_recording_id(&"x".repeat(80)).is_ok());
}

#[test]
fn validate_recording_id_rejects_path_traversal() {
    assert!(validate_recording_id("../etc/passwd").is_err());
    assert!(validate_recording_id("..").is_err());
    assert!(validate_recording_id("a/b").is_err(), "slash must be rejected");
    assert!(validate_recording_id("a b").is_err(), "space must be rejected");
    assert!(validate_recording_id("a%2eb").is_err(), "url-encoded chars rejected");
}

#[test]
fn validate_recording_id_rejects_too_long() {
    assert!(validate_recording_id(&"x".repeat(81)).is_err());
}
```

- [ ] **Step 3: Run to verify fail**

```bash
cd ion-provider && cargo test --test record_replay_test -- validate_recording_id
```
Expected: FAIL — module doesn't exist.

- [ ] **Step 4: Add module declaration to lib.rs**

In `ion-provider/src/lib.rs`, add after `pub mod faux;`:
```rust
pub mod replay;
pub mod record;
```

- [ ] **Step 5: Create replay.rs with validation + recordings_dir**

Create `ion-provider/src/replay.rs`:
```rust
//! Record/Replay — recording ID validation, path helpers, ReplayProvider.
//! See docs/design/RECORD_REPLAY.md.

use crate::error::ProviderResult;
use std::path::PathBuf;
use regex::Regex;
use std::sync::OnceLock;

static ID_REGEX: OnceLock<Regex> = OnceLock::new();

fn id_regex() -> &'static Regex {
    ID_REGEX.get_or_init(|| Regex::new(r"^[a-zA-Z0-9._-]{1,80}$").unwrap())
}

/// Validate a recording ID. Only [a-zA-Z0-9._-], 1-80 chars.
/// Prevents path traversal (../, /, url-encoded).
pub fn validate_recording_id(id: &str) -> ProviderResult<()> {
    if !id_regex().is_match(id) {
        return Err(crate::ProviderError::Stream(format!(
            "invalid recording id '{}': only [a-zA-Z0-9._-] allowed, 1-80 chars", id
        )));
    }
    Ok(())
}

/// Base directory for all recordings: ~/.ion/recordings
pub fn recordings_dir() -> PathBuf {
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".ion").join("recordings")
}

/// Full path to a recording's trace file, after validating ID + canonicalizing.
pub fn recording_trace_path(id: &str) -> ProviderResult<PathBuf> {
    validate_recording_id(id)?;
    let base = recordings_dir();
    let path = base.join(id).join("trace.jsonl");
    // Canonicalize-check: resolved path must stay under base.
    // (Use lexical starts_with when file doesn't exist yet — canonicalize fails on nonexistent.)
    let canonical_base = base.canonicalize().unwrap_or_else(|_| base.clone());
    if !path.starts_with(&canonical_base) {
        return Err(crate::ProviderError::Stream(format!(
            "recording id escapes recordings dir: {}", id
        )));
    }
    Ok(path)
}

/// Full path to a recording's meta file.
pub fn recording_meta_path(id: &str) -> ProviderResult<PathBuf> {
    validate_recording_id(id)?;
    Ok(recordings_dir().join(id).join("meta.json"))
}
```

- [ ] **Step 6: Run, verify pass**

```bash
cd ion-provider && cargo test --test record_replay_test
```
Expected: PASS (5 tests).

- [ ] **Step 7: Commit**

```bash
cd ion-provider
git add Cargo.toml Cargo.lock src/lib.rs src/replay.rs tests/record_replay_test.rs
git commit -m "feat(provider): recordings_dir + recording ID validation (path traversal prevention)"
```

---

## Task 4: RecordingProvider

The recorder. Wraps a real provider, taps Done, writes trace + meta.

**Files:**
- Create: `ion-provider/src/record.rs`
- Modify: `ion-provider/tests/record_replay_test.rs`

- [ ] **Step 1: Write failing test — RecordingProvider captures a response**

Append to tests:
```rust
use ion_provider::record::RecordingProvider;
use ion_provider::faux::{FauxProvider, FauxResponseStep, faux_assistant_message, FauxContent, FauxMessageOptions};
use ion_provider::registry::{ApiRegistry, ApiProvider, stream};
use ion_provider::types::{Model, Context};
use std::sync::Arc;
use tempfile::tempdir;

fn faux_model_with_api(api: &str) -> Model {
    Model {
        id: "faux-1".into(), name: "Faux".into(), api: api.into(), provider: "faux".into(),
        base_url: String::new(), reasoning: false, input: vec!["text".into()],
        cost: ion_provider::types::Cost::default(),
        context_window: 128000, max_tokens: 8192, compat: None, headers: None,
    }
}

#[tokio::test]
async fn recording_provider_writes_trace_on_done() {
    let tmp = tempdir().unwrap();
    let trace_path = tmp.path().join("trace.jsonl");
    let meta_path = tmp.path().join("meta.json");

    // Inner provider is a FauxProvider with one response
    let inner_faux = Arc::new(FauxProvider::new());
    inner_faux.set_responses(vec![FauxResponseStep::Static(
        faux_assistant_message(FauxContent::Text("recorded hello".into()), FauxMessageOptions::default())
    )]);

    // Wrap inner in a handle that impls ApiProvider
    struct InnerHandle(Arc<FauxProvider>);
    #[async_trait::async_trait]
    impl ApiProvider for InnerHandle {
        async fn stream(&self, model: &Model, ctx: &Context, opts: Option<&ion_provider::types::StreamOptions>) -> ion_provider::error::ProviderResult<ion_provider::event_stream::EventStream> {
            self.0.stream(model, ctx, opts).await
        }
    }

    let recording = RecordingProvider::new(
        Box::new(InnerHandle(inner_faux)),
        trace_path.clone(),
        meta_path.clone(),
    );

    // Register under "test-rec" and stream
    let mut reg = ApiRegistry::new();
    reg.register("test-rec", Box::new(recording));
    let model = faux_model_with_api("test-rec");
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None).await.unwrap();
    while es.recv().await.is_some() {}

    // trace.jsonl should have 1 line
    let trace = std::fs::read_to_string(&trace_path).unwrap();
    assert_eq!(trace.lines().count(), 1, "exactly one trace line");
    assert!(trace.contains("recorded hello"), "trace contains the response text");

    // meta.json should exist with response_count >= 1
    let meta = std::fs::read_to_string(&meta_path).unwrap();
    assert!(meta.contains("\"response_count\":1"), "meta has response_count");
}
```

- [ ] **Step 2: Run to verify fail**

```bash
cd ion-provider && cargo test --test record_replay_test -- recording_provider_writes_trace
```
Expected: FAIL — module/types don't exist.

- [ ] **Step 3: Implement RecordingProvider**

Create `ion-provider/src/record.rs`:
```rust
//! RecordingProvider — wraps a real provider, taps Done, writes trace + meta.

use crate::error::ProviderResult;
use crate::event_stream::EventStream;
use crate::registry::ApiProvider;
use crate::types::{AssistantMessage, AssistantContentBlock, Context, Model, StreamOptions, TextContent, ThinkingContent, ToolCall};
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use serde::{Deserialize, Serialize};

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

#[derive(Serialize)]
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
    ) -> ProviderResult<EventStream> {
        // Update meta with model info (first call)
        {
            let mut m = self.meta.lock().unwrap();
            if m.model.is_empty() {
                m.model = model.id.clone();
                m.provider = model.provider.clone();
            }
        }
        // Compute request_hash for this step (Phase 1: record only, don't enforce)
        let req_hash = request_hash(context, model);

        let inner_stream = self.inner.stream(model, context, options).await?;
        let trace_path = self.trace_path.clone();
        let meta_path = self.meta_path.clone();
        let meta_arc = self.meta.clone();

        Ok(EventStream::forward_with_done_tap(inner_stream, move |msg| {
            write_trace_line(&trace_path, msg, &req_hash);
            update_meta(&meta_arc, &meta_path, msg);
        }))
    }
}

/// Stable hash of the request context (messages count + roles + model + system prompt).
/// Used for replay divergence detection (Phase 1: record only).
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

/// Serialize an AssistantMessage to a trace line (faux-script-compatible + request_hash).
/// request_hash is an extra field that load_script's parse_script_line ignores (forward-compatible).
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
    if matches!(msg.stop_reason, crate::types::StopReason::Error) {
        obj["stop_reason"] = serde_json::Value::String("error".into());
        if let Some(em) = &msg.error_message {
            obj["error_message"] = serde_json::Value::String(em.clone());
        }
    }
    // request_hash — extra field, ignored by load_script but recorded for Phase 2 strict mode
    obj["request_hash"] = serde_json::Value::String(req_hash.to_string());
    obj.to_string()
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
```

- [ ] **Step 4: Run, verify pass**

```bash
cd ion-provider && cargo test --test record_replay_test
```
Expected: PASS (6 tests).

- [ ] **Step 5: Commit**

```bash
cd ion-provider
git add src/record.rs src/lib.rs tests/record_replay_test.rs
git commit -m "feat(provider): RecordingProvider wraps real provider, writes trace + meta"
```

---

## Task 5: ReplayProvider (load trace + delegate to FauxProvider)

The shell. Validates ID, loads trace, hands to FauxProvider.

**Files:**
- Modify: `ion-provider/src/replay.rs`
- Modify: `ion-provider/tests/record_replay_test.rs`

- [ ] **Step 1: Write failing test — record then replay round-trip**

Append to tests:
```rust
use ion_provider::replay::ReplayProvider;

#[tokio::test]
async fn replay_provider_round_trips_a_recording() {
    // First: record via a faux-backed RecordingProvider
    let tmp = tempdir().unwrap();
    // Override recordings_dir by writing trace to tmp and patching... actually ReplayProvider
    // uses recordings_dir() which is ~/.ion/recordings. For test isolation, we test the
    // load-from-path logic directly.
    let trace_path = tmp.path().join("my-rec").join("trace.jsonl");
    std::fs::create_dir_all(trace_path.parent().unwrap()).unwrap();
    std::fs::write(&trace_path, r#"{"text":"replayed hi"}
"#).unwrap();

    // Load via load_script (same path ReplayProvider uses)
    let steps = ion_provider::faux::load_script(&trace_path).unwrap();
    assert_eq!(steps.len(), 1);

    // Drive through FauxProvider to confirm
    let faux = Arc::new(FauxProvider::new());
    faux.set_responses(steps);
    struct H(Arc<FauxProvider>);
    #[async_trait::async_trait]
    impl ApiProvider for H {
        async fn stream(&self, m: &Model, c: &Context, o: Option<&StreamOptions>) -> ProviderResult<EventStream> {
            self.0.stream(m, c, o).await
        }
    }
    let mut reg = ApiRegistry::new();
    reg.register("test", Box::new(H(faux)));
    let model = faux_model_with_api("test");
    let ctx = Context::default();
    let mut es = stream(&reg, &model, &ctx, None).await.unwrap();
    let mut text = None;
    while let Some(ev) = es.recv().await {
        if let ion_provider::types::StreamEvent::Done { message, .. } = ev {
            if let Some(ion_provider::types::AssistantContentBlock::Text(t)) = message.content.first() {
                text = Some(t.text.clone());
            }
        }
    }
    assert_eq!(text, Some("replayed hi".to_string()));
}

#[test]
fn replay_provider_rejects_path_traversal_id() {
    use ion_provider::replay::recording_trace_path;
    assert!(recording_trace_path("../../etc/passwd").is_err());
    assert!(recording_trace_path("a/b").is_err());
}
```

- [ ] **Step 2: Run to verify fail (ReplayProvider not defined)**

```bash
cd ion-provider && cargo test --test record_replay_test -- replay_provider
```
Expected: FAIL — `cannot find type ReplayProvider`.

- [ ] **Step 3: Implement ReplayProvider**

Append to `ion-provider/src/replay.rs`:
```rust
use crate::faux::{FauxProvider, FauxResponseStep};
use crate::event_stream::EventStream;
use crate::types::{Context, Model, StreamOptions};
use async_trait::async_trait;

/// Replay provider: loads a recording by model.id, delegates to FauxProvider.
/// Register under "replay" key. Use via `--model replay/<recording-id>`.
pub struct ReplayProvider;

#[async_trait]
impl ApiProvider for ReplayProvider {
    async fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: Option<&StreamOptions>,
    ) -> ProviderResult<EventStream> {
        let recording_id = &model.id;
        let trace_path = recording_trace_path(recording_id)?;

        if !trace_path.exists() {
            return Err(crate::ProviderError::Stream(format!(
                "recording '{}' not found at {}", recording_id, trace_path.display()
            )));
        }

        // Loud warning: tools will execute for real
        eprintln!("[replay] ⚠️  Tools will execute for real. Replaying decisions from '{}'.", recording_id);
        eprintln!("[replay] ⚠️  Ensure you are in an isolated workspace.");

        let steps = crate::faux::load_script(&trace_path)?;
        let faux = FauxProvider::new();
        faux.set_responses(steps);
        // Delegate
        faux.stream(model, context, options).await
    }
}
```

- [ ] **Step 4: Run, verify pass**

```bash
cd ion-provider && cargo test --test record_replay_test
```
Expected: PASS (8 tests).

- [ ] **Step 5: Commit**

```bash
cd ion-provider
git add src/replay.rs tests/record_replay_test.rs
git commit -m "feat(provider): ReplayProvider — loads trace, delegates to FauxProvider, loud safety warning"
```

---

## Task 6: Concurrency lock + ID conflict protection

Prevents two processes from writing the same recording; refuses to overwrite without `ION_RECORD_OVERWRITE=1`.

**Files:**
- Modify: `ion-provider/src/replay.rs` (add `acquire_recording_lock`)
- Modify: `ion-provider/tests/record_replay_test.rs`

- [ ] **Step 1: Write failing tests**

Append to tests:
```rust
use ion_provider::replay::acquire_recording_lock;

#[test]
fn acquire_lock_succeeds_for_new_recording() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("new-rec");
    std::fs::create_dir_all(&rec_dir).unwrap();
    let lock = acquire_recording_lock(&rec_dir, false).unwrap();
    assert!(lock.is_some(), "first acquire should succeed");
    drop(lock);
}

#[test]
fn acquire_lock_fails_when_already_held() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("held-rec");
    std::fs::create_dir_all(&rec_dir).unwrap();
    let _lock1 = acquire_recording_lock(&rec_dir, false).unwrap();
    // Second acquire without overwrite → should fail
    let lock2 = acquire_recording_lock(&rec_dir, false);
    assert!(lock2.is_err(), "second acquire without OVERWRITE must fail");
}

#[test]
fn acquire_lock_overwrite_clears_existing() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("ow-rec");
    std::fs::create_dir_all(&rec_dir).unwrap();
    let _lock1 = acquire_recording_lock(&rec_dir, false).unwrap();
    drop(_lock1);
    // Existing lock file + trace; overwrite=true should clear and succeed
    std::fs::write(rec_dir.join("trace.jsonl"), "stale").unwrap();
    let lock2 = acquire_recording_lock(&rec_dir, true).unwrap();
    assert!(lock2.is_some());
}
```

- [ ] **Step 2: Run to verify fail**

```bash
cd ion-provider && cargo test --test record_replay_test -- acquire_lock
```
Expected: FAIL — `cannot find function acquire_recording_lock`.

- [ ] **Step 3: Implement lock**

Append to `ion-provider/src/replay.rs`:
```rust
use std::fs::File;
use std::io::Write;

/// RAII guard for a recording lock file. Releases on drop (deletes the file).
pub struct RecordingLock(PathBuf);
impl Drop for RecordingLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Acquire an exclusive lock for a recording directory.
/// Returns Ok(Some(lock)) on success, Ok(None) if allow_none, Err on conflict.
/// If `overwrite` is true, clears existing lock + trace.
pub fn acquire_recording_lock(rec_dir: &Path, overwrite: bool) -> ProviderResult<Option<RecordingLock>> {
    let lock_path = rec_dir.join(".lock");
    if lock_path.exists() {
        if !overwrite {
            return Err(crate::ProviderError::Stream(format!(
                "recording already exists or is active. Set ION_RECORD_OVERWRITE=1 to overwrite."
            )));
        }
        // Overwrite: clear lock + trace
        let _ = std::fs::remove_file(&lock_path);
        let trace = rec_dir.join("trace.jsonl");
        if trace.exists() { let _ = std::fs::remove_file(&trace); }
        let meta = rec_dir.join("meta.json");
        if meta.exists() { let _ = std::fs::remove_file(&meta); }
    }
    std::fs::create_dir_all(rec_dir)
        .map_err(|e| crate::ProviderError::Stream(format!("failed to create recording dir: {}", e)))?;
    let _ = std::fs::set_permissions(rec_dir, std::os::unix::fs::PermissionsExt::from_mode(0o700));
    let mut f = std::fs::OpenOptions::new().create_new(true).write(true).open(&lock_path)
        .map_err(|e| crate::ProviderError::Stream(format!("failed to acquire lock: {}", e)))?;
    let _ = writeln!(f, "{}", std::process::id());
    Ok(Some(RecordingLock(lock_path)))
}
```

- [ ] **Step 4: Run, verify pass**

```bash
cd ion-provider && cargo test --test record_replay_test
```
Expected: PASS (11 tests).

- [ ] **Step 5: Commit**

```bash
cd ion-provider
git add src/replay.rs tests/record_replay_test.rs
git commit -m "feat(provider): recording concurrency lock + ID conflict protection"
```

---

## Task 7: Wire into ion binaries (ion.rs + ion_worker.rs)

Register ReplayProvider always; activate RecordingProvider when `ION_RECORD` is set.

**Files:**
- Modify: `ion/src/bin/ion.rs` (build_registry_and_model)
- Modify: `ion/src/bin/ion_worker.rs`
- Modify: `ion/src/worker_registry.rs` (propagate ION_RECORD env vars)

- [ ] **Step 1: Register ReplayProvider in ion.rs build_registry_and_model**

In `ion/src/bin/ion.rs`, find `build_registry_and_model` (around line 603-604, after `registry.register_builtins();`). Add:
```rust
// Replay provider (always registered; activated via --model replay/<id>)
registry.register("replay", Box::new(ion_provider::replay::ReplayProvider));

// Recording (activated via ION_RECORD env var)
if let Ok(rec_id) = std::env::var("ION_RECORD") {
    let overwrite = std::env::var("ION_RECORD_OVERWRITE").is_ok();
    match ion_provider::replay::recording_trace_path(&rec_id) {
        Ok(trace_path) => {
            let rec_dir = trace_path.parent().unwrap().to_path_buf();
            match ion_provider::replay::acquire_recording_lock(&rec_dir, overwrite) {
                Ok(_lock) => {
                    let meta_path = ion_provider::replay::recording_meta_path(&rec_id).unwrap();
                    // Build a real provider via factory, wrap in RecordingProvider
                    let factory = ion_provider::registry::BuiltinProviderFactory;
                    if let Some(real) = factory.create(&model.api) {
                        let recording = ion_provider::record::RecordingProvider::new(
                            real, trace_path, meta_path,
                        );
                        registry.register(&model.api, Box::new(recording));
                        eprintln!("[record] recording to {} (model: {})", rec_dir.display(), model.id);
                        // Hold the lock for the process lifetime — leak intentionally
                        std::mem::forget(_lock);
                    } else {
                        eprintln!("[record] ⚠️  no builtin provider for api '{}', recording disabled", model.api);
                    }
                }
                Err(e) => {
                    eprintln!("[record] ⚠️  failed to acquire lock: {}", e);
                }
            }
        }
        Err(e) => eprintln!("[record] ⚠️  invalid recording id: {}", e),
    }
}
```

**NOTE on `model` mutability:** `model` must be `let mut` at this point in build_registry_and_model. Check the current declaration — if it's `let model = ...`, change to `let mut model = ...`. The recording block reads `model.api` and `model.id`.

- [ ] **Step 2: Same wiring in ion_worker.rs**

In `ion/src/bin/ion_worker.rs` (around line 60-61, after `registry.register_builtins();`), add the same block (replay registration + recording activation). The ion_worker also has `model` in scope by that point.

- [ ] **Step 3: Register ReplayProvider unconditionally in both binaries**

The `registry.register("replay", Box::new(ion_provider::replay::ReplayProvider));` line goes BEFORE the ION_RECORD check, so replay is always available.

- [ ] **Step 4: Propagate ION_RECORD env vars to subprocess in worker_registry.rs**

In `ion/src/worker_registry.rs` (around line 261, after the ION_FAUX_* propagation block), add:
```rust
// Propagate recording env vars to child workers
for var in &["ION_RECORD", "ION_RECORD_OVERWRITE"] {
    if let Ok(val) = std::env::var(var) {
        child_cmd.env(var, &val);
    }
}
```

- [ ] **Step 5: Build ion + ion-worker**

```bash
cd ion && cargo build --bin ion --bin ion-worker 2>&1 | tail -5
```
Expected: no errors.

- [ ] **Step 6: Commit**

```bash
cd ion
git add src/bin/ion.rs src/bin/ion_worker.rs src/worker_registry.rs
git commit -m "feat: wire Record/Replay into ion binaries + subprocess propagation"
```

---

## Task 8: `ion recordings list` subcommand

Listing recordings for management (privacy + UX).

**Files:**
- Modify: `ion/src/bin/ion.rs` (add subcommand + handler)

- [ ] **Step 1: Add `Recordings` variant to Commands enum**

Find the `Commands` enum in `ion/src/bin/ion.rs` (around line 242). Add:
```rust
/// List all recordings
Recordings,
```

- [ ] **Step 2: Add dispatch in main()**

Find the dispatch match (around line 1746). Add:
```rust
Some(Commands::Recordings) => cmd_recordings().await,
```

- [ ] **Step 3: Implement cmd_recordings**

Add a function in ion.rs:
```rust
async fn cmd_recordings() {
    let dir = ion_provider::replay::recordings_dir();
    if !dir.exists() {
        println!("No recordings ({} doesn't exist)", dir.display());
        return;
    }
    println!("{:<30} {:<20} {:<10} {:<20}", "ID", "MODEL", "RESPONSES", "CREATED");
    println!("{}", "-".repeat(80));
    let mut entries: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten().filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let id = entry.file_name().to_string_lossy().to_string();
        let meta_path = entry.path().join("meta.json");
        if let Ok(content) = std::fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                println!(
                    "{:<30} {:<20} {:<10} {:<20}",
                    id,
                    meta.get("model").and_then(|v| v.as_str()).unwrap_or("?"),
                    meta.get("response_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    meta.get("created_at").and_then(|v| v.as_i64())
                        .map(|t| chrono_ms_to_date(t)).unwrap_or_else(|| "?".into()),
                );
                continue;
            }
        }
        println!("{:<30} {:<20} {:<10} {:<20}", id, "?", "?", "(no meta)");
    }
}

fn chrono_ms_to_date(ms: i64) -> String {
    // Simple: just show the ms as-is; full date formatting requires chrono.
    // For now, convert to seconds since epoch.
    format!("{}s", ms / 1000)
}
```

- [ ] **Step 4: Build + test manually**

```bash
cd ion && cargo build --bin ion 2>&1 | tail -3
./target/debug/ion recordings
```
Expected: lists recordings or "No recordings".

- [ ] **Step 5: Commit**

```bash
cd ion
git add src/bin/ion.rs
git commit -m "feat: ion recordings list subcommand"
```

---

## Task 9: Bash E2E test — record/replay across 3 scenarios

End-to-end verification.

**Files:**
- Create: `ion/tests/record_replay_ci.sh`

- [ ] **Step 1: Write the script**

Create `ion/tests/record_replay_ci.sh`:
```bash
#!/usr/bin/env bash
# Record/Replay 三场景验证
set -uo pipefail
PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$PROJECT_DIR"

PASS=0; FAIL=0
green(){ printf "\033[32m%s\033[0m\n" "$1"; }
red(){ printf "\033[31m%s\033[0m\n" "$1"; }
pass(){ green "✅ PASS: $1"; ((PASS++)); }
fail(){ red "❌ FAIL: $1"; ((FAIL++)); }

echo "── Phase 0: Build ──"
cargo build --bin ion --bin ion-worker 2>&1 | tail -2
ION_BIN="$PROJECT_DIR/target/debug/ion"

# Clean test dir
TEST_DIR=$(mktemp -d)
cd "$TEST_DIR"

# Clean recordings for this test
rm -rf ~/.ion/recordings/rr-test-*

echo ""
echo "── Group A: 录制 ──"

# A1: 基本录制（用 faux 作为"真实"provider 录制，避免真实 LLM 依赖）
# 这是个元测试：用 faux 产生响应，录制器录下来
ION_FAUX_REPLY="recorded response A1" ION_RECORD=rr-test-a1 timeout 30 "$ION_BIN" --no-session --no-tools "say hi" >/tmp/a1.log 2>&1
if [ -f ~/.ion/recordings/rr-test-a1/trace.jsonl ] && grep -q "recorded response A1" ~/.ion/recordings/rr-test-a1/trace.jsonl; then
    pass "A1 录制基本响应"
else
    fail "A1 录制基本响应 (log: $(cat /tmp/a1.log))"
fi

# A2: 路径穿越拒绝
OUTPUT=$(timeout 10 "$ION_BIN" --no-session --no-tools --model replay/../../etc/hostname "x" 2>&1 || true)
if echo "$OUTPUT" | grep -qi "invalid recording id\|recording id"; then
    pass "A2 路径穿越拒绝"
else
    fail "A2 路径穿越拒绝 (output: $OUTPUT)"
fi

echo ""
echo "── Group B: 回放 ──"

# B1: 基本回放
OUTPUT=$(timeout 30 "$ION_BIN" --no-session --no-tools --model replay/rr-test-a1 "replay hi" 2>&1)
if echo "$OUTPUT" | grep -q "recorded response A1"; then
    pass "B1 回放复现录制响应"
else
    fail "B1 回放复现 (output: $OUTPUT)"
fi

# B2: 回放不存在 ID
OUTPUT=$(timeout 10 "$ION_BIN" --no-session --no-tools --model replay/nonexistent-xyz "x" 2>&1 || true)
if echo "$OUTPUT" | grep -qi "not found"; then
    pass "B2 回放不存在 ID 报错"
else
    fail "B2 回放不存在 ID (output: $OUTPUT)"
fi

# B3: 回放安全提示
OUTPUT=$(timeout 30 "$ION_BIN" --no-session --no-tools --model replay/rr-test-a1 "x" 2>&1)
if echo "$OUTPUT" | grep -qi "Tools will execute for real\|⚠️"; then
    pass "B3 回放安全提示"
else
    fail "B3 回放安全提示 (output: $OUTPUT)"
fi

echo ""
echo "── Group C: 管理 + 边界 ──"

# C1: ion recordings list
OUTPUT=$(timeout 10 "$ION_BIN" recordings 2>&1)
if echo "$OUTPUT" | grep -q "rr-test-a1"; then
    pass "C1 ion recordings list"
else
    fail "C1 ion recordings list (output: $OUTPUT)"
fi

# C2: 录制冲突报错
OUTPUT=$(ION_FAUX_REPLY="x" ION_RECORD=rr-test-a1 timeout 10 "$ION_BIN" --no-session --no-tools "x" 2>&1 || true)
if echo "$OUTPUT" | grep -qi "already exists\|OVERWRITE"; then
    pass "C2 录制冲突报错"
else
    fail "C2 录制冲突报错 (output: $OUTPUT)"
fi

# C3: OVERWRITE 覆盖
OUTPUT=$(ION_FAUX_REPLY="overwritten" ION_RECORD=rr-test-a1 ION_RECORD_OVERWRITE=1 timeout 10 "$ION_BIN" --no-session --no-tools "x" 2>&1)
if grep -q "overwritten" ~/.ion/recordings/rr-test-a1/trace.jsonl; then
    pass "C3 OVERWRITE 覆盖"
else
    fail "C3 OVERWRITE 覆盖 (output: $OUTPUT)"
fi

echo ""
echo "── 结果 ──"
echo "PASS=$PASS FAIL=$FAIL"
[ "$FAIL" -eq 0 ] && green "全部通过" || red "有失败"
exit $FAIL
```

Make executable: `chmod +x tests/record_replay_ci.sh`

- [ ] **Step 2: Run and adjust**

```bash
cd ion && bash tests/record_replay_ci.sh
```

**Adjust as needed:** the grep patterns are based on predicted output. If actual differs, fix the patterns. The script MUST pass before committing. Common adjustments:
- Error message wording (`invalid recording id` vs `invalid id`)
- The path traversal test might exit 0 vs 1 — handle both
- `recordings` subcommand output format

- [ ] **Step 3: Commit**

```bash
cd ion
git add tests/record_replay_ci.sh
git commit -m "test: Record/Replay 三场景 bash E2E (record/replay/list/conflict/safety)"
```

---

## Task 10: Spec coverage self-check + final verification

Verify all P0/P1 cases from spec §7 are covered.

- [ ] **Step 1: Run ion-provider record/replay tests**

```bash
cd ion-provider && cargo test --test record_replay_test -- --nocapture
```
Expected: 11+ tests pass.

- [ ] **Step 2: Run full ion-provider test suite (no regressions)**

```bash
cd ion-provider && cargo test
```
Expected: faux_test (22) + record_replay_test (11+) all pass.

- [ ] **Step 3: Run ion build**

```bash
cd ion && cargo build --bin ion --bin ion-worker
```
Expected: no errors.

- [ ] **Step 4: Run the bash E2E**

```bash
cd ion && bash tests/record_replay_ci.sh
```
Expected: all groups pass.

- [ ] **Step 5: Spec coverage mapping**

| Spec case | Covered by |
|-----------|-----------|
| RR-P0.1 basic record | Task 9 A1 |
| RR-P0.2 record with tool call | (covered by RecordingProvider test if extended; A1 uses faux with no tools — note as partial) |
| RR-P0.3 basic replay | Task 9 B1 |
| RR-P0.4 replay multi-turn | (extends naturally; add if time) |
| RR-P0.5 replay no API key | B1 (no key set) |
| RR-P0.6 ID conflict error | Task 9 C2 |
| RR-P0.7 path traversal reject | Task 9 A2 |
| RR-P0.8 illegal chars reject | Task 3 unit tests |
| RR-P0.9 replay via SecuredRuntime | (existing behavior — agent loop unchanged) |
| RR-P0.10 safety warning | Task 9 B3 |
| RR-P0.11 exhausted loud failure | (FauxProvider inherits; covered by faux_test) |
| RR-P0.12 file permissions | (set in write_trace_line; verify manually) |

- [ ] **Step 6: Commit any final fixes + push**

```bash
cd ion && git add -A && git commit -m "test: spec coverage fixes for Record/Replay" || echo "nothing to commit"
```

---

## Completion Criteria

- [ ] `cargo test -p ion-provider` passes (faux_test 22 + record_replay_test 11+)
- [ ] `cargo build --bin ion --bin ion-worker` succeeds
- [ ] `bash tests/record_replay_ci.sh` all groups pass
- [ ] Path traversal rejected (A2)
- [ ] Recording conflict error without OVERWRITE (C2)
- [ ] OVERWRITE works (C3)
- [ ] `ion recordings list` shows recordings (C1)
- [ ] Replay emits safety warning (B3)
- [ ] Recording writes 0600 perms, dir 0700

---

## Notes for the implementer

- **Two repos:** `ion-provider` (Tasks 1-6) and `ion` (Tasks 7-9). ion-provider is a separate git repo at `../ion-provider`.
- **`model` mutability:** in build_registry_and_model, ensure `let mut model` so the recording block can read its fields (or restructure).
- **Lock lifetime:** the `std::mem::forget(_lock)` in Task 7 holds the lock for the process lifetime (so subprocesses see it). The lock file is cleaned on next OVERWRITE.
- **tempfile dep:** ion-provider already has `tempfile` as dev-dependency from the faux work.
- **Multiple tool calls per message:** serialize_response emits only the first tool_call (faux script format limitation). Acceptable for Phase 1; rare in practice.
- **`chrono_ms_to_date`:** placeholder returns seconds; if chrono is available, use real date formatting. Not critical for Phase 1.
