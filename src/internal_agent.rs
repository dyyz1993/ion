//! Internal agent runner — lets extensions spawn a full agent loop
//! (with tools, thinking, schema, max_turns) without persisting a session.
//!
//! This is the "scene 1 capability wrapper" for extensions:
//! - AutoSessionTitle wants a single LLM call → use `query_llm`
//! - LearningExtension wants to distill a skill from current messages
//!   with tools (read/write/bash) → use `run_agent`
//! - GoalSupervisor wants to run verification checks → use `run_agent`
//!
//! Key: the spawned agent runs **in-memory only**. No session.jsonl,
//! no session index update, no AutoSessionTitle injection. The caller
//! gets back the final output + messages vec.

use std::sync::Arc;

use ion_provider::registry::ApiRegistry;
use ion_provider::types::{Message, Model};

use crate::agent::agent_loop::{Agent, AgentConfig};
use crate::agent::tool::ToolRegistry;

/// Request to run an internal agent loop.
#[derive(Debug, Default)]
pub struct RunAgentRequest {
    /// Tier: "fast" / "pro" / "max" (resolved from tier_models, fallback default).
    pub tier: String,

    /// The user prompt for the agent.
    pub prompt: String,

    /// Override system prompt (None = use tier model's default).
    pub system_prompt: Option<String>,

    /// Snapshot of messages to prepend (e.g. current session context).
    /// The agent sees these BEFORE the prompt, as prior conversation.
    /// **Not modified** — the original session is untouched.
    pub messages_snapshot: Option<Vec<Message>>,

    /// Max agent turns (None = unlimited). 1 = single call (no tool loop).
    pub max_turns: Option<u64>,

    /// Tool whitelist (None = use all registered tools).
    pub tools: Option<Vec<String>>,

    /// Thinking level override ("off" / "low" / "medium" / "high" / "xhigh").
    pub thinking: Option<String>,

    /// Force JSON output format.
    pub json: bool,

    /// JSON Schema to validate output (implies json=true).
    pub json_schema: Option<serde_json::Value>,

    /// Schema validation retries (default 3).
    pub schema_retries: u32,
}

impl RunAgentRequest {
    pub fn new(tier: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            tier: tier.into(),
            prompt: prompt.into(),
            schema_retries: 3,
            ..Default::default()
        }
    }

    pub fn with_system_prompt(mut self, sp: impl Into<String>) -> Self {
        self.system_prompt = Some(sp.into());
        self
    }

    pub fn with_messages(mut self, msgs: Vec<Message>) -> Self {
        self.messages_snapshot = Some(msgs);
        self
    }

    pub fn with_max_turns(mut self, n: u64) -> Self {
        self.max_turns = Some(n);
        self
    }

    pub fn with_tools(mut self, tools: Vec<String>) -> Self {
        self.tools = Some(tools);
        self
    }

    pub fn with_thinking(mut self, level: impl Into<String>) -> Self {
        self.thinking = Some(level.into());
        self
    }

    pub fn with_json_schema(mut self, schema: serde_json::Value) -> Self {
        self.json_schema = Some(schema);
        self.json = true;
        self
    }
}

/// Result of an internal agent run.
#[derive(Debug)]
pub struct RunAgentResult {
    /// Final assistant text output.
    pub output: String,
    /// Full message history (in-memory, not persisted).
    pub messages: Vec<Message>,
    /// How many turns the agent ran.
    pub turn_count: u64,
    /// How many tool calls were made.
    pub tool_call_count: u64,
}

