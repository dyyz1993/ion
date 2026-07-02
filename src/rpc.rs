//! ION RPC protocol — JSONL over stdin/stdout.
//!
//! Follows the pi-momo RPC protocol reference:
//! - 26 commands covering prompting, session, model, tools, config
//! - Streaming events via the Extension system (text_delta, thinking_delta, tool_execution)
//! - Response format: `{"id":"...","type":"response","command":"...","success":true,"data":{...}}`
//! - Event format: `{"type":"event","event":{"type":"text_delta","delta":"..."}}`

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::agent_loop::{Agent, AgentContext};
use crate::agent::error::AgentResult;
use crate::agent::extension::{
    Extension, ExtensionRegistry, ModelSelectContext, ToolExecutionContext,
    TurnContext,
};
use crate::agent::tool::{
    BashTool, CalculatorTool, EchoTool, EditTool, FindTool, GrepTool, LsTool, ReadTool,
    ToolRegistry, WriteTool,
};
use crate::agent_config;
use crate::config::IonConfig;
use ion_provider::registry::ApiRegistry;
use ion_provider::types::*;

// ---------------------------------------------------------------------------
// RPC Config
// ---------------------------------------------------------------------------

pub struct RpcConfig {
    pub registry: Arc<ApiRegistry>,
    pub model: Model,
    pub agent_config: crate::agent::agent_loop::AgentConfig,
    pub thinking: Option<String>,
    pub max_turns: u64,
}

// ---------------------------------------------------------------------------
// Shared stdout writer
// ---------------------------------------------------------------------------

struct RpcOutput {
    writer: Mutex<io::BufWriter<io::Stdout>>,
}

impl RpcOutput {
    fn new() -> Self {
        Self {
            writer: Mutex::new(io::BufWriter::new(io::stdout())),
        }
    }

    fn write_json(&self, value: &serde_json::Value) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = writeln!(w, "{}", serde_json::to_string(value).unwrap());
            let _ = w.flush();
        }
    }
}

// ---------------------------------------------------------------------------
// RpcExtension — writes streaming events to stdout
// ---------------------------------------------------------------------------

struct RpcExtension {
    output: Arc<RpcOutput>,
}

