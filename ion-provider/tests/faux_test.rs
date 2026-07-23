use ion_provider::faux::{
    faux_assistant_message, FauxContent, FauxMessageOptions, FauxProvider, FauxResponseStep,
    FauxState,
};
use ion_provider::registry::{ApiRegistry, ApiProvider, stream};
use ion_provider::types::{AssistantContentBlock, Context, Model, StopReason, StreamOptions};
use ion_provider::ProviderResult;
use ion_provider::EventStream;

#[test]
fn faux_provider_can_be_created_empty() {
    let _provider = FauxProvider::new();
}

#[test]
fn faux_state_has_call_count() {
    let state = FauxState { call_count: 0 };
    assert_eq!(state.call_count, 0);
}

/// Helper: build a minimal static text response step.
fn static_text_step(text: &str) -> FauxResponseStep {
    FauxResponseStep::Static(faux_assistant_message(
        FauxContent::Text(text.into()),
        FauxMessageOptions::default(),
    ))
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
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        self.0.stream(model, context, options, cancel).await
    }
}

#[tokio::test]
async fn stream_returns_queued_static_message() {
    let provider = std::sync::Arc::new(FauxProvider::new());
    provider.set_responses(vec![static_text_step("hello")]);
    let reg = registry_with_faux(provider.clone());
    let model = faux_model();
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None, None).await.unwrap();
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
async fn stream_emits_text_deltas_for_long_text() {
    let provider = std::sync::Arc::new(FauxProvider::new());
    let long_text = "This is a long enough message to be chunked into multiple deltas.";
    provider.set_responses(vec![static_text_step(long_text)]);
    let reg = registry_with_faux(provider);
    let model = faux_model();
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None, None).await.unwrap();
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

#[tokio::test]
async fn stream_loud_failure_on_empty_queue() {
    let provider = std::sync::Arc::new(FauxProvider::new());
    // Queue is empty
    let reg = registry_with_faux(provider.clone());
    let model = faux_model();
    let ctx = Context::default();

    let result = stream(&reg, &model, &ctx, None, None).await;
    assert!(result.is_err(), "empty queue must error loudly");
    // Match to avoid requiring EventStream: Debug (unwrap_err needs T: Debug).
    let err = match result {
        Err(e) => e,
        Ok(_) => unreachable!("checked is_err above"),
    };
    assert!(format!("{err}").to_lowercase().contains("no more faux responses")
        || format!("{err}").to_lowercase().contains("faux"));
}

// ── Builder function tests (Task 6) ──

#[test]
fn faux_text_builder() {
    let b = ion_provider::faux::faux_text("hi");
    match b {
        AssistantContentBlock::Text(t) => assert_eq!(t.text, "hi"),
        _ => panic!("expected Text"),
    }
}

#[test]
fn faux_thinking_builder() {
    let b = ion_provider::faux::faux_thinking("plan");
    match b {
        AssistantContentBlock::Thinking(t) => assert_eq!(t.thinking, "plan"),
        _ => panic!("expected Thinking"),
    }
}

#[test]
fn faux_tool_call_builder_has_unique_id() {
    let b1 = ion_provider::faux::faux_tool_call("echo", serde_json::json!({"x":1}));
    let b2 = ion_provider::faux::faux_tool_call("echo", serde_json::json!({"x":2}));
    let id1 = match b1 { AssistantContentBlock::ToolCall(t) => t.id, _ => panic!() };
    let id2 = match b2 { AssistantContentBlock::ToolCall(t) => t.id, _ => panic!() };
    assert_ne!(id1, id2, "tool call ids must be unique");
}

#[test]
fn faux_assistant_message_from_text_string() {
    let msg = ion_provider::faux::faux_assistant_message(
        ion_provider::faux::FauxContent::Text("hello".into()),
        ion_provider::faux::FauxMessageOptions::default(),
    );
    assert_eq!(msg.content.len(), 1);
    assert_eq!(msg.api, "faux");
    assert_eq!(msg.provider, "faux");
    assert_eq!(msg.model, "faux-1");
    assert_eq!(msg.stop_reason, StopReason::Stop);
}

#[test]
fn faux_assistant_message_from_blocks() {
    let msg = ion_provider::faux::faux_assistant_message(
        ion_provider::faux::FauxContent::Many(vec![
            ion_provider::faux::faux_text("a"),
            ion_provider::faux::faux_tool_call("t", serde_json::json!({})),
        ]),
        ion_provider::faux::FauxMessageOptions { stop_reason: Some(StopReason::ToolUse), ..Default::default() },
    );
    assert_eq!(msg.content.len(), 2);
    assert_eq!(msg.stop_reason, StopReason::ToolUse);
}

// ── Factory response tests (Task 7) ──

