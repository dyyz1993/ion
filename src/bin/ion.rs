//! `ion` CLI — AI Agent orchestration.
//!
//! Usage:
//!   ion run <message>                 Run agent
//!   ion config set <key> <value>     Set config
//!   ion config show                  Show config
//!   ion submit <message>             Submit task to manager
//!   ion manager start --port 8080    HTTP server
//!   ion help

use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::compact::CompactConfig;
use ion::agent::tool::{ReadTool, GrepTool, FindTool, LsTool, BashTool, WriteTool, EditTool, CalculatorTool, EchoTool, ToolRegistry};
use ion::config::{IonConfig, default_model_for_provider};
use ion::manager::AgentManager;
use ion::types::{PoolOptions, TaskConfig, TaskPayload};
use ion::worker::agent_worker::AgentWorker;
use ion_provider::registry::{ApiRegistry, ModelRegistry};
use ion_provider::types::*;

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "ion",
    version = "0.1.0",
    about = "AI Agent orchestration CLI",
    trailing_var_arg = true
)]
struct Cli {
    /// Messages and @file references to send
    #[arg(required = false)]
    messages: Vec<String>,

    /// Provider name (opencode, anthropic, openai, deepseek…)
    #[arg(long, global = true)]
    provider: Option<String>,

    /// API base URL override
    #[arg(long, global = true)]
    base_url: Option<String>,

    /// API key (falls back to auth.json, config, env vars)
    #[arg(long, global = true)]
    api_key: Option<String>,

    /// Model ID (e.g. deepseek-v4-flash, gpt-4o, claude-opus-4-8)
    #[arg(long, global = true)]
    model: Option<String>,

    /// Comma-separated model list for multi-model switching
    #[arg(long, global = true)]
    models: Option<String>,

    /// Resume a specific session by ID
    #[arg(long, global = true)]
    resume: Option<String>,

    /// Custom system prompt
    #[arg(long, short = 'P', global = true)]
    prompt: Option<String>,

    /// Use a named agent (build, explore, plan) or path to .md file
    #[arg(long, global = true)]
    agent: Option<String>,

    /// Thinking level (off, minimal, low, medium, high, xhigh)
    #[arg(long, global = true)]
    thinking: Option<String>,

    /// Tool allowlist (comma separated)
    #[arg(long, global = true)]
    tools: Option<String>,

    /// Tool blocklist (comma separated)
    #[arg(long, global = true)]
    exclude_tools: Option<String>,

    /// Disable built-in tools
    #[arg(long, global = true, default_value_t = false)]
    no_builtin_tools: bool,

    /// Trust local project files
    #[arg(long, short = 'a', global = true, default_value_t = false)]
    approve: bool,

    /// Ignore local project files
    #[arg(long, global = true, default_value_t = false)]
    no_approve: bool,

    /// Disable network operations at startup
    #[arg(long, global = true, default_value_t = false)]
    offline: bool,

    /// Load extension file (can be used multiple times)
    #[arg(long, short = 'e', global = true)]
    extension: Vec<String>,

    /// Disable extension auto-discovery
    #[arg(long, global = true, default_value_t = false)]
    no_extensions: bool,

    /// Load skill file or directory (can be used multiple times)
    #[arg(long, global = true)]
    skill: Vec<String>,

    /// Disable skill discovery
    #[arg(long, global = true, default_value_t = false)]
    no_skills: bool,

    /// Load prompt template (can be used multiple times)
    #[arg(long, global = true)]
    prompt_template: Vec<String>,

    /// Disable prompt template discovery
    #[arg(long, global = true, default_value_t = false)]
    no_prompt_templates: bool,

    /// Load theme file (can be used multiple times)
    #[arg(long, global = true)]
    theme: Vec<String>,

    /// Disable theme discovery
    #[arg(long, global = true, default_value_t = false)]
    no_themes: bool,

    /// Export session to HTML file
    #[arg(long, global = true)]
    export: Option<String>,

    /// Disable AGENTS.md / CLAUDE.md / GEMINI.md loading
    #[arg(long, global = true, default_value_t = false)]
    no_context_files: bool,

    /// Session name
    #[arg(long, short = 'n', global = true)]
    name: Option<String>,

    /// Append text to system prompt (can be used multiple times)
    #[arg(long, global = true)]
    append_system_prompt: Vec<String>,

    /// Fork from an existing session (creates a new session with its history)
    #[arg(long, global = true)]
    fork: Option<String>,

    /// Session ID to resume or continue
    #[arg(long, global = true)]
    session: Option<String>,

    /// Custom session directory
    #[arg(long, global = true)]
    session_dir: Option<String>,

    /// Continue the last session
    #[arg(long, short = 'c', global = true, default_value_t = false)]
    continue_session: bool,

    /// Run without persisting session
    #[arg(long, global = true, default_value_t = false)]
    no_session: bool,

    /// Maximum conversation turns
    #[arg(long, global = true, default_value_t = 20)]
    max_turns: u64,

    /// Verbose logging
    #[arg(long, short, global = true, default_value_t = false)]
    verbose: bool,

    /// Request JSON output via prompt injection
    #[arg(long, global = true, default_value_t = false)]
    json: bool,

    /// JSON Schema to validate output (e.g. '{"type":"object","properties":{"name":{"type":"string"}}}')
    #[arg(long, global = true)]
    json_schema: Option<String>,

    /// Max retries for JSON schema validation (default: 3)
    #[arg(long, global = true, default_value_t = 3)]
    schema_retries: u32,

