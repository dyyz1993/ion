//! E2E 真实 API 烟测 — 需要环境变量配置才会运行
//!
//! 运行方式：
//!   ION_E2E_ANTHROPIC=1 cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture
//!   ION_E2E_OPENAI=1     cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture
//!   ION_E2E_MISTRAL=1    cargo test -p ion-provider --test e2e_real_api -- --ignored --nocapture
//!
//! 环境变量：
//!   ION_ANTHROPIC_BASE_URL  — Anthropic 兼容端点（默认 z.ai 代理）
//!   ION_ANTHROPIC_API_KEY   — API key
//!   ION_ANTHROPIC_MODEL     — 模型 id（默认 glm-4.6）
//!   ION_OPENAI_BASE_URL     — OpenAI 兼容端点（默认 opencode）
//!   ION_OPENAI_API_KEY      — API key
//!   ION_OPENAI_MODEL        — 模型 id（默认 deepseek-v4-flash）
//!   ION_MISTRAL_API_KEY     — Mistral API key（MISTRAL_API_KEY 亦可）
//!   ION_MISTRAL_MODEL       — 模型 id（默认 mistral-large-latest）

use ion_provider::event_stream::EventStream;
use ion_provider::provider::anthropic::AnthropicMessagesProvider;
use ion_provider::provider::mistral::MistralProvider;
use ion_provider::provider::openai::OpenAICompletionsProvider;
use ion_provider::registry::ApiProvider;
use ion_provider::types::*;

fn anthropic_conf() -> Option<(String, String, String)> {
    if std::env::var("ION_E2E_ANTHROPIC").is_err() { return None; }
    let base = std::env::var("ION_ANTHROPIC_BASE_URL")
        .unwrap_or_else(|_| "https://p.19930810.xyz:8443/k/glm/https://api.z.ai/api/anthropic".into());
    let key = std::env::var("ION_ANTHROPIC_API_KEY")
        .unwrap_or_else(|_| std::env::var("ION_API_KEY").unwrap_or_default());
    let model = std::env::var("ION_ANTHROPIC_MODEL")
        .unwrap_or_else(|_| "glm-4.6".into());
    if key.is_empty() { return None; }
    Some((base, key, model))
}

fn openai_conf() -> Option<(String, String, String)> {
    if std::env::var("ION_E2E_OPENAI").is_err() { return None; }
    let base = std::env::var("ION_OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://opencode.ai/zen/go/v1".into());
    let key = std::env::var("ION_OPENAI_API_KEY")
        .unwrap_or_else(|_| std::env::var("ION_API_KEY").unwrap_or_default());
    let model = std::env::var("ION_OPENAI_MODEL")
        .unwrap_or_else(|_| "deepseek-v4-flash".into());
    if key.is_empty() { return None; }
    Some((base, key, model))
}

fn make_user(text: &str) -> Message {
    Message::User(UserMessage {
        role: "user".into(),
        content: vec![ContentBlock::Text(TextContent { text: text.into(), text_signature: None })],
        timestamp: 0,
        source: MessageSource::Prompt,
    })
}

async fn drain_stream(mut stream: EventStream, label: &str) -> AssistantMessage {
    let mut text_parts: Vec<String> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut had_start = false;
    let mut had_done = false;
    let mut event_count = 0u32;

    while let Some(event) = stream.recv().await {
        event_count += 1;
        println!("[{label}] event #{event_count}: {}", event_name(&event));
        match event {
            StreamEvent::Start { .. } => { had_start = true; println!("[{label}] Start"); }
            StreamEvent::TextStart { .. } => println!("[{label}] TextStart"),
            StreamEvent::TextDelta { delta, .. } => {
                print!("{delta}");
                use std::io::Write;
                let _ = std::io::stdout().flush();
                text_parts.push(delta);
            }
            StreamEvent::TextEnd { content, .. } => println!("\n[{label}] TextEnd ({} chars)", content.len()),
            StreamEvent::ThinkingStart { .. } => println!("[{label}] ThinkingStart"),
            StreamEvent::ThinkingDelta { delta, .. } => {
                thinking_parts.push(delta);
            }
            StreamEvent::ThinkingEnd { content, .. } => println!("[{label}] ThinkingEnd ({} chars)", content.len()),
            StreamEvent::ToolCallStart { .. } => println!("[{label}] ToolCallStart"),
            StreamEvent::ToolCallDelta { delta, .. } => println!("[{label}] ToolCallDelta: {delta}"),
            StreamEvent::ToolCallEnd { tool_call, .. } => {
                println!("[{label}] ToolCallEnd: {}({})", tool_call.name, tool_call.arguments);
                tool_calls.push(tool_call);
            }
            StreamEvent::Done { message, .. } => {
                                println!("[{label}] Done: stop_reason={:?}, text_len={}, thinking_len={}, tool_calls={}",
                    message.stop_reason,
                    text_parts.iter().map(|s| s.len()).sum::<usize>(),
                    thinking_parts.iter().map(|s| s.len()).sum::<usize>(),
                    tool_calls.len());
                return message;
            }
            StreamEvent::Error { reason, message } => {
                println!("[{label}] Error: reason={:?}, msg={:?}", reason, message.error_message);
                return message;
            }
        }
    }

    panic!("[{label}] stream ended without Done event (had_start={had_start}, had_done={had_done})");
}

