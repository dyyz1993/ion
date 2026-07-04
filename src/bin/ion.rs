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
use ion::agent::tool::{ReadTool, GrepTool, FindTool, LsTool, BashTool, WriteTool, EditTool, CalculatorTool, EchoTool, GitStatusTool, GitDiffTool, GitLogTool, GitAddTool, GitCommitTool, GitBranchTool, ToolRegistry};
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

    /// Team mode: start a self-organizing agent team for current project
    #[arg(long, global = true, default_value_t = false)]
    team: bool,

    /// Serve mode: run as global bus (RPC + optional WebSocket), no auto-run
    #[arg(long, global = true, default_value_t = false)]
    serve: bool,

    /// WebSocket port for --serve mode
    #[arg(long, global = true, default_value_t = 8080)]
    ws: u16,

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
    /// Launch the TUI dashboard
    Dashboard,
    /// RPC client: send one command to a running Manager via Unix socket.
    ///   ion rpc --method list_sessions
    ///   ion rpc --method create_session --params '{"agent":"coordinator"}'
    ///   ion rpc --session <id> --method spawn_worker --params '{...}'
    ///   ion rpc --session <id> --method prompt --params '{"text":"hi"}'
    Rpc {
        /// Target session id (omit for Manager-level commands like list_sessions)
        #[arg(long)]
        session: Option<String>,
        /// RPC method name
        #[arg(long)]
        method: String,
        /// JSON params (string; will be parsed)
        #[arg(long, default_value = "{}")]
        params: String,
    },
    /// Subscribe to real-time events from a session or plugin.
    ///   ion subscribe --session sess_xxx
    ///   ion subscribe --session sess_xxx --plugin memory
    ///   ion subscribe --plugin memory
    /// Ctrl+C to disconnect.
    Subscribe {
        /// Session to subscribe to
        #[arg(long)]
        session: Option<String>,
        /// Plugin name to filter (omit for all events)
        #[arg(long)]
        plugin: Option<String>,
    },
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
    no_extensions: bool,
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
        no_extensions: cli.no_extensions,
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
    let cfg = IonConfig::load();

    // Resolve base_url: CLI override → config custom provider → config base_url
    // → auth.json provider_base_urls → builtin model base_url → known defaults
    let auth = ion::auth::AuthStorage::load();
    let base_url = eff
        .base_url
        .clone()
        .or_else(|| {
            // Look up custom provider definition in config
            cfg.providers.get(&eff.provider).map(|p| p.base_url.clone())
        })
        .or_else(|| cfg.base_url.clone())
        .or_else(|| auth.provider_base_urls.get(&eff.provider).cloned())
        .or_else(|| {
            // 最后兜底：去 ModelRegistry 找 builtin model 的 base_url
            let mut mr = ion_provider::registry::ModelRegistry::new();
            mr.register_builtins();
            mr.find_model(&eff.model).map(|m| m.base_url.clone())
        })
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

    let mut model = model_registry
        .find_model(&eff.model)
        .cloned()
        .unwrap_or_else(|| {
            // Check if this model is defined in a custom provider
            if let Some(cp) = cfg.providers.get(&eff.provider) {
                if let Some(cm) = cp.models.iter().find(|m| m.id == eff.model) {
                    return Model {
                        id: cm.id.clone(),
                        name: cm.name.clone().unwrap_or_else(|| cm.id.clone()),
                        api: cp.api.clone(),
                        provider: eff.provider.clone(),
                        base_url: cp.base_url.clone(),
                        reasoning: cm.reasoning.unwrap_or(false),
                        input: vec!["text".into()],
                        cost: Cost {
                            input: cm.cost.as_ref().and_then(|c| Some(c.input)).unwrap_or(0.0),
                            output: cm.cost.as_ref().and_then(|c| Some(c.output)).unwrap_or(0.0),
                            cache_read: 0.0,
                            cache_write: 0.0,
                        },
                        context_window: cm.context_window.unwrap_or(128000),
                        max_tokens: cm.max_tokens.unwrap_or(8192),
                        compat: None,
                        headers: cp.headers.clone(),
                    };
                }
            }
            // Fallback: construct from effective config
            Model {
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
            }
        });

    // 如果 auth.json 里有该 provider 的 base_url 覆盖（比如代理），
    // 用它替换 model.base_url（builtin model 的 base_url 是直连，可能不通）。
    if let Some(override_url) = auth.provider_base_urls.get(&eff.provider) {
        if !override_url.is_empty() {
            model.base_url = override_url.clone();
        }
    }

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
        retry_config: None,
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
        tools.register(Box::new(GitStatusTool));
        tools.register(Box::new(GitDiffTool));
        tools.register(Box::new(GitLogTool));
        tools.register(Box::new(GitAddTool));
        tools.register(Box::new(GitCommitTool));
        tools.register(Box::new(GitBranchTool));
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
        .with_writer(std::io::stderr)
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

    // WASM plugin registry (hot‑pluggable — used by worker RPC too)
    let plugin_registry = std::sync::Arc::new(ion::plugin::PluginRegistry::new());

    // ── WASM 插件自动发现（优先于 --extension）──
    // 扫描 ~/.ion/agent/extensions/ 和 {cwd}/.ion/extensions/ 下的 .wasm 文件
    if !eff.no_extensions {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let ext_dirs: Vec<std::path::PathBuf> = vec![
            ion::paths::extensions_dir(),
            ion::paths::project_extensions_dir(&cwd),
        ];
        for dir in &ext_dirs {
            if !dir.exists() { continue; }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "wasm").unwrap_or(false) {
                        let canonical = std::fs::canonicalize(&path)
                            .unwrap_or_else(|_| path.to_path_buf());
                        let canonical_str = canonical.to_string_lossy().to_string();
                        let ext_name = ion::plugin::ext_name_from_path(&canonical_str);
                        match plugin_registry.add(&canonical_str) {
                            Ok(tool_defs) => {
                                for td in &tool_defs {
                                    tools.register(Box::new(ion::plugin::WasmCallingTool {
                                        name: td.name.clone(),
                                        description: td.description.clone(),
                                        parameters: td.parameters.clone(),
                                        plugin_path: canonical_str.clone(),
                                        ext_name: ext_name.clone(),
                                        registry: plugin_registry.clone(),
                                    }));
                                    tracing::info!("[wasm] auto-discovered {ext_name}: {}", td.name);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("[wasm] failed to load {}: {e}", path.display());
                            }
                        }
                    }
                }
            }
        }
    }

    // Load WASM plugins via the registry (from --extension flags)
    for ext_path in &eff.extension {
        if ext_path.ends_with(".wasm") {
            let abs = std::path::Path::new(ext_path);
            // Determine canonical path before calling plugin_registry.add(),
            // so WasmCallingTool holds the canonicalised path.
            let canonical = std::fs::canonicalize(abs)
                .unwrap_or_else(|_| abs.to_path_buf());
            let canonical_str = canonical.to_string_lossy().to_string();

            match plugin_registry.add(&canonical_str) {
                Ok(tool_defs) => {
                    let ext_name = ion::plugin::ext_name_from_path(&canonical_str);
                    for td in &tool_defs {
                        tools.register(Box::new(ion::plugin::WasmCallingTool {
                            name: td.name.clone(),
                            description: td.description.clone(),
                            parameters: td.parameters.clone(),
                            plugin_path: canonical_str.clone(),
                            ext_name: ext_name.clone(),
                            registry: plugin_registry.clone(),
                        }));
                        tracing::info!("[wasm] registered tool: {} (WASM-backed)", td.name);
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

    // Check if plan tools are loaded (before tools is moved into Agent)
    let has_plan_tools = tools.get("plan_enter").is_some();

    let mut agent = Agent::new(registry, model, Some(sys_prompt), tools, config);
    if let Some(msgs) = preloaded {
        agent = agent.with_messages(msgs);
    }
    let mut ext_reg = ion::agent::extension::ExtensionRegistry::new();

    // Register per-turn session index extension if session is active
    if !session_id.is_empty() {
        ext_reg.register(Box::new(SessionIndexExtension::new(
            session_id, &eff.model, &eff.provider,
        )));
    }

    // Load extensions from --extension flags
    let exts = ion::agent::extension::load_extensions(&eff.extension);
    for e in exts {
        ext_reg.register(e);
    }

    // Auto-register PlanExtension if plan_enter tool was loaded from a WASM plugin
    if has_plan_tools {
        ext_reg.register(Box::new(ion::agent::plan_extension::PlanExtension::new()));
        tracing::info!("[plan] PlanExtension auto-registered (plan tools detected)");
    }

    agent = agent.with_extensions(ext_reg);

    tracing::info!("Running agent...");

    // Schema validation loop
    let max_attempts = if eff.json_schema.is_some() {
        eff.schema_retries + 1
    } else {
        1
    };
    let mut retry_prompt = message.to_string();

    // Inject session context into the plugin registry so WASM plugin data
    // host functions know where to read/write.
    {
        let mut ctx = plugin_registry.ctx.write().unwrap();
        ctx.session_id = session_id.to_string();
        ctx.cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        ctx.project_root = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
    }

    for attempt in 1..=max_attempts {
        let prompt = if attempt == 1 { message } else { &retry_prompt };

        match agent.run(prompt).await {
            Ok(()) => {
                let output = extract_assistant_text(&agent).unwrap_or_else(|| "(no response)".into());

                // JSON schema validation
                if let Some(ref schema_str) = eff.json_schema {
                    match serde_json::from_str::<serde_json::Value>(&output) {
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
                                            print_output(&output, true);
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
                                        print_output(&output, true);
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
                                    print_output(&output, true);
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
                                print_output(&output, true);
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
                    print_output(&output, true);
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

/// RPC client: 连 Manager 的 Unix socket，发一条命令，打印响应，退出。
/// 让外部脚本能直接驱动 Manager / 任意 session，不跑 team 也能验证 worker 机制。
async fn cmd_rpc(session: Option<&str>, method: &str, params: &str) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sock_path = ion::paths::manager_socket_path();
    let mut stream = match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ Cannot connect to Manager at {}\n   先启动: ion manager start\n   错误: {e}",
                sock_path.display());
            std::process::exit(1);
        }
    };

    let params_val: serde_json::Value = serde_json::from_str(params).unwrap_or_else(|e| {
        eprintln!("⚠ params 不是合法 JSON ({e})，用 {{}} 代替");
        serde_json::Value::Object(serde_json::Map::new())
    });

    let mut req = serde_json::json!({
        "id": "rpc-client",
        "method": method,
        "params": params_val,
    });
    if let Some(sid) = session {
        req["session"] = serde_json::json!(sid);
    }

    let req_line = format!("{req}\n");
    if let Err(e) = stream.write_all(req_line.as_bytes()).await {
        eprintln!("❌ write socket failed: {e}");
        std::process::exit(1);
    }
    let _ = stream.flush().await;

    // 读一行响应（一问一答协议）
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match reader.read_line(&mut line).await {
        Ok(0) => eprintln!("(Manager closed connection without response)"),
        Ok(_) => {
            // 美化输出
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or(line));
            } else {
                print!("{line}");
            }
        }
        Err(e) => eprintln!("❌ read socket failed: {e}"),
    }
}

/// Subscribe to real-time events from a session or plugin.
/// Connects to Manager socket, sends subscribe, prints events line by line.
async fn cmd_subscribe(session: Option<&str>, plugin: Option<&str>) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sock_path = ion::paths::manager_socket_path();
    let mut stream = match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ Cannot connect to Manager at {}\n   先启动: ion manager start\n   错误: {e}", sock_path.display());
            std::process::exit(1);
        }
    };

    let mut req = serde_json::json!({"method": "subscribe"});
    if let Some(sid) = session { req["session"] = serde_json::json!(sid); }
    if let Some(p) = plugin { req["plugin"] = serde_json::json!(p); }

    let req_line = format!("{req}\n");
    if stream.write_all(req_line.as_bytes()).await.is_err() {
        eprintln!("❌ write failed");
        return;
    }
    let _ = stream.flush().await;

    // 读事件流直到断开
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line).await.is_ok() && !line.is_empty() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            println!("{}", serde_json::to_string_pretty(&v).unwrap_or(line.trim().to_string()));
        } else {
            print!("{line}");
        }
        line.clear();
    }
    eprintln!("(disconnected)");
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