    /// Disable all tools
    #[arg(long, global = true, default_value_t = false)]
    no_tools: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Submit {
        message: String,
        #[arg(long, default_value_t = 2)]
        workers: usize,
        #[arg(long, default_value_t = 4)]
        max_workers: usize,
    },
    Status {
        task_id: String,
    },
    Cancel {
        task_id: String,
    },
    Wait {
        task_id: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    List,
    Stats,
    /// Run in RPC mode (JSONL over stdin/stdout)
    Rpc,
    /// List all sessions with stats
    Sessions,
    /// List available agents
    ListAgents,
    /// List available models
    ListModels {
        /// Optional search filter
        search: Option<String>,
    },
    Manager {
        #[command(subcommand)]
        action: ManagerAction,
    },
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ManagerAction {
    Start {
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long, default_value_t = 10)]
        max_workers: usize,
        #[arg(long, default_value_t = 0)]
        min_workers: usize,
    },
    Status,
}

#[derive(Subcommand)]
enum ConfigAction {
    Show,
    Set { key: String, value: String },
    Get { key: String },
}

// ---------------------------------------------------------------------------
// Resolve CLI + config
// ---------------------------------------------------------------------------

struct EffectiveConfig {
    provider: String,
    model: String,
    api_key: Option<String>,
    base_url: Option<String>,
    json: bool,
    json_schema: Option<String>,
    schema_retries: u32,
    prompt: Option<String>,
    append_prompts: Vec<String>,
    thinking: Option<String>,
    max_turns: u64,
    name: Option<String>,
    tools: Option<String>,
    exclude_tools: Option<String>,
    extension: Vec<String>,
    skill: Vec<String>,
    no_tools: bool,
    no_builtin_tools: bool,
    message: String,
}

impl EffectiveConfig {
    /// Parse messages: @file → file contents, otherwise literal text.
    /// Joins all parts with newlines.
    fn parse_messages(cli_messages: &[String]) -> String {
        let mut parts: Vec<String> = Vec::new();
        for arg in cli_messages {
            if let Some(path) = arg.strip_prefix('@') {
                match std::fs::read_to_string(path) {
                    Ok(content) => parts.push(content),
                    Err(e) => {
                        eprintln!("Warning: cannot read file '{path}': {e}");
                        parts.push(arg.clone());
                    }
                }
            } else {
                parts.push(arg.clone());
            }
        }
        parts.join("\n")
    }
}

fn resolve_effective(cli: &Cli) -> EffectiveConfig {
    // Resolve --agent: find and apply agent config
    if let Some(ref agent_name) = cli.agent {
        if let Some(agent) = ion::agent_config::find_agent(agent_name) {
            tracing::info!("loaded agent: {} ({})", agent.name, agent.description);
            // The agent config will be applied after building EffectiveConfig
            // We'll store it in a special field or just override CLI params directly
        }
    }
    let cfg = IonConfig::load();

    let provider = cli
        .provider
        .clone()
        .or_else(|| cfg.default_provider.clone())
        .unwrap_or_else(|| "opencode".into());

    let model = cli
        .model
        .clone()
        .or_else(|| {
            // First model from --models list
            cli.models.as_ref().and_then(|m| m.split(',').next().map(|s| s.trim().to_string()))
        })
        .or_else(|| cfg.default_model.clone())
        .unwrap_or_else(|| default_model_for_provider(&provider).to_string());

    let api_key = cfg.resolve_api_key(cli.api_key.as_deref(), &provider);
    let base_url = cli.base_url.clone().or_else(|| cfg.base_url.clone());

    let mut eff = EffectiveConfig {
        provider,
        model,
        api_key,
        base_url,
        json: cli.json,
        json_schema: cli.json_schema.clone(),
        schema_retries: cli.schema_retries,
        prompt: cli.prompt.clone(),
        append_prompts: cli.append_system_prompt.clone(),
        thinking: cli.thinking.clone(),
        max_turns: cli.max_turns,
        name: cli.name.clone(),
        tools: cli.tools.clone(),
        exclude_tools: cli.exclude_tools.clone(),
        extension: cli.extension.clone(),
        skill: cli.skill.clone(),
        no_tools: cli.no_tools,
        no_builtin_tools: cli.no_builtin_tools,
        message: EffectiveConfig::parse_messages(&cli.messages),
    };

    // Apply --agent config if set
    if let Some(ref agent_name) = cli.agent {
        if let Some(agent) = ion::agent_config::find_agent(agent_name) {
            agent.apply(&mut eff.model, &mut eff.thinking, &mut eff.max_turns, &mut eff.prompt);
        }
    }
    eff
}