// ──────────────────────────────────────────────────────────────
// Anthropic 烟测
// ──────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn anthropic_basic_stream() {
    let Some((base, key, model_id)) = anthropic_conf() else {
        eprintln!("跳过：未设置 ION_E2E_ANTHROPIC");
        return;
    };
    let model = Model {
        id: model_id.clone(), name: model_id.clone(),
        api: "anthropic-messages".into(), provider: "anthropic".into(),
        base_url: base, reasoning: false,
        input: vec!["text".into()],
        cost: Cost::default(), context_window: 128000, max_tokens: 100,
        compat: None, headers: None,
    };
    let ctx = Context::new(None, vec![make_user("说一句话，不超过 20 字")]);
    let opts = StreamOptions {
        api_key: Some(key),
        max_tokens: Some(100),
        reasoning: None, timeout_ms: Some(30000),
        max_retries: Some(1), response_format: None,
    };

    let provider = AnthropicMessagesProvider;
    let stream = provider.stream(&model, &ctx, Some(&opts), None).await
        .expect("stream 创建失败");
    let msg = drain_stream(stream, "anthropic-basic").await;

    assert!(msg.stop_reason == StopReason::Stop, "stop_reason 应该是 Stop，实际 {:?}", msg.stop_reason);
    let text: String = msg.content.iter().filter_map(|b| match b {
        AssistantContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }).collect();
    assert!(!text.is_empty(), "应该有文本输出");
    println!("✓ anthropic_basic_stream 通过");
}

#[tokio::test]
#[ignore]
async fn anthropic_tool_call() {
    let Some((base, key, model_id)) = anthropic_conf() else {
        eprintln!("跳过：未设置 ION_E2E_ANTHROPIC");
        return;
    };
    let model = Model {
        id: model_id.clone(), name: model_id.clone(),
        api: "anthropic-messages".into(), provider: "anthropic".into(),
        base_url: base, reasoning: false,
        input: vec!["text".into()],
        cost: Cost::default(), context_window: 128000, max_tokens: 300,
        compat: None, headers: None,
    };
    let ctx = Context {
        system_prompt: Some("你是天气助手，必须调用 get_weather 工具查询天气".into()),
        messages: vec![make_user("北京今天天气怎么样？")],
        tools: Some(vec![ToolDef {
            name: "get_weather".into(),
            description: "查询指定城市的天气".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string", "description": "城市名" }
                },
                "required": ["location"]
            }),
        }]),
    };
    let opts = StreamOptions {
        api_key: Some(key), max_tokens: Some(300),
        reasoning: None, timeout_ms: Some(30000),
        max_retries: Some(1), response_format: None,
    };

    let provider = AnthropicMessagesProvider;
    let stream = provider.stream(&model, &ctx, Some(&opts), None).await
        .expect("stream 创建失败");
    let msg = drain_stream(stream, "anthropic-tool").await;

    let has_tool_call = msg.content.iter().any(|b| matches!(b, AssistantContentBlock::ToolCall(_)));
    assert!(has_tool_call, "应该有 tool_call，content: {:?}", msg.content);
    println!("✓ anthropic_tool_call 通过");
}

// ──────────────────────────────────────────────────────────────
// OpenAI Completions 烟测（reasoning_content + tool_calls）
// ──────────────────────────────────────────────────────────────