async fn cmd_submit(eff: &EffectiveConfig, message: &str, _workers: usize, _max_workers: usize) {
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use ion::worker_registry::{WorkerCreateConfig, WorkerRegistry};

    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    tracing::info!("Submitting: {}", message);
    {
        let mut reg = registry.lock().await;
        let w = reg.create_worker(WorkerCreateConfig {
            model: Some(eff.model.clone()),
            provider: Some(eff.provider.clone()),
            ..Default::default()
        }).await.unwrap_or_else(|e| panic!("{e}"));
        tracing::info!("Worker: {}", w.worker_id);
        
        // Send prompt
        let _ = reg.send_to_worker(&w.worker_id, "prompt",
            serde_json::json!({"text": message})).await;
    }
    
    // Wait for execution
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;
    
    // Get result
    {
        let mut reg = registry.lock().await;
        let workers = reg.list_workers();
        if let Some(w) = workers.first() {
            match reg.send_to_worker(&w.worker_id, "get_last_assistant_text", serde_json::json!({})).await {
                Ok(r) => println!("{}", r.get("data").and_then(|v| v.as_str()).unwrap_or("(no response)")),
                Err(e) => eprintln!("Error: {e}"),
            }
            let _ = reg.kill_worker(&w.worker_id);
        }
    }
}

