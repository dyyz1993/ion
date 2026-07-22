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

    /// Branch from a specific entry in the current session (Session Tree)
    #[arg(long, global = true, value_name = "ENTRY_ID")]
    branch: Option<String>,

    /// Name the branch created by --branch
    #[arg(long, global = true, value_name = "NAME", requires = "branch")]
    branch_name: Option<String>,

    /// Switch to a named branch (Session Tree)
    #[arg(long, global = true, value_name = "NAME")]
    checkout: Option<String>,

    /// Rollback to a specific entry (path preserved, Session Tree)
    #[arg(long, global = true, value_name = "ENTRY_ID")]
    rollback: Option<String>,

    /// Reason for rollback (recorded as tombstone, plain text)
    #[arg(long, global = true, requires = "rollback")]
    rollback_reason: Option<String>,

    /// Restore code files when rolling back (requires file-snapshot extension)
    #[arg(long, global = true, requires = "rollback")]
    restore_code: bool,

    /// Restore mode for --restore-code: "delta" (default, only tracked files) or "full" (complete disk state via tree)
    /// full mode = restore_to_tree（恢复完整磁盘状态，含删除 target 之后新增的文件）
    /// delta mode = restore_code_to_turn（只恢复被快照追踪的文件改动）
    #[arg(long, global = true, requires = "restore_code", value_name = "delta|full", default_value = "delta")]
    restore_mode: Option<String>,

    /// Fork a new session from a specific leaf: <SESSION_ID>/<ENTRY_ID>
    #[arg(long, global = true, value_name = "SID/ENTRY_ID")]
    fork_from_leaf: Option<String>,

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
        /// Replay last N events on connect (refresh recovery)
        #[arg(long)]
        replay: Option<usize>,
    },
    /// List sessions for the current project (or all projects with --all)
    ///   ion sessions                  当前主仓库的会话（含 worktree）
    ///   ion sessions --json           JSON 输出（供脚本/UI 消费）
    ///   ion sessions --all            所有项目（不过滤）
    ///   ion sessions --limit 50       最多显示条数
    Sessions {
        /// Output as JSON (full fields, for scripts/UI)
        #[arg(long)]
        json: bool,
        /// Show sessions from ALL projects (disable project filtering)
        #[arg(long)]
        all: bool,
        /// Max sessions to display (table mode only)
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// View session message history (paginated)
    ///   ion history <session_id> [--limit 20] [--view live|full|since_compaction]
    History {
        /// Session ID or path
        session: String,
        /// Max messages to show
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// View: live (default) / since_compaction / full
        #[arg(long, default_value = "live")]
        view: String,
    },
    /// Session Tree operations (branch tree / named branches / path)
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
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
    /// Workflow operations (validate / run / status)
    Workflow {
        #[command(subcommand)]
        action: WorkflowAction,
    },
    /// Extension management (install / remove / list WASM extensions)
    Extension {
        #[command(subcommand)]
        action: ExtensionAction,
    },
}