/// Run an internal agent loop — full scene 1 capability, **no session persistence**.
///
/// This is what extensions use when they need:
/// - Multi-turn agent loop (with tool calls, thinking, retries)
/// - Based on current conversation context (messages_snapshot)
/// - Without polluting the user's session.jsonl
///
/// # Arguments
/// - `registry`: Provider registry (from cmd_run / worker_rpc)
/// - `tools`: Pre-built tool registry (from build_tools). If the request
///   specifies `tools` whitelist, it's filtered here.
/// - `req`: RunAgentRequest with tier/prompt/options
///
/// # Returns
/// - `Ok(RunAgentResult)` — agent completed, output + messages in memory
/// - `Err(String)` — tier unresolvable, agent.run() failed, or schema mismatch
///
/// # What this does NOT do (by design):
/// - ❌ Write session.jsonl
/// - ❌ Update SessionIndex
/// - ❌ Trigger AutoSessionTitle / LearningExtension
/// - ❌ Register extensions (unless caller pre-registers)
///
/// # Example
/// ```no_run
/// let snapshot = agent.messages().clone();
/// let result = ion::internal_agent::run_agent(
///     &registry,
///     tools_registry,
///     ion::internal_agent::RunAgentRequest::new("fast", "Summarize key decisions")
///         .with_messages(snapshot)
///         .with_max_turns(1),
/// ).await?;
/// println!("Summary: {}", result.output);
/// // agent.messages() unchanged — snapshot was a clone
/// ```
pub async fn run_agent(
    registry: &Arc<ApiRegistry>,
    mut tools: ToolRegistry,
    req: RunAgentRequest,
) -> Result<RunAgentResult, String> {
    let cfg = crate::config::IonConfig::load();

    // 1. Resolve model from tier
    let model: Model = cfg.resolve_tier_model(&req.tier).ok_or_else(|| {
        format!(
            "tier '{}' not configured in tier_models and no default_model available",
            req.tier
        )
    })?;

    // 2. Resolve API key
    let api_key = cfg.resolve_provider_api_key(&model.provider);

    // 3. Apply tool whitelist
    if let Some(ref allowed) = req.tools {
        let allowed_refs: Vec<&str> = allowed.iter().map(|s| s.as_str()).collect();
        tools.filter(allowed_refs);
    }

    // 4. Build agent config
    let agent_config = AgentConfig {
        max_turns: req.max_turns,
        api_key,
        response_format: if req.json { Some("json_object".into()) } else { None },
        thinking: req.thinking.clone(),
        ..Default::default()
    };

    // 5. Build system prompt
    let sys_prompt = req.system_prompt.clone();

    // 6. Create agent (in-memory, no extensions, no session)
    let mut agent = Agent::new(
        Arc::clone(registry),
        model,
        sys_prompt,
        tools,
        agent_config,
    );

    // 7. Prepend messages snapshot if provided
    if let Some(snapshot) = req.messages_snapshot {
        agent.set_messages(snapshot);
    }

    // 8. Run agent loop — with schema retry if json_schema is set.
    //    Schema retry mirrors cmd_run (src/bin/ion.rs:2184-2402):
    //    1. Run agent.run(prompt)
    //    2. Extract output
    //    3. If schema set: validate → fail → inject error as new prompt → re-run
    //    4. Repeat up to schema_retries+1 total attempts
    let max_attempts = if req.json_schema.is_some() {
        req.schema_retries + 1
    } else {
        1
    };

    let mut current_prompt = req.prompt.clone();
    let mut last_output = String::new();
    let mut last_error: Option<String> = None;

    for attempt in 1..=max_attempts {
        // Run agent loop
        agent
            .run(&current_prompt)
            .await
            .map_err(|e| format!("agent.run() failed: {e}"))?;

        // Extract last assistant text
        last_output = agent
            .messages()
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(a) => {
                    let text: String = a
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ion_provider::types::AssistantContentBlock::Text(t) => {
                                Some(t.text.as_str())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    if text.trim().is_empty() { None } else { Some(text) }
                }
                _ => None,
            })
            .unwrap_or_default();

        // No schema → done
        if req.json_schema.is_none() {
            break;
        }

        // Schema validation
        let schema = req.json_schema.as_ref().unwrap();
        let parsed_result = serde_json::from_str::<serde_json::Value>(last_output.trim());
        match parsed_result {
            Err(e) => {
                last_error = Some(format!("not valid JSON: {e}"));
                if attempt < max_attempts {
                    let schema_str = serde_json::to_string_pretty(schema)
                        .unwrap_or_else(|_| schema.to_string());
                    current_prompt = format!(
                        "Your previous output was not valid JSON.\n\
                         Error: {e}\n\n\
                         Your output:\n```\n{last_output}\n```\n\n\
                         Please reply with valid JSON matching this schema:\n```json\n{schema_str}\n```"
                    );
                    tracing::warn!(
                        "[run_agent] JSON parse fail attempt {attempt}/{max_attempts}: {e}"
                    );
                    continue;
                }
            }
            Ok(parsed) => {
                let validator = jsonschema::Validator::new(schema)
                    .map_err(|e| format!("invalid schema: {e}"))?;
                match validator.validate(&parsed) {
                    Ok(()) => {
                        // Schema pass!
                        last_error = None;
                        break;
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        last_error = Some(format!("schema mismatch: {err_msg}"));
                        if attempt < max_attempts {
                            let schema_str = serde_json::to_string_pretty(schema)
                                .unwrap_or_else(|_| schema.to_string());
                            current_prompt = format!(
                                "Your previous output did not match the schema.\n\
                                 Error: {err_msg}\n\n\
                                 Your output:\n```json\n{last_output}\n```\n\n\
                                 Fix it to match this schema:\n```json\n{schema_str}\n```"
                            );
                            tracing::warn!(
                                "[run_agent] schema mismatch attempt {attempt}/{max_attempts}: {err_msg}"
                            );
                            continue;
                        }
                    }
                }
            }
        }
    }

    // If schema was requested and all attempts failed, return error
    if req.json_schema.is_some() && last_error.is_some() {
        return Err(format!(
            "schema mismatch after {max_attempts} attempts: {}\n\
             last output: {}",
            last_error.unwrap(),
            last_output
        ));
    }

    // 9. Extract final result
    let messages = agent.messages().to_vec();
    let turn_count = agent.current_message_count() as u64;
    let tool_call_count = messages
        .iter()
        .filter(|m| matches!(m, Message::ToolResult(_)))
        .count() as u64;

    Ok(RunAgentResult {
        output: last_output,
        messages,
        turn_count,
        tool_call_count,
    })
}
