//! PreToolUse denial harness.
//!
//! A pre-tool veto is a failed tool invocation, not a failed agent run.  The
//! conversation must remain structurally complete so providers and exporters
//! see: user -> assistant(tool call) -> toolResult(error) -> assistant.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::error::{AgentError, AgentResult};
use ion::agent::extension::{Extension, ExtensionRunner};
use ion::agent::messages::Message;
use ion::agent::tool::{Tool, ToolRegistry};
use ion_provider::faux;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;

fn faux_model() -> Model {
    Model {
        id: "faux-test".into(),
        name: "Faux Test".into(),
        api: "faux".into(),
        provider: "faux".into(),
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

struct CountingTool {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl Tool for CountingTool {
    fn name(&self) -> &str {
        "denial_probe"
    }

    fn description(&self) -> &str {
        "A tool that must not execute when a pre-tool extension vetoes it"
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    async fn execute(
        &self,
        _args: serde_json::Value,
        _rt: &dyn ion::runtime::Runtime,
    ) -> AgentResult<String> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Ok("unexpected execution".into())
    }
}

struct DenyBeforeTool;

#[async_trait]
impl Extension for DenyBeforeTool {
    fn name(&self) -> &str {
        "deny_before_tool"
    }

    async fn before_tool_call(&self, call: &mut ToolCall) -> AgentResult<()> {
        if call.name == "denial_probe" {
            return Err(AgentError::Tool(
                "blocked by PreToolUse test hook: policy denied".into(),
            ));
        }
        Ok(())
    }
}

fn tool_call_step() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Single(faux::faux_tool_call("denial_probe", serde_json::json!({}))),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::ToolUse),
            error_message: None,
        },
    ))
}

fn final_step() -> faux::FauxResponseStep {
    faux::FauxResponseStep::Static(faux::faux_assistant_message(
        faux::FauxContent::Text("The tool was denied, so I did not run it.".into()),
        faux::FauxMessageOptions {
            stop_reason: Some(StopReason::Stop),
            error_message: None,
        },
    ))
}

#[tokio::test]
async fn pre_tool_veto_becomes_error_tool_result_and_agent_continues() {
    let mut registry = ApiRegistry::new();
    let faux_handle = faux::register_faux(&mut registry);
    faux_handle.set_responses(vec![tool_call_step(), final_step()]);

    let executions = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRegistry::new();
    tools.register(Box::new(CountingTool {
        executions: executions.clone(),
    }));

    let mut extensions = ExtensionRunner::new();
    extensions.register(Box::new(DenyBeforeTool));

    let config = AgentConfig {
        max_turns: Some(5),
        max_retries: 0,
        retry_on_no_tool_use: 0,
        ..Default::default()
    };
    let mut agent = Agent::new(Arc::new(registry), faux_model(), None, tools, config)
        .with_extensions(extensions);

    agent
        .run("Try the denial probe")
        .await
        .expect("a pre-tool veto must not abort the agent run");

    assert_eq!(
        executions.load(Ordering::SeqCst),
        0,
        "the vetoed tool must never execute"
    );

    // Budget reminders are valid custom entries.  Assert the provider-facing
    // conversation sequence while leaving those timeline records intact.
    let messages = agent
        .messages()
        .iter()
        .filter(|message| {
            matches!(
                message,
                Message::User(_) | Message::Assistant(_) | Message::ToolResult(_)
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        messages.len(),
        4,
        "the conversation must stay structurally complete: {messages:#?}"
    );
    assert!(matches!(messages[0], Message::User(_)));

    let tool_call = match messages[1] {
        Message::Assistant(message) => message
            .content
            .iter()
            .find_map(|block| match block {
                AssistantContentBlock::ToolCall(call) => Some(call),
                _ => None,
            })
            .expect("the first assistant message must contain the tool call"),
        other => panic!("expected assistant tool call, got {other:?}"),
    };

    let result = match messages[2] {
        Message::ToolResult(result) => result,
        other => panic!("expected denied tool result, got {other:?}"),
    };
    assert_eq!(result.tool_call_id, tool_call.id);
    assert_eq!(result.tool_name, "denial_probe");
    assert!(
        result.is_error,
        "the synthetic denial must be an error result"
    );
    let result_text = result
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert!(result_text.contains("PreToolUse"));
    assert!(result_text.contains("policy denied"));
    assert_eq!(
        result.details.as_ref().and_then(|v| v["status"].as_str()),
        Some("denied")
    );

    assert!(matches!(messages[3], Message::Assistant(_)));
}