fn build_registry_and_model(eff: &EffectiveConfig) -> (Arc<ApiRegistry>, Model) {
    let base_url = eff
        .base_url
        .clone()
        .unwrap_or_else(|| match eff.provider.as_str() {
            "opencode" => "https://opencode.ai/zen/go/v1".to_string(),
            other => {
                eprintln!("Unknown provider '{other}'. Use --base-url or set base_url in config.");
                std::process::exit(1);
            }
        });

    let mut registry = ApiRegistry::new();
    registry.register_builtins();

    let mut model_registry = ModelRegistry::new();
    model_registry.register_builtins();

    let model = model_registry
        .find_model(&eff.model)
        .cloned()
        .unwrap_or_else(|| Model {
            id: eff.model.clone(),
            name: eff.model.clone(),
            api: "openai-completions".into(),
            provider: eff.provider.clone(),
            base_url,
            reasoning: false,
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

fn build_agent_config(eff: &EffectiveConfig) -> AgentConfig {
    AgentConfig {
        max_turns: eff.max_turns,
        max_outer_iterations: 3,
        max_retries: 2,
        retry_base_delay_ms: 1000,
        enable_compact: true,
        compact_config: CompactConfig::default(),
        api_key: eff.api_key.clone(),
        response_format: if eff.json { Some("json_object".into()) } else { None },
        thinking: eff.thinking.clone(),
    }
}

fn build_tools(eff: &EffectiveConfig) -> ToolRegistry {
    let mut tools = ToolRegistry::new();
    if eff.no_tools {
        return tools;
    }
    // Built-in tools (skip if --no-builtin-tools)
    if !eff.no_builtin_tools {
        tools.register(Box::new(ReadTool));
        tools.register(Box::new(GrepTool));
        tools.register(Box::new(FindTool));
        tools.register(Box::new(LsTool));
        tools.register(Box::new(BashTool));
        tools.register(Box::new(WriteTool));
        tools.register(Box::new(EditTool));
        tools.register(Box::new(CalculatorTool));
        tools.register(Box::new(EchoTool));
    }
    // Apply tool filtering (--tools allowlist)
    if let Some(ref allow) = eff.tools {
        let allowed: Vec<&str> = allow.split(',').map(|s| s.trim()).collect();
        tools.filter(allowed);
    }
    // Apply exclude list
    if let Some(ref block) = eff.exclude_tools {
        let blocked: Vec<&str> = block.split(',').map(|s| s.trim()).collect();
        for name in blocked {
            tools.remove(name);
        }
    }
    tools
}

fn init_logging(verbose: bool) {
    let filter = if verbose { "info" } else { "warn" };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| format!("{filter}").parse().unwrap()),
        )
        .with_target(false)
        .try_init();
}

// ---------------------------------------------------------------------------
// Config commands
// ---------------------------------------------------------------------------

async fn cmd_config_show() {
    let cfg = IonConfig::load();
    println!("Config file: {}", IonConfig::path().display());
    println!("{}", serde_json::to_string_pretty(&cfg).unwrap());
}

async fn cmd_config_set(key: &str, value: &str) {
    match key {
        "api-key" | "api_key" => {
            let mut auth = ion::auth::AuthStorage::load();
            auth.api_key = Some(value.into());
            auth.save()
                .unwrap_or_else(|e| eprintln!("Failed to save auth: {e}"));
            println!(
                "API key saved to {} (permissions 600)",
                ion::auth::AuthStorage::path().display()
            );
        }
        "default-provider" | "default_provider" => {
            let mut cfg = IonConfig::load();
            cfg.default_provider = Some(value.into());
            cfg.save()
                .unwrap_or_else(|e| eprintln!("Failed to save config: {e}"));
            println!("Default provider set to {value}");
        }
        "default-model" | "default_model" => {
            let mut cfg = IonConfig::load();
            cfg.default_model = Some(value.into());
            cfg.save()
                .unwrap_or_else(|e| eprintln!("Failed to save config: {e}"));
            println!("Default model set to {value}");
        }
        "base-url" | "base_url" => {
            let mut cfg = IonConfig::load();
            cfg.base_url = Some(value.into());
            cfg.save()
                .unwrap_or_else(|e| eprintln!("Failed to save config: {e}"));
            println!("Base URL set to {value}");
        }
        other => {
            eprintln!("Unknown key: {other}");
            eprintln!("Valid keys: api-key, default-provider, default-model, base-url");
        }
    }
}

async fn cmd_config_get(key: &str) {
    let cfg = IonConfig::load();
    let val = match key {
        "api-key" | "api_key" => cfg.api_key.as_deref(),
        "default-provider" | "default_provider" => cfg.default_provider.as_deref(),
        "default-model" | "default_model" => cfg.default_model.as_deref(),
        "base-url" | "base_url" => cfg.base_url.as_deref(),
        other => {
            eprintln!("Unknown key: {other}");
            return;
        }
    };
    match val {
        Some(v) => println!("{v}"),
        None => println!("(not set)"),
    }
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

async fn cmd_run(
    eff: &EffectiveConfig,
    message: &str,
    _no_tools: bool,
    session_id: &str,
    preloaded: Option<Vec<ion::agent::messages::Message>>,
) {
    let (registry, model) = build_registry_and_model(eff);
    
    let config = build_agent_config(eff);

    let mut tools = build_tools(eff);

    // Load WASM plugins
    for ext_path in &eff.extension {
        if ext_path.ends_with(".wasm") {
            match ion::plugin::WasmPlugin::load(std::path::Path::new(ext_path)) {
                Ok(plugin) => {
                    let plugin = std::sync::Arc::new(std::sync::Mutex::new(plugin));
                    for t in &plugin.lock().unwrap().tools {
                        tools.register(Box::new(ion::plugin::WasmCallingTool {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            parameters: t.parameters.clone(),
                            plugin: plugin.clone(),
                        }));
                        tracing::info!("[wasm] registered tool: {} (WASM-backed)", t.name);
                    }
                }
                Err(e) => {
                    tracing::warn!("[wasm] failed: {e}");
                }
            }
        }
    }

    // Register extension tools into the tool registry (before Agent takes ownership)
    for ext_path in &eff.extension {
        if let Ok(content) = std::fs::read_to_string(ext_path) {
            if let Ok(def) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(tool_defs) = def.get("tools").and_then(|v| v.as_array()) {
                    for tool_def in tool_defs {
                        let name = tool_def.get("name").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
                        let desc = tool_def.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        let params = tool_def.get("parameters").cloned().unwrap_or(serde_json::Value::Null);
                        tools.register(Box::new(ion::agent::tool::GenericTool {
                            name, description: desc, parameters: params,
                        }));
                    }
                }
            }
        }
    }

    // Build system prompt: --prompt > --json > default, then append --append-system-prompt
    let mut sys_prompt = if let Some(ref custom) = eff.prompt {
        custom.clone()
    } else if eff.json {
        "You MUST output valid JSON only, no other text.".into()
    } else if _no_tools {
        "You are a helpful AI assistant.".into()
    } else {
        "You are a helpful AI assistant with access to tools.".into()
    };
    for append in &eff.append_prompts {
        sys_prompt.push_str("\n");
        sys_prompt.push_str(append);
    }
    // Apply skill prompts
    for skill_path in &eff.skill {
        if let Ok(content) = std::fs::read_to_string(skill_path) {
            // Parse frontmatter (--- yaml ---) and body
            let body = if content.starts_with("---") {
                if let Some(end) = content[3..].find("---") {
                    content[3+end+3..].trim()
                } else { content.trim() }
            } else { content.trim() };
            if !body.is_empty() {
                sys_prompt.push_str("\n");
                sys_prompt.push_str(body);
            }
        }
    }
    // Apply extension system prompts
    for ext_path in &eff.extension {
        if let Ok(content) = std::fs::read_to_string(ext_path) {
            if let Ok(def) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(sp) = def.get("system_prompt").and_then(|v| v.as_str()) {
                    sys_prompt.push_str("\n");
                    sys_prompt.push_str(sp);
                }
            }
        }
    }

    let mut agent = Agent::new(registry, model, Some(sys_prompt), tools, config);
    if let Some(msgs) = preloaded {
        agent = agent.with_messages(msgs);
    }
    // Register per-turn session index extension & load extensions
    if !session_id.is_empty() {
        let mut ext_reg = ion::agent::extension::ExtensionRegistry::new();
        ext_reg.register(Box::new(SessionIndexExtension::new(
            session_id, &eff.model, &eff.provider,
        )));
        // Load extensions from --extension flags
        let exts = ion::agent::extension::load_extensions(&eff.extension);
        for e in exts {
            ext_reg.register(e);
        }

        agent = agent.with_extensions(ext_reg);
    }

    tracing::info!("Running agent...");

    // Schema validation loop
    let max_attempts = if eff.json_schema.is_some() {
        eff.schema_retries + 1
    } else {
        1
    };
    let mut retry_prompt = message.to_string();

    for attempt in 1..=max_attempts {
        let prompt = if attempt == 1 { message } else { &retry_prompt };

        match agent.run(prompt).await {
            Ok(()) => {
                let output = extract_assistant_text(&agent).unwrap_or("(no response)");

                // JSON schema validation
                if let Some(ref schema_str) = eff.json_schema {
                    match serde_json::from_str::<serde_json::Value>(output) {
                        Ok(json) => {
                            let schema_val: serde_json::Value =
                                serde_json::from_str(schema_str).unwrap_or_default();
                            match jsonschema::Validator::new(&schema_val) {
                                Ok(validator) => {
                                    if let Err(e) = validator.validate(&json) {
                                        let err_msg = e.to_string();
                                        if attempt < max_attempts {
                                            tracing::warn!(
                                                "Schema mismatch (attempt {attempt}/{max_attempts}): {err_msg}"
                                            );
                                            retry_prompt = format!(
                                                "Your previous output did not match the schema.\n\
                                                 Error: {err_msg}\n\n\
                                                 Your output:\n```json\n{output}\n```\n\n\
                                                 Fix it to match this schema:\n```json\n{schema_str}\n```"
                                            );
                                            continue;
                                        } else {
                                            eprintln!(
                                                "Warning: schema mismatch after {max_attempts} attempts"
                                            );
                                            print_output(output, true);
                                            if eff.json_schema.is_some() {
                                                let mc = agent.messages().len();
                                                let ac = agent
                                                    .messages()
                                                    .iter()
                                                    .filter(|m| matches!(m, Message::Assistant(_)))
                                                    .count();
                                                let tc = agent
                                                    .messages()
                                                    .iter()
                                                    .filter(|m| matches!(m, Message::ToolResult(_)))
                                                    .count();
                                                eprintln!("─── Summary ───");
                                                eprintln!(
                                                    "  msgs={mc} assistant={ac} tools={tc} attempts={max_attempts}"
                                                );
                                            }
                                            save_session(session_id, agent.messages(), &eff.model, &eff.provider, eff.name.as_deref());
                                            return;
                                        }
                                    } else {
                                        print_output(output, true);
                                        if eff.json_schema.is_some() {
                                            let mc = agent.messages().len();
                                            let ac = agent
                                                .messages()
                                                .iter()
                                                .filter(|m| matches!(m, Message::Assistant(_)))
                                                .count();
                                            let tc = agent
                                                .messages()
                                                .iter()
                                                .filter(|m| matches!(m, Message::ToolResult(_)))
                                                .count();
                                            eprintln!("─── Summary ───");
                                            eprintln!(
                                                "  msgs={mc} assistant={ac} tools={tc} attempts={max_attempts}"
                                            );
                                        }
                                        save_session(session_id, agent.messages(), &eff.model, &eff.provider, eff.name.as_deref());
                                        return;
                                    }
                                }
                                Err(e) => {
                                    eprintln!("Warning: invalid schema: {e}");
                                    print_output(output, true);
                                    if eff.json_schema.is_some() {
                                        let mc = agent.messages().len();
                                        let ac = agent
                                            .messages()
                                            .iter()
                                            .filter(|m| matches!(m, Message::Assistant(_)))
                                            .count();
                                        let tc = agent
                                            .messages()
                                            .iter()
                                            .filter(|m| matches!(m, Message::ToolResult(_)))
                                            .count();
                                        eprintln!("─── Summary ───");
                                        eprintln!(
                                            "  msgs={mc} assistant={ac} tools={tc} attempts={max_attempts}"
                                        );
                                    }
                                    save_session(session_id, agent.messages(), &eff.model, &eff.provider, eff.name.as_deref());
                                    return;
                                }
                            }
                        }
                        Err(_) => {
                            if attempt < max_attempts {
                                tracing::warn!("Not valid JSON (attempt {attempt}/{max_attempts})");
                                retry_prompt = format!(
                                    "Your output was not valid JSON:\n{output}\n\nPlease output valid JSON only."
                                );
                                continue;
                            } else {
                                print_output(output, true);
                                if eff.json_schema.is_some() {
                                    let mc = agent.messages().len();
                                    let ac = agent
                                        .messages()
                                        .iter()
                                        .filter(|m| matches!(m, Message::Assistant(_)))
                                        .count();
                                    let tc = agent
                                        .messages()
                                        .iter()
                                        .filter(|m| matches!(m, Message::ToolResult(_)))
                                        .count();
                                    eprintln!("─── Summary ───");
                                    eprintln!(
                                        "  msgs={mc} assistant={ac} tools={tc} attempts={max_attempts}"
                                    );
                                }
                                save_session(session_id, agent.messages(), &eff.model, &eff.provider, eff.name.as_deref());
                                return;
                            }
                        }
                    }
                } else if eff.json {
                    print_output(output, true);
                    if eff.json_schema.is_some() {
                        let mc = agent.messages().len();
                        let ac = agent
                            .messages()
                            .iter()
                            .filter(|m| matches!(m, Message::Assistant(_)))
                            .count();
                        let tc = agent
                            .messages()
                            .iter()
                            .filter(|m| matches!(m, Message::ToolResult(_)))
                            .count();
                        eprintln!("─── Summary ───");
                        eprintln!("  msgs={mc} assistant={ac} tools={tc} attempts={max_attempts}");
                    }
                    save_session(session_id, agent.messages(), &eff.model, &eff.provider, eff.name.as_deref());
                    return;
                } else {
                    println!("{output}");
                    if eff.json_schema.is_some() {
                        let mc = agent.messages().len();
                        let ac = agent
                            .messages()
                            .iter()
                            .filter(|m| matches!(m, Message::Assistant(_)))
                            .count();
                        let tc = agent
                            .messages()
                            .iter()
                            .filter(|m| matches!(m, Message::ToolResult(_)))
                            .count();
                        eprintln!("─── Summary ───");
                        eprintln!("  msgs={mc} assistant={ac} tools={tc} attempts={max_attempts}");
                    }
                    save_session(session_id, agent.messages(), &eff.model, &eff.provider, eff.name.as_deref());
                    return;
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("MissingApiKey") || msg.contains("API key") {
                    eprintln!("No API key found. Run: ion config set api-key <key>");
                } else {
                    eprintln!("Error: {e}");
                }
                std::process::exit(1);
            }
        }
    } // end for

    // Print summary (verbose or schema mode)
    if eff.json_schema.is_some() {
        let msg_count = agent.messages().len();
        let assistant_count = agent
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::Assistant(_)))
            .count();
        let tool_count = agent
            .messages()
            .iter()
            .filter(|m| matches!(m, Message::ToolResult(_)))
            .count();
        let total_input: u64 = agent
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.usage.input),
                _ => None,
            })
            .sum();
        let total_output: u64 = agent
            .messages()
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.usage.output),
                _ => None,
            })
            .sum();

        eprintln!(">>> SUMMARY <<<");
        eprintln!("─── Summary ───");
        eprintln!(
            "  Messages:  {msg_count} total, {assistant_count} assistant, {tool_count} tool calls"
        );
        eprintln!("  Schema attempts:  {max_attempts} total");
        eprintln!("  Token usage:  {total_input} in / {total_output} out");
    }
}

