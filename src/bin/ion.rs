//! `ion` CLI — AI Agent orchestration.
//!
//! Usage:
//!   ion run <message>                 Run agent
//!   ion config set <key> <value>     Set config
//!   ion config show                  Show config
//!   ion submit <message>             Submit task to manager
//!   ion serve --port 8080    HTTP server
//!   ion help

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;

use clap::{Parser, Subcommand};
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::compact::CompactConfig;
use ion::agent::tool::{ReadTool, GrepTool, FindTool, LsTool, BashTool, WriteTool, EditTool, CalculatorTool, EchoTool, GitStatusTool, GitDiffTool, GitLogTool, GitAddTool, GitCommitTool, GitBranchTool, ToolRegistry};
use ion::backend_registry::BackendRegistry;
use ion::config::{IonConfig, default_model_for_provider};
use ion::event_bus::ExtensionEvent;
use ion::manager::AgentManager;
use ion::types::{PoolOptions, TaskConfig, TaskPayload};
use ion::worker::agent_worker::AgentWorker;
use ion_provider::registry::{ApiRegistry, ModelRegistry, ProviderFactory};
use ion_provider::types::*;
use std::io::IsTerminal;
use tokio::sync::oneshot;

/// 待处理的 UI 确认请求（request_id → 回复通道）
static PENDING_UI: OnceLock<Mutex<HashMap<String, oneshot::Sender<String>>>> = OnceLock::new();
fn pending_ui() -> &'static Mutex<HashMap<String, oneshot::Sender<String>>> {
    PENDING_UI.get_or_init(|| Mutex::new(HashMap::new()))
}

// ---------------------------------------------------------------------------
// CLI arguments
// ---------------------------------------------------------------------------

#[derive(clap::ValueEnum, Clone, Debug)]
enum OutputMode {
    Text,
    Json,
    Rpc,
}

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

    /// Model to use for session compaction (smaller/cheaper model, defaults to main model)
    #[arg(long, global = true)]
    compact_model: Option<String>,

    /// Comma-separated model list for multi-model switching
    #[arg(long, global = true)]
    models: Option<String>,

    /// Resume a specific session by ID
    #[arg(long, short = 'r', global = true)]
    resume: Option<String>,

    /// Custom system prompt (also: --system-prompt)
    #[arg(long, short = 'P', global = true, alias = "system-prompt")]
    prompt: Option<String>,

    /// Use a named agent (build, explore, plan) or path to .md file
    #[arg(long, global = true)]
    agent: Option<String>,

    /// Thinking level (off, minimal, low, medium, high, xhigh)
    #[arg(long, global = true)]
    thinking: Option<String>,

    /// Tool allowlist (comma separated)
    #[arg(long, short = 't', global = true)]
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

    /// Exact session ID to use (creates new session with this ID if not found)
    #[arg(long, global = true)]
    session_id: Option<String>,

    /// Custom session directory
    #[arg(long, global = true)]
    session_dir: Option<String>,

    /// Continue the last session
    #[arg(long = "continue", short = 'c', global = true, default_value_t = false, alias = "continue-session")]
    continue_session: bool,

    /// Run without persisting session
    #[arg(long, global = true, default_value_t = false)]
    no_session: bool,

    /// Maximum conversation turns (default: unlimited)
    #[arg(long, global = true)]
    max_turns: Option<u64>,

    /// Verbose logging
    #[arg(long, short, global = true, default_value_t = false)]
    verbose: bool,

    /// List available models (with optional search filter)
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "true")]
    list_models: Option<String>,

    /// Request JSON output via prompt injection
    #[arg(long, global = true, default_value_t = false)]
    json: bool,

    /// Output mode: text (default), json, or rpc
    #[arg(long, global = true)]
    mode: Option<OutputMode>,

    /// JSON Schema to validate output (also: --output-schema)
    #[arg(long, global = true, alias = "output-schema")]
    json_schema: Option<String>,

    /// Non-interactive mode: process prompt and exit
    #[arg(long, short = 'p', global = true, default_value_t = false)]
    print: bool,

    /// Max retries for JSON schema validation (default: 3)
    #[arg(long, global = true, default_value_t = 3)]
    schema_retries: u32,

    /// Disable all tools
    #[arg(long, global = true, default_value_t = false)]
    no_tools: bool,

    /// Host mode: start a temporary host with event pump, auto-exit when idle
    #[arg(long, global = true, default_value_t = false)]
    host: bool,

    /// Force local runtime (overrides config's runtime.default_mode)
    #[arg(long, global = true, conflicts_with = "remote")]
    local: bool,

    /// Force remote runtime (overrides config's runtime.default_mode)
    #[arg(long, global = true, conflicts_with = "local")]
    remote: bool,

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
    /// Subscribe to real-time events.
    ///   ion subscribe --session sess_xxx
    ///   ion subscribe --session sess_xxx --extension memory
    ///   ion subscribe --extension memory
    ///   ion subscribe --ui              (UI events: Ask/Confirm/Notif/Alert/Prompt)
    /// Ctrl+C to disconnect.
    Subscribe {
        /// Session to subscribe to
        #[arg(long)]
        session: Option<String>,
        /// Plugin/extension name to filter (omit for all events)
        #[arg(long)]
        extension: Option<String>,
        /// Subscribe to UI events (Ask/Confirm/Notif/Alert/Prompt)
        #[arg(long)]
        ui: bool,
    },
    /// List all sessions with stats
    Sessions,
    /// List all LLM recordings (Record/Replay)
    Recordings,
    /// List available agents
    ListAgents,
    /// List available models
    ListModels {
        /// Optional search filter
        search: Option<String>,
    },
    /// Start/stop/manage the host server (Unix socket RPC)
    Serve {
        #[command(subcommand)]
        action: Option<ServeAction>,
    },
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
}

#[derive(Subcommand)]
enum ServeAction {
    /// Start the host server
    #[command(hide = true)]
    Start {
        #[arg(long, default_value_t = 8080)]
        port: u16,
        #[arg(long, default_value_t = 10)]
        max_workers: usize,
        #[arg(long, default_value_t = 0)]
        min_workers: usize,
    },
    /// Stop the host server (sends shutdown RPC)
    Stop,
    /// Check host server status
    Status,
}

#[derive(Subcommand)]
enum ConfigAction {
    Show,
    Set { key: String, value: String },
    Get { key: String },
    /// List all available config keys with descriptions
    List,
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
    max_turns: Option<u64>,
    /// All models from --models list (for future multi-model cycling)
    #[allow(dead_code)]
    all_models: Vec<String>,
    /// Separate model for session compaction (defaults to main model)
    compact_model: Option<String>,
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

    /// Resolve --json-schema / --output-schema value:
    ///   - Inline JSON (`{...}`) → return as-is
    ///   - File path (`@path` or bare path) → read file contents
    ///   - None → None
    fn resolve_schema(schema: &Option<String>) -> Option<String> {
        let s = schema.as_ref()?;
        if s.trim().starts_with('{') {
            return Some(s.clone()); // inline JSON
        }
        // Try as @file or bare file path
        let path = s.strip_prefix('@').unwrap_or(s);
        match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(e) => {
                eprintln!("Warning: cannot read schema file '{path}': {e}");
                Some(s.clone()) // fallback to raw value
            }
        }
    }
}