/// Session Tree 子命令
#[derive(Subcommand, Clone)]
enum SessionAction {
    /// Show the message tree of a session
    Tree {
        /// Session ID (or prefix)
        session: String,
    },
    /// List named branches of a session
    Branches {
        /// Session ID (or prefix)
        session: String,
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

#[derive(Subcommand)]
enum WorkflowAction {
    /// Validate a workflow YAML file
    Validate {
        /// Path to workflow.yaml
        path: String,
    },
    /// Show workflow status (which stages are done/pending/failed)
    Status {
        /// Path to workflow.yaml
        path: String,
    },
    /// Run a workflow (spawns wf agent to execute stages)
    Run {
        /// Path to workflow.yaml
        path: String,
        /// Set context values before running (format: key=value, e.g. --set topic="修 bug")
        /// Can be repeated. Values are written into the yaml's context section.
        /// This is the deterministic escape hatch when you don't want to rely
        /// on the LLM editing the yaml itself.
        #[arg(long, value_name = "KEY=VALUE")]
        set: Vec<String>,
    },
}

/// Extension 子命令（install / remove / list）
#[derive(Subcommand, Clone)]
enum ExtensionAction {
    /// Install a WASM extension (.wasm) to the global extensions directory
    Install {
        /// Path to the .wasm file to install
        path: String,
    },
    /// Remove an installed WASM extension by name (filename without .wasm)
    Remove {
        /// Extension name (filename without .wasm)
        name: String,
    },
    /// List installed WASM extensions
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
    no_skills: bool,
    message: String,
    /// Agent name (from --agent), for session header banner
    agent: Option<String>,
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

    // Step 2.5: Resolve tier alias (fast/pro/max → provider/model-id)
    // 用户可以 --model fast，底层解析成具体模型；也支持直接指定模型
    let raw_model = if let Some(resolved) = cfg.tier_models.get(raw_model.trim()) {
        eprintln!("[model] tier alias '{}' → '{}'", raw_model.trim(), resolved);
        resolved.clone()
    } else {
        raw_model
    };

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
        no_skills: cli.no_skills,
        message: EffectiveConfig::parse_messages(&cli.messages),
        agent: cli.agent.clone(),
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
    // replay 模式：强制 model.api 指向 replay provider（绕过 find_model fallback 的 openai-completions）
    if eff.provider == "replay" {
        model.api = "replay".into();
        eprintln!("[replay] model.api forced to 'replay' (model_id={})", eff.model);
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
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> ion_provider::error::ProviderResult<ion_provider::event_stream::EventStream> {
        self.0.stream(model, context, options, cancel).await
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
        retry_on_no_tool_use: 0,
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

        // Skill tool — lets the LLM autonomously load skills by name.
        // Scans global (~/.ion/agent/skills/) and project (<cwd>/.ion/skills/) dirs.
        // Without this registration the LLM cannot invoke skills on its own;
        // only the --skill <path> CLI flag works (which injects into system_prompt).
        if !eff.no_skills {
            let cwd_str = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            // 扫描三个位置：
            // 1. ~/.ion/agent/skills/（ION 全局 skill）
            // 2. <project>/.ion/skills/（项目级 skill）
            // 3. ~/.agents/skills/（全局 skill 库，跟 ZCode 共享，111 个）
            let agents_skills = std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".agents").join("skills"))
                .unwrap_or_else(|| std::path::PathBuf::from("~/.agents/skills"));
            let skill_dirs: Vec<std::path::PathBuf> = vec![
                ion::paths::skills_dir(),
                ion::paths::project_skills_dir(&cwd_str),
                agents_skills,
            ];
            tools.register(Box::new(ion::agent::tool::SkillTool { skill_dirs }));
        }
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
// Workflow commands
// ---------------------------------------------------------------------------

async fn cmd_workflow_validate(path: &str) {
    match ion::workflow::WorkflowConfig::load(path) {
        Ok(wf) => {
            let gate_count = wf.stages.iter().filter(|s| s.gate.is_some()).count();
            let loop_count = wf.stages.iter().filter(|s| s.on_fail.is_some()).count();
            println!("✅ Valid workflow: {}", wf.name);
            println!("   {} stages, {} gates, {} loop_backs", wf.stages.len(), gate_count, loop_count);
            for stage in &wf.stages {
                let gate_str = if stage.gate.is_some() { " 🔒gate" } else { "" };
                let wt_str = if stage.worktree { " 🌳worktree" } else { "" };
                let lb_str = stage.on_fail.as_ref()
                    .map(|f| format!(" ↩︎loop_back→{}", f.loop_back))
                    .unwrap_or_default();
                println!("   • {} [{}]{}{}{}", stage.id, stage.status, gate_str, wt_str, lb_str);
            }
        }
        Err(e) => {
            eprintln!("❌ {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_workflow_status(path: &str) {
    match ion::workflow::WorkflowConfig::load(path) {
        Ok(wf) => {
            println!("Workflow: {}", wf.name);
            for stage in &wf.stages {
                let icon = match stage.status.as_str() {
                    "done" => "✅",
                    "failed" => "❌",
                    "running" => "🔄",
                    "skipped" => "⏭️",
                    _ => "⏳",
                };
                println!("  {}: {} {}", stage.id, icon, stage.status);
            }
            if wf.is_complete() {
                println!("\nPIPELINE COMPLETE ✅");
            } else if let Some(next) = wf.next_pending_stage() {
                println!("\nNext: {} ({})", next.id, next.status);
            }
        }
        Err(e) => {
            eprintln!("❌ {}", e);
            std::process::exit(1);
        }
    }
}

async fn cmd_workflow_run(path: &str, set: &[String]) {
    // 先校验
    if let Err(e) = ion::workflow::WorkflowConfig::load(path) {
        eprintln!("❌ {}", e);
        std::process::exit(1);
    }

    // 如果有 --set key=value，先写进 yaml 的 context 段
    // 这是"确定性逃生通道"：不依赖 LLM edit yaml，直接用命令行参数注入 context
    if !set.is_empty() {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let mut updated = content.clone();
        for kv in set {
            // 解析 key=value（value 含 = 也允许，按第一个 = 切）
            let (key, value) = match kv.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => {
                    eprintln!("⚠️ 忽略无效 --set 参数（需要 key=value 格式）: {}", kv);
                    continue;
                }
            };
            // 纯字符串行级替换（不引入 regex 依赖）
            // 匹配 yaml context 段下 `  key: "xxx"` 或 `  key: xxx` 这一行
            let prefix = format!("{}:", key);
            let new_value_quoted = format!("\"{}\"", value.replace('"', "\\\""));
            let mut found = false;
            let lines: Vec<&str> = updated.lines().collect();
            let mut out_lines: Vec<String> = Vec::with_capacity(lines.len());
            let mut in_context = false;
            for line in lines {
                // 检测 context: 段（顶层 key，不以空格开头）
                if line == "context:" || line.trim() == "context:" {
                    in_context = true;
                    out_lines.push(line.to_string());
                    continue;
                }
                // 离开 context 段（遇到另一个顶层 key 且非空行）
                if in_context && !line.is_empty() && !line.starts_with(' ') && !line.starts_with('#') {
                    in_context = false;
                }
                // 在 context 段里找 key:
                if in_context && line.trim_start().starts_with(&prefix) {
                    let indent = line.len() - line.trim_start().len();
                    out_lines.push(format!("{}{}: {}", " ".repeat(indent), key, new_value_quoted));
                    found = true;
                    eprintln!("✅ --set {}=<value>（已更新 yaml）", key);
                } else {
                    out_lines.push(line.to_string());
                }
            }
            if !found {
                eprintln!("⚠️ --set {} 没匹配到 yaml context 字段（跳过）", key);
            } else {
                updated = out_lines.join("\n");
                if !updated.ends_with('\n') {
                    updated.push('\n');
                }
            }
        }
        if updated != content {
            std::fs::write(path, &updated).ok();
        }
    }

    // 用绝对路径（wf agent 需要读这个文件）
    let abs_path = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| path.to_string());

    eprintln!("🚀 Starting workflow: {}", abs_path);

    // 强制 wf agent 用唯一新 session（不复用 cwd-hash 旧 session）
    // 避免 wf agent "记得上次跑过"导致跳步
    //
    // 两步配合：
    // 1. ION_FORCE_SESSION_ID 设唯一 sid（让 WorkerCreateConfig.session 用它）
    // 2. ION_FORK_CHILD=1 让 ion_worker 用 <sid>.jsonl 独立文件（不复用 cwd-hash 的 session.jsonl）
    //    否则即使 sid 是新的，文件位置还是按 cwd hash 定位，会加载旧 session 的历史
    //
    // 注意：ION_FORK_CHILD 只在 ion 主进程设，create_worker spawn entry worker 时继承，
    // 但 wf spawn 的子 worker（developer/build）也会继承——这是可接受的（它们也应该用独立 session 文件）
    let wf_session_id = format!("sess_wf_{}", std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis());
    // Rust 2024 edition 里 set_var 是 unsafe
    unsafe {
        std::env::set_var("ION_FORCE_SESSION_ID", &wf_session_id);
        std::env::set_var("ION_FORK_CHILD", "1");
        std::env::set_var("ION_AUTO_CONTINUE", "1");
        std::env::set_var("ION_MAX_OUTER_ITERATIONS", "30");
        // wf agent 要跑完所有 stage（10 个）。
        // 不能设 0（无限）——那样 inner_loop 永不退出，outer_loop 的 auto_continue 不触发。
        // 设 15：让 inner_loop 跑 15 turn（够 2-3 stage）就 Stop 返回 outer_loop，
        // outer_loop 的 auto_continue 注入 follow-up 继续。
        unsafe { std::env::set_var("ION_MAX_TURNS", "15"); }
        // auto-continue 的 gate：当 workflow yaml 所有 10 stage 都有 status 时停止注入 follow-up
        unsafe {
            std::env::set_var("ION_AUTO_CONTINUE_GATE", "test -f .ion/workflow.yaml && [ $(grep -c 'status:' .ion/workflow.yaml) -ge 10 ] && echo ALL_DONE || echo NOT_DONE");
            std::env::set_var("ION_AUTO_CONTINUE_EXPECTED", "ALL_DONE");
        }
    }

    // 同步更新 last_session，让 export_report stage 的 ion --export 能找到 wf 的 session
    // （ION_FORK_CHILD=1 让 wf 用 <sid>.jsonl 独立文件，但 last_session 不自动更新）
    let _ = std::fs::write(
        ion::session_jsonl::last_session_path(),
        &wf_session_id,
    );

    // 启动 wf agent（--host 模式）
    // wf agent 读取 yaml 文件，执行 stages
    //
    // message 措辞关键：明确告诉 wf agent "yaml 是全新的，所有 stage 都没 status，
    // 必须从第一个 stage 开始执行"。避免 LLM 幻觉"已经跑过了"。
    let message = format!(
        "Read the workflow file at {} and execute ALL stages from the first one. \
         The yaml is fresh — no stage has a status field yet, so every stage is pending. \
         Do NOT say 'already executed' or 'no pending stages'. \
         Start by reading the yaml, then execute stage by stage: \
         edit status to running → execute (spawn_worker or bash) → check gate → edit status to done. \
         Follow the instructions in your system prompt exactly.",
        abs_path
    );

    // 复用 cmd_host 的逻辑
    cmd_host(&message, Some("wf")).await;
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
    session_id_in: &str,
    preloaded: Option<Vec<ion::agent::messages::Message>>,
    raw_messages: &[String],
    export_after: Option<&str>,
) {
    // Generate a stable session id up-front if none was provided.
    // This id is used for: session header, save_session, --export resolution.
    // Avoids the "empty id" problem when exporting after a new-session run.
    // Resolve the session id we'll use for this run.
    // - If caller passed one in (resume/fork), use it.
    // - Else if a session file already exists for this cwd, reuse its header id
    //   (so we append to the same session instead of inventing a mismatched id).
    // - Else generate a fresh sess_<8-char> id for the new session.
    let owned_sid = if !session_id_in.is_empty() {
        session_id_in.to_string()
    } else {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let existing_path = ion::session_jsonl::session_path(&cwd);
        std::fs::read_to_string(&existing_path)
            .ok()
            .and_then(|c| c.lines().next().map(|s| s.to_string()))
            .and_then(|h| serde_json::from_str::<serde_json::Value>(&h).ok())
            .and_then(|v| {
                v.get("id")
                    .and_then(|i| i.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| format!("sess_{}", &uuid::Uuid::new_v4().to_string()[..8]))
    };
    let session_id: &str = &owned_sid;
    // Persist to last_session so --continue / --export can find it later.
    let _ = std::fs::write(ion::session_jsonl::last_session_path(), session_id);

    // Set session header env vars (for save_session to include agent/model in header)
    unsafe {
        if let Some(ref a) = eff.agent { std::env::set_var("ION_SESSION_AGENT", a); }
        std::env::set_var("ION_SESSION_MODEL", &eff.model);
        std::env::set_var("ION_SESSION_PROVIDER", &eff.provider);
    }

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

    // ── MCP（场景 1：直接持有 McpManager + 直连 McpTool，不走 bridge）──
    let mcp_config = ion::config::IonConfig::load().mcp_servers;
    if !mcp_config.is_empty() && !eff.no_extensions {
        let mcp_manager = std::sync::Arc::new(ion::mcp::McpManager::new(mcp_config));
        tracing::info!("[mcp] connecting {} server(s)...", mcp_manager.server_count());
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            mcp_manager.connect_all(),
        ).await;
        let mcp_tools = mcp_manager.all_discovered_tools().await;
        for tool in &mcp_tools {
            tools.register(Box::new(ion::mcp::tool::McpTool::new(tool, mcp_manager.clone())));
        }
        tracing::info!("[mcp] {} tools registered from {} server(s)",
            mcp_tools.len(), mcp_manager.connected_count().await);
        mcp_manager.spawn_reconnect_monitor();
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

    // Snapshot tool definitions before passing ownership to Agent.
    // Used for --export-after-run: HTML export shows the tools panel.
    let tool_defs_snapshot: Vec<ion::export::ExportToolInfo> = tools
        .tool_defs()
        .into_iter()
        .map(|td| ion::export::ExportToolInfo {
            name: td.name,
            description: td.description,
            parameters: td.parameters,
        })
        .collect();

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
            source: ion_provider::types::MessageSource::Prompt,
        }));
    }
    if !initial_messages.is_empty() {
        agent = agent.with_messages(initial_messages);
    }
    let mut ext_reg = ion::agent::extension::ExtensionRegistry::new();

    // ── 注入 ctx.fs 统一文件访问能力（RuntimeFileSystem）──
    // 场景 1（直接执行）：用 LocalRuntime（本地 fs）+ allowed_roots 白名单。
    // 内置扩展通过 registry.filesystem() 拿到，WASM 扩展通过 host_read_file 拿到。
    {
        let fs_rt: std::sync::Arc<dyn ion::runtime::Runtime> =
            std::sync::Arc::new(ion::runtime::LocalRuntime::new());
        let fs_allowed_roots =
            ion::agent::extension::RuntimeFileSystem::default_allowed_roots(
                std::path::Path::new(&cwd),
            );
        let runtime_fs = std::sync::Arc::new(
            ion::agent::extension::RuntimeFileSystem::new(fs_rt, fs_allowed_roots),
        );
        ext_reg = ext_reg.with_filesystem(runtime_fs.clone());
        // WASM 扩展用（注入到 WASM registry 的共享 Context）
        {
            let mut ctx = wasm_ext_registry.ctx.write().unwrap();
            ctx.fs = Some(runtime_fs);
            ctx.tokio_handle = Some(tokio::runtime::Handle::current());
        }
        tracing::info!("[extension] ctx.fs (RuntimeFileSystem) injected");
    }

    // ── 注入 StorageContext（扩展通过 registry.data_dirs(name) 拿 4 级数据目录）──
    ext_reg = ext_reg.with_storage(ion::storage_context::StorageContext::new(
        &cwd, &session_id, &cwd,
    ));
    tracing::info!("[extension] StorageContext injected (data_dirs available)");

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
                                            break;
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
                                        break;
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
                                    break;
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
                                break;
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
                    break;
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
                    break;
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

    // ── Export after run (if --export was given alongside a prompt) ──
    // Produces HTML with the agent's actual tool registry populated, so the
    // "Available Tools" panel renders. Standalone `--export` (no prompt) goes
    // through the earlier branch and has no tools — matching pi's exportFromFile.
    if let Some(export_path) = export_after {
        let tools_opt = if tool_defs_snapshot.is_empty() {
            None
        } else {
            Some(tool_defs_snapshot.clone())
        };
        match ion::export::export_session_with_tools(
            session_id,
            std::path::Path::new(export_path),
            tools_opt,
        ) {
            Ok(()) => println!("Exported to {export_path}"),
            Err(e) => eprintln!("Export failed: {e}"),
        }
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

    // 读响应——host 可能先推事件（worker_created/project_changed 等），
    // 我们要跳过事件，找到带 `id` 字段的真正响应（rpc-client 标记）。
    let mut reader = BufReader::new(stream);
    let mut attempts = 0;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line).await {
            Ok(0) => {
                eprintln!("(Manager closed connection without response)");
                break;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() { continue; }
                // 尝试解析
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
                    // 跳过事件（type:event / type:worker_created 等没有 id 字段）
                    if v.get("id").is_some() {
                        // 这是真正的 RPC 响应
                        println!("{}", serde_json::to_string_pretty(&v).unwrap_or(line));
                        break;
                    }
                    // 是事件，跳过（不打印，避免污染 stdout）
                    continue;
                }
                // 非 JSON 行，打印 + 继续
                print!("{line}");
            }
            Err(e) => {
                eprintln!("❌ read socket failed: {e}");
                break;
            }
        }
        attempts += 1;
        if attempts > 100 {
            eprintln!("❌ rpc 超时：读了 100 行还没找到响应");
            break;
        }
    }
}