async fn cmd_rpc(eff: &EffectiveConfig) {
    let (registry, model) = build_registry_and_model(eff);
    let config = build_agent_config(eff);

    let rpc_cfg = ion::rpc::RpcConfig {
        registry,
        model,
        agent_config: config,
        thinking: eff.thinking.clone(),
        max_turns: eff.max_turns,
    };

    ion::rpc::handle_rpc(rpc_cfg).await;
}

async fn cmd_sessions() {
    let index = ion::session_index::SessionIndex::load();
    if index.sessions.is_empty() {
        println!("No sessions found.");
        return;
    }
    println!("{:<12} {:<24} {:<10} {:<10} {:<8} {:<8} {}", 
        "ID", "NAME", "MODEL", "TOKENS_IN", "TOKENS_OUT", "MSGS", "UPDATED");
    println!("{}", "-".repeat(100));
    let mut entries: Vec<_> = index.sessions.iter().collect();
    entries.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));
    for (id, meta) in entries.iter().take(20) {
        let short_id = if id.len() > 10 { &id[..10] } else { id.as_str() };
        let name = meta.name.as_deref().unwrap_or("-");
        let ts = {
            let secs = meta.updated_at / 1000;
            let days = secs / 86400;
            let h = (secs % 86400) / 3600;
            let m = (secs % 3600) / 60;
            format!("{}-{} {:02}:{:02}", days / 30 + 1, days % 30 + 1, h, m)
        };
        println!("{:<12} {:<24} {:<10} {:<10} {:<8} {:<8} {}",
            short_id, name, meta.model,
            meta.token_input, meta.token_output,
            meta.message_count, ts,
        );
    }
    println!();
    println!("Total sessions: {} | Total tokens: {} in / {} out",
        index.sessions.len(),
        index.sessions.values().map(|s| s.token_input).sum::<u64>(),
        index.sessions.values().map(|s| s.token_output).sum::<u64>(),
    );
}