/// Detect image files from `@file` CLI arguments and return ContentBlock::Image blocks.
/// Supported formats: .png, .jpg, .jpeg, .gif, .webp
fn parse_image_blocks(raw_messages: &[String]) -> Vec<ContentBlock> {
    let image_extensions = ["png", "jpg", "jpeg", "gif", "webp"];
    let mut blocks: Vec<ContentBlock> = Vec::new();

    for arg in raw_messages {
        let path = if let Some(p) = arg.strip_prefix('@') {
            p
        } else {
            continue; // only process @file references
        };
        let ext = match std::path::Path::new(path).extension().and_then(|e| e.to_str()) {
            Some(e) => e.to_lowercase(),
            None => continue,
        };
        if !image_extensions.contains(&ext.as_str()) {
            continue;
        }
        // Read the file and base64-encode it
        match std::fs::read(path) {
            Ok(data) => {
                use base64::Engine;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&data);
                let mime = format!("image/{}", if ext == "jpg" { "jpeg" } else { &ext });
                blocks.push(ContentBlock::Image(ImageContent {
                    data: b64,
                    mime_type: mime,
                }));
            }
            Err(e) => {
                eprintln!("Warning: cannot read image file '{path}': {e}");
            }
        }
    }
    blocks
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

    // Step 1: Resolve provider from --provider / config / default
    let mut provider = cli
        .provider
        .clone()
        .or_else(|| cfg.default_provider.clone())
        .unwrap_or_else(|| "opencode".into());

    // Step 2: Resolve raw model string from --model / --models / config
    let raw_model = cli
        .model
        .clone()
        .or_else(|| {
            cli.models.as_ref().and_then(|m| m.split(',').next().map(|s| s.trim().to_string()))
        })
        .or_else(|| cfg.default_model.clone())
        .unwrap_or_else(|| default_model_for_provider(&provider).to_string());

    // Step 3: Parse --model provider/id:thinking syntax (对齐 pi)
    // Examples:
    //   --model openai/gpt-4o          → provider=openai, model=gpt-4o
    //   --model sonnet:high            → model=sonnet, thinking=high
    //   --model openai/gpt-4o:high     → provider=openai, model=gpt-4o, thinking=high
    let mut model_id = raw_model.clone();
    let mut parsed_thinking: Option<String> = None;

    // Check for provider/id pattern
    if let Some(slash_pos) = raw_model.find('/') {
        let maybe_provider = &raw_model[..slash_pos];
        // Only treat as provider if it's a known provider name (or looks like one)
        // Known providers match common patterns: lowercase, optionally with hyphens/digits
        let rest = &raw_model[slash_pos + 1..];
        provider = maybe_provider.to_string();
        model_id = rest.to_string();
    }

    // Check for model:thinking pattern (after provider/id extraction)
    if let Some(colon_pos) = model_id.rfind(':') {
        let maybe_level = &model_id[colon_pos + 1..];
        let valid_levels = ["off", "minimal", "low", "medium", "high", "xhigh"];
        if valid_levels.contains(&maybe_level) {
            parsed_thinking = Some(maybe_level.to_string());
            model_id = model_id[..colon_pos].to_string();
        }
    }

    // Step 4: Determine final thinking level
    // --thinking takes precedence over :thinking suffix
    let thinking = cli.thinking.clone().or(parsed_thinking);

    // Parse full models list for multi-model support
    let all_models: Vec<String> = cli.models.as_ref()
        .map(|m| m.split(',').map(|s| s.trim().to_string()).collect())
        .unwrap_or_default();

    let api_key = cfg.resolve_api_key(cli.api_key.as_deref(), &provider);
    let base_url = cli.base_url.clone().or_else(|| cfg.base_url.clone());

    let mut eff = EffectiveConfig {
        provider,
        model: model_id,
        api_key,
        base_url,
        json: cli.json || matches!(cli.mode, Some(OutputMode::Json)),
        json_schema: EffectiveConfig::resolve_schema(&cli.json_schema),
        schema_retries: cli.schema_retries,
        prompt: cli.prompt.clone(),
        append_prompts: cli.append_system_prompt.clone(),
        thinking: thinking,
        max_turns: cli.max_turns,
        all_models: all_models,
        compact_model: cli.compact_model.clone(),
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
            // internal/mock providers — no base_url needed
            "faux" | "replay" => String::new(),
            other => {
                eprintln!("❌ Unknown provider '{other}'");
                eprintln!();
                eprintln!("Available builtin providers: opencode");
                eprintln!();
                eprintln!("To fix this, you can:");
                eprintln!("  1. Use a builtin provider:  ion --provider opencode --model deepseek-v4-flash \"hi\"");
                eprintln!("  2. Set custom base URL:     ion --provider {other} --base-url https://your-api.com/v1 \"hi\"");
                eprintln!("  3. Define in config.json:   ion config set base-url https://your-api.com/v1");
                std::process::exit(1);
            }
        });

    let mut registry = ApiRegistry::new();
    registry.register_builtins();

    // ── FauxProvider 接入（场景 1 直接执行也支持）──
    let faux_script = std::env::var("ION_FAUX_SCRIPT").ok();
    let faux_reply = std::env::var("ION_FAUX_REPLY").ok();
    let using_faux = faux_script.is_some() || faux_reply.is_some();
    if using_faux {
        let faux = ion_provider::faux::register_faux(&mut registry);
        let responses = if let Some(path) = &faux_script {
            ion_provider::faux::load_script(std::path::Path::new(path))
                .expect("failed to load ION_FAUX_SCRIPT")
        } else {
            vec![ion_provider::faux::FauxResponseStep::Static(
                ion_provider::faux::faux_assistant_message(
                    ion_provider::faux::FauxContent::Text(faux_reply.clone().unwrap_or_default()),
                    ion_provider::faux::FauxMessageOptions::default(),
                ),
            )]
        };
        faux.set_responses(responses);
        eprintln!("[faux] enabled: {} responses queued", faux.pending_count());
    }

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
            // Fallback: construct from effective config + show hint
            tracing::warn!(
                "model '{}' not in registry, using fallback (context=128k). \
                 Use --list-models to see available models, or define it in ~/.ion/models.json.",
                eff.model
            );
            // Internal/mock providers route to themselves; others default to openai-completions
            let fallback_api = match eff.provider.as_str() {
                "faux" => "faux",
                "replay" => "replay",
                _ => "openai-completions",
            };
            Model {
                id: eff.model.clone(),
                name: eff.model.clone(),
                api: fallback_api.into(),
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

    // faux 模式：强制 model.api 指向 faux provider（覆盖任何真实 API 路由）
    if using_faux {
        model.api = "faux".into();
        eprintln!("[faux] model.api forced to 'faux'");
    }

    // ── ReplayProvider（始终注册；通过 --model replay/<id> 激活）──
    registry.register("replay", Box::new(ion_provider::replay::ReplayProvider));

    // ── RecordingProvider（通过 ION_RECORD 环境变量激活）──
    // 捕获真实 provider（含 faux）的输出，写入 trace.jsonl。
    if let Ok(rec_id) = std::env::var("ION_RECORD") {
        let overwrite = std::env::var("ION_RECORD_OVERWRITE").is_ok();
        match ion_provider::replay::recording_trace_path(&rec_id) {
            Ok(trace_path) => {
                let rec_dir = trace_path.parent().unwrap().to_path_buf();
                match ion_provider::replay::acquire_recording_lock(&rec_dir, overwrite) {
                    Ok(lock_opt) => {
                        // 构造被包裹的内层 provider：
                        //  - 若 faux 激活，用共享同一份队列的 faux 句柄；
                        //  - 否则用 builtin factory 按 model.api 创建真实 provider。
                        let inner: Option<Box<dyn ion_provider::registry::ApiProvider>> = if using_faux {
                            // 重新注册一个共享同一份队列的 faux（队列已在上面填充）
                            // 这里直接拿一个新的 FauxProvider，复用相同的 responses。
                            let new_faux = std::sync::Arc::new(ion_provider::faux::FauxProvider::new());
                            // 复用之前已注册的 faux 队列：从 env 重新构造一份
                            let responses = if let Some(path) = &faux_script {
                                ion_provider::faux::load_script(std::path::Path::new(path)).ok()
                            } else {
                                Some(vec![ion_provider::faux::FauxResponseStep::Static(
                                    ion_provider::faux::faux_assistant_message(
                                        ion_provider::faux::FauxContent::Text(faux_reply.as_deref().unwrap_or_default().to_string()),
                                        ion_provider::faux::FauxMessageOptions::default(),
                                    ),
                                )])
                            };
                            if let Some(rsps) = responses {
                                new_faux.set_responses(rsps);
                            }
                            Some(Box::new(ArcFauxProvider(new_faux)))
                        } else {
                            let factory = ion_provider::registry::BuiltinProviderFactory;
                            factory.create(&model.api)
                        };

                        match inner {
                            Some(real) => {
                                let meta_path = ion_provider::replay::recording_meta_path(&rec_id).unwrap();
                                let recording = ion_provider::record::RecordingProvider::new(
                                    real, trace_path, meta_path,
                                );
                                registry.register(&model.api, Box::new(recording));
                                eprintln!("[record] recording to {} (model: {})", rec_dir.display(), model.id);
                                // 持有锁到进程退出（故意泄漏，保持文件锁）
                                if let Some(l) = lock_opt { std::mem::forget(l); }
                            }
                            None => {
                                eprintln!("[record] ⚠️  no builtin provider for api '{}', recording disabled", model.api);
                            }
                        }
                    }
                    Err(e) => eprintln!("[record] ⚠️  {}", e),
                }
            }
            Err(e) => eprintln!("[record] ⚠️  invalid recording id: {}", e),
        }
    }

    (Arc::new(registry), model)
}