#[async_trait::async_trait]
impl Extension for RpcExtension {
    async fn on_agent_start(&self, _ctx: &AgentContext) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {"type": "agent_start"}
        }));
        Ok(())
    }

    async fn on_agent_end(&self, _ctx: &AgentContext) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {"type": "agent_end"}
        }));
        Ok(())
    }

    async fn on_turn_start(&self, ctx: &mut TurnContext) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {"type": "turn_start", "turnIndex": ctx.turn_index}
        }));
        Ok(())
    }

    async fn on_turn_end(&self, ctx: &TurnContext) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "turn_end",
                "turnIndex": ctx.turn_index,
                "hasToolCalls": ctx.has_tool_calls,
            }
        }));
        Ok(())
    }

    async fn on_message_start(&self, role: &str, _content: &str) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_start",
                "message": {"role": role, "content": []}
            }
        }));
        Ok(())
    }

    async fn on_message_delta(&self, delta: &str, role: &str) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_update",
                "role": role,
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "delta": delta
                }
            }
        }));
        Ok(())
    }

    async fn on_message_end(&self, role: &str, content: &str, usage: &Usage) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_end",
                "message": {"role": role},
                "usage": {
                    "input": usage.input,
                    "output": usage.output,
                    "cache_read": usage.cache_read,
                    "cache_write": usage.cache_write,
                }
            }
        }));
        Ok(())
    }

    async fn on_thinking_delta(&self, delta: &str) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "thinking_delta",
                    "delta": delta
                }
            }
        }));
        Ok(())
    }

    async fn on_thinking_end(&self, content: &str) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "thinking_end",
                    "content": content
                }
            }
        }));
        Ok(())
    }

    async fn on_tool_call_delta(&self, delta: &str, name: &str) -> AgentResult<()> {
        if !delta.is_empty() {
            self.output.write_json(&serde_json::json!({
                "type": "event",
                "event": {
                    "type": "message_update",
                    "assistantMessageEvent": {
                        "type": "tool_call_delta",
                        "delta": delta,
                        "name": name
                    }
                }
            }));
        }
        Ok(())
    }

    async fn on_tool_execution_start(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_start",
                "toolCallId": ctx.tool_call_id,
                "toolName": ctx.tool_name,
                "args": ctx.args,
            }
        }));
        Ok(())
    }

    async fn on_tool_execution_update(&self, ctx: &ToolExecutionContext, partial: &str) -> crate::agent::error::AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_update",
                "toolCallId": ctx.tool_call_id,
                "toolName": ctx.tool_name,
                "partialResult": partial,
            }
        }));
        Ok(())
    }


    async fn on_tool_execution_end(&self, ctx: &ToolExecutionContext) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_end",
                "toolCallId": ctx.tool_call_id,
                "toolName": ctx.tool_name,
                "isError": ctx.is_error,
                "durationMs": ctx.duration_ms,
            }
        }));
        Ok(())
    }

    async fn on_model_select(&self, ctx: &ModelSelectContext) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "model_select",
                "model": {"id": ctx.new_model, "provider": ctx.new_provider},
                "previousModel": {"id": ctx.old_model, "provider": ctx.old_provider},
            }
        }));
        Ok(())
    }

    async fn on_thinking_level_select(&self, level: &str, old: Option<&str>) -> AgentResult<()> {
        self.output.write_json(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "thinking_level_select",
                "level": level,
                "previousLevel": old,
            }
        }));
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Session state
// ---------------------------------------------------------------------------

struct RpcSession {
    agent: Agent,
    output: Arc<RpcOutput>,
    current_provider: String,
    current_model: String,
    thinking_level: Option<String>,
}

impl RpcSession {
    fn new(
        registry: Arc<ApiRegistry>,
        model: Model,
        config: crate::agent::agent_loop::AgentConfig,
        output: Arc<RpcOutput>,
        thinking: Option<String>,
        max_turns: u64,
    ) -> Self {
        let mut tools = ToolRegistry::new();
        tools.register(Box::new(CalculatorTool));
        tools.register(Box::new(EchoTool));
        tools.register(Box::new(ReadTool));
        tools.register(Box::new(GrepTool));
        tools.register(Box::new(FindTool));
        tools.register(Box::new(LsTool));
        tools.register(Box::new(BashTool));
        tools.register(Box::new(WriteTool));
        tools.register(Box::new(EditTool));

        let rpc_ext = RpcExtension {
            output: Arc::clone(&output),
        };
        let mut exts = ExtensionRegistry::new();
        exts.register(Box::new(rpc_ext));

        let mut agent = Agent::new(
            registry,
            model.clone(),
            Some("You are a helpful AI assistant with access to tools.".into()),
            tools,
            config,
        );
        agent = agent.with_extensions(exts);

        let mut thinking_str = thinking.unwrap_or_default();
        if thinking_str.is_empty() {
            thinking_str = "medium".into();
        }

        Self {
            agent,
            output,
            current_provider: model.provider.clone(),
            current_model: model.id.clone(),
            thinking_level: Some(thinking_str),
        }
    }

    fn messages(&self) -> &[Message] {
        self.agent.messages()
    }

    async fn run_prompt(&mut self, text: &str) -> Result<(), String> {
        self.agent.run(text).await.map_err(|e| e.to_string())
    }

    fn steer(&mut self, text: &str) {
        self.agent.steer(Message::User(UserMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            timestamp: now_ms(),
        }));
    }

    fn follow_up(&mut self, text: &str) {
        self.agent.follow_up(Message::User(UserMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            timestamp: now_ms(),
        }));
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// RPC command handler
// ---------------------------------------------------------------------------