#[tokio::test]
#[ignore]
async fn openai_reasoning_stream() {
    let Some((base, key, model_id)) = openai_conf() else {
        eprintln!("跳过：未设置 ION_E2E_OPENAI");
        return;
    };
    let model = Model {
        id: model_id.clone(), name: model_id.clone(),
        api: "openai-completions".into(), provider: "opencode".into(),
        base_url: base, reasoning: true,
        input: vec!["text".into()],
        cost: Cost::default(), context_window: 128000, max_tokens: 200,
        compat: Some(CompatConfig::OpenAICompletions(OpenAICompletionsCompat {
            max_tokens_field: Some("max_tokens".into()),
            requires_reasoning_content_on_assistant_messages: Some(true),
            ..Default::default()
        })),
        headers: None,
    };
    let ctx = Context::new(None, vec![make_user("2+3 等于几？只回答数字")]);
    let opts = StreamOptions {
        api_key: Some(key), max_tokens: Some(200),
        reasoning: Some(ThinkingLevel::Medium),
        timeout_ms: Some(30000),
        max_retries: Some(1), response_format: None,
    };

    let provider = OpenAICompletionsProvider;
    let stream = provider.stream(&model, &ctx, Some(&opts), None).await
        .expect("stream 创建失败");
    let msg = drain_stream(stream, "openai-reasoning").await;

    // deepseek-v4-flash 应该输出 reasoning_content
    let has_thinking = msg.content.iter().any(|b| matches!(b, AssistantContentBlock::Thinking(_)));
    let has_text = msg.content.iter().any(|b| matches!(b, AssistantContentBlock::Text(_)));
    assert!(has_text, "应该有文本输出");
    if has_thinking {
        println!("✓ 检测到 reasoning_content（thinking block）");
    } else {
        println!("⚠ 未检测到 reasoning_content（可能模型未触发思考）");
    }
    println!("✓ openai_reasoning_stream 通过");
}

#[tokio::test]
#[ignore]
async fn openai_tool_call() {
    let Some((base, key, model_id)) = openai_conf() else {
        eprintln!("跳过：未设置 ION_E2E_OPENAI");
        return;
    };
    let model = Model {
        id: model_id.clone(), name: model_id.clone(),
        api: "openai-completions".into(), provider: "opencode".into(),
        base_url: base, reasoning: true,
        input: vec!["text".into()],
        cost: Cost::default(), context_window: 128000, max_tokens: 500,
        compat: Some(CompatConfig::OpenAICompletions(OpenAICompletionsCompat {
            max_tokens_field: Some("max_tokens".into()),
            requires_reasoning_content_on_assistant_messages: Some(true),
            ..Default::default()
        })),
        headers: None,
    };
    let ctx = Context {
        system_prompt: Some("你是天气助手，必须调用 get_weather 工具查询天气".into()),
        messages: vec![make_user("北京今天天气怎么样？")],
        tools: Some(vec![ToolDef {
            name: "get_weather".into(),
            description: "查询指定城市的天气".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string", "description": "城市名" }
                },
                "required": ["location"]
            }),
        }]),
    };
    let opts = StreamOptions {
        api_key: Some(key), max_tokens: Some(500),
        reasoning: Some(ThinkingLevel::Medium),
        timeout_ms: Some(30000),
        max_retries: Some(1), response_format: None,
    };

    let provider = OpenAICompletionsProvider;
    let stream = provider.stream(&model, &ctx, Some(&opts), None).await
        .expect("stream 创建失败");
    let msg = drain_stream(stream, "openai-tool").await;

    let has_tool_call = msg.content.iter().any(|b| matches!(b, AssistantContentBlock::ToolCall(_)));
    assert!(has_tool_call, "应该有 tool_call");
    println!("✓ openai_tool_call 通过");
}

// ──────────────────────────────────────────────────────────────
// Mistral Conversations 烟测（mistral-conversations 协议）
//
// 差异验证点：
//   - delta.content 字符串形态（非思考模型）/ 数组形态（思考模型）
//   - tool_call 格式（id/name/arguments 累积）
//   - system role 直传
//   - tool result 带 name 字段
//
// 可用任何 OpenAI 兼容端点做 smoke test（设 ION_MISTRAL_BASE_URL/MODEL）。
// 真正的 Mistral API（mistral-large-latest 等非思考模型）走字符串 content 分支；
// 思考型模型（如 DeepSeek reasoning）可能走 reasoning 分支。
// ──────────────────────────────────────────────────────────────

fn mistral_conf() -> Option<(String, String)> {
    if std::env::var("ION_E2E_MISTRAL").is_err() { return None; }
    // base 默认官方端点；model 默认 mistral-large-latest（非思考模型，最稳定）
    let base = std::env::var("ION_MISTRAL_BASE_URL")
        .unwrap_or_else(|_| "https://api.mistral.ai/v1".into());
    let key = std::env::var("ION_MISTRAL_API_KEY")
        .or_else(|_| std::env::var("MISTRAL_API_KEY"))
        .unwrap_or_default();
    if key.is_empty() { return None; }
    Some((base, key))
}

fn mistral_model_id() -> String {
    std::env::var("ION_MISTRAL_MODEL")
        .unwrap_or_else(|_| "mistral-large-latest".into())
}

