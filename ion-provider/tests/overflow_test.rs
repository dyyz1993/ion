//! Context overflow boundary tests.
//!
//! These tests construct Contexts whose total token count sits at, above, or
//! far above the model's `context_window` and verify that the streaming layer
//! never panics. They use the FauxProvider (an offline, deterministic mock)
//! registered under the "faux" API key, so no network or API key is required.
//!
//! Token counting convention used here is the standard "4 characters ≈ 1 token"
//! heuristic. The goal is NOT to validate exact token math — it is to exercise
//! the boundary behavior of Context construction and streaming without panic.
//!
//! Coverage:
//!   1. Context exactly at context_window limit  → should stream successfully.
//!   2. Context 1 token over limit               → must not panic.
//!   3. Empty context                            → verify no crash.
//!   4. Single message larger than context_window→ verify behavior.
//!   5. Many small messages totaling > window    → verify behavior.

use ion_provider::faux::{
    faux_assistant_message, FauxContent, FauxMessageOptions, FauxProvider, FauxResponseStep,
    register_faux,
};
use ion_provider::registry::{stream, ApiRegistry};
use ion_provider::types::{
    ContentBlock, Context, Cost, Message, MessageSource, Model, StreamEvent, TextContent,
    UserMessage,
};

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

/// Build a Model whose context_window is the caller-supplied value.
/// Uses the "faux" API so routing stays offline.
fn model_with_window(context_window: u64) -> Model {
    Model {
        id: "faux-1".into(),
        name: "Faux Overflow".into(),
        api: "faux".into(),
        provider: "faux".into(),
        base_url: String::new(),
        reasoning: false,
        input: vec!["text".into()],
        cost: Cost::default(),
        context_window,
        max_tokens: 8192,
        compat: None,
        headers: None,
    }
}

/// Build a single user Message whose text content is `chars` characters long.
/// This gives precise control over the resulting (approximate) token count.
fn user_message_with_chars(chars: usize) -> Message {
    // Repeat 'a' so the string is ASCII and char count == byte count.
    let text = "a".repeat(chars);
    Message::User(UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent {
            text,
            text_signature: None,
        })],
        timestamp: 0,
        source: MessageSource::Prompt,
    })
}

/// 4 characters ≈ 1 token (standard heuristic).
fn tokens_for_chars(chars: usize) -> u64 {
    (chars / 4) as u64
}

/// Register faux and queue a single static "ok" reply so every stream call
/// succeeds deterministically regardless of the input context.
fn faux_registry_with_ok_reply() -> (ApiRegistry, std::sync::Arc<FauxProvider>) {
    let mut reg = ApiRegistry::new();
    let faux = register_faux(&mut reg);
    faux.set_responses(vec![FauxResponseStep::Static(faux_assistant_message(
        FauxContent::Text("ok".into()),
        FauxMessageOptions::default(),
    ))]);
    (reg, faux)
}