pub async fn handle_rpc(cfg: RpcConfig) {
    let output = Arc::new(RpcOutput::new());
    let mut sessions: HashMap<String, RpcSession> = HashMap::new();
    let stdin = io::stdin().lock();

    for line in stdin.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };
        if line.trim().is_empty() {
            continue;
        }

        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                output.write_json(&serde_json::json!({
                    "type": "response", "success": false,
                    "error": "invalid JSON"
                }));
                continue;
            }
        };

        let cmd_type = req["type"].as_str().unwrap_or("").to_string();
        let cmd_id = req["id"].as_str().unwrap_or("0").to_string();

        // Dispatch
        let result = match cmd_type.as_str() {
            // ── Core prompting ──
            "prompt" => cmd_prompt(&mut sessions, &cfg, Arc::clone(&output), &req, &cmd_id).await,
            "steer" => cmd_steer(&mut sessions, &req, &cmd_id).await,
            "follow_up" => cmd_follow_up(&mut sessions, &req, &cmd_id).await,
            "continue" => cmd_continue(&mut sessions, &cfg, Arc::clone(&output), &req, &cmd_id).await,
            "abort" => cmd_abort(&mut sessions, &req, &cmd_id),

            // ── State ──
            "get_state" | "state" => cmd_get_state(&sessions, &req, &cmd_id),

            // ── Messages ──
            "get_messages" => cmd_get_messages(&sessions, &req, &cmd_id),
            "get_last_assistant_text" => cmd_get_last_assistant_text(&sessions, &req, &cmd_id),

            // ── Tools ──
            "get_tools" => cmd_get_tools(&sessions, &req, &cmd_id),

            // ── Models ──
            "set_model" => cmd_set_model(&mut sessions, &req, &cmd_id),
            "get_available_models" => cmd_get_available_models(&cmd_id),

            // ── Thinking ──
            "set_thinking_level" => cmd_set_thinking_level(&mut sessions, &req, &cmd_id),
            "cycle_thinking_level" => cmd_cycle_thinking_level(&mut sessions, &req, &cmd_id),

            // ── Sessions ──
            "new_session" => cmd_new_session(&mut sessions, &cfg, Arc::clone(&output), &req, &cmd_id).await,
            "fork" => cmd_fork(&mut sessions, &cfg, Arc::clone(&output), &req, &cmd_id).await,
            "set_session_name" => cmd_set_session_name(&req, &cmd_id),
            "dispose" => { sessions.remove(&get_sid(&req)); ok(&output, &cmd_id) }

            // ── Agent ──
            "get_agents" => cmd_get_agents(&cmd_id),
            "get_agent_detail" => cmd_get_agent_detail(&req, &cmd_id),

            // ── System ──
            "get_settings" => cmd_get_settings(&cmd_id),
            "set_settings" => cmd_set_settings(&req, &cmd_id),
            "get_system_prompt" => cmd_get_system_prompt(&sessions, &req, &cmd_id),

            // ── Lifecycle ──
            "compact" => cmd_compact(&req, &cmd_id),

            // ── Export ──
            "export_html" => cmd_export_html(&req, &cmd_id),

            // ── Unknown ──
            _ => {
                output.write_json(&serde_json::json!({
                    "id": cmd_id, "type": "response",
                    "command": cmd_type,
                    "success": false,
                    "error": format!("unknown command: {cmd_type}")
                }));
                continue;
            }
        };

        if let Err(e) = result {
            output.write_json(&serde_json::json!({
                "id": cmd_id, "type": "response",
                "command": cmd_type,
                "success": false,
                "error": e
            }));
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn get_sid(req: &serde_json::Value) -> String {
    req.get("session")
        .and_then(|s| s.as_str())
        .unwrap_or("default")
        .to_string()
}

fn ok(output: &RpcOutput, cmd_id: &str) -> Result<(), String> {
    output.write_json(&serde_json::json!({
        "id": cmd_id, "type": "response", "success": true
    }));
    Ok(())
}

fn ok_data(output: &RpcOutput, cmd_id: &str, data: serde_json::Value) -> Result<(), String> {
    output.write_json(&serde_json::json!({
        "id": cmd_id, "type": "response", "success": true, "data": data
    }));
    Ok(())
}

fn get_or_create_session<'a>(
    sessions: &'a mut HashMap<String, RpcSession>,
    cfg: &RpcConfig,
    output: Arc<RpcOutput>,
    sid: &str,
) -> &'a mut RpcSession {
    if !sessions.contains_key(sid) {
        let session = RpcSession::new(
            Arc::clone(&cfg.registry),
            cfg.model.clone(),
            cfg.agent_config.clone(),
            output,
            cfg.thinking.clone(),
            cfg.max_turns,
        );
        sessions.insert(sid.to_string(), session);
    }
    sessions.get_mut(sid).unwrap()
}

