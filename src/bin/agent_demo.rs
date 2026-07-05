use std::sync::Arc;

use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::compact::CompactConfig;
use ion::agent::tool::{CalculatorTool, EchoTool, ToolRegistry};
use ion_provider::registry::{ApiRegistry, ModelRegistry};
use ion_provider::types::{
    AssistantContentBlock, ContentBlock, Cost, CustomContent, Message, Model, StopReason,
};
use tracing_subscriber::EnvFilter;

/// Demo: inner Agent Loop with real LLM calls.
///
/// Usage:
///   cargo run --bin agent-demo
const API_BASE: &str = "https://opencode.ai/zen/go/v1";
const API_KEY: &str = "sk-sniMbFE0l8wIGsTAsbfERSGrvcrBv97iBfDuppzN99kg5Wp2a2dMYxntMFBN9lEg";
const MODEL_ID: &str = "deepseek-v4-flash";

fn build_registry_and_model() -> (Arc<ApiRegistry>, Model) {
    let mut registry = ApiRegistry::new();
    registry.register_builtins();

    let mut model_registry = ModelRegistry::new();
    model_registry.register_builtins();

    let model = model_registry
        .find_model(MODEL_ID)
        .cloned()
        .unwrap_or_else(|| Model {
            id: MODEL_ID.into(),
            name: MODEL_ID.into(),
            api: "openai-completions".into(),
            provider: "opencode".into(),
            base_url: API_BASE.into(),
            reasoning: true,
            input: vec!["text".into()],
            cost: Cost {
                input: 0.0,
                output: 0.0,
                cache_read: 0.0,
                cache_write: 0.0,
            },
            context_window: 128000,
            max_tokens: 8192,
            compat: None,
            headers: None,
        });

    (Arc::new(registry), model)
}

fn describe_message(msg: &Message) -> Vec<String> {
    match msg {
        Message::User(user) => user
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) if !text.text.is_empty() => {
                    Some(format!("[user] {}", text.text))
                }
                _ => None,
            })
            .collect(),
        Message::Assistant(assistant) => {
            let mut lines = Vec::new();
            for block in &assistant.content {
                match block {
                    AssistantContentBlock::Text(text) if !text.text.is_empty() => {
                        lines.push(format!("[assistant] {}", text.text));
                    }
                    AssistantContentBlock::Thinking(thinking)
                        if !thinking.thinking.is_empty() =>
                    {
                        lines.push(format!("[thinking] {}", thinking.thinking));
                    }
                    AssistantContentBlock::ToolCall(tool_call) => {
                        lines.push(format!(
                            "[tool_call] {}({})",
                            tool_call.name, tool_call.arguments
                        ));
                    }
                    _ => {}
                }
            }
            if lines.is_empty() {
                lines.push(format!(
                    "[assistant] stop_reason={:?}",
                    assistant.stop_reason
                ));
            }
            lines
        }
        Message::ToolResult(result) => result
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) if !text.text.is_empty() => Some(format!(
                    "[tool_result:{}] {}",
                    result.tool_name, text.text
                )),
                _ => None,
            })
            .collect(),
        Message::BashExecution(b) => vec![format!(
            "[bashExecution] `{}` exit={:?} cancelled={} truncated={}",
            b.command, b.exit_code, b.cancelled, b.truncated
        )],
        Message::Custom(c) => vec![format!("[custom:{}] {}", c.custom_type, match &c.content {
            CustomContent::Text(s) => s.clone(),
            CustomContent::Blocks(_) => "<blocks>".into(),
        })],
        Message::BranchSummary(b) => vec![format!("[branchSummary from={}] {}", b.from_id, b.summary)],
        Message::CompactionSummary(c) => vec![format!("[compactionSummary tokens={}] {}", c.tokens_before, c.summary)],
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tracing::info!("=== Agent Loop Demo ===");
    tracing::info!("Model: {MODEL_ID} @ {API_BASE}");

    let (registry, model) = build_registry_and_model();

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(CalculatorTool));
    tools.register(Box::new(EchoTool));

    tracing::info!(
        "Tools: {}",
        tools
            .tool_defs()
            .iter()
            .map(|t| t.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    );

    let mut agent = Agent::new(
        registry,
        model,
        Some(
            "You are a helpful coding assistant. You can use the calculator tool for math, and the echo tool to echo text."
                .into(),
        ),
        tools,
        AgentConfig {
            max_turns: 10,
            max_outer_iterations: 3,
            max_retries: 2,
            retry_base_delay_ms: 1000,
            enable_compact: true,
            compact_config: CompactConfig {
                threshold: 32000,
                target: 16000,
                keep_newest: 4,
            },
            api_key: Some(API_KEY.into()),
            response_format: None,
            thinking: None,
            retry_config: None,
        },
    );

    let prompt =
        "What is 1234 * 5678? Use the calculator tool to compute it, then tell me the answer.";

    tracing::info!("=== User ===");
    tracing::info!("{prompt}");

    match agent.run(prompt).await {
        Ok(()) => {
            tracing::info!("=== Agent completed ===");
            let messages = agent.messages();
            for line in messages.iter().flat_map(describe_message) {
                tracing::info!("{line}");
            }
            tracing::info!("Total messages: {}", messages.len());

            let final_stop = messages.iter().rev().find_map(|msg| match msg {
                Message::Assistant(assistant) => Some(&assistant.stop_reason),
                _ => None,
            });
            if let Some(reason) = final_stop {
                tracing::info!("Final stop reason: {:?}", reason);
            }
        }
        Err(e) => {
            tracing::error!("Agent error: {e}");
            tracing::info!("Final stop reason: {:?}", StopReason::Error);
        }
    }

    Ok(())
}
