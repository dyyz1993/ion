//! Tier 降级 harness — LLM 永久性错误自动降级 tier_models。
//!
//! 背景（容错调研 P2）：401/402/403/配额类永久错误只发 error 事件，worker 空转
//! 等死（无人值守场景整晚挂）。修复语义：错误分类为永久时（永久错误不重试当前
//! 模型，即时判定 = 该模型的"重试耗尽"），若有与当前不同的可用 tier 档，自动
//! set_model 降级 + 发 ModelFallback 事件 + 继续当前任务；无可用档保持现状报死。
//!
//! 🔴 不与 ION_LLM_MAX_RETRIES/retry_budget 打架：降级只对**永久性**错误触发
//! （瞬时错误仍走既有重试通路，不提前降级）——见 transient_error_does_not_fallback。

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::error::AgentResult;
use ion::agent::extension::{Extension, ExtensionRunner};
use ion::agent::tool::ToolRegistry;
use ion_provider::error::ProviderResult;
use ion_provider::event_stream::EventStream;
use ion_provider::registry::{ApiProvider, ApiRegistry};
use ion_provider::types::*;

fn model_with_id(id: &str) -> Model {
    Model {
        id: id.into(),
        name: id.into(),
        api: "scripted".into(),
        provider: "scripted".into(),
        base_url: "".into(),
        reasoning: false,
        input: vec!["text".into()],
        cost: Cost::default(),
        context_window: 128_000,
        max_tokens: 4096,
        compat: None,
        headers: None,
    }
}

/// 脚本化 provider：前 `fail_first` 次调用返回 `error`（Err → 走 agent 重试/降级
/// 通路），之后每次返回文本 "recovered"。调用计数跨模型累计（不因 set_model 清零）。
struct ScriptedErrorProvider {
    calls: AtomicUsize,
    fail_first: usize,
    error: String,
}

impl ScriptedErrorProvider {
    fn new(fail_first: usize, error: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            fail_first,
            error: error.into(),
        })
    }
}

#[async_trait]
impl ApiProvider for ScriptedErrorProvider {
    async fn stream(
        &self,
        model: &Model,
        _context: &Context,
        _options: Option<&StreamOptions>,
        _cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ProviderResult<EventStream> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n < self.fail_first {
            return Err(ion_provider::error::ProviderError::Stream(
                self.error.clone(),
            ));
        }
        let msg = AssistantMessage {
            role: "assistant".into(),
            content: vec![AssistantContentBlock::Text(TextContent {
                text: format!("recovered on call {} (model={})", n + 1, model.id),
                text_signature: None,
            })],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: 0,
        };
        let (event_stream, sender) = EventStream::new();
        let text = format!("recovered on call {} (model={})", n + 1, model.id);
        tokio::spawn(async move {
            // 真实 provider 形状：Start → TextStart → TextDelta×N → TextEnd → Done
            //（inner_loop 从 TextDelta 事件收集文本，不是从 Done 的 message）
            sender.push(StreamEvent::Start {
                partial: msg.clone(),
            });
            sender.push(StreamEvent::TextStart {
                content_index: 0,
                partial: msg.clone(),
            });
            sender.push(StreamEvent::TextDelta {
                content_index: 0,
                delta: text.clone(),
                partial: msg.clone(),
            });
            sender.push(StreamEvent::TextEnd {
                content_index: 0,
                content: text.clone(),
                partial: msg.clone(),
            });
            sender.end(msg);
        });
        Ok(event_stream)
    }
}

/// Arc 共享 provider 的薄包装（ApiRegistry 需要 owned Box）
struct ProviderHandle(Arc<ScriptedErrorProvider>);