async fn cmd_list_agents() {
    let agents = ion::agent_config::builtin_agents();
    println!("{:<16} {:<12} {:<8}  {}", "NAME", "TIER", "TOOLS", "DESCRIPTION");
    println!("{}", "-".repeat(90));
    for a in &agents {
        let tool_count = a.tools.as_ref().map(|t| t.len()).unwrap_or(0);
        let tier = a.tier.as_deref().unwrap_or("-");
        println!("{:<16} {:<12} {:<8}  {}", a.name, tier, tool_count, a.description);
    }
    // Check global agents dir
    let global_dir = ion::agent_config::global_agents_dir();
    if global_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&global_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "md").unwrap_or(false) {
                    if let Some(agent) = ion::agent_config::parse_agent_file(&path) {
                        let tc = agent.tools.as_ref().map(|t| t.len()).unwrap_or(0);
                        let tier = agent.tier.as_deref().unwrap_or("-");
                        println!("{:<16} {:<12} {:<8}  {} (global)", agent.name, tier, tc, agent.description);
                    }
                }
            }
        }
    }
    // Check project agents dir
    if let Some(proj_dir) = ion::agent_config::project_agents_dir() {
        if proj_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&proj_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false) {
                        if let Some(agent) = ion::agent_config::parse_agent_file(&path) {
                            let tc = agent.tools.as_ref().map(|t| t.len()).unwrap_or(0);
                            let tier = agent.tier.as_deref().unwrap_or("-");
                            println!("{:<16} {:<12} {:<8}  {} (project)", agent.name, tier, tc, agent.description);
                        }
                    }
                }
            }
        }
    }
    println!();
    println!("Use --agent <name> to select an agent.");
}