/// Adapter: box an `Arc<FauxProvider>` so it can be used as the inner
/// provider of a `RecordingProvider` (sharing the same response queue).
struct ArcFauxProvider(std::sync::Arc<ion_provider::faux::FauxProvider>);
#[async_trait::async_trait]
impl ion_provider::registry::ApiProvider for ArcFauxProvider {
    async fn stream(
        &self,
        model: &ion_provider::types::Model,
        context: &ion_provider::types::Context,
        options: Option<&ion_provider::types::StreamOptions>,
    ) -> ion_provider::error::ProviderResult<ion_provider::event_stream::EventStream> {
        self.0.stream(model, context, options).await
    }
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
        compact_model_id: eff.compact_model.clone(),
        retry_on_no_tool_use: 1,
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

/// Read all content from piped stdin.
/// Returns None if stdin is a TTY (interactive terminal).
fn read_piped_stdin() -> Option<String> {
    use std::io::Read;
    let mut buf = String::new();
    let stdin = std::io::stdin();
    let mut handle = stdin.lock();
    // Check if stdin is a TTY (interactive)
    if handle.is_terminal() {
        return None;
    }
    // Try to read all content
    match handle.read_to_string(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(_) => {
            let trimmed = buf.trim().to_string();
            if trimmed.is_empty() { None } else { Some(trimmed) }
        }
    }
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

async fn cmd_config_list() {
    println!("Available config keys:");
    println!("  api-key              Set API key (stored in auth.json, permissions 600)");
    println!("  default-provider     Set default provider name (e.g. opencode, anthropic)");
    println!("  default-model        Set default model ID (e.g. deepseek-v4-flash, gpt-4o)");
    println!("  base-url             Set API base URL override");
    println!();
    println!("Usage: ion config set <key> <value>");
    println!("       ion config get <key>");
    println!("       ion config show");
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

/// --mode rpc: JSON-RPC protocol over stdin/stdout (aligned with pi).
async fn cmd_mode_rpc(eff: &EffectiveConfig, _session_id: &str) {
    let (registry, model) = build_registry_and_model(eff);
    let config = build_agent_config(eff);

    let cfg = ion::rpc::RpcConfig {
        registry,
        model,
        agent_config: config,
        thinking: eff.thinking.clone(),
        max_turns: eff.max_turns,
    };
    ion::rpc::handle_rpc(cfg).await;
}

async fn cmd_run(
    eff: &EffectiveConfig,
    message: &str,
    _no_tools: bool,
    session_id: &str,
    preloaded: Option<Vec<ion::agent::messages::Message>>,
    raw_messages: &[String],
) {
    let (registry, model) = build_registry_and_model(eff);
    
    let config = build_agent_config(eff);

    let mut tools = build_tools(eff);

    // WASM plugin registry (hot‑pluggable — used by worker RPC too)
    let wasm_ext_registry = std::sync::Arc::new(ion::wasm_extension::Registry::new());
    let mut loaded_wasm_paths: Vec<String> = Vec::new();

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
                        let ext_name = ion::wasm_extension::ext_name_from_path(&canonical_str);
                        match wasm_ext_registry.add(&canonical_str) {
                            Ok(tool_defs) => {
                                loaded_wasm_paths.push(canonical_str.clone());
                                for td in &tool_defs {
                                    tools.register(Box::new(ion::wasm_extension::ToolAdapter {
                                        name: td.name.clone(),
                                        description: td.description.clone(),
                                        parameters: td.parameters.clone(),
                                        extension_path: canonical_str.clone(),
                                        ext_name: ext_name.clone(),
                                        registry: wasm_ext_registry.clone(),
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
            // Determine canonical path before calling wasm_ext_registry.add(),
            // so ToolAdapter holds the canonicalised path.
            let canonical = std::fs::canonicalize(abs)
                .unwrap_or_else(|_| abs.to_path_buf());
            let canonical_str = canonical.to_string_lossy().to_string();

            match wasm_ext_registry.add(&canonical_str) {
                Ok(tool_defs) => {
                    let ext_name = ion::wasm_extension::ext_name_from_path(&canonical_str);
                    loaded_wasm_paths.push(canonical_str.clone());
                    for td in &tool_defs {
                        tools.register(Box::new(ion::wasm_extension::ToolAdapter {
                            name: td.name.clone(),
                            description: td.description.clone(),
                            parameters: td.parameters.clone(),
                            extension_path: canonical_str.clone(),
                            ext_name: ext_name.clone(),
                            registry: wasm_ext_registry.clone(),
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

    // Build runtime from config (aligned with ion_worker.rs)
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let runtime_cfg = ion::config::IonConfig::load().runtime;
    let backend_registry = BackendRegistry::from_config(&runtime_cfg, &cwd);
    let rt = ion::runtime::SecuredRuntime::new(backend_registry)
        .with_profile(ion::kernel::SecurityProfile::default());
    let mut agent = Agent::new(registry, model, Some(sys_prompt), tools, config)
        .with_runtime(Box::new(rt));

    // Resolve compact model for summarization (if specified via --compact-model)
    if let Some(ref cm_id) = eff.compact_model {
        let mut mr = ion_provider::registry::ModelRegistry::new();
        mr.register_builtins();
        if let Some(cm) = mr.find_model(cm_id).cloned() {
            agent = agent.with_compact_model(Some(cm));
            tracing::info!("using separate compact model: {}", cm_id);
        } else {
            tracing::warn!("compact model '{}' not found, using main model", cm_id);
        }
    }

    // ── @file 图片注入 ──
    // 构建初始消息队列：preloaded 会话历史 + 图片 blocks
    let image_blocks = parse_image_blocks(raw_messages);
    let mut initial_messages: Vec<Message> = Vec::new();
    if let Some(msgs) = preloaded {
        initial_messages = msgs;
    }
    if !image_blocks.is_empty() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        initial_messages.push(Message::User(UserMessage {
            role: "user".into(),
            content: image_blocks,
            timestamp: now,
        }));
    }
    if !initial_messages.is_empty() {
        agent = agent.with_messages(initial_messages);
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

    // ── 注册 WASM Extension 的 HookAdapter（让 WASM 也能实现 29 个钩子）──
    for wasm_path in &loaded_wasm_paths {
        if let Some(hook_adapter) = wasm_ext_registry.create_hook_adapter(wasm_path) {
            ext_reg.register(Box::new(hook_adapter));
            tracing::info!("[wasm] registered HookAdapter for {}", wasm_path);
        }
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
        let mut ctx = wasm_ext_registry.ctx.write().unwrap();
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

    let sock_path = ion::paths::host_socket_path();
    let mut stream = match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ Cannot connect to Host at {}\n   先启动: ion serve\n   错误: {e}",
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
async fn cmd_subscribe(session: Option<&str>, extension: Option<&str>, ui: bool) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sock_path = ion::paths::host_socket_path();
    let mut stream = match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("❌ Cannot connect to Host at {}\n   先启动: ion serve\n   错误: {e}", sock_path.display());
            std::process::exit(1);
        }
    };

    let mut req = serde_json::json!({"method": "subscribe"});
    if let Some(sid) = session { req["session"] = serde_json::json!(sid); }
    if let Some(p) = extension { req["extension"] = serde_json::json!(p); }
    if ui { req["ui"] = serde_json::json!(true); }

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

async fn cmd_recordings() {
    let dir = ion_provider::replay::recordings_dir();
    if !dir.exists() {
        println!("No recordings ({} doesn't exist)", dir.display());
        return;
    }
    println!("{:<30} {:<20} {:<10} {:<20}", "ID", "MODEL", "RESPONSES", "CREATED");
    println!("{}", "-".repeat(80));
    let mut entries: Vec<_> = std::fs::read_dir(&dir).into_iter().flatten()
        .filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let id = entry.file_name().to_string_lossy().to_string();
        let meta_path = entry.path().join("meta.json");
        if let Ok(content) = std::fs::read_to_string(&meta_path) {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&content) {
                println!(
                    "{:<30} {:<20} {:<10} {:<20}",
                    id,
                    meta.get("model").and_then(|v| v.as_str()).unwrap_or("?"),
                    meta.get("response_count").and_then(|v| v.as_u64()).unwrap_or(0),
                    meta.get("created_at").and_then(|v| v.as_i64())
                        .map(|t| format!("{}s", t / 1000)).unwrap_or_else(|| "?".into()),
                );
                continue;
            }
        }
        println!("{:<30} {:<20} {:<10} {:<20}", id, "?", "?", "(no meta)");
    }
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
        if let Some(_model) = registry.get_model(
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
        let w = registry.lock().await.create_worker(WorkerCreateConfig {
            model: Some(eff.model.clone()),
            provider: Some(eff.provider.clone()),
            ..Default::default()
        }, &registry).await.unwrap_or_else(|e| panic!("{e}"));
        tracing::info!("Worker: {}", w.worker_id);

        // Send prompt
        let _ = registry.lock().await.send_to_worker(&w.worker_id, "prompt",
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

#[allow(dead_code)]
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

    // --local / --remote override: set env var before any config load
    // (IonConfig::load reads this to override runtime.default_mode)
    // Safety: this runs at the very start of main(), before any other threads exist.
    if cli.local {
        unsafe { std::env::set_var("ION_RUNTIME_OVERRIDE", "local"); }
    } else if cli.remote {
        unsafe { std::env::set_var("ION_RUNTIME_OVERRIDE", "remote"); }
    }

    let mut eff = resolve_effective(&cli);

    // ── 管道 stdin 自动检测（对齐 pi）──
    // 当 stdin 不是 TTY（有管道输入），自动读取并用做消息
    let piped_stdin = read_piped_stdin();
    if let Some(ref stdin_content) = piped_stdin {
        if !stdin_content.is_empty() {
            if eff.message.is_empty() {
                eff.message = stdin_content.clone();
            } else {
                eff.message = format!("{}\n{}", stdin_content, eff.message);
            }
        }
    }

    // ── --list-models [search] flag ──
    if let Some(ref lm) = cli.list_models {
        let search = if lm == "true" { None } else { Some(lm.clone()) };
        cmd_list_models(&search).await;
        return;
    }

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

    let effective_message = eff.message.clone();

    // ── --mode rpc: RPC 模式（JSON-RPC over stdin/stdout）──
    if matches!(cli.mode, Some(OutputMode::Rpc)) {
        let (session_id, _preloaded) = resolve_session_id(&cli);
        cmd_mode_rpc(&eff, &session_id).await;
        return;
    }

    // ── --host: 临时 host 模式（快速编排）──
    if cli.host {
        let msg = if effective_message.is_empty() { "Hello".to_string() } else { effective_message };
        cmd_host(&msg, cli.agent.as_deref()).await;
        return;
    }

    if !effective_message.is_empty() {
        let (session_id, preloaded) = resolve_session_id(&cli);
        cmd_run(&eff, &eff.message, cli.no_tools, &session_id, preloaded, &cli.messages).await;
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
        Some(Commands::Serve { action }) => match action {
            // `ion serve` (no subcommand) → defaults to `ion serve start`
            None => cmd_serve_start(&cli, 8080, 10, 2).await,
            Some(ServeAction::Start { port, max_workers, min_workers }) => {
                cmd_serve_start(&cli, *port, *max_workers, *min_workers).await;
            }
            Some(ServeAction::Stop) => cmd_serve_stop().await,
            Some(ServeAction::Status) => cmd_serve_status().await,
        },
        Some(Commands::Config { action }) => match action {
            ConfigAction::Show => cmd_config_show().await,
            ConfigAction::Set { key, value } => cmd_config_set(key, value).await,
            ConfigAction::Get { key } => cmd_config_get(key).await,
            ConfigAction::List => cmd_config_list().await,
        },
        Some(Commands::Dashboard) => {
            // Dashboard 用 Bun + OpenTUI 实现（dashboard/ 子目录）
            // 自动启动 Manager（如果没在跑），然后 fork bun 进程
            launch_dashboard().await;
        }
        Some(Commands::Rpc { session, method, params }) => {
            cmd_rpc(session.as_deref(), method, params).await;
        }
        Some(Commands::Sessions) => cmd_sessions().await,
        Some(Commands::Recordings) => cmd_recordings().await,
        Some(Commands::Subscribe { session, extension, ui }) => cmd_subscribe(session.as_deref(), extension.as_deref(), *ui).await,
        Some(Commands::ListAgents) => cmd_list_agents().await,
        Some(Commands::ListModels { search }) => cmd_list_models(search).await,
        None => {
            println!("ion: AI Agent orchestration CLI");
            println!("Usage: ion <message>");
            println!("       ion submit <message>");
            println!("       ion serve");
            println!("       ion config set api-key <key>");
            println!("       ion --help");
        }
	}
}

// ---------------------------------------------------------------------------
// Serve commands
// ---------------------------------------------------------------------------

/// Stop the host server: connect to Unix socket and send shutdown.
async fn cmd_serve_stop() {
    let sock_path = ion::paths::host_socket_path();
    match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(mut stream) => {
            use tokio::io::AsyncWriteExt;
            let req = serde_json::json!({
                "id": "serve-stop",
                "method": "shutdown",
                "params": {}
            });
            let _ = stream.write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes()).await;
            println!("✔ Shutdown signal sent to host server");
        }
        Err(_) => {
            // Socket not available, try force-kill from PID file
            if let Some(pid) = ion::paths::host_running() {
                #[cfg(unix)]
                let _ = std::process::Command::new("kill")
                    .args([&pid.to_string()])
                    .status();
                println!("✔ Host stopped");
            } else {
                println!("✘ Host not running");
            }
        }
    }
    // Clean up stale files
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&ion::paths::host_pid_path());
}

/// Check host server status: read PID file and verify process.
async fn cmd_serve_status() {
    if let Some(pid) = ion::paths::host_running() {
        println!("✔ Host running (pid {pid})");
        println!("   Socket: {}", ion::paths::host_socket_path().display());
    } else {
        println!("✘ Host not running");
        println!("   Start with: ion serve");
    }
}

async fn cmd_serve_start(
    _cli: &Cli,
    _port: u16,
    _max_workers: usize,
    _min_workers: usize,
) {
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use ion::worker_registry::WorkerRegistry;

    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    let event_bus = Arc::new(tokio::sync::Mutex::new(ion::event_bus::ExtensionEventBus::new()));

    // ── Host 单例检查 + Unix socket 启动 ──
    // PID 文件防重复启动；Unix socket 让外部 `ion rpc` 能连进来。
    if let Some(pid) = ion::paths::host_running() {
        eprintln!("❌ Host already running (pid {pid}). Stop it first or use `ion rpc` to connect.");
        return;
    }
    let sock_path = ion::paths::host_socket_path();
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
    let pid_path = ion::paths::host_pid_path();
    let _ = std::fs::write(&pid_path, std::process::id().to_string());
    eprintln!("🔌 Host listening on Unix socket: {}", sock_path.display());

    // socket accept loop —— 支持两种模式：
    //   RPC mode（默认）：一问一答，返回后关闭
    //   Stream mode（subscribe）：长连接，持续推事件
    let sock_registry = Arc::clone(&registry);
    let sock_event_bus = Arc::clone(&event_bus);
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        
        let _reader_timeout = std::time::Duration::from_secs(600);
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
                                let _session = cmd.get("session").and_then(|v| v.as_str()).map(|s| s.to_string());

                                // ── Stream mode: subscribe ──
                                if method == "subscribe" {
                                    let extension = cmd.get("extension").and_then(|v| v.as_str()).unwrap_or("");
                                    let session = cmd.get("session").and_then(|v| v.as_str()).map(|s| s.to_string());

                                    if extension.is_empty() && session.is_some() {
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

                                    // ── UI subscribe：订阅 UI 事件（Ask/Confirm/Prompt/Notif/Alert）──
                                    let is_ui = cmd.get("ui").and_then(|v| v.as_bool()).unwrap_or(false);
                                    if is_ui {
                                        let mut bus = ev_bus.lock().await;
                                        let mut rx = bus.subscribe_ui();
                                        drop(bus);
                                        let ack = serde_json::json!({"type":"subscribed","stream":"ui"});
                                        let _ = write_half.write_all(format!("{ack}\n").as_bytes()).await;
                                        let _ = write_half.flush().await;
                                        loop {
                                            match rx.recv().await {
                                                Some(event) => {
                                                    let msg = serde_json::json!({
                                                        "type": "ui_event",
                                                        "ui_type": event.custom_type,
                                                        "extension": event.extension,
                                                        "session": event.session,
                                                        "data": event.data,
                                                        "route": event.route,
                                                    });
                                                    if write_half.write_all(format!("{msg}\n").as_bytes()).await.is_err() { break; }
                                                    let _ = write_half.flush().await;
                                                }
                                                None => break,
                                            }
                                        }
                                        return;
                                    }

                                    // ── Plugin subscribe：通过 EventBus ──
                                    let mut bus = ev_bus.lock().await;
                                    let rx = if !extension.is_empty() {
                                        if let Some(ref sid) = session {
                                            bus.subscribe_with_session(extension, sid)
                                        } else {
                                            bus.subscribe(extension)
                                        }
                                    } else {
                                        bus.subscribe_all()
                                    };
                                    drop(bus);
                                    // 返回 subscribed ack
                                    let ack = serde_json::json!({
                                        "type":"subscribed",
                                        "extension": extension,
                                        "session": session,
                                    });
                                    let _ = write_half.write_all(format!("{ack}\n").as_bytes()).await;
                                    let _ = write_half.flush().await;
                                    // 持续推事件
                                    
                                    let mut rx = rx;
                                    loop {
                                        match rx.recv().await {
                                            Some(event) => {
                                                let msg = serde_json::json!({
                                                    "type": "extension_event",
                                                    "extension": event.extension,
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

                                // ── UI respond: 回复 Ask/Confirm/Prompt ──
                                if method == "ui_respond" {
                                    let request_id = cmd.get("params").and_then(|p| p.get("request_id"))
                                        .and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let response = cmd.get("params").and_then(|p| p.get("response"))
                                        .and_then(|v| v.as_str()).unwrap_or("deny").to_string();
                                    // 取出发送者，立即释放锁
                                    let sender = {
                                        let mut map = pending_ui().lock().unwrap();
                                        map.remove(&request_id)
                                    };
                                    if let Some(tx) = sender {
                                        let _ = tx.send(response.clone());
                                        // 推 AskResolved 到 UI 事件通道（锁已释放）
                                        let resolved = ExtensionEvent::new_ui("AskResolved", &request_id, &response)
                                            .with_data(serde_json::json!({"response": response, "resolved_by": "cli"}));
                                        let mut bus = ev_bus.lock().await;
                                        bus.broadcast(&resolved);
                                        drop(bus);
                                        let resp = serde_json::json!({"type":"response","id":cmd.get("id"),"success":true,"data":{"request_id":request_id,"response":response}});
                                        let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                    } else {
                                        let resp = serde_json::json!({"type":"response","id":cmd.get("id"),"success":false,"error":"request not found or already expired"});
                                        let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
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
                                        let _rx = match inner_reg.subscribe(&wid) {
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
                                        // Step 1: send command + register oneshot (brief lock)
                                        let params = cmd.get("params").cloned().unwrap_or_default();
                                        let send_result = inner_reg.send_command(&wid, &method, params).await;
                                        let rx = send_result.ok()
                                            .and_then(|rid| inner_reg.register_pending(&wid, &rid));
                                        drop(inner_reg); // RELEASE LOCK

                                        match rx {
                                            Some(rx) => {
                                                // Step 2: wait for oneshot (NO lock held)
                                                match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
                                                    Ok(Ok(resp)) => {
                                                        let mut r = resp.clone();
                                                        if let Some(id) = cmd.get("id") { r["id"] = id.clone(); }
                                                        let _ = write_half.write_all(format!("{r}\n").as_bytes()).await;
                                                        let _ = write_half.flush().await;
                                                    }
                                                    _ => {
                                                        let resp = serde_json::json!({"type":"response","id":cmd.get("id"),"success":false,"error":"timeout"});
                                                        let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                                    }
                                                }
                                            }
                                            None => {
                                                let resp = serde_json::json!({"type":"response","id":cmd.get("id"),"success":false,"error":"send failed"});
                                                let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                            }
                                        }
                                        return;
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
                    // 不调 drain_events — reader task 已实时转发 event 给 subscribers
                    // drain_events 会从 stdout_rx 偷走 send_command 等待的 response
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
                        // ── ExtensionEvent → 广播到 EventBus ──
                        if mtype == "extension_event" {
                            let ev = msg.clone();
                            let mut bus = pump_event_bus.lock().await;
                            let extension = ev.get("extension").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let ct = ev.get("customType").and_then(|v| v.as_str()).unwrap_or("");
                            let data = ev.get("data").cloned().unwrap_or_default();
                            let ev_session = ev.get("session").and_then(|v| v.as_str());
                            let mut event = ion::event_bus::ExtensionEvent::new(extension, ct).with_data(data);
                            if let Some(s) = ev_session { event = event.with_session(s); }
                            eprintln!("[debug] broadcasting extension_event: {} {} session={:?}", extension, ct, ev_session);
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
                reg.process_pending_commands(&cmd_registry).await;
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

    eprintln!("Host started (async RPC, stdin/stdout + Unix socket). Commands: create_worker, create_session, list_sessions, list_workers, send, send_to_worker, kill, channel_send, channel_subscribe, get_overview, quit");

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
    eprintln!("Host stopped");
}

/// 处理一条 Manager 命令（来自 stdin 或 Unix socket）。
/// 返回完整的 JSON response（含 id/success/data 字段）。
/// 被 cmd_serve_start 的 stdin 主循环和 socket accept loop 共用。
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
            let mut cfg: WorkerCreateConfig = serde_json::from_value(cfg_source.clone()).unwrap_or_default();
            // 支持从 params 显式传 session（重建 worker 时保留 SID）
            if cfg.session.is_none() {
                cfg.session = cfg_source.get("session").or_else(|| cfg_source.get("session_id"))
                    .and_then(|v| v.as_str()).map(String::from);
            }
            // 兼容旧测试脚本：如果 params 传了 cwd 但没有 project_path，映射过去
            if cfg.project_path.is_none() {
                if let Some(cwd_val) = cmd.get("params").and_then(|p| p.get("cwd")).or_else(|| cmd.get("cwd")) {
                    if let Some(cwd) = cwd_val.as_str() {
                        cfg.project_path = Some(cwd.to_string());
                    }
                }
            }
            drop(reg);
            match registry.lock().await.create_worker(cfg, &registry).await {
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
            drop(reg);
            match registry.lock().await.create_worker(cfg, &registry).await {
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
// Host mode — temporary WorkerRegistry + event pump + auto-exit
// ---------------------------------------------------------------------------
//
// 架构原则（AGENTS.md）：内核只提供对等原语，编排策略全交给 .md 提示词。
// 详见 docs/design/TEAM_ORCHESTRATION.md
//
// --host: 快速编排模式。启动一个临时 WorkerRegistry + 事件泵，
// spawn 入口 Worker（agent 通过 --agent 参数指定，加载对应 .md）、
// 等全部 idle 后自动清理退出。
// 对应 CLI_ARCHITECTURE.md 场景 2。
//
// 架构原则：内核只提供对等原语，编排策略全交给 LLM + agent 提示词。
// entry Worker 通过 spawn_worker(child, ...) 工具派生子 Worker；
// wait loop 检测递归 idle → 所有 Worker 完成后退出。

async fn cmd_host(user_message: &str, agent_name: Option<&str>) {
    use std::sync::Arc;
    use tokio::sync::Mutex;
    use ion::worker_registry::{WorkerCreateConfig, WorkerRegistry};

    let ion_cfg = ion::config::IonConfig::load();
    let model = ion_cfg.default_model.clone().unwrap_or_else(|| "deepseek-v4-flash".to_string());
    let provider = ion_cfg.default_provider.clone().unwrap_or_else(|| "opencode".to_string());
    let agent = agent_name.unwrap_or("build").to_string();

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    eprintln!("[host] Starting WorkerRegistry");

    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));

    // 1. Event pump → stdout
    let pump_registry = Arc::clone(&registry);
    eprintln!("[pump] spawning...");
    tokio::spawn(async move {
        let mut subs: std::collections::HashMap<String, tokio::sync::mpsc::Receiver<serde_json::Value>> =
            std::collections::HashMap::new();
        // Per-worker line buffer: accumulate text_delta, flush on newline / agent_end
        let mut line_bufs: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        loop {
            {
                let mut reg = pump_registry.lock().await;
                let ids: Vec<String> = reg.workers.keys().cloned().collect();
                for wid in &ids {
                    if !subs.contains_key(wid) {
                        if let Ok(rx) = reg.subscribe(wid) {
                            subs.insert(wid.clone(), rx);
                            line_bufs.insert(wid.clone(), String::new());
                        }
                    }
                }
            }
            for (wid, rx) in subs.iter_mut() {
                while let Ok(msg) = rx.try_recv() {
                    if msg.get("type").and_then(|v| v.as_str()) != Some("event") { continue; }
                    let ev = msg.get("event").cloned().unwrap_or_default();
                    let et = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match et {
                        "text_delta" => {
                            if let Some(delta) = ev.get("delta").and_then(|v| v.as_str()) {
                                if delta.is_empty() { continue; }
                                let buf = line_bufs.entry(wid.clone()).or_default();
                                buf.push_str(delta);
                                // Flush complete lines
                                while let Some(nl) = buf.find('\n') {
                                    let line: String = buf.drain(..=nl).collect();
                                    let trimmed = line.trim_end();
                                    if !trimmed.is_empty() {
                                        println!("[{}] {}", &wid[..12.min(wid.len())], trimmed);
                                    }
                                }
                            }
                        }
                        "tool_call" => {
                            // Flush any pending buffer first
                            if let Some(buf) = line_bufs.get_mut(wid) {
                                if !buf.trim().is_empty() {
                                    println!("[{}] {}", &wid[..12.min(wid.len())], buf.trim());
                                    buf.clear();
                                }
                            }
                            if let Some(tn) = ev.get("tool").and_then(|v| v.as_str()) {
                                println!("[{}] 🔧 {}", &wid[..12.min(wid.len())], tn);
                            }
                        }
                        "agent_end" => {
                            // Flush any remaining buffered text
                            if let Some(buf) = line_bufs.get_mut(wid) {
                                if !buf.trim().is_empty() {
                                    println!("[{}] {}", &wid[..12.min(wid.len())], buf.trim());
                                    buf.clear();
                                }
                            }
                            println!("[{}] ✓ done", &wid[..12.min(wid.len())]);
                        }
                        "agent_start" => {
                            println!("[{}] ▶ start", &wid[..12.min(wid.len())]);
                        }
                        _ => {}
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    // 2. Manager command processing loop
    let cmd_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        loop {
            {
                let mut reg = cmd_registry.lock().await;
                reg.process_pending_commands(&cmd_registry).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });

    // 3. Spawn entry Worker (lock released before set_entry_worker to avoid deadlock)
    let mut cfg = WorkerCreateConfig::default();
    cfg.agent = Some(agent.clone());
    cfg.model = Some(model.clone());
    cfg.provider = Some(provider.clone());
    cfg.project_path = Some(cwd.clone());
    cfg.initial_prompt = Some(user_message.to_string());

    let entry = {
        let mut reg = registry.lock().await;
        match reg.create_worker(cfg, &registry).await {
            Ok(info) => {
                eprintln!("[host] spawned {} ({})", &info.worker_id[..12], agent);
                info
            }
            Err(e) => {
                eprintln!("[host] ❌ Failed to spawn worker: {e}");
                return;
            }
        }
    };

    // Set entry worker for recursive idle detection
    registry.lock().await.set_entry_worker(&entry.worker_id);
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    // 4. Wait for idle with configurable timeout
    let timeout_secs = std::env::var("ION_HOST_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30 * 60);
    eprintln!("[host] waiting for workers to complete... (timeout {timeout_secs}s)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let all_idle = {
            let reg = registry.lock().await;
            match reg.entry_worker_id.as_ref() {
                Some(eid) => reg.all_workers_idle(eid).unwrap_or(false),
                None => true,
            }
        };

        if all_idle {
            eprintln!("[host] recursive idle check passed, cleaning up");
            break;
        }

        if std::time::Instant::now() > deadline {
            eprintln!("[host] timeout reached, forcing exit");
            break;
        }
    }

    // 5. Cleanup
    eprintln!("[host] cleaning up");
}

// ---------------------------------------------------------------------------
// Session management (pi JSONL v3)
// ---------------------------------------------------------------------------

fn save_session(id: &str, messages: &[ion::agent::messages::Message], model: &str, provider: &str, name: Option<&str>) {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // 读已有文件，判断已写入的 message 数量 + 当前 leaf（光标）
    let path = ion::session_jsonl::session_path(&cwd);
    let mut existing_entries: Vec<serde_json::Value> = Vec::new();
    let mut header_existed = false;
    if let Ok(content) = std::fs::read_to_string(&path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }
            if let Ok(e) = serde_json::from_str::<serde_json::Value>(line) {
                if e.get("type").and_then(|v| v.as_str()) == Some("session") {
                    header_existed = true;
                }
                existing_entries.push(e);
            }
        }
    }

    // 文件不存在 → 先写 header
    if !header_existed {
        let header = ion::session_jsonl::SessionHeader {
            entry_type: "session".into(), version: 3, id: id.to_string(),
            timestamp: ion::session_jsonl::timestamp_iso(),
            cwd: cwd.clone(),
            parent_session: None,
        };
        if let Ok(h) = serde_json::to_string(&header) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&path) {
                let _ = writeln!(f, "{}", h);
            }
        }
    }

    // 统计已有 message 数（用 saved_msg_count 判断哪些是新增的）
    let saved_msg_count = existing_entries.iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("message"))
        .count();

    // 只 append 新增的 message（saved_msg_count 之后的部分）
    let new_msgs = if messages.len() > saved_msg_count {
        &messages[saved_msg_count..]
    } else {
        &[][..]
    };

    if !new_msgs.is_empty() {
        // parentId 从 resolve_current_leaf 取（leaf 感知，对齐 Session Tree）
        let parent_id = ion::session_tree::resolve_current_leaf(&existing_entries)
            .unwrap_or_else(|| id.to_string());
        let mut parent_id = parent_id;
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            if let Ok(meta) = f.metadata() {
                if meta.len() > 0 {
                    let _ = write!(f, "\n");
                }
            }
            for msg in new_msgs {
                let entry = ion::session_jsonl::message_to_entry(msg, &parent_id);
                if let Some(eid) = entry["id"].as_str() { parent_id = eid.to_string(); }
                let _ = writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default());
            }
        }
    }

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
    // Strategy 0: Direct file path
    if id.contains('/') || id.contains('\\') || id.ends_with(".jsonl") {
        if let Ok(content) = std::fs::read_to_string(id) {
            return parse_jsonl_messages(&content);
        }
    }

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
            return parse_jsonl_messages(&content);
        }
    }

    // Strategy 3: Treat id as cwd path (encoded)
    ion::session_jsonl::SessionFile::load(id).map(|f| f.messages)
}

/// Parse JSONL content into messages (skipping the header line).
fn parse_jsonl_messages(content: &str) -> Option<Vec<ion::agent::messages::Message>> {
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
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
    Some(messages)
}

/// Load session from a direct file path, extracting the actual session ID from the header.
/// Returns (session_id, messages).
fn load_session_from_path(path: &str) -> Option<(String, Vec<ion::agent::messages::Message>)> {
    let content = std::fs::read_to_string(path).ok()?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return None;
    }
    // Extract session ID from header (first line)
    let header: serde_json::Value = serde_json::from_str(lines[0]).ok()?;
    let sid = header.get("id")?.as_str()?.to_string();
    let msgs = parse_jsonl_messages(&content)?;
    Some((sid, msgs))
}

fn resolve_session_id(cli: &Cli) -> (String, Option<Vec<ion::agent::messages::Message>>) {
    if cli.no_session { return (String::new(), None); }
    if let Some(ref sid) = cli.fork {
        // File path support
        if sid.contains('/') || sid.contains('\\') || sid.ends_with(".jsonl") {
            if let Some((_real_id, msgs)) = load_session_from_path(sid) {
                let new_id = uuid::Uuid::new_v4().to_string();
                return (new_id, Some(msgs));
            }
        }
        if let Some(msgs) = load_session(sid) {
            let new_id = uuid::Uuid::new_v4().to_string();
            return (new_id, Some(msgs));
        }
        // Fallback: prefix match
        if let Some((_prefix_id, msgs)) = find_session_by_prefix(sid) {
            let new_id = uuid::Uuid::new_v4().to_string();
            return (new_id, Some(msgs));
        }
    }
    if let Some(ref sid) = cli.resume {
        if let Some(msgs) = load_session(sid) {
            return (sid.clone(), Some(msgs));
        }
        // Fallback: prefix match
        if let Some((prefix_id, msgs)) = find_session_by_prefix(sid) {
            return (prefix_id, Some(msgs));
        }
    }
    if let Some(ref sid) = cli.session {
        // Check if it's a file path (not a session ID)
        if sid.contains('/') || sid.contains('\\') || sid.ends_with(".jsonl") {
            if let Some((real_id, msgs)) = load_session_from_path(sid) {
                return (real_id, Some(msgs));
            }
        }
        if let Some(msgs) = load_session(sid) {
            return (sid.clone(), Some(msgs));
        }
        // Fallback: prefix match
        if let Some((prefix_id, msgs)) = find_session_by_prefix(sid) {
            return (prefix_id, Some(msgs));
        }
    }
    // --session-id: exact ID (create new with this ID if not found)
    if let Some(ref sid) = cli.session_id {
        if let Some(msgs) = load_session(sid) {
            return (sid.clone(), Some(msgs));
        }
        // Not found - return ID as-is so cmd_run creates new session with it
        return (sid.clone(), None);
    }
    if cli.continue_session {
        // 按 mtime 找最近的 session（对齐 pi 行为）
        if let Some((id, msgs)) = find_most_recent_session() {
            return (id, Some(msgs));
        }
        // Fallback: last_session file
        if let Ok(id) = std::fs::read_to_string(ion::session_jsonl::last_session_path()) {
            let id = id.trim();
            if !id.is_empty() {
                if let Some(msgs) = load_session(id) {
                    return (id.to_string(), Some(msgs));
                }
            }
        }
    }
    (String::new(), None)
}

/// Try to find a session by prefix match against the session index.
/// Returns (matched_id, messages) on first match.
fn find_session_by_prefix(prefix: &str) -> Option<(String, Vec<ion::agent::messages::Message>)> {
    let index = ion::session_index::SessionIndex::load();
    // Search session index keys for prefix match
    let matches: Vec<String> = index.sessions.keys()
        .filter(|k| k.starts_with(prefix))
        .cloned()
        .collect();
    if let Some(matched_id) = matches.first() {
        if let Some(msgs) = load_session(matched_id) {
            return Some((matched_id.clone(), msgs));
        }
    }
    // Fallback: scan sessions directory for matching file names
    let sessions_dir = ion::paths::sessions_dir();
    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        let mut candidates: Vec<(String, std::path::PathBuf)> = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let session_file = path.join("session.jsonl");
                if session_file.exists() {
                    // Read the header to get the session ID
                    if let Ok(content) = std::fs::read_to_string(&session_file) {
                        if let Some(first_line) = content.lines().next() {
                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(first_line) {
                                if let Some(sid) = val.get("id").and_then(|v| v.as_str()) {
                                    if sid.starts_with(prefix) {
                                        candidates.push((sid.to_string(), session_file));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Sort by recency (by dir name which includes timestamp) and take the first
        candidates.sort_by(|a, b| b.1.cmp(&a.1));
        if let Some((matched_id, _path)) = candidates.first() {
            if let Some(msgs) = load_session(matched_id) {
                return Some((matched_id.clone(), msgs));
            }
        }
    }
    None
}

/// Find the most recent session by scanning sessions directory for latest mtime.
/// Returns (session_id, messages) for the most recent session.
fn find_most_recent_session() -> Option<(String, Vec<ion::agent::messages::Message>)> {
    let sessions_dir = ion::paths::sessions_dir();
    let mut candidates: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let session_file = path.join("session.jsonl");
            if let Ok(meta) = session_file.metadata() {
                if let Ok(mtime) = meta.modified() {
                    candidates.push((session_file, mtime));
                }
            }
        }
    }

    // Sort by mtime descending, take the most recent
    candidates.sort_by(|a, b| b.1.cmp(&a.1));
    if let Some((path, _)) = candidates.first() {
        // Read session ID from header
        if let Ok(content) = std::fs::read_to_string(path) {
            if let Some(first_line) = content.lines().next() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(first_line) {
                    if let Some(sid) = val.get("id").and_then(|v| v.as_str()) {
                        if let Some(msgs) = load_session(sid) {
                            return Some((sid.to_string(), msgs));
                        }
                    }
                }
            }
        }
    }
    None
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

/// 启动 Dashboard：自动起 Manager（如果没在跑），然后 fork bun 进程
async fn launch_dashboard() {
    use std::process::Command;

    // 1. 检查 Host 是否在跑，没在跑就后台启动
    let sock = ion::paths::host_socket_path();
    let need_start = if !sock.exists() {
        true
    } else {
        // socket 文件在，验证能不能连
        match tokio::net::UnixStream::connect(&sock).await {
            Ok(_) => false,
            Err(_) => {
                // stale socket，删掉
                let _ = std::fs::remove_file(&sock);
                true
            }
        }
    };

    if need_start {
        let ion_bin = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("ion"));
        // Host 的 stdout/stderr 都重定向到日志文件，不污染 TUI
        let mgr_log = ion::paths::root().join("host.log");
        let mgr_out = std::fs::File::create(&mgr_log).ok();
        match Command::new(&ion_bin).arg("serve").arg("start")
            .stdout(std::process::Stdio::from(mgr_out.unwrap()))
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_child) => {
                // 等待 socket 就绪（最多 5 秒）
                for _ in 0..25 {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    if sock.exists() {
                        if tokio::net::UnixStream::connect(&sock).await.is_ok() {
                            break;
                        }
                    }
                }
                if !sock.exists() {
                    eprintln!("[ion] Host failed to start (see {})", mgr_log.display());
                    return;
                }
            }
            Err(e) => {
                eprintln!("[ion] Failed to start Host: {e}");
                return;
            }
        }
    }

    // 2. 找 dashboard 目录（相对可执行文件或当前目录）
    let candidates = [
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboard"),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.join("dashboard")))
            .unwrap_or_default(),
        std::path::PathBuf::from("dashboard"),
    ];
    let dashboard_dir = candidates.iter()
        .find(|p| p.join("src/index.ts").exists() || p.join("src/index.tsx").exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone());

    if !dashboard_dir.join("src/index.ts").exists() && !dashboard_dir.join("src/index.tsx").exists() {
        eprintln!("[ion] Dashboard not found at {}", dashboard_dir.display());
        return;
    }

    let entry_file = if dashboard_dir.join("src/index.tsx").exists() { "src/index.tsx" } else { "src/index.ts" };

    // 3. 检查 node_modules，没有就 bun install
    if !dashboard_dir.join("node_modules").exists() {
        eprintln!("[ion] Installing dashboard dependencies...");
        let _ = Command::new("bun").arg("install")
            .current_dir(&dashboard_dir)
            .status();
    }

    // 4. fork bun 进程跑 dashboard（前台，继承 TTY）
    let status = Command::new("bun")
        .arg("run")
        .arg(entry_file)
        .current_dir(&dashboard_dir)
        .status();

    match status {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!("[ion] Dashboard exited with code: {:?}", s.code()),
        Err(e) => eprintln!("[ion] Failed to launch bun (is bun installed?): {e}"),
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    // ── -p / --print ──
    #[test]
    fn test_print_short_flag() {
        let cli = Cli::try_parse_from(["ion", "-p", "hello"]).unwrap();
        assert!(cli.print);
        assert_eq!(cli.messages, vec!["hello"]);
    }

    #[test]
    fn test_print_long_flag() {
        let cli = Cli::try_parse_from(["ion", "--print", "hello"]).unwrap();
        assert!(cli.print);
        assert_eq!(cli.messages, vec!["hello"]);
    }

    #[test]
    fn test_print_no_message_is_false() {
        let cli = Cli::try_parse_from(["ion", "hello"]).unwrap();
        assert!(!cli.print);
    }

    // ── --system-prompt alias ──
    #[test]
    fn test_system_prompt_alias() {
        let cli = Cli::try_parse_from(["ion", "--system-prompt", "be concise", "hi"]).unwrap();
        assert_eq!(cli.prompt, Some("be concise".into()));
    }

    #[test]
    fn test_old_prompt_still_works() {
        let cli = Cli::try_parse_from(["ion", "-P", "be concise", "hi"]).unwrap();
        assert_eq!(cli.prompt, Some("be concise".into()));
    }

    // ── --continue / -c ──
    #[test]
    fn test_continue_short_flag() {
        let cli = Cli::try_parse_from(["ion", "-c", "hello"]).unwrap();
        assert!(cli.continue_session);
    }

    #[test]
    fn test_continue_long_flag() {
        let cli = Cli::try_parse_from(["ion", "--continue", "hello"]).unwrap();
        assert!(cli.continue_session);
    }

    #[test]
    fn test_continue_session_alias() {
        let cli = Cli::try_parse_from(["ion", "--continue-session", "hello"]).unwrap();
        assert!(cli.continue_session);
    }

    // ── --resume -r ──
    #[test]
    fn test_resume_short_flag() {
        let cli = Cli::try_parse_from(["ion", "-r", "sess_123"]).unwrap();
        assert_eq!(cli.resume, Some("sess_123".into()));
    }

    // ── --tools -t ──
    #[test]
    fn test_tools_short_flag() {
        let cli = Cli::try_parse_from(["ion", "-t", "read,write", "hello"]).unwrap();
        assert_eq!(cli.tools, Some("read,write".into()));
    }

    // ── --output-schema alias ──
    #[test]
    fn test_output_schema_alias() {
        let cli = Cli::try_parse_from(["ion", "--output-schema", r#"{"type":"object"}"#, "hi"]).unwrap();
        assert_eq!(cli.json_schema, Some(r#"{"type":"object"}"#.into()));
    }

    #[test]
    fn test_json_schema_still_works() {
        let cli = Cli::try_parse_from(["ion", "--json-schema", r#"{"type":"object"}"#, "hi"]).unwrap();
        assert_eq!(cli.json_schema, Some(r#"{"type":"object"}"#.into()));
    }

    // ── --mode ──
    #[test]
    fn test_mode_text() {
        let cli = Cli::try_parse_from(["ion", "--mode", "text", "hi"]).unwrap();
        assert!(matches!(cli.mode, Some(OutputMode::Text)));
    }

    #[test]
    fn test_mode_json() {
        let cli = Cli::try_parse_from(["ion", "--mode", "json", "hi"]).unwrap();
        assert!(matches!(cli.mode, Some(OutputMode::Json)));
    }

    #[test]
    fn test_mode_rpc() {
        let cli = Cli::try_parse_from(["ion", "--mode", "rpc"]).unwrap();
        assert!(matches!(cli.mode, Some(OutputMode::Rpc)));
    }

    #[test]
    fn test_mode_default_none() {
        let cli = Cli::try_parse_from(["ion", "hi"]).unwrap();
        assert!(cli.mode.is_none());
    }

    // ── --max-turns ──
    #[test]
    fn test_max_turns_default_is_none() {
        let cli = Cli::try_parse_from(["ion", "hi"]).unwrap();
        assert!(cli.max_turns.is_none());
    }

    #[test]
    fn test_max_turns_explicit_value() {
        let cli = Cli::try_parse_from(["ion", "--max-turns", "5", "hi"]).unwrap();
        assert_eq!(cli.max_turns, Some(5));
    }

    // ── resolve_schema ──
    #[test]
    fn test_resolve_schema_inline_json() {
        let schema = r#"{"type":"object"}"#;
        let result = EffectiveConfig::resolve_schema(&Some(schema.into()));
        assert_eq!(result, Some(schema.into()));
    }

    #[test]
    fn test_resolve_schema_file_path() {
        // Create a temp schema file
        let dir = std::env::temp_dir();
        let path = dir.join("ion_test_schema.json");
        let content = r#"{"type":"string"}"#;
        std::fs::write(&path, content).unwrap();

        let result = EffectiveConfig::resolve_schema(&Some(path.to_string_lossy().to_string()));
        assert_eq!(result, Some(content.into()));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_resolve_schema_at_file() {
        let dir = std::env::temp_dir();
        let path = dir.join("ion_test_schema_at.json");
        let content = r#"{"type":"number"}"#;
        std::fs::write(&path, content).unwrap();

        let result = EffectiveConfig::resolve_schema(&Some(format!("@{}", path.display())));
        assert_eq!(result, Some(content.into()));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_resolve_schema_none() {
        let result = EffectiveConfig::resolve_schema(&None);
        assert!(result.is_none());
    }

    // ── --session-id ──
    #[test]
    fn test_session_id_flag() {
        let cli = Cli::try_parse_from(["ion", "--session-id", "sess_custom_123", "hi"]).unwrap();
        assert_eq!(cli.session_id, Some("sess_custom_123".into()));
    }

    #[test]
    fn test_session_id_default_none() {
        let cli = Cli::try_parse_from(["ion", "hi"]).unwrap();
        assert!(cli.session_id.is_none());
    }

    // ── --session prefix matching (via Cli struct completeness) ──
    #[test]
    fn test_session_partial_uuid() {
        let cli = Cli::try_parse_from(["ion", "--session", "sess_abc", "hi"]).unwrap();
        assert_eq!(cli.session, Some("sess_abc".into()));
    }

    // ── parse_image_blocks ──
    #[test]
    fn test_parse_image_blocks_ignores_text() {
        let blocks = parse_image_blocks(&["hello".into(), "world".into()]);
        assert!(blocks.is_empty());
    }

    #[test]
    fn test_parse_image_blocks_invalid_path() {
        // Non-existent image file should not panic
        let blocks = parse_image_blocks(&["@/nonexistent/image.png".into()]);
        assert!(blocks.is_empty()); // silently skipped
    }

    // ── --model provider/id:thinking 解析 ──
    #[test]
    fn test_model_provider_id_parses() {
        let cli = Cli::try_parse_from(["ion", "--model", "opencode/deepseek-v4-flash", "hi"]).unwrap();
        assert_eq!(cli.model, Some("opencode/deepseek-v4-flash".into()));
        // Note: actual provider/model split happens in resolve_effective at runtime
    }

    #[test]
    fn test_model_thinking_suffix_parses() {
        let cli = Cli::try_parse_from(["ion", "--model", "deepseek-v4-flash:high", "hi"]).unwrap();
        assert_eq!(cli.model, Some("deepseek-v4-flash:high".into()));
    }

    #[test]
    fn test_model_provider_thinking_combined() {
        let cli = Cli::try_parse_from(["ion", "--model", "opencode/deepseek-v4-flash:high", "hi"]).unwrap();
        assert_eq!(cli.model, Some("opencode/deepseek-v4-flash:high".into()));
    }

    #[test]
    fn test_model_thinking_takes_precedence() {
        // --thinking flag should override :thinking suffix
        let cli = Cli::try_parse_from([
            "ion", "--model", "deepseek-v4-flash:high",
            "--thinking", "low", "hi"
        ]).unwrap();
        assert_eq!(cli.model, Some("deepseek-v4-flash:high".into()));
        assert_eq!(cli.thinking, Some("low".into()));
    }

    // ── --list-models flag ──
    #[test]
    fn test_list_models_flag_no_search() {
        let cli = Cli::try_parse_from(["ion", "--list-models"]).unwrap();
        assert_eq!(cli.list_models, Some("true".into()));
    }

    #[test]
    fn test_list_models_flag_with_search() {
        let cli = Cli::try_parse_from(["ion", "--list-models", "gpt"]).unwrap();
        assert_eq!(cli.list_models, Some("gpt".into()));
    }

    #[test]
    fn test_list_models_flag_default_none() {
        let cli = Cli::try_parse_from(["ion", "hi"]).unwrap();
        assert!(cli.list_models.is_none());
    }

    // ── ion config list ──
    #[test]
    fn test_config_list_subcommand() {
        let cli = Cli::try_parse_from(["ion", "config", "list"]).unwrap();
        assert!(matches!(cli.command, Some(Commands::Config { action: ConfigAction::List })));
    }

    // ── --compact-model flag ──
    #[test]
    fn test_compact_model_flag() {
        let cli = Cli::try_parse_from(["ion", "--compact-model", "gpt-4o-mini", "hi"]).unwrap();
        assert_eq!(cli.compact_model, Some("gpt-4o-mini".into()));
    }

    #[test]
    fn test_compact_model_default_none() {
        let cli = Cli::try_parse_from(["ion", "hi"]).unwrap();
        assert!(cli.compact_model.is_none());
    }
}