// ---------------------------------------------------------------------------
// Command: prompt
// ---------------------------------------------------------------------------

async fn cmd_prompt(
    sessions: &mut HashMap<String, RpcSession>,
    cfg: &RpcConfig,
    output: Arc<RpcOutput>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    let text = req["message"].as_str().or_else(|| req["text"].as_str()).unwrap_or("");
    if text.is_empty() {
        return Err("missing 'message' field".into());
    }

    let session = get_or_create_session(sessions, cfg, Arc::clone(&output), &sid);

    // Write immediate response
    output.write_json(&serde_json::json!({
        "id": cmd_id, "type": "response", "command": "prompt", "success": true
    }));

    // Run the agent (events are streamed via RpcExtension)
    session.run_prompt(text).await
}

// ---------------------------------------------------------------------------
// Command: steer / follow_up
// ---------------------------------------------------------------------------

async fn cmd_steer(
    sessions: &mut HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    let text = req["message"].as_str().unwrap_or("");
    if text.is_empty() {
        return Err("missing 'message' field".into());
    }
    if let Some(session) = sessions.get_mut(&sid) {
        session.steer(text);
    }
    ok(&RpcOutput::new(), cmd_id)
}

async fn cmd_follow_up(
    sessions: &mut HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    let text = req["message"].as_str().unwrap_or("");
    if text.is_empty() {
        return Err("missing 'message' field".into());
    }
    if let Some(session) = sessions.get_mut(&sid) {
        session.follow_up(text);
    }
    ok(&RpcOutput::new(), cmd_id)
}

// ---------------------------------------------------------------------------
// Command: continue / abort
// ---------------------------------------------------------------------------

async fn cmd_continue(
    sessions: &mut HashMap<String, RpcSession>,
    cfg: &RpcConfig,
    output: Arc<RpcOutput>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    let session = get_or_create_session(sessions, cfg, Arc::clone(&output), &sid);
    output.write_json(&serde_json::json!({
        "id": cmd_id, "type": "response", "command": "continue", "success": true
    }));
    // Run with empty prompt to trigger continue logic in outer_loop
    session.run_prompt("").await
}

fn cmd_abort(
    sessions: &mut HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    if let Some(session) = sessions.get_mut(&sid) {
        session.agent.pause();
    }
    ok(&RpcOutput::new(), cmd_id)
}

// ---------------------------------------------------------------------------
// Command: get_state
// ---------------------------------------------------------------------------

fn cmd_get_state(
    sessions: &HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    let output = RpcOutput::new();
    if let Some(session) = sessions.get(&sid) {
        let msg_count = session.messages().len();
        ok_data(&output, cmd_id, serde_json::json!({
            "messageCount": msg_count,
            "isStreaming": false,
            "model": {"id": session.current_model, "provider": session.current_provider},
            "thinkingLevel": session.thinking_level,
        }))
    } else {
        ok_data(&output, cmd_id, serde_json::json!({
            "messageCount": 0,
            "isStreaming": false,
        }))
    }
}