/// 是否为 reasoning 模型。可用 ION_MISTRAL_REASONING=1 强制开启
/// （用 DeepSeek 等 reasoning 模型做 smoke test 时需要）。
fn mistral_reasoning() -> bool {
    std::env::var("ION_MISTRAL_REASONING")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[tokio::test]
#[ignore]
async fn mistral_basic_stream() {
    let Some((base, key)) = mistral_conf() else {
        eprintln!("跳过：未设置 ION_E2E_MISTRAL");
        return;
    };
    let model_id = mistral_model_id();
    let reasoning = mistral_reasoning();
    let model = Model {
        id: model_id.clone(), name: model_id.clone(),
        api: "mistral-conversations".into(), provider: "mistral".into(),
        base_url: base, reasoning,
        input: vec!["text".into()],
        cost: Cost::default(), context_window: 128000, max_tokens: 2000,
        compat: None, headers: None,
    };
    let ctx = Context::new(
        Some("你是一个简洁的助手".into()),
        vec![make_user("说一句话，不超过 20 字")],
    );
    let opts = StreamOptions {
        api_key: Some(key), max_tokens: Some(2000),
        reasoning: if reasoning { Some(ThinkingLevel::Medium) } else { None },
        timeout_ms: Some(30000),
        max_retries: Some(1), response_format: None,
    };

    let provider = MistralProvider;
    let stream = provider.stream(&model, &ctx, Some(&opts), None).await
        .expect("stream 创建失败");
    let msg = drain_stream(stream, "mistral-basic").await;

    // 非思考模型应返回 Stop；思考模型可能因 reasoning 占用预算返回 Length，
    // 但只要不是 Error/Aborted 且有文本就算通过。
    assert!(
        matches!(msg.stop_reason, StopReason::Stop | StopReason::Length),
        "stop_reason 应该是 Stop/Length，实际 {:?}", msg.stop_reason,
    );
    let text: String = msg.content.iter().filter_map(|b| match b {
        AssistantContentBlock::Text(t) => Some(t.text.clone()),
        _ => None,
    }).collect();
    assert!(!text.is_empty(), "应该有文本输出，content: {:?}", msg.content);
    println!("✓ mistral_basic_stream 通过（输出: {text}）");
}

#[tokio::test]
#[ignore]
async fn mistral_tool_call() {
    let Some((base, key)) = mistral_conf() else {
        eprintln!("跳过：未设置 ION_E2E_MISTRAL");
        return;
    };
    let model_id = mistral_model_id();
    let reasoning = mistral_reasoning();
    let model = Model {
        id: model_id.clone(), name: model_id.clone(),
        api: "mistral-conversations".into(), provider: "mistral".into(),
        base_url: base, reasoning,
        input: vec!["text".into()],
        cost: Cost::default(), context_window: 128000, max_tokens: 4000,
        compat: None, headers: None,
    };
    let ctx = Context {
        system_prompt: Some("你是天气助手，必须调用 get_weather 工具查询天气".into()),
        messages: vec![make_user("北京今天天气怎么样？")],
        tools: Some(vec![ToolDef {
            name: "get_weather".into(),
            description: "查询指定城市的天气".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "location": { "type": "string", "description": "城市名" }
                },
                "required": ["location"]
            }),
        }]),
    };
    let opts = StreamOptions {
        api_key: Some(key), max_tokens: Some(4000),
        reasoning: if reasoning { Some(ThinkingLevel::Medium) } else { None },
        timeout_ms: Some(30000),
        max_retries: Some(1), response_format: None,
    };

    let provider = MistralProvider;
    let stream = provider.stream(&model, &ctx, Some(&opts), None).await
        .expect("stream 创建失败");
    let msg = drain_stream(stream, "mistral-tool").await;

    let has_tool_call = msg.content.iter().any(|b| matches!(b, AssistantContentBlock::ToolCall(_)));
    assert!(has_tool_call, "应该有 tool_call，content: {:?}", msg.content);
    println!("✓ mistral_tool_call 通过");
}

fn event_name(e: &StreamEvent) -> &'static str {
    match e {
        StreamEvent::Start { .. } => "Start",
        StreamEvent::TextStart { .. } => "TextStart",
        StreamEvent::TextDelta { .. } => "TextDelta",
        StreamEvent::TextEnd { .. } => "TextEnd",
        StreamEvent::ThinkingStart { .. } => "ThinkingStart",
        StreamEvent::ThinkingDelta { .. } => "ThinkingDelta",
        StreamEvent::ThinkingEnd { .. } => "ThinkingEnd",
        StreamEvent::ToolCallStart { .. } => "ToolCallStart",
        StreamEvent::ToolCallDelta { .. } => "ToolCallDelta",
        StreamEvent::ToolCallEnd { .. } => "ToolCallEnd",
        StreamEvent::Done { .. } => "Done",
        StreamEvent::Error { .. } => "Error",
    }
}