async fn cmd_submit_old(eff: &EffectiveConfig, message: &str, workers: usize, max_workers: usize) {
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


#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let eff = resolve_effective(&cli);

    if let Some(ref export_path) = cli.export {
        let session_id = match (&cli.session, cli.continue_session, &cli.resume) {
            (Some(sid), _, _) => sid.clone(),
            (_, _, Some(sid)) => sid.clone(),
            (_, true, _) => std::fs::read_to_string(ion::session_jsonl::last_session_path()).unwrap_or_default().trim().to_string(),
            _ => { eprintln!("No session. Use --session or --continue-session"); return; }
        };
        if !session_id.is_empty() {
            match ion::export::export_session(&session_id, std::path::Path::new(export_path)) {
                Ok(()) => println!("Exported to {export_path}"),
                Err(e) => eprintln!("Export failed: {e}"),
            }
        }
        return;
    }

    // ── 兼容处理：消息末尾带 --team ──
    let mut effective_team = cli.team;
    let mut effective_message = eff.message.clone();
    if !effective_team && effective_message.ends_with("--team") {
        effective_team = true;
        let new_len = effective_message.len().saturating_sub("--team".len());
        effective_message = effective_message[..new_len].trim().to_string();
    }

    // ── --serve: 全局总线模式 ──
    if cli.serve {
        cmd_manager_start(&cli, cli.ws, 10, 2).await;
        return;
    }

    // ── --team: 单项目团队模式 ──
    if effective_team {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let msg = if effective_message.is_empty() { "根据 PRD 开发项目".to_string() } else { effective_message };
        cmd_team(&cwd, &msg, 4).await;
        return;
    }

    if !effective_message.is_empty() {
        let (session_id, preloaded) = resolve_session_id(&cli);
        cmd_run(&eff, &eff.message, cli.no_tools, &session_id, preloaded).await;
        return;
    }

    match &cli.command {
        Some(Commands::Submit { message, workers, max_workers }) => {
            cmd_submit(&eff, message, *workers, *max_workers).await;
        }
        Some(Commands::Status { task_id }) => cmd_status(&eff, task_id).await,
        Some(Commands::Cancel { task_id }) => cmd_cancel(&eff, task_id).await,
        Some(Commands::Wait { task_id, timeout }) => cmd_wait(&eff, task_id, *timeout).await,
        Some(Commands::List) => cmd_list(&eff).await,
        Some(Commands::Stats) => cmd_stats(&eff).await,
        Some(Commands::Manager { action }) => match action {
            ManagerAction::Start { port, max_workers, min_workers } => {
                cmd_manager_start(&cli, *port, *max_workers, *min_workers).await;
            }
            ManagerAction::Status => println!("Manager status: use `manager start` first"),
        },
        Some(Commands::Config { action }) => match action {
            ConfigAction::Show => cmd_config_show().await,
            ConfigAction::Set { key, value } => cmd_config_set(key, value).await,
            ConfigAction::Get { key } => cmd_config_get(key).await,
        },
        Some(Commands::Dashboard) => {
            if let Err(e) = ion::tui::run_dashboard().await {
                eprintln!("Dashboard error: {e}");
            }
        }
        Some(Commands::Rpc { session, method, params }) => {
            cmd_rpc(session.as_deref(), method, params).await;
        }
        Some(Commands::Sessions) => cmd_sessions().await,
        Some(Commands::Subscribe { session, plugin }) => cmd_subscribe(session.as_deref(), plugin.as_deref()).await,
        Some(Commands::ListAgents) => cmd_list_agents().await,
        Some(Commands::ListModels { search }) => cmd_list_models(search).await,
        None => {
            println!("ion: AI Agent orchestration CLI");
            println!("Usage: ion <message>");
            println!("       ion submit <message>");
            println!("       ion manager start");
            println!("       ion config set api-key <key>");
            println!("       ion --help");
        }
    }
}