// ---------------------------------------------------------------------------
// Command: get_messages
// ---------------------------------------------------------------------------

fn cmd_get_messages(
    sessions: &HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    let output = RpcOutput::new();
    if let Some(session) = sessions.get(&sid) {
        let msgs: Vec<&Message> = session.messages().iter().collect();
        let msgs_json: Vec<serde_json::Value> = msgs.iter().map(|m| serde_json::to_value(m).unwrap_or_default()).collect();
        ok_data(&output, cmd_id, serde_json::json!({
            "messages": msgs_json
        }))
    } else {
        ok_data(&output, cmd_id, serde_json::json!({
            "messages": []
        }))
    }
}

// ---------------------------------------------------------------------------
// Command: get_last_assistant_text
// ---------------------------------------------------------------------------

fn cmd_get_last_assistant_text(
    sessions: &HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    let output = RpcOutput::new();
    if let Some(session) = sessions.get(&sid) {
        let text: String = session
            .messages()
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(a) => a.content.iter().find_map(|b| match b {
                    AssistantContentBlock::Text(t) if !t.text.is_empty() => Some(t.text.clone()),
                    _ => None,
                }),
                _ => None,
            })
            .unwrap_or_default();
        ok_data(&output, cmd_id, serde_json::json!({"text": text}))
    } else {
        ok_data(&output, cmd_id, serde_json::json!({"text": ""}))
    }
}

// ---------------------------------------------------------------------------
// Command: get_tools
// ---------------------------------------------------------------------------

fn cmd_get_tools(
    sessions: &HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    // Return the standard 9 built-in tools
    let tools: Vec<serde_json::Value> = vec![
        tool_def("calculator", "Evaluate a math expression", serde_json::json!({"type":"object","properties":{"expression":{"type":"string"}},"required":["expression"]})),
        tool_def("echo", "Echo back input", serde_json::json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})),
        tool_def("read", "Read file contents", serde_json::json!({"type":"object","properties":{"file_path":{"type":"string"}},"required":["file_path"]})),
        tool_def("grep", "Search for pattern in files", serde_json::json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]})),
        tool_def("find", "Find files by glob pattern", serde_json::json!({"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]})),
        tool_def("ls", "List directory contents", serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}})),
        tool_def("bash", "Execute a shell command", serde_json::json!({"type":"object","properties":{"command":{"type":"string"}},"required":["command"]})),
        tool_def("write", "Write content to a file", serde_json::json!({"type":"object","properties":{"file_path":{"type":"string"},"content":{"type":"string"}},"required":["file_path","content"]})),
        tool_def("edit", "Search-and-replace in a file", serde_json::json!({"type":"object","properties":{"file_path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}},"required":["file_path","old","new"]})),
    ];
    ok_data(&output, cmd_id, serde_json::json!({"tools": tools}))
}

fn tool_def(name: &str, description: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"name": name, "description": description, "parameters": params})
}

// ---------------------------------------------------------------------------
// Command: set_model / get_available_models
// ---------------------------------------------------------------------------

fn cmd_set_model(
    sessions: &mut HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    let sid = get_sid(req);
    if let Some(session) = sessions.get_mut(&sid) {
        if let Some(model_id) = req["modelId"].as_str().or_else(|| req["model"].as_str()) {
            session.current_model = model_id.to_string();
        }
        if let Some(provider) = req["provider"].as_str() {
            session.current_provider = provider.to_string();
        }
    }
    ok(&output, cmd_id)
}