#[async_trait]
impl ApiProvider for ProviderHandle {
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

/// 捕获 ModelFallback 事件的扩展
#[derive(Default)]
struct FallbackCapture {
    events: Mutex<Vec<(String, String, String)>>, // (from, to, reason)
}

impl FallbackCapture {
    fn shared(self) -> (SharedExt, Arc<FallbackCapture>) {
        let arc = Arc::new(self);
        (SharedExt(arc.clone()), arc)
    }
}

#[async_trait]
impl Extension for FallbackCapture {
    fn name(&self) -> &str {
        "fallback-capture"
    }
    async fn on_model_fallback(&self, from: &str, to: &str, reason: &str) -> AgentResult<()> {
        self.events
            .lock()
            .unwrap()
            .push((from.to_string(), to.to_string(), reason.to_string()));
        Ok(())
    }
}

/// Arc 共享扩展的薄包装（ExtensionRunner 需要 owned Box）
struct SharedExt(Arc<FallbackCapture>);

#[async_trait]
impl Extension for SharedExt {
    fn name(&self) -> &str {
        "fallback-shared"
    }
    async fn on_model_fallback(&self, from: &str, to: &str, reason: &str) -> AgentResult<()> {
        self.0.on_model_fallback(from, to, reason).await
    }
}

#[tokio::test]
async fn permanent_error_falls_back_and_task_continues() {
    let mut registry = ApiRegistry::new();
    let provider = ScriptedErrorProvider::new(1, "HTTP error: 401 Unauthorized (scripted)");
    registry.register("scripted", Box::new(ProviderHandle(provider)));

    let (ext, capture) = FallbackCapture::default().shared();
    let mut extensions = ExtensionRunner::new();
    extensions.register(Box::new(ext));

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 3,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        fallback_models: vec![model_with_id("backup-tier")],
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        model_with_id("primary-tier"),
        None,
        ToolRegistry::new(),
        config,
    )
    .with_extensions(extensions);

    agent
        .run("do the task")
        .await
        .expect("降级后任务应继续并成功");

    // 模型已切换
    assert_eq!(
        agent.model().id,
        "backup-tier",
        "永久错误后应 set_model 降级"
    );
    // 任务继续：拿到真实补全
    let has_answer = agent.messages().iter().any(|m| {
        matches!(
            m,
            ion::agent::messages::Message::Assistant(a)
                if a.content.iter().any(|c| matches!(c,
                    AssistantContentBlock::Text(t) if t.text.contains("recovered")))
        )
    });
    assert!(has_answer, "降级后应换得真实补全继续任务");
    // ModelFallback 事件：from/to/reason 齐全
    let events = capture.events.lock().unwrap();
    assert_eq!(events.len(), 1, "应恰好发一次 ModelFallback 事件");
    let (from, to, reason) = &events[0];
    assert_eq!(from, "scripted/primary-tier");
    assert_eq!(to, "scripted/backup-tier");
    assert!(reason.contains("401"), "reason 应带原始错误: {reason}");
}

#[tokio::test]
async fn permanent_error_without_fallback_still_fails() {
    let mut registry = ApiRegistry::new();
    let provider = ScriptedErrorProvider::new(10, "HTTP error: 402 Payment Required (scripted)");
    registry.register("scripted", Box::new(ProviderHandle(provider)));

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 1,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        fallback_models: vec![], // 无可用档
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        model_with_id("primary-tier"),
        None,
        ToolRegistry::new(),
        config,
    );

    let result = agent.run("do the task").await;
    assert!(result.is_err(), "无可用档应保持现状报死");
    assert_eq!(agent.model().id, "primary-tier", "模型不应被切换");
}

#[tokio::test]
async fn transient_error_does_not_fallback() {
    // 🔴 不提前降级：瞬时错误（timeout）走既有重试通路，同一模型重试成功，
    // 绝不因瞬时错误切换模型 / 发事件
    let mut registry = ApiRegistry::new();
    let provider = ScriptedErrorProvider::new(1, "request timeout (scripted transient)");
    registry.register("scripted", Box::new(ProviderHandle(provider)));

    let (ext, capture) = FallbackCapture::default().shared();
    let mut extensions = ExtensionRunner::new();
    extensions.register(Box::new(ext));

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 3,
        retry_base_delay_ms: 1,
        retry_on_no_tool_use: 0,
        fallback_models: vec![model_with_id("backup-tier")],
        ..Default::default()
    };
    let mut agent = Agent::new(
        Arc::new(registry),
        model_with_id("primary-tier"),
        None,
        ToolRegistry::new(),
        config,
    )
    .with_extensions(extensions);

    agent.run("do the task").await.expect("瞬时错误重试应成功");
    assert_eq!(agent.model().id, "primary-tier", "瞬时错误不应触发降级");
    assert!(
        capture.events.lock().unwrap().is_empty(),
        "瞬时错误不应发 ModelFallback 事件"
    );
}