async fn cmd_manager_start(
    _cli: &Cli,
    _port: u16,
    _max_workers: usize,
    _min_workers: usize,
) {
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use ion::worker_registry::{WorkerCreateConfig, WorkerRegistry};

    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    let event_bus = Arc::new(tokio::sync::Mutex::new(ion::event_bus::PluginEventBus::new()));

    // ── Manager 单例检查 + Unix socket 启动 ──
    // PID 文件防重复启动；Unix socket 让外部 `ion rpc` 能连进来。
    if let Some(pid) = ion::paths::manager_running() {
        eprintln!("❌ Manager already running (pid {pid}). Stop it first or use `ion rpc` to connect.");
        return;
    }
    let sock_path = ion::paths::manager_socket_path();
    // 清理 stale socket 文件（上次崩溃残留）
    let _ = std::fs::remove_file(&sock_path);
    let listener = match tokio::net::UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("❌ Failed to bind Unix socket at {}: {e}", sock_path.display());
            return;
        }
    };
    // 写 PID 文件
    let pid_path = ion::paths::manager_pid_path();
    let _ = std::fs::write(&pid_path, std::process::id().to_string());
    eprintln!("🔌 Manager listening on Unix socket: {}", sock_path.display());

    // socket accept loop —— 支持两种模式：
    //   RPC mode（默认）：一问一答，返回后关闭
    //   Stream mode（subscribe）：长连接，持续推事件
    let sock_registry = Arc::clone(&registry);
    let sock_event_bus = Arc::clone(&event_bus);
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        use std::collections::HashMap;
        let reader_timeout = std::time::Duration::from_secs(600);
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let reg = Arc::clone(&sock_registry);
                    let ev_bus = Arc::clone(&sock_event_bus);
                    tokio::spawn(async move {
                        let (read_half, mut write_half) = stream.into_split();
                        let mut reader = BufReader::new(read_half);
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.is_ok() {
                            let line = line.trim().to_string();
                            if !line.is_empty() {
                                let cmd: serde_json::Value = match serde_json::from_str(&line) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        let resp = serde_json::json!({
                                            "type":"response","id":null,
                                            "success":false,"error":format!("invalid JSON: {e}")
                                        });
                                        let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                        return;
                                    }
                                };
                                let method = cmd.get("method").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                let session = cmd.get("session").and_then(|v| v.as_str()).map(|s| s.to_string());

                                // ── Stream mode: subscribe ──
                                if method == "subscribe" {
                                    let plugin = cmd.get("plugin").and_then(|v| v.as_str()).unwrap_or("");
                                    let session = cmd.get("session").and_then(|v| v.as_str()).map(|s| s.to_string());

                                    if plugin.is_empty() && session.is_some() {
                                        // ── Instance subscribe：订阅 worker 原始事件流 ──
                                        // 无 --plugin 有 --session → 收 text_delta / agent_start / agent_end 等
                                        let sid = session.as_ref().unwrap();
                                        let mut inner_reg = reg.lock().await;
                                        let worker_opt = inner_reg.workers.values()
                                            .find(|w| w.session_id == *sid)
                                            .map(|w| w.worker_id.clone());
                                        if let Some(wid) = worker_opt {
                                            let mut rx = match inner_reg.subscribe(&wid) {
                                                Ok(r) => r,
                                                Err(e) => {
                                                    let resp = serde_json::json!({"type":"error","error":e});
                                                    let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                                    return;
                                                }
                                            };
                                            drop(inner_reg);
                                            let ack = serde_json::json!({"type":"subscribed","session":sid,"stream":"instance"});
                                            let _ = write_half.write_all(format!("{ack}\n").as_bytes()).await;
                                            let _ = write_half.flush().await;
                                            loop {
                                                match rx.recv().await {
                                                    Some(msg) => {
                                                        let out = serde_json::json!({
                                                            "type": "instance_event",
                                                            "session": sid,
                                                            "event": msg.get("event").cloned().unwrap_or(msg),
                                                        });
                                                        if write_half.write_all(format!("{out}\n").as_bytes()).await.is_err() { break; }
                                                        let _ = write_half.flush().await;
                                                    }
                                                    None => break,
                                                }
                                            }
                                        } else {
                                            let resp = serde_json::json!({"type":"error","error":"session not found"});
                                            let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                        }
                                        return;
                                    }

                                    // ── Plugin subscribe：通过 EventBus ──
                                    let mut bus = ev_bus.lock().await;
                                    let rx = if !plugin.is_empty() {
                                        if let Some(ref sid) = session {
                                            bus.subscribe_with_session(plugin, sid)
                                        } else {
                                            bus.subscribe(plugin)
                                        }
                                    } else {
                                        bus.subscribe_all()
                                    };
                                    drop(bus);
                                    // 返回 subscribed ack
                                    let ack = serde_json::json!({
                                        "type":"subscribed",
                                        "plugin": plugin,
                                        "session": session,
                                    });
                                    let _ = write_half.write_all(format!("{ack}\n").as_bytes()).await;
                                    let _ = write_half.flush().await;
                                    // 持续推事件
                                    use tokio::sync::mpsc;
                                    let mut rx = rx;
                                    loop {
                                        match rx.recv().await {
                                            Some(event) => {
                                                let msg = serde_json::json!({
                                                    "type": "plugin_event",
                                                    "plugin": event.plugin,
                                                    "customType": event.custom_type,
                                                    "session": event.session,
                                                    "persisted": event.persisted,
                                                    "visibility": match event.visibility {
                                                        ion::event_bus::EventVisibility::LlmAndUi => "llm_and_ui",
                                                        ion::event_bus::EventVisibility::UiOnly => "ui_only",
                                                    },
                                                    "correlation_id": event.correlation_id,
                                                    "data": event.data,
                                                });
                                                if write_half.write_all(format!("{msg}\n").as_bytes()).await.is_err() {
                                                    break; // client disconnected
                                                }
                                                let _ = write_half.flush().await;
                                            }
                                            None => break, // channel closed
                                        }
                                    }
                                    return;
                                }

                                // ── Overview stream: subscribe_overview ──
                                if method == "subscribe_overview" {
                                    let (initial, rx) = {
                                        let mut reg = reg.lock().await;
                                        let overview = reg.get_overview();
                                        let rx = reg.subscribe_overview();
                                        (overview, rx)
                                    };
                                    // Return initial snapshot
                                    let ack = serde_json::json!({
                                        "type": "response",
                                        "id": cmd.get("id"),
                                        "success": true,
                                        "data": {
                                            "stream": "overview",
                                            "initial": initial,
                                        }
                                    });
                                    if write_half.write_all(format!("{ack}\n").as_bytes()).await.is_err() { return; }
                                    let _ = write_half.flush().await;
                                    // Continuously push subsequent changes
                                    let mut rx = rx;
                                    loop {
                                        match rx.recv().await {
                                            Some(snapshot) => {
                                                let msg = serde_json::json!({
                                                    "type": "overview_snapshot",
                                                    "data": snapshot,
                                                });
                                                if write_half.write_all(format!("{msg}\n").as_bytes()).await.is_err() { break; }
                                                let _ = write_half.flush().await;
                                            }
                                            None => break,
                                        }
                                    }
                                    return;
                                }

                                // ── RPC mode（以下为现有逻辑：session 转发 + 等响应）──
                                let session = cmd.get("session").and_then(|v| v.as_str()).map(|s| s.to_string());
                                if let Some(ref sid) = session {
                                    let mut inner_reg = reg.lock().await;
                                    // 找到 worker
                                    if let Some(wid) = inner_reg.workers.values()
                                        .find(|w| w.session_id == *sid)
                                        .map(|w| w.worker_id.clone())
                                    {
                                        // 订阅 worker 事件
                                        let mut rx = match inner_reg.subscribe(&wid) {
                                            Ok(rx) => rx,
                                            Err(e) => {
                                                let resp = serde_json::json!({
                                                    "type":"response","id":cmd.get("id"),
                                                    "success":false,"error":e
                                                });
                                                let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                                return;
                                            }
                                        };
                                        // 发命令
                                        let params = cmd.get("params").cloned().unwrap_or_default();
                                        match inner_reg.send_command(&wid, &method, params).await {
                                            Ok(()) => {
                                                        drop(inner_reg); // 释放 lock，让 pump 能继续
                                                        // 等响应（最大 600s，覆盖 spawn_worker 等 long run）
                                                        let timeout_at = std::time::Instant::now()
                                                            + std::time::Duration::from_secs(600);
                                                        let mut resp = None;
                                                        loop {
                                                            let remaining = timeout_at
                                                                .checked_duration_since(std::time::Instant::now())
                                                                .unwrap_or_default();
                                                            if remaining.is_zero() { break; }
                                                        tokio::select! {
                                                            ev = rx.recv() => {
                                                                match ev {
                                                                    Some(msg) => {
                                                                        let msg_type = msg.get("type")
                                                                            .and_then(|v| v.as_str()).unwrap_or("");
                                                                        if msg_type == "response" {
                                                                            resp = Some(msg);
                                                                            break;
                                                                        }
                                                                    }
                                                                    None => break,
                                                                }
                                                            }
                                                            _ = tokio::time::sleep(remaining) => {}
                                                        }
                                                        }
                                                let result = resp.unwrap_or_else(|| serde_json::json!({
                                                    "type":"response","success":false,"error":"timeout waiting for worker response"
                                                }));
                                                let _ = write_half.write_all(format!("{result}\n").as_bytes()).await;
                                            }
                                            Err(e) => {
                                                let resp = serde_json::json!({
                                                    "type":"response","id":cmd.get("id"),
                                                    "success":false,"error":format!("send_command failed: {e}")
                                                });
                                                let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                            }
                                        }
                                    } else {
                                        // session 不存在？创建？不，让 handle_manager_command 处理
                                        drop(inner_reg);
                                        let resp = handle_manager_command(&reg, cmd).await;
                                        let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                    }
                                } else {
                                    // 3. Manager 级命令：直接执行，不等
                                    let resp = handle_manager_command(&reg, cmd).await;
                                    let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                }
                            }
                        }
                        // 不 flush/close — stream drop 时自动关
                    });
                }
                Err(e) => {
                    eprintln!("[socket] accept error: {e}");
                    break;
                }
            }
        }
    });

    // 订阅全局事件（worker_created / worker_destroyed / project_changed）
    let global_rx = registry.lock().await.subscribe_global();

    // 后台任务 1：事件 pump — 遍历所有 worker，drain_events 推送到 stdout + EventBus
    let pump_registry = Arc::clone(&registry);
    let pump_event_bus = Arc::clone(&event_bus);
    tokio::spawn(async move {
        // subscriber channels 放在 lock 外面，避免和 send_to_worker 死锁
        let mut subs: std::collections::HashMap<String, (String, tokio::sync::mpsc::Receiver<serde_json::Value>)> = std::collections::HashMap::new();
        loop {
            // 1. 检查新 worker（短暂锁，subscribe + drain_events）
            {
                let mut reg = pump_registry.lock().await;
                let current_ids: Vec<String> = reg.workers.keys().cloned().collect();
                for wid in &current_ids {
                    if !subs.contains_key(wid) {
                        let session_id = reg.workers.get(wid).map(|r| r.session_id.clone()).unwrap_or_default();
                        if let Ok(rx) = reg.subscribe(wid) {
                            subs.insert(wid.clone(), (session_id, rx));
                        }
                    }
                    // 关键：drain_events 把 stdout_rx 的消息转发给 subscribers
                    reg.drain_events(wid, 10).await;
                }
                // 清理已死的 worker
                let dead: Vec<String> = subs.keys()
                    .filter(|wid| !current_ids.contains(wid))
                    .cloned()
                    .collect();
                for wid in dead { subs.remove(&wid); }
            }
            // 2. 无锁读取 subscriber 事件（不阻塞 send_to_worker）
            for (wid, (session_id, rx)) in subs.iter_mut() {
	                while let Ok(msg) = rx.try_recv() {
                        let mtype = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        // ── PluginEvent → 广播到 EventBus ──
                        if mtype == "plugin_event" {
                            let ev = msg.clone();
                            let mut bus = pump_event_bus.lock().await;
                            let plugin = ev.get("plugin").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let ct = ev.get("customType").and_then(|v| v.as_str()).unwrap_or("");
                            let data = ev.get("data").cloned().unwrap_or_default();
                            let ev_session = ev.get("session").and_then(|v| v.as_str());
                            let mut event = ion::event_bus::PluginEvent::new(plugin, ct).with_data(data);
                            if let Some(s) = ev_session { event = event.with_session(s); }
                            eprintln!("[debug] broadcasting plugin_event: {} {} session={:?}", plugin, ct, ev_session);
                            bus.broadcast(&event);
                        }
                        if mtype == "response" {
                        let out = serde_json::json!({
                            "type": "worker_response",
                            "worker_id": wid,
                            "session_id": session_id,
                            "response": msg,
                        });
                        println!("{}", out);
                    } else {
                        let out = serde_json::json!({
                            "type": "event",
                            "worker_id": wid,
                            "session_id": session_id,
                            "event": msg.get("event").cloned().unwrap_or(msg.clone()),
                        });
                        println!("{}", out);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    });

    // 后台任务 2：处理 Worker 发来的 manager_command（create_worker / channel_send）
    let cmd_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        loop {
            {
                let mut reg = cmd_registry.lock().await;
                reg.process_pending_commands().await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    // 后台任务 3：转发全局事件到 stdout
    tokio::spawn(async move {
        let mut rx = global_rx;
        while let Some(event) = rx.recv().await {
            println!("{}", event);
        }
    });

    // ── Background task 4: heartbeat stale detection ──
    let hb_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let mut reg = hb_registry.lock().await;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let mut changed = false;
            for record in reg.workers.values_mut() {
                if record.status != ion::worker_registry::WorkerStatus::Dead
                    && record.status != ion::worker_registry::WorkerStatus::Stale
                {
                    if now - record.last_heartbeat > 180_000 {
                        record.status = ion::worker_registry::WorkerStatus::Stale;
                        changed = true;
                    }
                }
            }
            if changed {
                reg.broadcast_overview();
            }
        }
    });

    eprintln!("Manager started (async RPC, stdin/stdout + Unix socket). Commands: create_worker, create_session, list_sessions, list_workers, send, send_to_worker, kill, channel_send, channel_subscribe, get_overview, quit");

    // 主循环：异步读 stdin。
    // stdin EOF 时不退出（nohup/daemon 场景 stdin 立刻 EOF，但 socket 还在用）。
    // 只有显式 `quit` 命令才退出。
    let main_registry = Arc::clone(&registry);
    let main_handle = tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, BufReader};
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin).lines();
        loop {
            match reader.next_line().await {
                Ok(Some(line)) => {
                    let line = line.trim().to_string();
                    if line.is_empty() { continue; }
                    let cmd: serde_json::Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(e) => {
                            println!(r#"{{"type":"response","id":null,"success":false,"error":"{e}"}}"#);
                            continue;
                        }
                    };
                    if cmd.get("method").and_then(|v| v.as_str()) == Some("quit")
                        || cmd.get("type").and_then(|v| v.as_str()) == Some("quit")
                    {
                        return; // 退出 stdin task → 主进程退出
                    }
                    let resp = handle_manager_command(&main_registry, cmd).await;
                    println!("{}", resp);
                }
                Ok(None) => {
                    // stdin EOF（nohup 场景）：不退出，等 socket 客户端发 quit
                    // 用 sleep 拉长下次检查间隔，避免 busy loop
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    });

    // 等待 stdin task 结束（用户输 quit，或被信号杀掉）
    let _ = main_handle.await;

    // 退出时清理 PID + socket 文件
    let _ = std::fs::remove_file(&pid_path);
    let _ = std::fs::remove_file(&sock_path);
    eprintln!("Manager stopped");
}

/// 处理一条 Manager 命令（来自 stdin 或 Unix socket）。
/// 返回完整的 JSON response（含 id/success/data 字段）。
/// 被 cmd_manager_start 的 stdin 主循环和 socket accept loop 共用。
async fn handle_manager_command(
    registry: &Arc<tokio::sync::Mutex<ion::worker_registry::WorkerRegistry>>,
    cmd: serde_json::Value,
) -> serde_json::Value {
    use ion::worker_registry::WorkerCreateConfig;

    let id = cmd.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = cmd.get("method").and_then(|v| v.as_str())
        .or_else(|| cmd.get("type").and_then(|v| v.as_str())).unwrap_or("");

    let mut reg = registry.lock().await;
    let result: Result<serde_json::Value, String> = match method {
        "create_worker" => {
            // 兼容两种格式：扁平（cmd 字段直接是 config）和 嵌套（cmd.params 里是 config）
            // RPC client 发的是嵌套格式 {method, params: {...}}，stdin 命令发的是扁平
            let cfg_source = if cmd.get("params").map(|v| v.is_object()).unwrap_or(false) {
                cmd.get("params").cloned().unwrap_or_default()
            } else {
                cmd.clone()
            };
            let cfg: WorkerCreateConfig = serde_json::from_value(cfg_source).unwrap_or_default();
            match reg.create_worker(cfg).await {
                Ok(info) => Ok(serde_json::json!({
                    "workerId": info.worker_id,
                    "sessionId": info.session_id,
                })),
                Err(e) => Err(e),
            }
        }
        "list_workers" => {
            let workers: Vec<_> = reg.list_workers().iter().map(|w| serde_json::json!({
                "workerId": w.worker_id,
                "sessionId": w.session_id,
                "project": w.project,
                "status": format!("{}", w.status),
                "model": w.model,
                "agent": w.agent,
                "parent": w.parent,
                "channels": w.channels,
            })).collect();
            Ok(serde_json::json!({"workers": workers}))
        }
        // 对外 API：列 sessions（不暴露 worker_id）
        "list_sessions" => {
            let sessions: Vec<_> = reg.workers.values().map(|w| serde_json::json!({
                "session_id": w.session_id,
                "agent": w.agent,
                "status": format!("{}", w.status),
                "model": w.model,
                "started_at": w.started_at,
                "latest_output": w.latest_output.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                "log_short": w.log_short,
                "model_size": w.model_size,
            })).collect();
            Ok(serde_json::json!({"sessions": sessions}))
        }
        // 对外 API：创建 session（自动 spawn worker，返回 session_id）
        "create_session" => {
            // 兼容嵌套格式（RPC client）和扁平（stdin）
            let source = if cmd.get("params").map(|v| v.is_object()).unwrap_or(false) {
                cmd.get("params").cloned().unwrap_or_default()
            } else {
                cmd.clone()
            };
            let agent = source.get("agent").and_then(|v| v.as_str()).unwrap_or("build").to_string();
            let session_id = source.get("session_id").and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| format!("sess_{}", &uuid::Uuid::new_v4().to_string()[..8]));
            let mut cfg = WorkerCreateConfig::default();
            cfg.session = Some(session_id.clone());
            cfg.agent = Some(agent.clone());
            cfg.project_path = source.get("project_path").and_then(|v| v.as_str()).map(String::from)
                .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string()));
            cfg.channels = Some(vec!["main".to_string()]);
            cfg.initial_prompt = source.get("initial_prompt").and_then(|v| v.as_str()).map(String::from);
            match reg.create_worker(cfg).await {
                Ok(_) => Ok(serde_json::json!({
                    "session_id": session_id,
                    "agent": agent,
                    "status": "created",
                })),
                Err(e) => Err(e),
            }
        }
        "get_overview" => {
            Ok(reg.get_overview())
        }
        "send" | "send_to_session" => {
            let session = cmd.get("session").and_then(|v| v.as_str()).unwrap_or("");
            let rpc_method = cmd.get("rpc_method").and_then(|v| v.as_str())
                .or_else(|| cmd.get("method").and_then(|v| v.as_str()))
                .unwrap_or("get_state");
            let params = cmd.get("params").cloned().unwrap_or(serde_json::json!({}));
            reg.send_to_session(session, rpc_method, params).await
        }
        "send_to_worker" => {
            let worker_id = cmd.get("workerId").and_then(|v| v.as_str()).unwrap_or("");
            let rpc_method = cmd.get("rpc_method").and_then(|v| v.as_str())
                .unwrap_or("get_state");
            let params = cmd.get("params").cloned().unwrap_or(serde_json::json!({}));
            reg.send_command(worker_id, rpc_method, params).await
                .map(|_| serde_json::json!({"queued": true}))
        }
        "kill" | "kill_worker" => {
            let target = cmd.get("workerId").and_then(|v| v.as_str())
                .or_else(|| cmd.get("target").and_then(|v| v.as_str()))
                .unwrap_or("");
            reg.kill_worker(target).map(|_| serde_json::json!({"killed": true}))
        }
        "channel_send" => {
            let channel = cmd.get("channel").and_then(|v| v.as_str()).unwrap_or("main").to_string();
            let from = cmd.get("from").and_then(|v| v.as_str()).unwrap_or("manager").to_string();
            let msg = cmd.get("msg").cloned().unwrap_or(serde_json::json!({}));
            reg.channel_send(&channel, &from, msg).await;
            Ok(serde_json::json!({"sent": true}))
        }
        "channel_subscribe" => {
            let channel = cmd.get("channel").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let worker_id = cmd.get("workerId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            if let Some(record) = reg.workers.get_mut(&worker_id) {
                if !record.channels.contains(&channel) {
                    record.channels.push(channel.clone());
                }
                reg.channels.entry(channel).or_default().push(worker_id.clone());
                Ok(serde_json::json!({"subscribed": true}))
            } else {
                Err("worker not found".into())
            }
        }
        "stats" => {
            Ok(serde_json::json!({"workers": reg.list_workers().len()}))
        }
        _ => {
            // 默认分支：如果 cmd 里有 session 字段，转发到对应 worker
            // 让 `ion rpc --session <id> --method spawn_worker` 这种用法走通
            let session_id = cmd.get("session").and_then(|v| v.as_str());
            if let Some(sid) = session_id {
                let params = cmd.get("params").cloned().unwrap_or_default();
                match reg.send_to_session(sid, method, params).await {
                    Ok(_) => Ok(serde_json::json!({"status": "forwarded", "session": sid})),
                    Err(e) => Err(e),
                }
            } else {
                Err(format!("unknown method: {method} (and no `session` field for forwarding)"))
            }
        }
    };

    match result {
        Ok(data) => serde_json::json!({"type":"response","id":id,"success":true,"data":data}),
        Err(e) => serde_json::json!({"type":"response","id":id,"success":false,"error":e}),
    }
}