/// Subscribe to real-time events from a session or plugin.
/// Connects to Manager socket, sends subscribe, prints events line by line.
async fn cmd_subscribe(session: Option<&str>, extension: Option<&str>, ui: bool, replay: Option<usize>) {
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
    if let Some(n) = replay { req["replay"] = serde_json::json!(n); }

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

/// 格式化 Unix-ms 时间戳为可读的相对时间（如 "2h ago" / "3d ago"）。
/// 避免手写日历/时区转换（跨平台易错），用相对时间给人看最直观。
fn fmt_ts(ms: i64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff_secs = (now - ms).max(0) / 1000;
    if diff_secs < 60 {
        return format!("{}s ago", diff_secs);
    }
    if diff_secs < 3600 {
        return format!("{}m ago", diff_secs / 60);
    }
    if diff_secs < 86400 {
        return format!("{}h ago", diff_secs / 3600);
    }
    format!("{}d ago", diff_secs / 86400)
}

async fn cmd_sessions(json: bool, all: bool, limit: usize) {
    let index = ion::session_index::SessionIndex::load();
    if index.sessions.is_empty() {
        if json {
            println!("{{\"project\":null,\"sessions\":[],\"totalCount\":0}}");
        } else {
            println!("No sessions found.");
        }
        return;
    }

    // 算当前主仓库的 project_key（用于过滤）。
    // 缓存每个 project 路径的 key，避免重复 fork git 子进程。
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let current_key = ion::paths::project_key_git(&cwd);
    let mut key_cache: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    key_cache.insert(cwd.clone(), current_key.clone());

    // 过滤：--all 时不过滤；否则只保留 project_key == 当前主仓库的会话
    let mut entries: Vec<(&String, &ion::session_index::SessionMeta)> =
        index.sessions.iter().collect();
    if !all {
        entries.retain(|(_, meta)| {
            let proj = meta.project.as_deref().unwrap_or("");
            let key = key_cache
                .entry(proj.to_string())
                .or_insert_with(|| ion::paths::project_key_git(proj))
                .clone();
            key == current_key
        });
    }
    entries.sort_by(|a, b| b.1.updated_at.cmp(&a.1.updated_at));

    // ── JSON 输出 ──
    if json {
        let sessions_json: Vec<_> = entries.iter().map(|(id, m)| {
            serde_json::json!({
                "id": id,
                "name": m.name,
                "project": m.project,
                "projectName": m.project_name,
                "worktree": m.worktree,
                "branch": m.branch,
                "model": m.model,
                "agent": m.agent,
                "provider": m.provider,
                "createdAt": m.created_at,
                "updatedAt": m.updated_at,
                "messageCount": m.message_count,
                "turnCount": m.turn_count,
                "tokenInput": m.token_input,
                "tokenOutput": m.token_output,
                "tokenCacheRead": m.token_cache_read,
                "tokenCacheWrite": m.token_cache_write,
                "parentSession": m.parent_session,
                "thinkingLevel": m.last_thinking_level,
            })
        }).collect();
        let project_label = if all {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "cwd": cwd,
                "projectKey": current_key,
            })
        };
        println!("{}", serde_json::json!({
            "project": project_label,
            "sessions": sessions_json,
            "totalCount": entries.len(),
        }).to_string());
        return;
    }

    // ── 表格输出 ──
    if entries.is_empty() {
        if all {
            println!("No sessions found.");
        } else {
            println!("No sessions found for current project: {}", cwd);
            println!("(use 'ion sessions --all' to list all projects)");
        }
        return;
    }

    if !all {
        println!("📦 Project: {}  (key: {})", cwd, &current_key[..8]);
        println!();
    }
    // ID  AGENT  MODEL  BRANCH  MSGS  TOKENS(IN/OUT/CACHE)  CREATED  UPDATED  WT
    println!(
        "{:<12} {:<12} {:<22} {:<16} {:<5} {:<19} {:<13} {:<13} {}",
        "ID", "AGENT", "MODEL", "BRANCH", "MSGS", "TOKENS(IN/OUT/CA)", "CREATED", "UPDATED", "WT"
    );
    println!("{}", "-".repeat(130));
    for (id, meta) in entries.iter().take(limit) {
        let short_id = if id.len() > 10 { &id[..10] } else { id.as_str() };
        let name = meta.name.as_deref().unwrap_or("");
        let branch = meta.branch.as_deref().unwrap_or("");
        let wt = if meta.worktree { "🌿" } else { "" };
        let cache = meta.token_cache_read + meta.token_cache_write;
        let _ = name;
        println!(
            "{:<12} {:<12} {:<22} {:<16} {:<5} {:<19} {:<13} {:<13} {}",
            short_id,
            meta.agent,
            meta.model,
            branch,
            meta.message_count,
            format!("{}/{}/{}", meta.token_input, meta.token_output, cache),
            fmt_ts(meta.created_at),
            fmt_ts(meta.updated_at),
            wt,
        );
    }
    let total_in: u64 = entries.iter().map(|(_, s)| s.token_input).sum();
    let total_out: u64 = entries.iter().map(|(_, s)| s.token_output).sum();
    let total_cache: u64 = entries.iter().map(|(_, s)| s.token_cache_read + s.token_cache_write).sum();
    println!();
    println!(
        "Total: {} sessions | {} tokens ({} in / {} out / {} cache)",
        entries.len(),
        total_in + total_out + total_cache,
        total_in,
        total_out,
        total_cache,
    );
}