fn cmd_get_available_models(cmd_id: &str) -> Result<(), String> {
    let output = RpcOutput::new();
    let models: Vec<serde_json::Value> = vec![
        model_entry("opencode", "deepseek-v4-flash"),
        model_entry("opencode", "deepseek-v4-pro"),
        model_entry("anthropic", "claude-opus-4-8"),
        model_entry("anthropic", "claude-sonnet-4-8"),
        model_entry("openai", "gpt-4o"),
        model_entry("openai", "o3"),
        model_entry("deepseek", "deepseek-chat"),
        model_entry("deepseek", "deepseek-reasoner"),
    ];
    ok_data(&output, cmd_id, serde_json::json!({"models": models}))
}

fn model_entry(provider: &str, id: &str) -> serde_json::Value {
    serde_json::json!({"provider": provider, "id": id, "name": id})
}

// ---------------------------------------------------------------------------
// Command: set_thinking_level / cycle_thinking_level
// ---------------------------------------------------------------------------

fn cmd_set_thinking_level(
    sessions: &mut HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    let sid = get_sid(req);
    let level = req["level"].as_str().unwrap_or("medium");
    if let Some(session) = sessions.get_mut(&sid) {
        session.thinking_level = Some(level.to_string());
    }
    ok(&output, cmd_id)
}

fn cmd_cycle_thinking_level(
    sessions: &mut HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    let sid = get_sid(req);
    if let Some(session) = sessions.get_mut(&sid) {
        let levels = ["off", "low", "medium", "high", "xhigh"];
        let current = session.thinking_level.clone().unwrap_or_else(|| "medium".into());
        let idx = levels.iter().position(|l| *l == current).unwrap_or(2);
        let next = levels[(idx + 1) % levels.len()];
        session.thinking_level = Some(next.to_string());
        ok_data(&output, cmd_id, serde_json::json!({
            "thinkingLevel": next,
            "previous": current,
        }));
        return Ok(());
    }
    ok(&output, cmd_id)
}

// ---------------------------------------------------------------------------
// Command: new_session
// ---------------------------------------------------------------------------

async fn cmd_new_session(
    sessions: &mut HashMap<String, RpcSession>,
    cfg: &RpcConfig,
    output: Arc<RpcOutput>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    // Remove existing session for this sid if any
    sessions.remove(&sid);
    // Create a fresh one
    get_or_create_session(sessions, cfg, output, &sid);
    ok(&RpcOutput::new(), cmd_id)
}

// ---------------------------------------------------------------------------
// Command: fork
// ---------------------------------------------------------------------------

async fn cmd_fork(
    sessions: &mut HashMap<String, RpcSession>,
    cfg: &RpcConfig,
    output: Arc<RpcOutput>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let sid = get_sid(req);
    let target_sid = req.get("fromSession").and_then(|s| s.as_str()).unwrap_or("default");
    let new_sid = format!("{}-fork-{}", target_sid, now_ms());

    // Copy messages from source session
    let old_msgs = sessions.get(target_sid).map(|s| s.messages().to_vec()).unwrap_or_default();

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(CalculatorTool));
    tools.register(Box::new(EchoTool));
    tools.register(Box::new(ReadTool));
    tools.register(Box::new(GrepTool));
    tools.register(Box::new(FindTool));
    tools.register(Box::new(LsTool));
    tools.register(Box::new(BashTool));
    tools.register(Box::new(WriteTool));
    tools.register(Box::new(EditTool));

    let rpc_ext = RpcExtension {
        output: Arc::clone(&output),
    };
    let mut exts = ExtensionRegistry::new();
    exts.register(Box::new(rpc_ext));

    let mut agent = Agent::new(
        Arc::clone(&cfg.registry),
        cfg.model.clone(),
        Some("You are a helpful AI assistant with access to tools.".into()),
        tools,
        cfg.agent_config.clone(),
    );
    agent = agent.with_extensions(exts);
    agent = agent.with_messages(old_msgs);

    let session = RpcSession {
        agent,
        output,
        current_provider: cfg.model.provider.clone(),
        current_model: cfg.model.id.clone(),
        thinking_level: cfg.thinking.clone(),
    };
    sessions.insert(new_sid.clone(), session);

    ok_data(&RpcOutput::new(), cmd_id, serde_json::json!({
        "sessionId": new_sid,
        "forkedFrom": target_sid,
    }))
}