// ---------------------------------------------------------------------------
// Team mode — single-project self-organizing agent team
// ---------------------------------------------------------------------------
//
// 架构原则（AGENTS.md）：内核只提供对等原语，编排策略全交给 .md 提示词。
//
// cmd_team 是"启动器 + 事件泵"，不做任何编排决策：
//   1. spawn 1 个入口 Worker（默认加载 .ion/agents/coordinator.md）
//   2. 启动 pump 转发事件 + 写 JSONL log
//   3. 等所有 Worker 进入 idle + 入口 Worker 的 agent_end → 退出
//
// 谁是协调者、派生谁、何时结束 —— 全部由 coordinator.md 决定。
// coordinator 通过 spawn_worker(child, ...) 工具派生 developer Worker；
// child 同步阻塞到子 Worker 首轮 agent_end，自然串起整个流水线。

async fn cmd_team(project_path: &str, user_message: &str, _max_workers: usize) {
    use std::path::Path;
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use ion::worker_registry::{WorkerCreateConfig, WorkerRegistry};

    let ion_cfg = ion::config::IonConfig::load();
    let team_model = ion_cfg.default_model.clone().unwrap_or_else(|| "deepseek-v4-flash".to_string());
    let team_provider = ion_cfg.default_provider.clone().unwrap_or_else(|| "opencode".to_string());

    let proj = Path::new(project_path);
    if !proj.exists() {
        eprintln!("❌ Project directory not found: {project_path}");
        return;
    }

    // 0. JSONL event log（保留 —— 审计/调试用）
    let log_dir = proj.join("logs");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("events.jsonl");
    let log_file: Arc<std::sync::Mutex<Option<std::fs::File>>> =
        Arc::new(std::sync::Mutex::new(std::fs::File::create(&log_path).ok()));

    eprintln!("🚀 ION Team — project: {project_path}");
    eprintln!("   Model: {team_model} ({team_provider})");
    eprintln!("   Entry agent: coordinator (override: .ion/agents/coordinator.md)");

    // 1. 创建 WorkerRegistry
    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));

    // 2. Worker 状态追踪（pump 根据 agent_start/agent_end 更新）
    let worker_busy: Arc<std::sync::Mutex<std::collections::HashMap<String, bool>>> =
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    // 入口 Worker 的 agent_end 标志
    let entry_done: Arc<std::sync::Mutex<bool>> =
        Arc::new(std::sync::Mutex::new(false));
    let entry_wid: Arc<std::sync::Mutex<Option<String>>> =
        Arc::new(std::sync::Mutex::new(None));

    // 3. 读取 PRD（作为 initial_prompt 的一部分）
    let prd_path = proj.join("PRD.md");
    let prd_text = if prd_path.exists() {
        std::fs::read_to_string(&prd_path).unwrap_or_default()
    } else {
        "(no PRD.md found)".to_string()
    };
    eprintln!("📄 PRD: {} bytes", prd_text.len());

    // 4. 启动 pump（纯事件转发 + JSONL log，零编排逻辑）
    let pump_registry = Arc::clone(&registry);
    let pump_busy = Arc::clone(&worker_busy);
    let pump_entry_done = Arc::clone(&entry_done);
    let pump_entry_wid = Arc::clone(&entry_wid);
    let pump_log = Arc::clone(&log_file);
    eprintln!("[pump] starting");
    tokio::spawn(async move {
        let mut subs: std::collections::HashMap<String, tokio::sync::mpsc::Receiver<serde_json::Value>> =
            std::collections::HashMap::new();
        loop {
            // 订阅所有现有 Worker
            {
                let mut reg = pump_registry.lock().await;
                let ids: Vec<String> = reg.workers.keys().cloned().collect();
                for wid in &ids {
                    if !subs.contains_key(wid) {
                        eprintln!("[pump] subscribing to {}", &wid[..12.min(wid.len())]);
                        if let Ok(rx) = reg.subscribe(wid) {
                            subs.insert(wid.clone(), rx);
                        }
                    }
                    reg.drain_events(wid, 10).await;
                }
            }
            // 排空订阅事件
            for (wid, rx) in subs.iter_mut() {
                while let Ok(msg) = rx.try_recv() {
                    if msg.get("type").and_then(|v| v.as_str()) != Some("event") { continue; }
                    let ev = msg.get("event").cloned().unwrap_or_default();
                    let et = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");

                    // JSONL 审计日志
                    if let Ok(mut f) = pump_log.lock() {
                        if let Some(ref mut file) = *f {
                            use std::io::Write;
                            let log_line = serde_json::json!({
                                "ts": std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .map(|d| d.as_secs_f64()).unwrap_or(0.0),
                                "worker_id": wid, "event_type": et, "event": ev,
                            });
                            writeln!(file, "{}", log_line).ok();
                        }
                    }

                    // 可见性：text_delta / tool_call / agent_end
                    match et {
                        "text_delta" => {
                            if let Some(delta) = ev.get("delta").and_then(|v| v.as_str()) {
                                if !delta.trim().is_empty() {
                                    eprintln!("   📝 [{:.12}] {}", wid, delta.lines().next().unwrap_or(""));
                                }
                                // CHANNEL_SEND 文本协议解析（调试/可视化用，
                                // 主路径是结构化的 follow_up + channel_send 工具）
                                if delta.contains("CHANNEL_SEND") {
                                    let parts: Vec<&str> = delta.splitn(3, ' ').collect();
                                    if parts.len() >= 3 && parts[0].ends_with("CHANNEL_SEND") {
                                        let channel = parts[1].trim();
                                        let msg_text: String = parts[2].trim().chars().take(80).collect();
                                        eprintln!("   ✉️ [{:.12}] → #{}: {}", wid, channel, msg_text);
                                        let mut reg = pump_registry.lock().await;
                                        reg.channel_send(channel, wid,
                                            serde_json::json!({"text": parts[2].trim()})).await;
                                    }
                                }
                            }
                        }
                        "tool_call" => {
                            if let Some(tn) = ev.get("tool").and_then(|v| v.as_str()) {
                                eprintln!("   🔧 [{:.12}] {}", wid, tn);
                            }
                        }
                        "agent_start" => {
                            if let Ok(mut busy) = pump_busy.lock() {
                                busy.insert(wid.clone(), true);
                            }
                        }
                        "agent_end" => {
                            eprintln!("   ✅ [{:.12}] agent_end", wid);
                            if let Ok(mut busy) = pump_busy.lock() {
                                busy.insert(wid.clone(), false);
                            }
                            // 只有 entry Worker 的 agent_end 才设置全局完成
                            if let Ok(lock) = pump_entry_wid.lock() {
                                if lock.as_deref() == Some(wid.as_str()) {
                                    *pump_entry_done.lock().unwrap() = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    // 4.5 Manager command 处理循环 —— 必须有，否则 spawn_worker/send_to_worker 等命令
    //     进入 manager_cmd_rx 后无人处理，coordinator 会永远 await。
    //     对齐 cmd_manager_start 的设计（那里也有一个相同的循环）。
    let cmd_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        loop {
            {
                let mut reg = cmd_registry.lock().await;
                reg.process_pending_commands().await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    // 5. spawn 入口 Worker（默认 coordinator）
    let initial_prompt = format!(
        "{prd_text}\n\n---\nUser request: {user_message}\n\n\
         请按 coordinator 角色描述执行：分析 PRD，对每个模块调用 spawn_worker(child, developer, <spec>)，\
         等所有 child 返回后汇总并结束。"
    );
    let mut cfg = WorkerCreateConfig::default();
    cfg.agent = Some("coordinator".to_string());
    cfg.model = Some(team_model.clone());
    cfg.provider = Some(team_provider.clone());
    cfg.project_path = Some(project_path.to_string());
    cfg.channels = Some(vec!["main".to_string()]);
    cfg.initial_prompt = Some(initial_prompt);

    let entry = match registry.lock().await.create_worker(cfg).await {
        Ok(info) => {
            eprintln!("   ✅ coordinator: {}", &info.worker_id[..12]);
            *entry_wid.lock().unwrap() = Some(info.worker_id.clone());
            // 等 pump subscribe（避免丢事件）
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            info
        }
        Err(e) => {
            eprintln!("❌ Failed to spawn coordinator: {e}");
            return;
        }
    };

    // 6. 等待 team 完成：所有 worker idle + entry agent_end
    //    超时兜底 30 分钟（避免死锁卡死）
    eprintln!("\n⏳ Team running... (timeout 30 min)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30 * 60);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let entry_finished = *entry_done.lock().unwrap();
        let any_busy = worker_busy.lock().unwrap().values().any(|&b| b);

        if entry_finished && !any_busy {
            eprintln!("   ✅ Team completed (coordinator finished + all workers idle)");
            break;
        }
        if std::time::Instant::now() > deadline {
            eprintln!("   ⚠ Team timeout (30 min), forcing exit");
            break;
        }
    }

    // 7. Summary（保留 —— 调试用）
    eprintln!("\n📊 Team Status:");
    for w in registry.lock().await.list_workers() {
        eprintln!("   ◈ {} [{}] agent={} status={:?}",
            &w.worker_id[..12], w.model, w.agent, w.status);
    }

    eprintln!("\n📦 Git branches:");
    if let Ok(out) = std::process::Command::new("git")
        .args(["-C", project_path, "branch", "--list"]).output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            eprintln!("   {}", line);
        }
    }

    eprintln!("\n📂 Worktrees:");
    if let Ok(out) = std::process::Command::new("git")
        .args(["-C", project_path, "worktree", "list"]).output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            eprintln!("   {}", line);
        }
    }

    eprintln!("\n✅ Team session complete. Events: logs/events.jsonl");
    let _ = entry; // suppress unused warning
}

// ---------------------------------------------------------------------------
// Session management (pi JSONL v3)
// ---------------------------------------------------------------------------

fn save_session(id: &str, messages: &[ion::agent::messages::Message], model: &str, provider: &str, name: Option<&str>) {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let header = ion::session_jsonl::SessionHeader {
        entry_type: "session".into(), version: 3, id: id.to_string(),
        timestamp: ion::session_jsonl::timestamp_iso(),
        cwd: cwd.clone(),
        parentSession: None,
    };
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut parent_id = id.to_string();
    for msg in messages {
        let entry = ion::session_jsonl::message_to_entry(msg, &parent_id);
        if let Some(eid) = entry["id"].as_str() { parent_id = eid.to_string(); }
        entries.push(entry);
    }
    ion::session_jsonl::SessionFile::save(&cwd, &header, &entries);
    let _ = std::fs::write(ion::session_jsonl::last_session_path(), id);
    let total_input: u64 = messages.iter().filter_map(|m| match m {
        ion::agent::messages::Message::Assistant(a) => Some(a.usage.input), _ => None
    }).sum();
    let total_output: u64 = messages.iter().filter_map(|m| match m {
        ion::agent::messages::Message::Assistant(a) => Some(a.usage.output), _ => None
    }).sum();
    ion::session_index::SessionIndex::update(id, model, provider, "default", name,
        total_input, total_output, messages.len() as u32,
        messages.iter().filter(|m| matches!(m, ion::agent::messages::Message::Assistant(_))).count() as u32);
}

fn load_session(id: &str) -> Option<Vec<ion::agent::messages::Message>> {
    // Strategy 1: Look up session in global index → get cwd
    let index = ion::session_index::SessionIndex::load();
    if let Some(meta) = index.get(id) {
        if let Some(ref project) = meta.project {
            if let Some(file) = ion::session_jsonl::SessionFile::load(project) {
                if file.header.id == id {
                    return Some(file.messages);
                }
            }
        }
    }

    // Strategy 2: Legacy flat format: sessions/{id}.jsonl
    let legacy_path = ion::paths::sessions_dir().join(format!("{id}.jsonl"));
    if legacy_path.exists() {
        if let Ok(content) = std::fs::read_to_string(&legacy_path) {
            let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
            if !lines.is_empty() {
                let mut messages = Vec::new();
                for line in &lines[1..] {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                        if val["type"].as_str() == Some("message") {
                            if let Some(msg_val) = val.get("message") {
                                if let Ok(msg) = serde_json::from_value::<ion::agent::messages::Message>(msg_val.clone()) {
                                    messages.push(msg);
                                }
                            }
                        }
                    }
                }
                return Some(messages);
            }
        }
    }

    // Strategy 3: Treat id as cwd path (encoded)
    ion::session_jsonl::SessionFile::load(id).map(|f| f.messages)
}

fn resolve_session_id(cli: &Cli) -> (String, Option<Vec<ion::agent::messages::Message>>) {
    if cli.no_session { return (String::new(), None); }
    if let Some(ref sid) = cli.fork {
        if let Some(msgs) = load_session(sid) {
            let new_id = uuid::Uuid::new_v4().to_string();
            return (new_id, Some(msgs));
        }
    }
    if let Some(ref sid) = cli.resume {
        return (sid.clone(), load_session(sid));
    }
    if let Some(ref sid) = cli.session {
        return (sid.clone(), load_session(sid));
    }
    if cli.continue_session {
        if let Ok(id) = std::fs::read_to_string(ion::session_jsonl::last_session_path()) {
            let id = id.trim();
            return (id.to_string(), load_session(id));
        }
    }
    (String::new(), None)
}

fn extract_assistant_text(agent: &Agent) -> Option<String> {
    for msg in agent.messages().iter().rev() {
        if let ion::agent::messages::Message::Assistant(a) = msg {
            for block in &a.content {
                if let ion::agent::messages::AssistantContentBlock::Text(t) = block {
                    if !t.text.is_empty() { return Some(t.text.clone()); }
                }
            }
        }
    }
    None
}

fn print_output(output: &str, json_mode: bool) {
    if json_mode {
        match serde_json::from_str::<serde_json::Value>(&output) {
            Ok(json) => println!("{}", serde_json::to_string_pretty(&json).unwrap()),
            Err(_) => println!("{output}"),
        }
    } else {
        println!("{output}");
    }
}

// ---------------------------------------------------------------------------
// SessionIndexExtension
// ---------------------------------------------------------------------------

use ion::agent::extension::Extension;

struct SessionIndexExtension {
    session_id: String,
    model: String,
    provider: String,
}

impl SessionIndexExtension {
    fn new(session_id: &str, model: &str, provider: &str) -> Self {
        Self { session_id: session_id.to_string(), model: model.to_string(), provider: provider.to_string() }
    }
}

#[async_trait::async_trait]
impl Extension for SessionIndexExtension {
    async fn on_turn_end(&self, ctx: &ion::agent::extension::TurnContext) -> ion::agent::error::AgentResult<()> {
        if self.session_id.is_empty() { return Ok(()); }
        let total_input: u64 = ctx.messages.iter().filter_map(|m| match m {
            ion::agent::messages::Message::Assistant(a) => Some(a.usage.input), _ => None
        }).sum();
        let total_output: u64 = ctx.messages.iter().filter_map(|m| match m {
            ion::agent::messages::Message::Assistant(a) => Some(a.usage.output), _ => None
        }).sum();
        ion::session_index::SessionIndex::update(
            &self.session_id, &self.model, &self.provider, "default", None,
            total_input, total_output, ctx.messages.len() as u32,
            ctx.messages.iter().filter(|m| matches!(m, ion::agent::messages::Message::Assistant(_))).count() as u32,
        );
        Ok(())
    }
}