/// Drain an EventStream to completion. Returns true if a `Done` event was seen.
async fn drain_to_done(es: &mut ion_provider::EventStream) -> bool {
    let mut saw_done = false;
    while let Some(ev) = es.recv().await {
        if matches!(ev, StreamEvent::Done { .. }) {
            saw_done = true;
        }
    }
    saw_done
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

/// 1. Context exactly at the context_window limit must stream successfully.
#[tokio::test]
async fn context_exactly_at_limit_works() {
    let model = model_with_window(128_000);

    // Build content whose token count == context_window exactly.
    let target_tokens = model.context_window;
    let chars = (target_tokens * 4) as usize;
    let msg = user_message_with_chars(chars);
    assert_eq!(tokens_for_chars(chars), target_tokens);

    let ctx = Context::new(None, vec![msg]);

    let (reg, _faux) = faux_registry_with_ok_reply();
    let mut es = stream(&reg, &model, &ctx, None, None)
        .await
        .expect("stream at exact limit must succeed");

    assert!(drain_to_done(&mut es).await, "expected a Done event");
}

/// 2. Context 1 token over the limit must be handled without panic.
#[tokio::test]
async fn context_one_token_over_limit_no_panic() {
    let model = model_with_window(128_000);

    // target_tokens = context_window + 1 → one token over the boundary.
    let target_tokens = model.context_window + 1;
    let chars = (target_tokens * 4) as usize;
    let msg = user_message_with_chars(chars);
    assert_eq!(tokens_for_chars(chars), target_tokens);

    let ctx = Context::new(None, vec![msg]);

    let (reg, _faux) = faux_registry_with_ok_reply();

    // The contract under test is strictly "no panic". Spawn the streaming
    // work onto a task and join it; if the task panicked, .unwrap() surfaces it.
    let handle = tokio::spawn(async move {
        let mut es = stream(&reg, &model, &ctx, None, None)
            .await
            .expect("stream one-over must not error in faux");
        while es.recv().await.is_some() {}
    });

    handle
        .await
        .expect("stream task must not panic for context 1 token over limit");
}

/// 3. Empty context must not crash.
#[tokio::test]
async fn empty_context_no_crash() {
    let model = model_with_window(128_000);
    let ctx = Context::default(); // no system prompt, no messages, no tools

    assert!(ctx.messages.is_empty());
    assert!(ctx.system_prompt.is_none());

    let (reg, _faux) = faux_registry_with_ok_reply();
    let mut es = stream(&reg, &model, &ctx, None, None)
        .await
        .expect("empty context must stream successfully");

    assert!(
        drain_to_done(&mut es).await,
        "empty context must produce a Done event"
    );
}

/// 4. A single message larger than the entire context_window must not panic.
#[tokio::test]
async fn single_message_larger_than_window_no_panic() {
    // Use a deliberately tiny window so the oversized payload is obvious.
    let model = model_with_window(1_000);

    // Build a single message whose token count is 10x the window.
    let target_tokens = model.context_window * 10;
    let chars = (target_tokens * 4) as usize;
    let msg = user_message_with_chars(chars);
    assert!(tokens_for_chars(chars) > model.context_window);

    let ctx = Context::new(None, vec![msg]);

    let (reg, _faux) = faux_registry_with_ok_reply();

    let handle = tokio::spawn(async move {
        let mut es = stream(&reg, &model, &ctx, None, None)
            .await
            .expect("oversized single message must not error in faux");
        while es.recv().await.is_some() {}
    });

    handle
        .await
        .expect("stream task must not panic for oversized single message");
}

/// 5. Many small messages whose combined total exceeds context_window must
///    not panic.
#[tokio::test]
async fn many_small_messages_exceeding_window_no_panic() {
    let model = model_with_window(1_000);

    // Each small message is 5 tokens (20 chars). We add enough messages that
    // the cumulative total comfortably exceeds the window.
    let per_msg_chars: usize = 20;
    let per_msg_tokens = tokens_for_chars(per_msg_chars);
    assert_eq!(per_msg_tokens, 5);

    let msg_count = (model.context_window / per_msg_tokens * 2) as usize; // 2x window
    let messages: Vec<Message> = (0..msg_count)
        .map(|_| user_message_with_chars(per_msg_chars))
        .collect();

    let total_tokens = per_msg_tokens * msg_count as u64;
    assert!(
        total_tokens > model.context_window,
        "cumulative tokens ({total_tokens}) must exceed window ({})",
        model.context_window
    );

    let ctx = Context::new(None, messages);

    let (reg, _faux) = faux_registry_with_ok_reply();

    let handle = tokio::spawn(async move {
        let mut es = stream(&reg, &model, &ctx, None, None)
            .await
            .expect("many-small-messages overflow must not error in faux");
        while es.recv().await.is_some() {}
    });

    handle
        .await
        .expect("stream task must not panic for many-small-messages overflow");
}