async fn cmd_list_models(search: &Option<String>) {
    use ion_provider::registry::ModelRegistry;
    let mut registry = ModelRegistry::new();
    registry.register_builtins();
    // List all providers
    for provider in ["opencode"] {
        if let Some(model) = registry.get_model(
            provider,
            if search.is_some() {
                ""
            } else {
                "deepseek-v4-flash"
            },
        ) {
            // Just show available
        }
    }
    // Simple approach: iterate known models
    let names = ["deepseek-v4-flash", "deepseek-v4-pro", "gpt-4o"];
    for name in names {
        if let Some(s) = search {
            if !name.contains(s) {
                continue;
            }
        }
        println!("{name}");
    }
    println!();
    println!("Use --model <name> to select a model.");
    println!("Use --provider <name> to select a provider.");
}

async fn cmd_submit(eff: &EffectiveConfig, message: &str, workers: usize, max_workers: usize) {
    let (registry, model) = build_registry_and_model(eff);
    let config = build_agent_config(eff);

    let mgr = AgentManager::new(
        PoolOptions {
            min_workers: workers,
            max_workers,
            ..Default::default()
        },
        TaskConfig { max_retries: 2 },
        {
            let reg = Arc::clone(&registry);
            let mdl = model.clone();
            let cfg = config.clone();
            move |_id| {
                let mut t = ToolRegistry::new();
                t.register(Box::new(ReadTool));
                t.register(Box::new(GrepTool));
                t.register(Box::new(FindTool));
                t.register(Box::new(LsTool));
                t.register(Box::new(BashTool));
                t.register(Box::new(WriteTool));
                t.register(Box::new(EditTool));
                Box::new(
                    AgentWorker::new(Arc::clone(&reg), mdl.clone(), None)
                        .with_tools(t)
                        .with_config(cfg.clone()),
                )
            }
        },
    );

    let id = mgr
        .handle
        .submit(TaskPayload::Prompt(message.into()))
        .await
        .unwrap();
    tracing::info!("Task {id} submitted");

    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if let Some(s) = mgr.handle.status(id).await.ok().flatten() {
                if s.status.is_terminal() {
                    tracing::info!("Done: {s:?}");
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("timeout");
}

async fn cmd_status(_eff: &EffectiveConfig, _task_id: &str) {
    println!("Status: use with a running manager server");
}

async fn cmd_cancel(_eff: &EffectiveConfig, _task_id: &str) {
    println!("Cancel: use with a running manager server");
}

async fn cmd_wait(_eff: &EffectiveConfig, _task_id: &str, _timeout_secs: u64) {
    println!("Wait: use with a running manager server");
}

async fn cmd_list(_eff: &EffectiveConfig) {
    println!("List: use with a running manager server");
}

async fn cmd_stats(_eff: &EffectiveConfig) {
    println!("Stats: use with a running manager server");
}

async fn cmd_manager_start(
    eff: &EffectiveConfig,
    port: u16,
    max_workers: usize,
    min_workers: usize,
) {
    use axum::{
        Json, Router,
        extract::Path,
        http::StatusCode,
        routing::{get, post},
    };

    let (registry, model) = build_registry_and_model(eff);
    let config = build_agent_config(eff);

    let manager = Arc::new(tokio::sync::RwLock::new(AgentManager::new(
        PoolOptions {
            min_workers,
            max_workers,
            ..Default::default()
        },
        TaskConfig { max_retries: 2 },
        {
            let reg = Arc::clone(&registry);
            let mdl = model.clone();
            let cfg = config.clone();
            move |_id| {
                let mut t = ToolRegistry::new();
                t.register(Box::new(ReadTool));
                t.register(Box::new(GrepTool));
                t.register(Box::new(FindTool));
                t.register(Box::new(LsTool));
                t.register(Box::new(BashTool));
                t.register(Box::new(WriteTool));
                t.register(Box::new(EditTool));
                Box::new(
                    AgentWorker::new(Arc::clone(&reg), mdl.clone(), None)
                        .with_tools(t)
                        .with_config(cfg.clone()),
                )
            }
        },
    )));

    let app = Router::new()
        .route(
            "/health",
            get(|| async { Json(serde_json::json!({"status":"ok"})) }),
        )
        .route(
            "/submit/{message}",
            post({
                let m = manager.clone();
                move |Path(msg): Path<String>| async move {
                    let mgr = m.read().await;
                    match mgr.handle.submit(TaskPayload::Prompt(msg)).await {
                        Ok(id) => (
                            StatusCode::OK,
                            Json(serde_json::json!({"task_id": id.to_string()})),
                        ),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error": e.to_string()})),
                        ),
                    }
                }
            }),
        )
        .route(
            "/status/{task_id}",
            get({
                let m = manager.clone();
                move |Path(tid): Path<String>| async move {
                    let id = uuid::Uuid::parse_str(&tid).map(ion::ids::TaskId::from_uuid);
                    let Ok(id) = id else {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({"error":"bad id"})),
                        );
                    };
                    let mgr = m.read().await;
                    match mgr.handle.status(id).await {
                        Ok(Some(s)) => (StatusCode::OK, Json(serde_json::json!(s))),
                        Ok(None) => (
                            StatusCode::NOT_FOUND,
                            Json(serde_json::json!({"error":"not found"})),
                        ),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error":e.to_string()})),
                        ),
                    }
                }
            }),
        )
        .route(
            "/list",
            get({
                let m = manager.clone();
                move || async move {
                    let mgr = m.read().await;
                    match mgr.handle.list().await {
                        Ok(list) => (StatusCode::OK, Json(serde_json::json!(list))),
                        Err(e) => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error":e.to_string()})),
                        ),
                    }
                }
            }),
        )
        .route(
            "/stats",
            get({
                let m = manager.clone();
                move || async move {
                    let mgr = m.read().await;
                    let pool = mgr.handle.pool_stats().await.unwrap_or_default();
                    let queue = mgr.handle.queue_stats().await.unwrap_or_default();
                    (
                        StatusCode::OK,
                        Json(serde_json::json!({"pool": pool, "queue": queue})),
                    )
                }
            }),
        );

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Manager on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let eff = resolve_effective(&cli);

    // Handle --export: export session then exit
    if let Some(ref export_path) = cli.export {
        let session_id = match (&cli.session, cli.continue_session, &cli.resume) {
            (Some(sid), _, _) => sid.clone(),
            (_, _, Some(sid)) => sid.clone(),
            (_, true, _) => std::fs::read_to_string(ion::session_jsonl::last_session_path()).unwrap_or_default().trim().to_string(),
            _ => {
                eprintln!("No session specified. Use --session <id> or --continue");
                return;
            }
        };
        if session_id.is_empty() {
            eprintln!("Session not found");
            return;
        }
        match ion::export::export_session(&session_id, std::path::Path::new(export_path)) {
            Ok(()) => println!("Exported to {export_path}"),
            Err(e) => eprintln!("Export failed: {e}"),
        }
        return;
    }

    // Direct message: ion "hello" or ion @file.txt "prompt"
    if !eff.message.is_empty() {
        let (session_id, preloaded) = resolve_session_id(&cli);
        cmd_run(&eff, &eff.message, cli.no_tools, &session_id, preloaded).await;
        return;
    }

    match &cli.command {
        Some(Commands::Submit {
            message,
            workers,
            max_workers,
        }) => {
            cmd_submit(&eff, message, *workers, *max_workers).await;
        }
        Some(Commands::Status { task_id }) => cmd_status(&eff, task_id).await,
        Some(Commands::Cancel { task_id }) => cmd_cancel(&eff, task_id).await,
        Some(Commands::Wait { task_id, timeout }) => cmd_wait(&eff, task_id, *timeout).await,
        Some(Commands::List) => cmd_list(&eff).await,
        Some(Commands::Stats) => cmd_stats(&eff).await,
        Some(Commands::Rpc) => cmd_rpc(&eff).await,
        Some(Commands::Sessions) => cmd_sessions().await,
        Some(Commands::ListAgents) => cmd_list_agents().await,
        Some(Commands::ListModels { search }) => cmd_list_models(search).await,
        Some(Commands::Manager { action }) => match action {
            ManagerAction::Start {
                port,
                max_workers,
                min_workers,
            } => {
                cmd_manager_start(&eff, *port, *max_workers, *min_workers).await;
            }
            ManagerAction::Status => println!("Manager status: use `manager start` first"),
        },
        Some(Commands::Config { action }) => match action {
            ConfigAction::Show => cmd_config_show().await,
            ConfigAction::Set { key, value } => cmd_config_set(key, value).await,
            ConfigAction::Get { key } => cmd_config_get(key).await,
        },
        None => {
            // No message and no subcommand — show help
            println!("ion: AI Agent orchestration CLI");
            println!("Usage: ion <message>");
            println!("       ion submit <message>");
            println!("       ion manager start --port 8080");
            println!("       ion config set api-key <key>");
            println!("       ion --help");
        }
    }
}

