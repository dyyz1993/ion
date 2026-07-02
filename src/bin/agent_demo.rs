use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::compact::CompactConfig;
use ion::agent::messages::Role;
use ion::agent::provider::OpenAIProvider;
use ion::agent::tool::{CalculatorTool, EchoTool, ToolRegistry};
/// Demo: inner Agent Loop with real LLM calls.
///
/// This demonstrates the full agent lifecycle:
/// 1. Provider → streaming LLM call
/// 2. Tool calls (the agent asks for calculator, we compute)
/// 3. Retry (try changing the prompt to cause an error)
/// 4. Steering (inject a message mid-turn)
/// 5. Pause/resume (send SIGINT or use the handle)
/// 6. Context compression (if messages grow large)
///
/// Usage:
///   cargo run --bin agent-demo
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

const API_BASE: &str = "https://opencode.ai/zen/go/v1";
const API_KEY: &str = "sk-sniMbFE0l8wIGsTAsbfERSGrvcrBv97iBfDuppzN99kg5Wp2a2dMYxntMFBN9lEg";
const MODEL: &str = "deepseek-v4-flash";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    tracing::info!("=== Agent Loop Demo ===");
    tracing::info!("Provider: {MODEL} @ {API_BASE}");

    // ---- 1. Create the provider ----
    let provider = Arc::new(OpenAIProvider::new(API_BASE, API_KEY, MODEL).with_max_tokens(8192))
        as Arc<dyn ion::agent::provider::Provider + Send + Sync>;

    // ---- 2. Register tools ----
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(CalculatorTool));
    tools.register(Box::new(EchoTool));

    tracing::info!(
        "Tools: {}",
        tools
            .tool_defs()
            .iter()
            .map(|t| t.function.name.clone())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // ---- 3. Create the agent ----
    let mut agent = Agent::new(
        provider,
        Some("You are a helpful coding assistant. You can use the calculator tool for math, and the echo tool to echo text.".into()),
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
        },
    );

    // ---- 4. Run the agent ----
    let prompt =
        "What is 1234 * 5678? Use the calculator tool to compute it, then tell me the answer.";

    tracing::info!("");
    tracing::info!("=== User ===");
    tracing::info!("{prompt}");

    // ---- 5. Run and observe events ----
    match agent.run(prompt).await {
        Ok(()) => {
            tracing::info!("");
            tracing::info!("=== Agent completed ===");

            // Print last assistant message
            for msg in agent.messages().iter().rev().take(3) {
                if msg.role == Role::Assistant
                    && let Some(ref content) = msg.content
                    && !content.is_empty()
                {
                    tracing::info!("Assistant: {content}");
                }
                // Print tool calls
                if let Some(ref tcs) = msg.tool_calls {
                    for tc in tcs {
                        tracing::info!(
                            "  Tool call: {}({})",
                            tc.function.name,
                            tc.function.arguments
                        );
                    }
                }
            }

            // ---- 6. Statistics ----
            tracing::info!("");
            tracing::info!("=== Stats ===");
            tracing::info!("Total messages: {}", agent.messages().len());
            tracing::info!(
                "Total tokens (approx): {}",
                agent
                    .messages()
                    .iter()
                    .map(|m| m.approx_tokens())
                    .sum::<usize>()
            );

            // ---- 7. Show conversation ----
            tracing::info!("");
            tracing::info!("=== Full Conversation ===");
            for msg in agent.messages() {
                let role = msg.role.as_str();
                if let Some(ref content) = msg.content
                    && !content.is_empty()
                {
                    tracing::info!("[{role}] {content}");
                }
                if let Some(ref tcs) = msg.tool_calls {
                    for tc in tcs {
                        tracing::info!(
                            "  → Tool call: {}({})",
                            tc.function.name,
                            tc.function.arguments
                        );
                    }
                }
            }
        }
        Err(e) => {
            tracing::error!("Agent error: {e}");
        }
    }

    Ok(())
}