#[tokio::test]
async fn factory_response_receives_context_and_state() {
    let provider = std::sync::Arc::new(FauxProvider::new());

    provider.set_responses(vec![FauxResponseStep::Factory(Box::new(
        |ctx, _opts, state, _model| {
            let n = ctx.messages.len();
            ion_provider::faux::faux_assistant_message(
                ion_provider::faux::FauxContent::Text(format!("call={} msgs={}", state.call_count, n)),
                ion_provider::faux::FauxMessageOptions::default(),
            )
        },
    ))]);

    let reg = registry_with_faux(provider);
    let model = faux_model();
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None, None).await.unwrap();
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
    provider.set_responses(vec![
        FauxResponseStep::Factory(Box::new(|_, _, s, _| {
            ion_provider::faux::faux_assistant_message(
                ion_provider::faux::FauxContent::Text(format!("first-{}", s.call_count)),
                ion_provider::faux::FauxMessageOptions::default(),
            )
        })),
        FauxResponseStep::Factory(Box::new(|_, _, s, _| {
            ion_provider::faux::faux_assistant_message(
                ion_provider::faux::FauxContent::Text(format!("second-{}", s.call_count)),
                ion_provider::faux::FauxMessageOptions::default(),
            )
        })),
    ]);
    let reg = registry_with_faux(provider);
    let model = faux_model();
    let ctx = Context::default();

    let mut texts = vec![];
    for _ in 0..2 {
        let mut es = stream(&reg, &model, &ctx, None, None).await.unwrap();
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

// ── register_faux convenience (Task 8) ──

#[tokio::test]
async fn register_faux_one_liner() {
    use ion_provider::faux::register_faux;
    let mut reg = ApiRegistry::new();
    reg.register_builtins();
    let faux = register_faux(&mut reg);
    faux.set_responses(vec![static_text_step("registered")]);
    assert_eq!(faux.pending_count(), 1);

    let model = faux_model();
    let ctx = Context::default();
    let mut es = stream(&reg, &model, &ctx, None, None).await.unwrap();
    while es.recv().await.is_some() {}
    assert_eq!(faux.call_count(), 1);
}

// ── complete() compatibility + no API key (Task 9) ──

#[tokio::test]
async fn complete_works_through_faux() {
    use ion_provider::registry::complete;

    let mut reg = ApiRegistry::new();
    let faux = ion_provider::faux::register_faux(&mut reg);
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
    // Ensure no relevant API key env vars are set during this test.
    let keys = ["FAUX_API_KEY", "ION_API_KEY", "OPENAI_API_KEY"];
    let saved: Vec<(String, Option<String>)> = keys
        .iter()
        .map(|k| (k.to_string(), std::env::var(k).ok()))
        .collect();
    for k in &keys {
        // SAFETY: tests are single-threaded with respect to env access here.
        unsafe { std::env::remove_var(k) };
    }

    let mut reg = ApiRegistry::new();
    let faux = ion_provider::faux::register_faux(&mut reg);
    faux.set_responses(vec![static_text_step("no key needed")]);
    let model = faux_model();
    let ctx = Context::default();

    let result = stream(&reg, &model, &ctx, None, None).await;
    assert!(result.is_ok(), "faux must not require an API key");

    // Restore env
    for (k, v) in saved {
        if let Some(val) = v {
            // SAFETY: tests are single-threaded with respect to env access here.
            unsafe { std::env::set_var(k, val) };
        }
    }
}

// ── Spec §7 coverage gaps: error path + isolation (Task 11) ──

#[tokio::test]
async fn faux_emits_error_for_stop_reason_error() {
    let mut reg = ApiRegistry::new();
    let faux = ion_provider::faux::register_faux(&mut reg);
    faux.set_responses(vec![FauxResponseStep::Static(ion_provider::faux::faux_assistant_message(
        ion_provider::faux::FauxContent::Text("".into()),
        ion_provider::faux::FauxMessageOptions {
            stop_reason: Some(StopReason::Error),
            error_message: Some("simulated".into()),
        },
    ))]);
    let model = faux_model();
    let ctx = Context::default();

    let mut es = stream(&reg, &model, &ctx, None, None).await.unwrap();
    let mut saw_error = false;
    while let Some(ev) = es.recv().await {
        if let ion_provider::types::StreamEvent::Error { reason, .. } = ev {
            assert_eq!(reason, StopReason::Error);
            saw_error = true;
        }
    }
    assert!(saw_error, "must emit Error event for stop_reason=Error");
}

#[tokio::test]
async fn real_providers_still_dispatch_when_faux_not_routed() {
    let mut reg = ApiRegistry::new();
    reg.register_builtins();
    let faux = ion_provider::faux::register_faux(&mut reg);
    faux.set_responses(vec![static_text_step("faux reply")]);

    // A model with api="openai-completions" should NOT route to faux.
    let real_model = Model {
        id: "test".into(),
        name: "T".into(),
        api: "openai-completions".into(),
        provider: "openai".into(),
        base_url: "http://127.0.0.1:1/invalid".into(), // unreachable
        reasoning: false,
        input: vec!["text".into()],
        cost: ion_provider::types::Cost::default(),
        context_window: 128000,
        max_tokens: 8192,
        compat: None,
        headers: None,
    };
    let ctx = Context::default();
    let result = stream(&reg, &real_model, &ctx, None, None).await;
    assert!(result.is_err(), "real model must not route to faux");
    assert_eq!(faux.call_count(), 0, "faux must not have been called");
}

// ── load_script tests ──

use std::io::Write;

#[test]
fn load_script_parses_text_and_tool_call_lines() {
    let mut tmp = tempfile::NamedTempFile::new().unwrap();
    writeln!(tmp, r#"{{"text":"hello"}}"#).unwrap();
    writeln!(tmp, r#"{{"tool_call":{{"name":"read","input":{{"path":"x"}}}}}}"#).unwrap();
    writeln!(tmp, "# comment line").unwrap();
    writeln!(tmp).unwrap(); // blank line
    writeln!(tmp, r#"{{"thinking":"plan","text":"done"}}"#).unwrap();
    tmp.flush().unwrap();

    let steps = ion_provider::faux::load_script(tmp.path()).unwrap();
    assert_eq!(steps.len(), 3); // comment + blank skipped

    // Verify by queuing them on a registered provider
    let mut reg = ApiRegistry::new();
    let faux = ion_provider::faux::register_faux(&mut reg);
    faux.set_responses(steps);
    assert_eq!(faux.pending_count(), 3);
}

#[test]
fn load_script_rejects_empty_file() {
    let tmp = tempfile::NamedTempFile::new().unwrap();
    let result = ion_provider::faux::load_script(tmp.path());
    assert!(result.is_err());
}