fn extract_assistant_text(agent: &Agent) -> Option<&str> {
    for msg in agent.messages().iter().rev() {
        if let Message::Assistant(a) = msg {
            for block in &a.content {
                if let AssistantContentBlock::Text(t) = block {
                    if !t.text.is_empty() {
                        return Some(&t.text);
                    }
                }
            }
        }
    }
    None
}

fn print_output(output: &str, json_mode: bool) {
    if json_mode {
        match serde_json::from_str::<serde_json::Value>(output) {
            Ok(json) => println!("{}", serde_json::to_string_pretty(&json).unwrap()),
            Err(_) => {
                let extracted: String = output
                    .lines()
                    .skip_while(|l| !l.trim().starts_with("```"))
                    .skip(1)
                    .take_while(|l| !l.trim().starts_with("```"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !extracted.is_empty() {
                    if let Ok(j) = serde_json::from_str::<serde_json::Value>(&extracted) {
                        println!("{}", serde_json::to_string_pretty(&j).unwrap());
                        return;
                    }
                }
                println!("{output}");
            }
        }
    } else {
        println!("{output}");
    }
}

// ---------------------------------------------------------------------------
// Session management (pi JSONL v3)
// ---------------------------------------------------------------------------

fn save_session(id: &str, messages: &[ion::agent::messages::Message], model: &str, provider: &str, name: Option<&str>) {
    let dir = ion::session_jsonl::sessions_dir();
    let _ = std::fs::create_dir_all(&dir);

    let header = ion::session_jsonl::SessionHeader {
        entry_type: "session".into(),
        version: 3,
        id: id.to_string(),
        timestamp: ion::session_jsonl::timestamp_iso(),
        cwd: std::env::current_dir().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        parent_session: None,
    };
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut parent_id = id.to_string();
    for msg in messages {
        let entry = ion::session_jsonl::message_to_entry(msg, &parent_id);
        if let Some(eid) = entry["id"].as_str() {
            parent_id = eid.to_string();
        }
        entries.push(entry);
    }
    ion::session_jsonl::SessionFile::save(id, &header, &entries);
    let _ = std::fs::write(ion::session_jsonl::last_session_path(), id);

    let total_input: u64 = messages.iter().filter_map(|m| match m {
        ion::agent::messages::Message::Assistant(a) => Some(a.usage.input), _ => None
    }).sum();
    let total_output: u64 = messages.iter().filter_map(|m| match m {
        ion::agent::messages::Message::Assistant(a) => Some(a.usage.output), _ => None
    }).sum();
    let assistant_count = messages.iter().filter(|m| matches!(m, ion::agent::messages::Message::Assistant(_))).count() as u32;
    let tool_count = messages.iter().filter(|m| matches!(m, ion::agent::messages::Message::ToolResult(_))).count() as u32;
    ion::session_index::SessionIndex::update(
        id, model, provider, "default", name,
        total_input, total_output,
        messages.len() as u32, assistant_count + tool_count,
    );
}

fn load_session(id: &str) -> Option<Vec<ion::agent::messages::Message>> {
    ion::session_jsonl::SessionFile::load(id).map(|f| f.messages)
}

fn resolve_session_id(cli: &Cli) -> (String, Option<Vec<ion::agent::messages::Message>>) {
    if cli.no_session {
        return (String::new(), None);
    }
    // Fork: load original session, create new ID, keep messages
    if let Some(ref fork_id) = cli.fork {
        if let Some(msgs) = load_session(fork_id) {
            let new_id = uuid::Uuid::new_v4().to_string();
            tracing::info!("forked from {fork_id} → {new_id}");
            return (new_id, Some(msgs));
        }
    }
    if let Some(ref sid) = cli.resume {
        let msgs = load_session(sid);
        return (sid.clone(), msgs);
    }
    if let Some(ref sid) = cli.session {
        let msgs = load_session(sid);
        return (sid.clone(), msgs);
    }
    if cli.continue_session {
        if let Ok(id) = std::fs::read_to_string(ion::session_jsonl::last_session_path()) {
            let id = id.trim();
            let msgs = load_session(id);
            return (id.to_string(), msgs);
        }
    }
    // Default: no session persistence
    (String::new(), None)
}

// ---------------------------------------------------------------------------
// SessionIndexExtension — per-turn real-time index update
// ---------------------------------------------------------------------------

use ion::agent::extension::{Extension, TurnContext};
use ion::agent::messages::*;

struct SessionIndexExtension {
    session_id: String,
    model: String,
    provider: String,
}

impl SessionIndexExtension {
    fn new(session_id: &str, model: &str, provider: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            model: model.to_string(),
            provider: provider.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Extension for SessionIndexExtension {
    async fn on_turn_end(&self, ctx: &TurnContext) -> ion::agent::error::AgentResult<()> {
        if self.session_id.is_empty() { return Ok(()); }
        let total_input: u64 = ctx.messages.iter().filter_map(|m| match m {
            Message::Assistant(a) => Some(a.usage.input), _ => None
        }).sum();
        let total_output: u64 = ctx.messages.iter().filter_map(|m| match m {
            Message::Assistant(a) => Some(a.usage.output), _ => None
        }).sum();
        let assistant_count = ctx.messages.iter().filter(|m| matches!(m, Message::Assistant(_))).count() as u32;
        let tool_count = ctx.messages.iter().filter(|m| matches!(m, Message::ToolResult(_))).count() as u32;
        ion::session_index::SessionIndex::update(
            &self.session_id, &self.model, &self.provider, "default", None,
            total_input, total_output,
            ctx.messages.len() as u32, assistant_count + tool_count,
        );
        Ok(())
    }
    async fn on_model_select(&self, ctx: &ion::agent::extension::ModelSelectContext) -> ion::agent::error::AgentResult<()> {
        if self.session_id.is_empty() { return Ok(()); }
        ion::session_index::SessionIndex::update(
            &self.session_id, &ctx.new_model, &ctx.new_provider, "default", None,
            0, 0, 0, 0,
        );
        Ok(())
    }
}