/// `ion history <session_id>` — 查看会话消息历史（分页拉取）。
async fn cmd_history(session: &str, limit: usize, view: &str) {
    // 加载 entries（load_session_entries 已支持 session id 和文件路径两种）
    let entries = match load_session_entries(session) {
        Some(e) => e,
        None => {
            eprintln!("Session not found: {session}");
            std::process::exit(1);
        }
    };

    // 解析 view
    let v = match view {
        "since_compaction" => ion::message_retrieval::View::SinceCompaction,
        "full" => ion::message_retrieval::View::Full,
        s if s.starts_with("branch:") => ion::message_retrieval::View::Branch(s[7..].to_string()),
        _ => ion::message_retrieval::View::Live,
    };

    let params = ion::message_retrieval::RetrievalParams {
        view: v,
        limit,
        ..Default::default()
    };
    let result = ion::message_retrieval::retrieve_messages(&entries, &params);

    // 打印
    println!("═══ Session History: {} ═══", session);
    println!("View: {} | Showing {} of {} messages", result.view, result.messages.len(), result.total_count);
    if !result.compaction_points.is_empty() {
        println!("⚡ Compaction points: {}", result.compaction_points.len());
    }
    println!();

    for msg in &result.messages {
        let entry_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let role = msg
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("?");
        let content = msg
            .get("message")
            .and_then(|m| m.get("content"))
            .map(|c| {
                if let Some(s) = c.as_str() {
                    s.to_string()
                } else if let Some(arr) = c.as_array() {
                    arr.iter()
                        .filter_map(|b| {
                            if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                                b.get("text").and_then(|t| t.as_str()).map(|s| s.to_string())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("")
                } else {
                    String::new()
                }
            })
            .unwrap_or_default();

        // 截断长内容
        let display: String = content.chars().take(200).collect();
        let suffix = if content.chars().count() > 200 { "..." } else { "" };

        let role_icon = match role {
            "user" => "👤",
            "assistant" => "🤖",
            "toolResult" => "📄",
            _ => "  ",
        };
        println!("{} [{}] {}", role_icon, entry_id, display);
        if !suffix.is_empty() {
            println!("      {}", suffix);
        }
    }

    if result.has_more {
        println!("\n--- {} more messages (use --limit to load more) ---", result.total_count - result.messages.len());
    }
}

/// 应用 Session Tree 操作（branch/checkout/rollback）。
/// 在 agent.run 之前调用：往 session 文件追加 leaf_pointer（+可选 label/tombstone）。
/// 后续消息通过 leaf 感知的 append 正确接在新分支上。
fn apply_session_tree_ops(cli: &Cli, session_id: &str) {
    // 解析 session 的真实 cwd：优先从 index 查，fallback 到 CLI 进程 cwd
    let cwd = if !session_id.is_empty() {
        ion::session_index::SessionIndex::load()
            .get(session_id)
            .and_then(|m| m.project.clone())
            .unwrap_or_else(|| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            })
    } else {
        std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    // session_id 为空时，从当前 cwd 的 session 文件加载 entries
    let load_entries = |sid: &str| -> Option<Vec<serde_json::Value>> {
        if sid.is_empty() {
            // fallback: 直接从 cwd 读 session 文件
            let path = ion::session_jsonl::session_path(&cwd);
            let content = std::fs::read_to_string(&path).ok()?;
            let mut entries = Vec::new();
            for line in content.lines() {
                let line = line.trim();
                if line.is_empty() { continue; }
                if let Ok(e) = serde_json::from_str::<serde_json::Value>(line) {
                    entries.push(e);
                }
            }
            if entries.is_empty() { None } else { Some(entries) }
        } else {
            load_session_entries(sid)
        }
    };

    // --checkout <name>
    if let Some(name) = &cli.checkout {
        let entries = load_entries(session_id);
        match entries {
            Some(ents) => {
                match ion::session_tree::make_checkout(&ents, name) {
                    Ok(new_entries) => {
                        for e in &new_entries {
                            ion::session_jsonl::append_raw_entry(&cwd, e);
                        }
                        eprintln!("[checkout] switched to branch '{}'", name);
                    }
                    Err(e) => {
                        eprintln!("❌ {}", e);
                        std::process::exit(1);
                    }
                }
            }
            None => {
                eprintln!("❌ cannot checkout: session {} not found", session_id);
                std::process::exit(1);
            }
        }
        return;
    }

    // --rollback <id> [--rollback-reason <text>]
    if let Some(rollback_to) = &cli.rollback {
        let entries = load_entries(session_id);
        let ents = entries.unwrap_or_default();
        if !ion::session_tree::entry_exists(&ents, rollback_to) {
            eprintln!("❌ entry '{}' not found in session {}", rollback_to, session_id);
            std::process::exit(1);
        }
        // compaction 安全检查
        if let Some(c_id) = ion::session_tree::check_compaction_safety(&ents, rollback_to) {
            // XL1: --restore-code 穿越压缩点时，只恢复代码不回滚消息（消息层的压缩上下文已丢失）
            if cli.restore_code {
                eprintln!("⚠️  Cannot rollback messages to {}: it is before a compaction point ({}).", rollback_to, c_id);
                eprintln!("   --restore-code: only restoring code files, skipping message rollback.");
                eprintln!("   (快照层独立于压缩，代码可以恢复；但消息无法回滚到压缩点之前)");
                // 只走代码恢复，不走消息回滚
                let target_turn_id: Option<String> = ion::session_jsonl::find_turn_id_for_entry(&cwd, rollback_to);
                match target_turn_id {
                    Some(turn_id) => {
                        let pk = ion::file_snapshot::project_key(&cwd);
                        let store = ion::file_snapshot::SnapshotStore::new(&pk);
                        let result = ion::file_snapshot::restore::restore_code_to_turn(&store, &turn_id);
                        eprintln!("[restore-code] restored {} files (deleted {}, skipped {})",
                            result.summary.restored, result.summary.deleted, result.summary.skipped);
                        eprintln!("[restore-code] restore_point: {}", result.restore_point_id);
                    }
                    None => {
                        eprintln!("[restore-code] ⚠️  cannot find turnId for entry '{}' — skipping code restore", rollback_to);
                    }
                }
                return; // 不走消息回滚，直接返回
            }
            // 非 --restore-code：普通回滚穿越压缩点 → 拒绝（消息上下文会丢失）
            eprintln!("❌ Cannot rollback to {}: it is before a compaction point ({}).", rollback_to, c_id);
            eprintln!("   Branching across compaction loses summarized context.");
            eprintln!("   Hint: use `ion --fork-from-leaf {}/{}` instead, or add --restore-code to only restore files.", session_id, rollback_to);
            std::process::exit(1);
        }
        let old_leaf = ion::session_tree::resolve_current_leaf(&ents);

        // --restore-code：先恢复代码文件，再回滚消息
        if cli.restore_code {
            // 找到 rollback_to 所属的 turn_summary → 得到 turnId（不靠 entryRange，用位置回溯）
            let target_turn_id: Option<String> = ion::session_jsonl::find_turn_id_for_entry(&cwd, rollback_to);

            match target_turn_id {
                Some(turn_id) => {
                    let pk = ion::file_snapshot::project_key(&cwd);
                    let store = ion::file_snapshot::SnapshotStore::new(&pk);
                    // XL3: --restore-mode full 走 restore_to_tree（完整磁盘状态），否则走 delta
                    let is_full = cli.restore_mode.as_deref() == Some("full");
                    if is_full {
                        // full mode：按 turn_id 找 tree_hash → restore_to_tree
                        match store.find_tree_hash_by_turn_id(&turn_id) {
                            Some(tree_hash) => {
                                let result = ion::file_snapshot::restore::restore_to_tree(
                                    &store, &tree_hash, &cwd, false,
                                );
                                eprintln!("[restore-code:full] restored {} files (deleted {}, skipped {})",
                                    result.summary.restored, result.summary.deleted, result.summary.skipped);
                                eprintln!("[restore-code:full] restore_point: {}", result.restore_point_id);
                                // 检查是否有截断跳过
                                if result.restored_files.iter().any(|f| f.reason.as_deref() == Some("scan_truncated_skip_delete")) {
                                    eprintln!("[restore-code:full] ⚠️  scan truncated — deletion phase skipped to avoid data loss");
                                }
                            }
                            None => {
                                eprintln!("[restore-code:full] ⚠️  cannot find tree for turn '{}' — falling back to delta mode", turn_id);
                                let result = ion::file_snapshot::restore::restore_code_to_turn(&store, &turn_id);
                                eprintln!("[restore-code:delta] restored {} files (deleted {}, skipped {})",
                                    result.summary.restored, result.summary.deleted, result.summary.skipped);
                            }
                        }
                    } else {
                        // delta mode（默认）：只恢复被快照追踪的文件改动
                        let result = ion::file_snapshot::restore::restore_code_to_turn(&store, &turn_id);
                        eprintln!("[restore-code] restored {} files (deleted {}, skipped {})",
                            result.summary.restored, result.summary.deleted, result.summary.skipped);
                        eprintln!("[restore-code] restore_point: {}", result.restore_point_id);
                    }
                }
                None => {
                    eprintln!("[restore-code] ⚠️  cannot find turnId for entry '{}' — skipping code restore", rollback_to);
                }
            }
        }

        let new_entries = ion::session_tree::make_rollback(
            rollback_to,
            old_leaf.as_deref(),
            cli.rollback_reason.as_deref(),
        ).unwrap();
        for e in &new_entries {
            ion::session_jsonl::append_raw_entry(&cwd, e);
        }
        eprintln!("[rollback] moved leaf to {}", rollback_to);
        if cli.rollback_reason.is_some() {
            eprintln!("[rollback] tombstone recorded");
        }
        return;
    }

    // --branch <id> [--branch-name <name>]
    if let Some(from_id) = &cli.branch {
        let entries = load_entries(session_id);
        let ents = entries.unwrap_or_default();
        if !ion::session_tree::entry_exists(&ents, from_id) {
            eprintln!("❌ entry '{}' not found in session {}", from_id, session_id);
            std::process::exit(1);
        }
        // compaction 安全检查
        if let Some(c_id) = ion::session_tree::check_compaction_safety(&ents, from_id) {
            eprintln!("❌ Cannot branch at {}: it is before a compaction point ({}).", from_id, c_id);
            eprintln!("   Branching across compaction loses summarized context.");
            eprintln!("   Hint: use `ion --fork-from-leaf {}/{}` instead.", session_id, from_id);
            std::process::exit(1);
        }
        let new_entries = ion::session_tree::make_branch(from_id, cli.branch_name.as_deref()).unwrap();
        for e in &new_entries {
            ion::session_jsonl::append_raw_entry(&cwd, e);
        }
        eprintln!("[branch] moved leaf to {}", from_id);
        if let Some(name) = &cli.branch_name {
            eprintln!("[branch] labeled: {} → {}", name, from_id);
        }
        return;
    }
}

/// 执行 fork-from-leaf：<SESSION_ID>/<ENTRY_ID>
/// 提取 root→entry 的路径，写入新 session 文件（parentSession 记录源）。
/// 返回新 session id。
fn do_fork_from_leaf(spec: &str) -> Option<String> {
    let (src_sid, leaf_id) = spec.split_once('/')?;
    let src_sid = resolve_session_id_simple(src_sid);
    let entries = load_session_entries(&src_sid)?;
    // 提取 root→leaf 路径
    let path = ion::session_tree::get_branch_path(&entries, leaf_id);
    if path.is_empty() {
        eprintln!("❌ leaf '{}' not found in session {}", leaf_id, src_sid);
        return None;
    }
    // 生成新 session
    let new_id = uuid::Uuid::new_v4().to_string();
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    // 找源文件路径（用于 parentSession）
    let src_path = find_session_file(&src_sid);
    // 写新文件
    let new_path = ion::session_jsonl::session_path(&cwd);
    if let Some(parent) = new_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(&new_path) {
        // header
        let header = serde_json::json!({
            "type": "session", "version": 3, "id": new_id,
            "timestamp": ion::session_jsonl::timestamp_iso(),
            "cwd": cwd,
            "parentSession": src_path,
        });
        let _ = writeln!(f, "{}", serde_json::to_string(&header).unwrap_or_default());
        // path entries（保留原 id 和 parentId）
        for e in &path {
            let _ = writeln!(f, "{}", serde_json::to_string(e).unwrap_or_default());
        }
    }
    let _ = std::fs::write(ion::session_jsonl::last_session_path(), &new_id);
    eprintln!("[fork-from-leaf] new session: {} (parent: {}, path: {} entries)", new_id, src_sid, path.len());
    Some(new_id)
}

/// 找 session 文件的绝对路径
fn find_session_file(sid: &str) -> Option<String> {
    let index = ion::session_index::SessionIndex::load();
    let meta = index.get(sid)?;
    let cwd = meta.project.as_deref()?;
    ion::session_jsonl::session_path(cwd).to_str().map(|s| s.to_string())
}

async fn cmd_session(action: SessionAction) {
    match action {
        SessionAction::Tree { session } => {
            // Resolve session id (prefix match)
            let sid = resolve_session_id_simple(&session);
            let entries = load_session_entries(&sid);
            match entries {
                None => {
                    eprintln!("❌ session '{}' not found or empty", sid);
                }
                Some(ents) => {
                    print_session_tree(&ents, &sid);
                }
            }
        }
        SessionAction::Branches { session } => {
            let sid = resolve_session_id_simple(&session);
            let entries = load_session_entries(&sid);
            match entries {
                None => eprintln!("❌ session '{}' not found or empty", sid),
                Some(ents) => {
                    let branches = ion::session_tree::named_branches(&ents);
                    let current = ion::session_tree::resolve_current_leaf(&ents);
                    if branches.is_empty() {
                        println!("No named branches in session {}", sid);
                    } else {
                        println!("{:<25} {:<15} {}", "NAME", "TARGET", "CURRENT");
                        println!("{}", "-".repeat(50));
                        for (name, target) in &branches {
                            let is_current = current.as_deref() == Some(target.as_str());
                            println!("{:<25} {:<15} {}",
                                name, target,
                                if is_current { "*" } else { "" });
                        }
                    }
                }
            }
        }
    }
}

/// 简单解析 session id（支持前缀匹配）
fn resolve_session_id_simple(input: &str) -> String {
    if let Some((id, _)) = find_session_by_prefix(input) {
        return id;
    }
    input.to_string()
}

/// 加载 session 的所有 entries（裸 JSON）
fn load_session_entries(sid: &str) -> Option<Vec<serde_json::Value>> {
    let content = load_session_raw_content(sid)?;
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Ok(e) = serde_json::from_str::<serde_json::Value>(line) {
            entries.push(e);
        }
    }
    if entries.is_empty() { None } else { Some(entries) }
}

/// 读取 session 文件原始内容
fn load_session_raw_content(sid: &str) -> Option<String> {
    // 尝试直接路径
    if sid.contains('/') || sid.ends_with(".jsonl") {
        return std::fs::read_to_string(sid).ok();
    }
    // 通过 index 查 cwd，再算 session_path
    let index = ion::session_index::SessionIndex::load();
    let meta = index.get(sid)?;
    let cwd = meta.project.as_deref()?;
    let path = ion::session_jsonl::session_path(cwd);
    if path.exists() {
        return std::fs::read_to_string(&path).ok();
    }
    None
}

/// 打印 session 的消息树（ASCII）
fn print_session_tree(entries: &[serde_json::Value], sid: &str) {
    let tree = ion::session_tree::get_tree(entries);
    let current_leaf = ion::session_tree::resolve_current_leaf(entries);
    let cwd = entries.iter()
        .find(|e| e.get("type").and_then(|v| v.as_str()) == Some("session"))
        .and_then(|h| h.get("cwd").and_then(|v| v.as_str()))
        .unwrap_or("?");
    println!("Session: {}", sid);
    println!("cwd: {}", cwd);
    println!();
    if tree.is_empty() {
        println!("(no messages)");
        return;
    }
    for root in &tree {
        print_tree_node(root, "", true, &current_leaf);
    }
    // 命名分支
    let branches = ion::session_tree::named_branches(entries);
    if !branches.is_empty() {
        println!();
        println!("命名分支:");
        for (name, target) in &branches {
            let is_current = current_leaf.as_deref() == Some(target.as_str());
            println!("  {} → {} {}",
                name, target,
                if is_current { "[当前 leaf]" } else { "" });
        }
    }
}

fn print_tree_node(node: &ion::session_tree::TreeNode, prefix: &str, is_last: bool, current_leaf: &Option<String>) {
    let entry = &node.entry;
    let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    // 消息摘要
    let summary = if entry_type == "message" {
        let role = entry.get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("?");
        let text = entry.get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let text = if text.len() > 40 { &text[..40] } else { text };
        format!("[{}] \"{}\"", role, text)
    } else {
        format!("[{}]", entry_type)
    };
    let label = node.label.as_ref().map(|l| format!(" ← {}", l)).unwrap_or_default();
    let is_current = current_leaf.as_deref() == Some(id);
    let current_mark = if is_current { " ← [当前 leaf]" } else { "" };

    let connector = if is_last { "└─ " } else { "├─ " };
    println!("{}{}{} {}{}{}", prefix, connector, id, summary, label, current_mark);

    let child_prefix = if is_last {
        format!("{}   ", prefix)
    } else {
        format!("{}│  ", prefix)
    };
    let n = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        print_tree_node(child, &child_prefix, i == n - 1, current_leaf);
    }
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

/// Extension management: install / remove / list WASM extensions.
///
/// 扩展安装到全局目录 `~/.ion/agent/extensions/`，启动时自动发现。
/// 对齐 AGENTS.md「命令行可验证原则」：每个功能都能从 CLI 操作。
async fn cmd_extension(action: ExtensionAction) {
    let ext_dir = ion::paths::extensions_dir();

    match action {
        ExtensionAction::Install { path } => {
            let src = std::path::Path::new(&path);
            if !src.exists() {
                eprintln!("❌ file not found: {path}");
                std::process::exit(1);
            }
            // 只允许 .wasm 文件
            if src.extension().and_then(|e| e.to_str()) != Some("wasm") {
                eprintln!("❌ only .wasm files can be installed as extensions");
                std::process::exit(1);
            }
            let filename = src.file_name().unwrap_or_default();
            let dest = ext_dir.join(filename);
            // 确保目录存在
            if let Err(e) = std::fs::create_dir_all(&ext_dir) {
                eprintln!("❌ failed to create extensions dir: {e}");
                std::process::exit(1);
            }
            match std::fs::copy(src, &dest) {
                Ok(_) => {
                    let name = filename.to_string_lossy();
                    println!("✅ installed extension: {name}");
                    println!("   → {}", dest.display());
                    println!("   restart ion to load it (or use extension_reload RPC)");
                }
                Err(e) => {
                    eprintln!("❌ install failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        ExtensionAction::Remove { name } => {
            // name 可以带或不带 .wasm 后缀
            let filename = if name.ends_with(".wasm") {
                name.clone()
            } else {
                format!("{name}.wasm")
            };
            let target = ext_dir.join(&filename);
            if !target.exists() {
                eprintln!("❌ extension '{name}' not found in {}", ext_dir.display());
                std::process::exit(1);
            }
            match std::fs::remove_file(&target) {
                Ok(_) => {
                    println!("✅ removed extension: {filename}");
                    println!("   restart ion to unload it");
                }
                Err(e) => {
                    eprintln!("❌ remove failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        ExtensionAction::List => {
            if !ext_dir.exists() {
                println!("(no extensions installed — {} does not exist)", ext_dir.display());
                return;
            }
            let mut entries: Vec<String> = match std::fs::read_dir(&ext_dir) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok())
                    .filter(|e| {
                        e.file_type().map(|t| t.is_file()).unwrap_or(false)
                            && e.path().extension().and_then(|x| x.to_str()) == Some("wasm")
                    })
                    .map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        let size = e.metadata().map(|m| m.len()).unwrap_or(0);
                        format!("{name:<30} {:>8} bytes", size)
                    })
                    .collect(),
                Err(e) => {
                    eprintln!("❌ failed to read extensions dir: {e}");
                    std::process::exit(1);
                }
            };
            if entries.is_empty() {
                println!("(no .wasm extensions in {})", ext_dir.display());
                return;
            }
            entries.sort();
            println!("Installed extensions ({}):", ext_dir.display());
            for e in &entries {
                println!("  {e}");
            }
            println!();
            println!("Total: {} extension(s)", entries.len());
        }
    }
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

    // ── --export: 决定是 standalone 还是 export-after-run ──
    // - 有 prompt/agent 任务 → 跑完 agent 后再 export（带 tools 面板）
    // - 无 prompt（纯 --export）→ 直接 export 现有 session（无 tools，对齐 pi exportFromFile）
    let export_after_run: Option<String> = if let Some(ref export_path) = cli.export {
        // 检查后面有没有 prompt / agent 任务（cmd_run 路径）
        let has_run_intent = !eff.message.is_empty() || cli.host;
        if has_run_intent {
            Some(export_path.clone())
        } else {
            // Standalone export: no agent run, just dump existing session
            let session_id = match (&cli.session, cli.continue_session, &cli.resume) {
                (Some(sid), _, _) => sid.clone(),
                (_, _, Some(sid)) => sid.clone(),
                (_, true, _) => std::fs::read_to_string(ion::session_jsonl::last_session_path()).unwrap_or_default().trim().to_string(),
                _ => std::fs::read_to_string(ion::session_jsonl::last_session_path()).unwrap_or_default().trim().to_string(),
            };
            if session_id.is_empty() {
                eprintln!("No session to export. Run a prompt first, or use --session <id>.");
            } else {
                match ion::export::export_session_rich(&session_id, std::path::Path::new(export_path)) {
                    Ok(()) => println!("Exported to {export_path}"),
                    Err(e) => eprintln!("Export failed: {e}"),
                }
            }
            return;
        }
    } else {
        None
    };

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

        // ── Session Tree 操作：branch / checkout / rollback（在 agent.run 之前追加 leaf_pointer）──
        if !cli.no_session && (cli.branch.is_some() || cli.checkout.is_some() || cli.rollback.is_some()) {
            apply_session_tree_ops(&cli, &session_id);
        }

        // ── fork-from-leaf：从某 leaf 提取新 session ──
        if let Some(spec) = &cli.fork_from_leaf {
            if let Some(new_sid) = do_fork_from_leaf(spec) {
                // 用新 session 继续
                cmd_run(&eff, &eff.message, cli.no_tools, &new_sid, None, &cli.messages, export_after_run.as_deref()).await;
                return;
            } else {
                eprintln!("❌ --fork-from-leaf '{}' failed", spec);
                std::process::exit(1);
            }
        }

        cmd_run(&eff, &eff.message, cli.no_tools, &session_id, preloaded, &cli.messages, export_after_run.as_deref()).await;
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
        Some(Commands::Workflow { action }) => match action {
            WorkflowAction::Validate { path } => cmd_workflow_validate(path).await,
            WorkflowAction::Status { path } => cmd_workflow_status(path).await,
            WorkflowAction::Run { path, set } => cmd_workflow_run(path, &set).await,
        },
        Some(Commands::Dashboard) => {
            // Dashboard 用 Bun + OpenTUI 实现（dashboard/ 子目录）
            // 自动启动 Manager（如果没在跑），然后 fork bun 进程
            launch_dashboard().await;
        }
        Some(Commands::Rpc { session, method, params }) => {
            cmd_rpc(session.as_deref(), method, params).await;
        }
        Some(Commands::Sessions { json, all, limit }) => {
            cmd_sessions(*json, *all, *limit).await
        }
        Some(Commands::History { session, limit, view }) => {
            cmd_history(session, *limit, view).await
        }
        Some(Commands::Session { action }) => cmd_session(action.clone()).await,
        Some(Commands::Recordings) => cmd_recordings().await,
        Some(Commands::Subscribe { session, extension, ui, replay }) => cmd_subscribe(session.as_deref(), extension.as_deref(), *ui, *replay).await,
        Some(Commands::ListAgents) => cmd_list_agents().await,
        Some(Commands::ListModels { search }) => cmd_list_models(search).await,
        Some(Commands::Extension { action }) => cmd_extension(action.clone()).await,
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

/// 创建一个 session 的 helper（统一 4 处调用点：cmd_serve_start 默认 session、
/// create_session RPC handler、send_to_session fallback、proxy watchdog 重建）。
///
/// **调用前必须确保没有持有 registry 的 MutexGuard**（函数内部会重新 lock）。
///
/// `source` 是 RPC params（兼容嵌套/扁平格式），支持字段：
/// - `agent`（默认 "build"）
/// - `session_id`（不传则自动生成 `sess_<8-hex>`）
/// - `project_path` / `cwd`（三级 fallback：project_path > cwd > host cwd）
/// - `initial_prompt`（可选）
///
/// 成功返回 session_id。
async fn do_create_session(
    registry: &std::sync::Arc<tokio::sync::Mutex<ion::worker_registry::WorkerRegistry>>,
    source: &serde_json::Value,
) -> Result<String, String> {
    use ion::worker_registry::WorkerCreateConfig;
    let agent = source.get("agent").and_then(|v| v.as_str()).unwrap_or("build").to_string();
    let session_id = source.get("session_id").and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("sess_{}", &uuid::Uuid::new_v4().to_string()[..8]));
    let mut cfg = WorkerCreateConfig::default();
    cfg.session = Some(session_id.clone());
    cfg.agent = Some(agent);
    cfg.project_path = source.get("project_path").and_then(|v| v.as_str()).map(String::from)
        .or_else(|| source.get("cwd").and_then(|v| v.as_str()).map(String::from))
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string()));
    cfg.channels = Some(vec!["main".to_string()]);
    cfg.initial_prompt = source.get("initial_prompt").and_then(|v| v.as_str()).map(String::from);
    registry.lock().await.create_worker(cfg, registry).await?;
    Ok(session_id)
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

    // ── 注册单例扩展（host 级，只在 serve 模式）──
    {
        let mut reg = registry.lock().await;
        reg.register_singleton(Box::new(ion::global_memory_ext::GlobalMemoryExtension::new()));
        reg.init_singletons().await;
    }
    // post_init（释放 lock 后调，让单例能 create_worker spawn 系统级 agent）
    ion::worker_registry::WorkerRegistry::post_init_singletons(&registry).await;

    // ── Host 级 MCP 管理器（方案 C：host 持有连接，所有 Worker 代理调用）──
    {
        let ion_cfg = ion::config::IonConfig::load();
        let mcp_config = ion_cfg.mcp_servers.clone();
        if !mcp_config.is_empty() {
            let mcp_manager = std::sync::Arc::new(ion::mcp::McpManager::new(mcp_config));
            eprintln!("[mcp] host connecting {} server(s)...", mcp_manager.server_count());
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                mcp_manager.connect_all(),
            ).await;
            eprintln!("[mcp] {} server(s) connected", mcp_manager.connected_count().await);
            mcp_manager.spawn_reconnect_monitor();
            registry.lock().await.set_mcp_manager(mcp_manager);
        }
    }

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

    // 自动创建一个默认 build session，让首次 RPC 不用先 create_session（修复 #1）
    // 对齐 pi：pi 启动后默认有一个 SessionManager.create 出的 session
    match do_create_session(&registry, &serde_json::json!({"agent": "build"})).await {
        Ok(sid) => eprintln!("🌱 Default session ready: {sid}"),
        Err(e) => eprintln!("⚠️  Default session 创建失败（后续 RPC 会按需创建）: {e}"),
    }

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
                                            // 支持 replay 参数(刷新时恢复之前的事件)
                                            let replay = cmd.get("replay")
                                                .and_then(|v| v.as_u64())
                                                .unwrap_or(0) as usize;
                                            let (mut rx, replay_events) = match inner_reg.subscribe_with_replay(&wid, replay) {
                                                Ok(r) => r,
                                                Err(e) => {
                                                    let resp = serde_json::json!({"type":"error","error":e});
                                                    let _ = write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                                    return;
                                                }
                                            };
                                            drop(inner_reg);
                                            let ack = serde_json::json!({"type":"subscribed","session":sid,"stream":"instance","replayed":replay_events.len()});
                                            let _ = write_half.write_all(format!("{ack}\n").as_bytes()).await;
                                            // 先发送回放的历史事件
                                            for evt in &replay_events {
                                                let out = serde_json::json!({
                                                    "type": "instance_event",
                                                    "session": sid,
                                                    "event": evt.get("event").cloned().unwrap_or(evt.clone()),
                                                    "replayed": true,
                                                });
                                                if write_half.write_all(format!("{out}\n").as_bytes()).await.is_err() { return; }
                                            }
                                            let _ = write_half.flush().await;
                                            // 然后转发实时事件
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
                            // 审批类事件路由到 ui（让 subscribe --ui 也能收到）
                            let ui_custom_types = [
                                "ApprovalRequest", "ApprovalResolved", "ApprovalReset",
                                "Ask", "AskResolved", "AskTimedOut",
                                "Confirm", "Prompt", "Alert", "Notif",
                            ];
                            if ui_custom_types.contains(&ct) {
                                event = event.with_route("ui");
                            }
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
        // 对外 API：列出所有磁盘 session（带血缘字段，从 index 读）
        "list_all_sessions" => {
            let index = ion::session_index::SessionIndex::load();
            let sessions: Vec<_> = index.sessions.iter().map(|(id, m)| {
                serde_json::json!({
                    "id": id,
                    "name": m.name,
                    "firstMessage": m.first_name,
                    "model": m.model,
                    "messageCount": m.message_count,
                    "turnCount": m.turn_count,
                    "updatedAt": m.updated_at,
                    "project": m.project,
                    "lastEntryId": m.last_entry_id,
                    "parentSession": m.parent_session,
                    "parentType": m.parent_type,
                    "hasChildren": index.has_children(id),
                    "childCount": index.child_count(id),
                })
            }).collect();
            Ok(serde_json::json!({"sessions": sessions, "totalCount": sessions.len()}))
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
            drop(reg); // 必须先放锁，do_create_session 内部会重新 lock
            match do_create_session(&registry, &source).await {
                Ok(session_id) => Ok(serde_json::json!({
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
            // 检查 session 对应的 worker 是否存在，不存在则自动创建（修复 #2）
            let exists = reg.workers.values().any(|w| w.session_id == session);
            if exists {
                reg.send_to_session(session, rpc_method, params).await
            } else {
                drop(reg); // 放锁，让 do_create_session 能重新 lock
                tracing::info!("[send_to_session] session {session} not found, auto-creating");
                match do_create_session(&registry, &serde_json::json!({
                    "session_id": session,
                    "agent": "build",
                })).await {
                    Ok(_) => {
                        // 创建后立即转发原请求
                        registry.lock().await.send_to_session(session, rpc_method, params).await
                    }
                    Err(e) => Err(e),
                }
            }
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
        "extension_rpc" => {
            // 单例扩展的 extension_rpc：直接从 SingletonRegistry 调
            let params = cmd.get("params").cloned().unwrap_or_default();
            let extension = params.get("extension").and_then(|v| v.as_str()).unwrap_or("");
            let method = params.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or_default();
            drop(reg); // 释放锁，让扩展能工作
            let mut reg2 = registry.lock().await;
            if let Some(entry) = reg2.singletons.get_mut(extension) {
                match entry.instance.on_extension_rpc(method, args).await {
                    Ok(val) => Ok(val),
                    Err(e) => Err(format!("{:?}", e)),
                }
            } else {
                Err(format!("singleton extension '{}' not found", extension))
            }
        }
        _ => {
            // 默认分支：如果 cmd 里有 session 字段，转发到对应 worker
            let session_id = cmd.get("session").and_then(|v| v.as_str());
            if let Some(sid) = session_id {
                let params = cmd.get("params").cloned().unwrap_or_default();

                // 检查 session 是否存在，不存在则自动创建（修复 #2 的另一条路径）
                // 对齐 pi：pi 用 SessionManager 隐式管理，永远有 session
                let exists = reg.workers.values().any(|w| w.session_id == sid);
                if !exists {
                    tracing::info!("[forward] session {sid} not found, auto-creating");
                    drop(reg); // 放锁，让 do_create_session 能重新 lock
                    if let Err(e) = do_create_session(&registry, &serde_json::json!({
                        "session_id": sid,
                        "agent": "build",
                    })).await {
                        return serde_json::json!({"type":"response","id":id,"success":false,"error":format!("auto-create session failed: {e}")});
                    }
                    reg = registry.lock().await;
                }

                // prompt 用 fire-and-forget(不等 oneshot)——
                // agent.run 会阻塞 worker 主循环很久,如果等 oneshot,
                // Manager 锁不释放,后续命令(如 abort)进不来。
                // prompt 的 worker handler 会在 agent.run 前立刻 output_response(null)
                if method == "prompt" {
                    // 找 worker_id,用 send_command(fire-and-forget)
                    let wid = reg.workers.iter()
                        .find(|(_, w)| w.session_id == sid)
                        .map(|(id, _)| id.clone());
                    match wid {
                        Some(wid) => {
                            reg.send_command(&wid, &method, params).await
                                .map(|_| serde_json::json!({"status": "forwarded", "session": sid}))
                        }
                        None => Err(format!("worker not found for session: {sid} (auto-create should have made it)")),
                    }
                } else {
                    // 其他命令等响应(list_turns/get_messages/abort 等)
                    match reg.send_to_session(sid, method, params).await {
                        Ok(_) => Ok(serde_json::json!({"status": "forwarded", "session": sid})),
                        Err(e) => Err(e),
                    }
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
                            eprintln!("[pump] 订阅新 worker: {}", &wid[..12.min(wid.len())]);
                            subs.insert(wid.clone(), rx);
                            line_bufs.insert(wid.clone(), String::new());
                        }
                    }
                }
            }
            for (wid, rx) in subs.iter_mut() {
                while let Ok(msg) = rx.try_recv() {
                    if msg.get("type").and_then(|v| v.as_str()) != Some("event") {
                        continue;
                    }
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

    // 如果设了 ION_FORCE_SESSION_ID，强制用这个 session_id（不复用 cwd-hash 旧 session）
    // workflow run 用它确保 wf agent 每次跑都是干净 session，不会"记得上次跑过"
    if let Ok(forced_sid) = std::env::var("ION_FORCE_SESSION_ID") {
        if !forced_sid.is_empty() {
            cfg.session = Some(forced_sid.clone());
            eprintln!("[host] 强制使用 session_id: {}", forced_sid);
        }
    }

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

    // idle 宽限期：worker 刚 Idle 不能立刻算"完成"。
    // workflow 场景下 wf agent 每个 stage 是一个 turn，turn 之间会短暂 Idle（等下一轮 LLM 调用），
    // 如果立刻判定完成会提前清理。给 8 秒宽限，让 wf 有时间启动下一个 turn。
    let idle_grace_secs = std::env::var("ION_HOST_IDLE_GRACE")
        .ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(1800);
    let mut first_idle_at: Option<std::time::Instant> = None;

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
            // 首次进入 idle 状态，记录时间
            if first_idle_at.is_none() {
                first_idle_at = Some(std::time::Instant::now());
                eprintln!("[host] workers idle, waiting {idle_grace_secs}s grace period before cleanup...");
            }
            // 持续 idle 超过宽限期才真的清理
            if let Some(t0) = first_idle_at {
                if t0.elapsed() >= std::time::Duration::from_secs(idle_grace_secs) {
                    eprintln!("[host] idle for {}s, cleaning up", t0.elapsed().as_secs());
                    break;
                }
            }
        } else {
            // 不是全部 idle（有 worker 在干活），重置宽限期计时器
            first_idle_at = None;
        }

        if std::time::Instant::now() > deadline {
            eprintln!("[host] timeout reached, forcing exit");
            break;
        }
    }

    // 5. Cleanup — 通知所有 Worker shutdown（让它们执行退出前 save_worker_session）
    eprintln!("[host] cleaning up, notifying workers to save & exit");
    {
        let mut reg = registry.lock().await;
        let wids: Vec<String> = reg.workers.keys().cloned().collect();
        for wid in &wids {
            // 发 shutdown 命令（ion_worker 收到后 break 主循环 → 执行退出前 save）
            let _ = reg.send_command(wid, "shutdown", serde_json::json!({})).await;
        }
    }
    // 给 Worker 时间执行退出前 save_worker_session
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    eprintln!("[host] cleanup complete");
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
        // 确保父目录存在
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let header = ion::session_jsonl::SessionHeader {
            entry_type: "session".into(), version: 3, id: id.to_string(),
            timestamp: ion::session_jsonl::timestamp_iso(),
            cwd: cwd.clone(),
            parent_session: None,
            agent: std::env::var("ION_SESSION_AGENT").ok(),
            model: std::env::var("ION_SESSION_MODEL").ok(),
            provider: std::env::var("ION_SESSION_PROVIDER").ok(),
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
