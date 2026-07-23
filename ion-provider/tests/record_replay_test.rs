use ion_provider::event_stream::EventStream;
use ion_provider::types::{StreamEvent, AssistantMessage, StopReason};

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
    let (src_stream, src_sender) = EventStream::new();
    let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
    let captured_clone = captured.clone();
    let tapped = EventStream::forward_with_done_tap(src_stream, move |msg| {
        if let Some(ion_provider::types::AssistantContentBlock::Text(t)) = msg.content.first() {
            *captured_clone.lock().unwrap() = Some(t.text.clone());
        }
    });

    src_sender.push(StreamEvent::Start { partial: faux_done_message("hello") });
    src_sender.end(faux_done_message("hello"));

    let result = tapped.result().await.unwrap();
    assert_eq!(result.stop_reason, StopReason::Stop);
    // Yield a few times so the spawned task runs on_done
    for _ in 0..5 { tokio::task::yield_now().await; }
    assert_eq!(*captured.lock().unwrap(), Some("hello".to_string()));
}

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

use ion_provider::replay::{validate_recording_id, recording_trace_path};

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
}

#[test]
fn validate_recording_id_rejects_too_long() {
    assert!(validate_recording_id(&"x".repeat(81)).is_err());
}

#[test]
fn recording_trace_path_rejects_traversal() {
    assert!(recording_trace_path("../../etc/passwd").is_err());
    assert!(recording_trace_path("a/b").is_err());
    assert!(recording_trace_path("valid-id").is_ok());
}

use ion_provider::record::RecordingProvider;
use ion_provider::faux::{FauxProvider, FauxResponseStep, faux_assistant_message, FauxContent, FauxMessageOptions};
use ion_provider::registry::{ApiRegistry, ApiProvider, stream};
use std::sync::Arc;
use tempfile::tempdir;

fn faux_model_with_api(api: &str) -> ion_provider::types::Model {
    ion_provider::types::Model {
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

    let inner_faux = Arc::new(FauxProvider::new());
    inner_faux.set_responses(vec![FauxResponseStep::Static(
        faux_assistant_message(FauxContent::Text("recorded hello".into()), FauxMessageOptions::default())
    )]);

    struct InnerHandle(Arc<FauxProvider>);
    #[async_trait::async_trait]
    impl ApiProvider for InnerHandle {
        async fn stream(&self, model: &ion_provider::types::Model, ctx: &ion_provider::types::Context, opts: Option<&ion_provider::types::StreamOptions>, cancel: Option<tokio_util::sync::CancellationToken>) -> ion_provider::error::ProviderResult<ion_provider::event_stream::EventStream> {
            self.0.stream(model, ctx, opts, cancel).await
        }
    }

    let recording = RecordingProvider::new(
        Box::new(InnerHandle(inner_faux)),
        trace_path.clone(),
        meta_path.clone(),
    );

    let mut reg = ApiRegistry::new();
    reg.register("test-rec", Box::new(recording));
    let model = faux_model_with_api("test-rec");
    let ctx = ion_provider::types::Context::default();

    let mut es = stream(&reg, &model, &ctx, None, None).await.unwrap();
    while es.recv().await.is_some() {}

    let trace = std::fs::read_to_string(&trace_path).unwrap();
    assert_eq!(trace.lines().count(), 1);
    assert!(trace.contains("recorded hello"));
    assert!(trace.contains("request_hash"), "trace line includes request_hash");

    let meta = std::fs::read_to_string(&meta_path).unwrap();
    assert!(meta.contains("\"response_count\": 1"));
    assert!(meta.contains("\"schema_version\": 1"));
}

#[tokio::test]
async fn replay_provider_round_trips_via_load_script() {
    // Write a fake trace, load via load_script (same mechanism ReplayProvider uses)
    let tmp = tempdir().unwrap();
    let trace_path = tmp.path().join("my-rec").join("trace.jsonl");
    std::fs::create_dir_all(trace_path.parent().unwrap()).unwrap();
    std::fs::write(&trace_path, "{\"text\":\"replayed hi\"}\n").unwrap();

    let steps = ion_provider::faux::load_script(&trace_path).unwrap();
    assert_eq!(steps.len(), 1);

    let faux = Arc::new(FauxProvider::new());
    faux.set_responses(steps);
    struct H(Arc<FauxProvider>);
    #[async_trait::async_trait]
    impl ApiProvider for H {
        async fn stream(&self, m: &ion_provider::types::Model, c: &ion_provider::types::Context, o: Option<&ion_provider::types::StreamOptions>, cancel: Option<tokio_util::sync::CancellationToken>) -> ion_provider::error::ProviderResult<ion_provider::event_stream::EventStream> {
            self.0.stream(m, c, o, cancel).await
        }
    }
    let mut reg = ApiRegistry::new();
    reg.register("test", Box::new(H(faux)));
    let model = faux_model_with_api("test");
    let ctx = ion_provider::types::Context::default();
    let mut es = stream(&reg, &model, &ctx, None, None).await.unwrap();
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

use ion_provider::replay::acquire_recording_lock;

#[test]
fn acquire_lock_succeeds_for_new_recording() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("new-rec");
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
    // DON'T drop _lock1 — keep it held
    let lock2 = acquire_recording_lock(&rec_dir, false);
    assert!(lock2.is_err(), "second acquire without OVERWRITE must fail");
}

#[test]
fn acquire_lock_overwrite_clears_existing() {
    let tmp = tempdir().unwrap();
    let rec_dir = tmp.path().join("ow-rec");
    std::fs::create_dir_all(&rec_dir).unwrap();
    {
        let _lock1 = acquire_recording_lock(&rec_dir, false).unwrap();
    } // lock1 dropped, .lock file removed
    // Simulate stale trace
    std::fs::write(rec_dir.join("trace.jsonl"), "stale").unwrap();
    let lock2 = acquire_recording_lock(&rec_dir, true).unwrap();
    assert!(lock2.is_some());
    // trace should be cleared
    assert!(!rec_dir.join("trace.jsonl").exists(), "overwrite must clear stale trace");
}