// ---------------------------------------------------------------------------
// Command: set_session_name
// ---------------------------------------------------------------------------

fn cmd_set_session_name(
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    // In-memory session name — would persist in real impl
    ok(&output, cmd_id)
}

// ---------------------------------------------------------------------------
// Command: get_agents / get_agent_detail
// ---------------------------------------------------------------------------

fn cmd_get_agents(cmd_id: &str) -> Result<(), String> {
    let output = RpcOutput::new();
    let agents = agent_config::builtin_agents();
    let list: Vec<serde_json::Value> = agents.iter().map(|a| {
        serde_json::json!({
            "name": a.name,
            "description": a.description,
            "model": a.model,
            "tools": a.tools,
            "thinkingLevel": a.thinking_level,
        })
    }).collect();
    ok_data(&output, cmd_id, serde_json::json!({"agents": list}))
}

fn cmd_get_agent_detail(
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    let name = req["agentName"].as_str().or_else(|| req["name"].as_str()).unwrap_or("");
    if name.is_empty() {
        return Err("missing 'agentName' field".into());
    }
    if let Some(agent) = agent_config::find_agent(name) {
        ok_data(&output, cmd_id, serde_json::to_value(&agent).unwrap_or_default())
    } else {
        Err(format!("agent '{name}' not found"))
    }
}

// ---------------------------------------------------------------------------
// Command: get_settings / set_settings
// ---------------------------------------------------------------------------

fn cmd_get_settings(cmd_id: &str) -> Result<(), String> {
    let output = RpcOutput::new();
    let cfg = IonConfig::load();
    ok_data(&output, cmd_id, serde_json::json!({
        "defaultProvider": cfg.default_provider,
        "defaultModel": cfg.default_model,
        "baseUrl": cfg.base_url,
        "providers": cfg.providers,
    }))
}

fn cmd_set_settings(
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    let mut cfg = IonConfig::load();
    if let Some(v) = req.get("defaultProvider").and_then(|v| v.as_str()) {
        cfg.default_provider = Some(v.to_string());
    }
    if let Some(v) = req.get("defaultModel").and_then(|v| v.as_str()) {
        cfg.default_model = Some(v.to_string());
    }
    if let Some(v) = req.get("baseUrl").and_then(|v| v.as_str()) {
        cfg.base_url = Some(v.to_string());
    }
    let _ = cfg.save();
    ok(&output, cmd_id)
}

// ---------------------------------------------------------------------------
// Command: get_system_prompt
// ---------------------------------------------------------------------------

fn cmd_get_system_prompt(
    sessions: &HashMap<String, RpcSession>,
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    ok_data(&output, cmd_id, serde_json::json!({
        "systemPrompt": "You are a helpful AI assistant with access to tools.",
        "appendSystemPrompt": [],
    }))
}

// ---------------------------------------------------------------------------
// Command: compact
// ---------------------------------------------------------------------------

fn cmd_compact(
    _req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    // Context compaction happens automatically via maybe_compact()
    ok(&output, cmd_id)
}

// ---------------------------------------------------------------------------
// Command: export_html
// ---------------------------------------------------------------------------

fn cmd_export_html(
    req: &serde_json::Value,
    cmd_id: &str,
) -> Result<(), String> {
    let output = RpcOutput::new();
    let path = req.get("outputPath").and_then(|v| v.as_str()).unwrap_or("session-export.html");
    ok_data(&output, cmd_id, serde_json::json!({
        "path": path,
        "exported": false,
        "note": "HTML export not implemented in RPC mode. Use `ion --export <file>` on the CLI."
    }))
}
