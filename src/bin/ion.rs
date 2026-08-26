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
use ion::agent::tool::{
    BashTool, CalculatorTool, EchoTool, EditTool, FindTool, GrepTool, LsTool, ReadTool,
    ToolRegistry, WriteTool,
};
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

    /// Security profile: permissive | readonly | standard | strict | autopilot
    /// Controls permission engine rules + command guard.
    /// - permissive: no restrictions (yolo mode)
    /// - readonly: deny all writes/edits, allow reads + safe commands
    /// - standard: protect sensitive files (.env/.ssh/.aws/.ion), allow workspace writes
    /// - strict: deny all writes by default, require explicit allow rules
    /// - autopilot: auto-approve low-risk workspace writes (for self-healing/unattended runs)
    #[arg(long, global = true)]
    profile: Option<String>,

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

    /// Load a WASM Extension file (can be used multiple times)
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
    #[arg(
        long,
        global = true,
        requires = "restore_code",
        value_name = "delta|full",
        default_value = "delta"
    )]
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
    #[arg(
        long = "continue",
        short = 'c',
        global = true,
        default_value_t = false,
        alias = "continue-session"
    )]
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

    /// FauxProvider script file (JSONL) — for testing without real LLM
    #[arg(long, global = true)]
    faux_script: Option<String>,

    /// FauxProvider static reply text — for testing without real LLM
    #[arg(long, global = true)]
    faux_reply: Option<String>,

    /// FauxProvider repeat count (0 = no repeat, 1 = repeat each response once)
    #[arg(long, global = true)]
    faux_repeat: Option<u64>,

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
    /// One-shot LLM call — quick-test the internal query_tier() helper.
    /// Reads tier_models[tier] from config.json (falls back to default_model)
    /// → resolves Model + api_key → calls LLM → prints response.
    ///
    /// Examples:
    ///   ion llm --tier fast "hello"
    ///   ion llm --tier pro --json --system "Reply JSON {greeting}" "user X"
    ///   ion llm --tier max --system "Summarize" "long text..."
    ///   ion llm "hi"                    (default tier=fast)
    Query {
        /// Tier name: fast / pro / max
        #[arg(long, default_value = "fast")]
        tier: String,
        /// System prompt (optional)
        #[arg(long)]
        system: Option<String>,
        /// Force JSON response format
        #[arg(long)]
        json: bool,
        /// User message (positional)
        message: String,
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
        /// Extension ID to filter (omit for all events)
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
    Set {
        key: String,
        value: String,
    },
    Get {
        key: String,
    },
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
    /// Create a new WASM extension scaffold
    Create {
        /// Extension name in lower kebab-case (used as directory and crate name)
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
        let ext = match std::path::Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
        {
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
            cli.models
                .as_ref()
                .and_then(|m| m.split(',').next().map(|s| s.trim().to_string()))
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
    let all_models: Vec<String> = cli
        .models
        .as_ref()
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
            agent.apply(
                &mut eff.model,
                &mut eff.thinking,
                &mut eff.max_turns,
                &mut eff.prompt,
            );
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

    // Model resolution priority (highest first):
    //   1. User explicitly specified --provider AND that provider is defined in
    //      config.json with the requested model → use config.json's definition
    //      (this lets users override built-in models with their own base_url /
    //      api_key / proxy settings).
    //   2. Built-in registry (find_model — searches across all built-in providers).
    //   3. Fallback construction.
    //
    // Rationale: built-ins use official endpoints (e.g. open.bigmodel.cn for GLM).
    // Users who configure a custom provider in config.json with the same model id
    // (e.g. their own proxy) expect their config to win when they pass --provider.
    // Previously find_model ran first and always picked the built-in, ignoring
    // the user's --provider + config.json combination.
    let mut model = if let Some(cp) = cfg.providers.get(&eff.provider) {
        if let Some(cm) = cp.models.iter().find(|m| m.id == eff.model) {
            Some(Model {
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
            })
        } else {
            None
        }
    } else {
        None
    }
    .or_else(|| model_registry.find_model(&eff.model).cloned())
    .unwrap_or_else(|| {
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
        eprintln!(
            "[replay] model.api forced to 'replay' (model_id={})",
            eff.model
        );
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
                        let inner: Option<Box<dyn ion_provider::registry::ApiProvider>> =
                            if using_faux {
                                // 重新注册一个共享同一份队列的 faux（队列已在上面填充）
                                // 这里直接拿一个新的 FauxProvider，复用相同的 responses。
                                let new_faux =
                                    std::sync::Arc::new(ion_provider::faux::FauxProvider::new());
                                // 复用之前已注册的 faux 队列：从 env 重新构造一份
                                let responses = if let Some(path) = &faux_script {
                                    ion_provider::faux::load_script(std::path::Path::new(path)).ok()
                                } else {
                                    Some(vec![ion_provider::faux::FauxResponseStep::Static(
                                        ion_provider::faux::faux_assistant_message(
                                            ion_provider::faux::FauxContent::Text(
                                                faux_reply
                                                    .as_deref()
                                                    .unwrap_or_default()
                                                    .to_string(),
                                            ),
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
                                let meta_path =
                                    ion_provider::replay::recording_meta_path(&rec_id).unwrap();
                                let recording = ion_provider::record::RecordingProvider::new(
                                    real, trace_path, meta_path,
                                );
                                registry.register(&model.api, Box::new(recording));
                                eprintln!(
                                    "[record] recording to {} (model: {})",
                                    rec_dir.display(),
                                    model.id
                                );
                                // 持有锁到进程退出（故意泄漏，保持文件锁）
                                if let Some(l) = lock_opt {
                                    std::mem::forget(l);
                                }
                            }
                            None => {
                                eprintln!(
                                    "[record] ⚠️  no builtin provider for api '{}', recording disabled",
                                    model.api
                                );
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
        response_format: if eff.json {
            Some("json_object".into())
        } else {
            None
        },
        thinking: eff.thinking.clone(),
        compact_model_id: eff.compact_model.clone(),
        retry_on_no_tool_use: 0,
        retry_config: None,
    }
}

fn build_tools(eff: &EffectiveConfig) -> (ToolRegistry, Option<Vec<std::path::PathBuf>>) {
    let mut skill_dirs_for_prompt: Option<Vec<std::path::PathBuf>> = None;
    let mut tools = ToolRegistry::new();
    if eff.no_tools {
        return (tools, skill_dirs_for_prompt);
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
        // ── 内置 plan 工具（plan_enter/exit/add/list/done）──
        // 不依赖 WASM plan-extension（已删除，跟内置 PlanExtension 工具名冲突）。
        // 这 5 个工具共享一个 PlanExtension 实例。PlanExtension 的 mode 切换钩子
        // 通过下方的 has_plan_tools 始终为 true 来保证注册。
        for t in ion::agent::plan_tool::plan_tools() {
            tools.register(t);
        }

        // Skill tool — lets the LLM autonomously load skills by name.
        // Scans global (~/.ion/agent/skills/) and project (<cwd>/.ion/skills/) dirs.
        // Without this registration the LLM cannot invoke skills on its own;
        // only the --skill <path> CLI flag works (which injects into system_prompt).
        if !eff.no_skills {
            let cwd_str = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let home = std::env::var("HOME").unwrap_or_default();
            // 扫描位置：
            // 1. ~/.ion/agent/skills/（ION 全局 skill）
            // 2. <project>/.ion/skills/（项目级 skill）
            // 3. ~/.agents/skills/（全局 skill 库，跟 ZCode 共享，102 个）
            // 4. ~/.zcode/cli/plugins/cache/<marketplace>/<plugin>/<version>/skills/
            //    （ZCode plugin skill，68 个，如 document-skills:pdf / cloudflare:wrangler）
            let agents_skills = std::path::PathBuf::from(&home)
                .join(".agents")
                .join("skills");
            let mut skill_dirs: Vec<std::path::PathBuf> = vec![
                ion::paths::skills_dir(),
                ion::paths::project_skills_dir(&cwd_str),
                agents_skills,
            ];
            // 递归找 ZCode plugin 的 skills 目录（cache/<mp>/<plugin>/<ver>/skills/）
            let plugins_cache = std::path::PathBuf::from(&home).join(".zcode/cli/plugins/cache");
            if plugins_cache.exists() {
                if let Ok(mp_iter) = std::fs::read_dir(&plugins_cache) {
                    for mp_entry in mp_iter.flatten() {
                        // mp_entry = marketplace 目录（zcode-plugins-official / claude-plugins-official）
                        if let Ok(plugin_iter) = std::fs::read_dir(mp_entry.path()) {
                            for plugin_entry in plugin_iter.flatten() {
                                // plugin_entry = plugin 目录（cloudflare / android-emulator）
                                if let Ok(ver_iter) = std::fs::read_dir(plugin_entry.path()) {
                                    for ver_entry in ver_iter.flatten() {
                                        // ver_entry = version 目录（1.0.0）
                                        let skills_dir = ver_entry.path().join("skills");
                                        if skills_dir.is_dir() {
                                            skill_dirs.push(skills_dir);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            // 保存 skill_dirs 到外层，供后面 system prompt 注入大纲用
            skill_dirs_for_prompt = Some(skill_dirs.clone());
            // ★ 不注册 SkillTool 给 LLM（用户：'禁止提供 skill list 的能力给到 LLM，
            // 因为默认都注入到系统提示词'）。Skill 大纲已在 system prompt 里展示，
            // LLM 不需要主动调用 skill 工具来发现/加载 skill。
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
    (tools, skill_dirs_for_prompt)
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
/// Returns None if stdin is a TTY (interactive terminal) or if no data
/// arrives within 500ms (background/parallel mode where stdin might be
/// inherited from parent's TTY but not actually piped).
///
/// ★ 并发架构修复（用户：'10 个并发的场景，底层架构一定要兼容'）：
/// 之前 read_to_string 在后台/并行模式下永远阻塞（stdin 不是 TTY
/// 但也没有 EOF），导致所有后台 ion 进程卡住。
/// 修复：用 channel + recv_timeout(500ms)，超时返回 None，
/// 读线程被 abandon（进程退出时自动清理）。
fn read_piped_stdin() -> Option<String> {
    use std::io::Read;
    // Check if stdin is a TTY (interactive)
    if std::io::stdin().is_terminal() {
        return None;
    }
    // Spawn reader thread + timeout
    let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let mut buf = String::new();
        match std::io::stdin().lock().read_to_string(&mut buf) {
            Ok(0) | Err(_) => {
                let _ = tx.send(None);
            }
            Ok(_) => {
                let trimmed = buf.trim().to_string();
                let _ = tx.send(if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                });
            }
        }
    });
    rx.recv_timeout(std::time::Duration::from_millis(500))
        .unwrap_or(None)
}

// ---------------------------------------------------------------------------
// Config commands
// ---------------------------------------------------------------------------

async fn cmd_config_show() {
    let cfg = IonConfig::load();
    println!("Config file: {}", IonConfig::path().display());
    println!("{}", serde_json::to_string_pretty(&cfg).unwrap());

    // 运行时默认值（不在 config.json 里，硬编码在代码中）
    let retry = ion::retry::RetryConfig::default();
    println!("\n# ── Runtime defaults (not in config.json, hardcoded) ──");
    println!("{{");
    println!("  \"retry\": {{");
    println!("    \"max_retries\": {},", retry.max_retries);
    println!(
        "    \"initial_delay_secs\": {},",
        retry.initial_delay.as_secs()
    );
    println!("    \"max_delay_secs\": {},", retry.max_delay.as_secs());
    println!("    \"fixed_delay_secs\": {},", retry.fixed_delay.as_secs());
    println!("    \"multiplier\": {}", retry.multiplier);
    println!("  }}");
    println!("}}");
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
            println!(
                "   {} stages, {} gates, {} loop_backs",
                wf.stages.len(),
                gate_count,
                loop_count
            );
            for stage in &wf.stages {
                let gate_str = if stage.gate.is_some() {
                    " 🔒gate"
                } else {
                    ""
                };
                let wt_str = if stage.worktree { " 🌳worktree" } else { "" };
                let lb_str = stage
                    .on_fail
                    .as_ref()
                    .map(|f| format!(" ↩︎loop_back→{}", f.loop_back))
                    .unwrap_or_default();
                println!(
                    "   • {} [{}]{}{}{}",
                    stage.id, stage.status, gate_str, wt_str, lb_str
                );
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
                if in_context
                    && !line.is_empty()
                    && !line.starts_with(' ')
                    && !line.starts_with('#')
                {
                    in_context = false;
                }
                // 在 context 段里找 key:
                if in_context && line.trim_start().starts_with(&prefix) {
                    let indent = line.len() - line.trim_start().len();
                    out_lines.push(format!(
                        "{}{}: {}",
                        " ".repeat(indent),
                        key,
                        new_value_quoted
                    ));
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
    let wf_session_id = format!(
        "sess_wf_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
    );
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
        std::env::set_var("ION_MAX_TURNS", "15");
        // auto-continue 的 gate：当 workflow yaml 所有 10 stage 都有 status 时停止注入 follow-up
        std::env::set_var(
            "ION_AUTO_CONTINUE_GATE",
            "test -f .ion/workflow.yaml && [ $(grep -c 'status:' .ion/workflow.yaml) -ge 10 ] && echo ALL_DONE || echo NOT_DONE",
        );
        std::env::set_var("ION_AUTO_CONTINUE_EXPECTED", "ALL_DONE");
    }

    // 同步更新 last_session，让 export_report stage 的 ion --export 能找到 wf 的 session
    // （ION_FORK_CHILD=1 让 wf 用 <sid>.jsonl 独立文件，但 last_session 不自动更新）
    let _ = std::fs::write(ion::session_jsonl::last_session_path(), &wf_session_id);

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

    // 复用 cmd_host 的逻辑（workflow run 不导出 HTML，传 None）
    cmd_host(&message, Some("wf"), None).await;
}

// ---------------------------------------------------------------------------
// Command implementations
// ---------------------------------------------------------------------------

// --mode rpc 入口已迁移到 src/worker_rpc.rs（合并自原 ion-worker 二进制）。
// 单二进制方案：ion --mode rpc 由 lib 内的 ion::worker_rpc::run_worker_rpc 处理。

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
    //
    // Session isolation: previously the default branch read the shared
    // `session.jsonl` header and reused its id, so every run in the same cwd
    // appended to one ever-growing file (the 93MB incident). Now each run
    // gets a fresh id and writes its own `<sid>.jsonl`. `--continue`/`--resume`
    // discover prior sessions by scanning all `*.jsonl` (see find_most_recent_session).
    let owned_sid = if !session_id_in.is_empty() {
        session_id_in.to_string()
    } else {
        format!("sess_{}", &uuid::Uuid::new_v4().to_string()[..8])
    };
    let session_id: &str = &owned_sid;
    // Persist to last_session so --continue / --export can find it later.
    let _ = std::fs::write(ion::session_jsonl::last_session_path(), session_id);

    // Route this run's session storage to a per-run `<sid>.jsonl` file instead
    // of the shared `session.jsonl`. This mirrors what fork/spawn child workers
    // already do (ION_FORK_CHILD + set_session_file_override), so the main
    // session is now physically isolated from other runs in the same cwd.
    {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let session_file = ion::paths::session_jsonl_path_by_id(&cwd, session_id);
        ion::session_jsonl::set_session_file_override(Some(session_file));
        // ensure_session_header / resolve_session_file honor the override, but
        // a few code paths key off ION_FORK_CHILD; set it so they all agree.
        unsafe {
            std::env::set_var("ION_FORK_CHILD", "1");
        }
    }

    // Set session header env vars (for save_session to include agent/model in header)
    unsafe {
        if let Some(ref a) = eff.agent {
            std::env::set_var("ION_SESSION_AGENT", a);
        }
        std::env::set_var("ION_SESSION_MODEL", &eff.model);
        std::env::set_var("ION_SESSION_PROVIDER", &eff.provider);
    }

    let (registry, model) = build_registry_and_model(eff);
    // Keep clones for worker-level extensions (LearningExtension needs them
    // to call LLM in on_session_shutdown). Agent::new takes ownership below.
    let registry_for_ext = std::sync::Arc::clone(&registry);
    let model_for_ext = model.clone();

    let config = build_agent_config(eff);

    // Session GC: asynchronously clean old session files (non-blocking).
    // Mirrors file_snapshot GC — runs once at startup in a background thread,
    // protects the active cwd, never panics.
    {
        let ion_cfg = ion::config::IonConfig::load();
        let session_cfg = &ion_cfg.session;
        if session_cfg.gc_on_start {
            let gc = ion::session_gc::SessionGcConfig {
                max_age_days: session_cfg.max_age_days,
                max_sessions_per_cwd: session_cfg.max_sessions_per_cwd,
                gc_on_start: true,
            };
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            std::thread::spawn(move || {
                ion::session_gc::run_gc(&gc, &cwd);
            });
        }
    }

    let (mut tools, skill_dirs_for_prompt) = build_tools(eff);

    // ── Goal Supervisor：goal_set tool + shared state for extension ──
    // Tool is always registered; the Extension (registered below) shares this state.
    let shared_goal_state: ion::goal_supervisor_extension::SharedGoalState = std::sync::Arc::new(
        std::sync::Mutex::new(None::<ion::goal_supervisor_extension::GoalState>),
    );
    // Resolve fast-tier model for goal plan generation (avoids reasoning token waste).
    let fast_model: Option<ion_provider::types::Model> =
        { ion::config::IonConfig::load().resolve_tier_model("fast") };

    tools.register(Box::new(
        ion::goal_supervisor_extension::GoalSetTool::with_llm(
            shared_goal_state.clone(),
            registry_for_ext.clone(),
            model_for_ext.clone(),
            fast_model.clone(),
        ),
    ));
    tools.register(Box::new(ion::goal_supervisor_extension::GoalRefineTool(
        shared_goal_state.clone(),
    )));
    tools.register(Box::new(ion::goal_supervisor_extension::GoalDiagnoseTool(
        shared_goal_state.clone(),
    )));

    // Runtime-loadable WASM Extension registry (also used by worker RPC).
    let wasm_ext_registry =
        std::sync::Arc::new(ion::wasm_extension::WasmExtensionRegistry::new());
    let mut loaded_wasm_paths: Vec<String> = Vec::new();

    // Auto-discover WASM Extensions before processing explicit --extension paths.
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
            if !dir.exists() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "wasm").unwrap_or(false) {
                        let canonical =
                            std::fs::canonicalize(&path).unwrap_or_else(|_| path.to_path_buf());
                        let canonical_str = canonical.to_string_lossy().to_string();
                        let extension_id = ion::wasm_extension::extension_id_from_path(&canonical_str);
                        match wasm_ext_registry.add(&canonical_str) {
                            Ok(tool_defs) => {
                                loaded_wasm_paths.push(canonical_str.clone());
                                for td in &tool_defs {
                                    tools.register(Box::new(
                                        ion::wasm_extension::WasmToolAdapter {
                                        name: td.name.clone(),
                                        description: td.description.clone(),
                                        parameters: td.parameters.clone(),
                                        extension_path: canonical_str.clone(),
                                        extension_id: extension_id.clone(),
                                        registry: wasm_ext_registry.clone(),
                                        },
                                    ));
                                    tracing::info!(
                                        "[wasm] auto-discovered {extension_id}: {}",
                                        td.name
                                    );
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

    // Load WASM Extensions passed through --extension.
    for ext_path in &eff.extension {
        if !ext_path.ends_with(".wasm") {
            eprintln!("❌ --extension only accepts .wasm files: {ext_path}");
            std::process::exit(2);
        }

        let abs = std::path::Path::new(ext_path);
        // Determine canonical path before calling wasm_ext_registry.add(),
        // so WasmToolAdapter holds the canonicalized path.
        let canonical = std::fs::canonicalize(abs).unwrap_or_else(|_| abs.to_path_buf());
        let canonical_str = canonical.to_string_lossy().to_string();

        match wasm_ext_registry.add(&canonical_str) {
            Ok(tool_defs) => {
                let extension_id = ion::wasm_extension::extension_id_from_path(&canonical_str);
                loaded_wasm_paths.push(canonical_str.clone());
                for td in &tool_defs {
                    tools.register(Box::new(ion::wasm_extension::WasmToolAdapter {
                        name: td.name.clone(),
                        description: td.description.clone(),
                        parameters: td.parameters.clone(),
                        extension_path: canonical_str.clone(),
                        extension_id: extension_id.clone(),
                        registry: wasm_ext_registry.clone(),
                    }));
                    tracing::info!("[wasm] registered tool: {} (WASM-backed)", td.name);
                }
            }
            Err(e) => {
                eprintln!("❌ failed to load WASM Extension '{ext_path}': {e}");
                std::process::exit(2);
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
        sys_prompt.push_str("\n\n--- append-system-prompt ---\n");
        sys_prompt.push_str(append);
    }

    // Inject environment info (time, cwd, project root, git info)
    let env_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    sys_prompt.push_str(&build_env_info(&env_cwd));
    // bash tool guide 由 BashExtension.on_system_prompt 自动注入（见 ext_reg.register）

    // Inject skill outline（扫描 ~/.agents/skills + ~/.ion/skills + ./.ion/skills，
    // 把所有 skill 的 name + description 注入 system prompt，让 LLM 启动就知道有哪些
    // skill 可用，而不需要先调 skill(skill_name='list')）。
    if let Some(ref dirs) = skill_dirs_for_prompt {
        let skill_tool = ion::agent::tool::SkillTool {
            skill_dirs: dirs.clone(),
            disabled: ion::config::IonConfig::load().skills.disabled,
        };
        let outline = skill_tool.list_skills();
        if !outline.contains("No skills available") {
            sys_prompt.push_str("\n\n--- available-skills ---\n");
            sys_prompt.push_str(&outline);
        }
    }

    // Inject available-agents outline（builtin + ~/.ion/agents + <project>/.ion/agents）。
    // 让 LLM 启动就知道有哪些 agent 可用、各自擅长什么，需要时可 spawn_worker 对应 agent。
    {
        let agents_outline = ion::agent_config::agents_outline();
        if !agents_outline.is_empty() {
            sys_prompt.push_str("\n\n--- available-agents ---\n");
            sys_prompt.push_str(&agents_outline);
        }
    }
    // Apply skill prompts (--skill 指定的完整 skill 正文)
    for skill_path in &eff.skill {
        if let Ok(content) = std::fs::read_to_string(skill_path) {
            // Parse frontmatter (--- yaml ---) and body
            let body = if content.starts_with("---") {
                if let Some(end) = content[3..].find("---") {
                    content[3 + end + 3..].trim()
                } else {
                    content.trim()
                }
            } else {
                content.trim()
            };
            if !body.is_empty() {
                sys_prompt.push_str("\n\n--- skill-file: ");
                sys_prompt.push_str(skill_path);
                sys_prompt.push_str(" ---\n");
                sys_prompt.push_str(body);
            }
        }
    }
    // Apply extension system prompts
    for ext_path in &eff.extension {
        if let Ok(content) = std::fs::read_to_string(ext_path) {
            if let Ok(def) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(sp) = def.get("system_prompt").and_then(|v| v.as_str()) {
                    sys_prompt.push_str("\n\n--- extension: ");
                    sys_prompt.push_str(ext_path);
                    sys_prompt.push_str(" ---\n");
                    sys_prompt.push_str(sp);
                }
            }
        }
    }

    // ── MCP（场景 1：LAZY 延迟连接 — 不在启动时连 MCP server）──
    // ★ 架构修复（用户：'10 个并发的场景，底层架构一定要兼容'）：
    // 之前每次 ion 启动都立刻 spawn npx MCP 子进程 → 多进程并行时
    // npm cache lock 竞争 → 全部卡住。
    // 修复：MCP 改成延迟连接 — 启动时不连，只在 LLM 调 MCP 工具时才连。
    // 这样 10+ 个 ion 进程并行启动不会互相阻塞。
    //
    // McpManager 用 Arc 持有但不在启动时 connect_all()。
    // McpTool::execute 内部会检查连接状态，未连接时自动触发 connect。
    let mcp_config = ion::config::IonConfig::load().mcp_servers;
    let mcp_manager: Option<std::sync::Arc<ion::mcp::McpManager>> =
        if !mcp_config.is_empty() && !eff.no_extensions {
            let mgr = std::sync::Arc::new(ion::mcp::McpManager::new(mcp_config));
            tracing::info!(
                "[mcp] {} server(s) configured (LAZY — will connect on first tool use)",
                mgr.server_count()
            );
            // 注册 MCP 工具为「占位」—— McpTool::execute 内部负责 lazy connect
            // 先注册已知的工具名（从配置解析），实际连接在第一次调用时发生
            // 为了不改 McpTool 签名，我们在后台 spawn 一个非阻塞的 connect：
            // 用短超时（10s 而不是 30s），失败不阻塞主流程
            let mgr_clone = std::sync::Arc::clone(&mgr);
            tokio::spawn(async move {
                tracing::info!("[mcp] background connect starting (non-blocking)...");
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(10), // ★ 10s 而不是 30s
                    mgr_clone.connect_all(),
                )
                .await;
                tracing::info!(
                    "[mcp] background connect done: {} connected",
                    mgr_clone.connected_count().await
                );
            });
            // 不等 MCP 连接完成就继续 — agent 可以用内置工具先跑
            // MCP 工具会在后台连接完成后可用（或第一次调用时触发连接）
            // 给一点时间让后台连接（500ms — 足够本地 npx 启动，不够也不阻塞）
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                // 等 MCP 工具注册（非阻塞，超时就继续）
                let tools_list = mgr.all_discovered_tools().await;
                for tool in &tools_list {
                    // 无法注册到已 move 的 tools — 这里只是探测
                }
            })
            .await;
            // 直接尝试注册已发现的 MCP 工具（可能为空，如果 MCP 还没连上）
            let mcp_tools = mgr.all_discovered_tools().await;
            for tool in &mcp_tools {
                tools.register(Box::new(ion::mcp::tool::McpTool::new(
                    tool,
                    std::sync::Arc::clone(&mgr),
                )));
            }
            if !mcp_tools.is_empty() {
                tracing::info!("[mcp] {} tools registered", mcp_tools.len());
            } else {
                tracing::info!("[mcp] no tools yet (will connect in background)");
            }
            mgr.spawn_reconnect_monitor();
            Some(mgr)
        } else {
            None
        };

    // Check if plan tools are loaded (before tools is moved into Agent)
    let has_plan_tools = tools.get("plan_enter").is_some();

    // Build runtime from config (aligned with ion_worker.rs)
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let runtime_cfg = ion::config::IonConfig::load().runtime;
    let backend_registry = BackendRegistry::from_config(&runtime_cfg, &cwd);

    // Parse security profile from config.json (CLI --profile handled in main, passed via eff)
    let cfg = ion::config::IonConfig::load();
    let profile = cfg.security_mode.as_deref().unwrap_or("standard");
    let sec_profile = match profile {
        "permissive" | "yolo" => ion::kernel::SecurityProfile::Permissive,
        "readonly" => ion::kernel::SecurityProfile::ReadOnly,
        "strict" => ion::kernel::SecurityProfile::Strict,
        "autopilot" => ion::kernel::SecurityProfile::Autopilot,
        _ => ion::kernel::SecurityProfile::Standard,
    };
    tracing::info!("[security] profile: {profile}");
    let rt = ion::runtime::SecuredRuntime::new(backend_registry).with_profile(sec_profile);

    // Snapshot tool definitions before passing ownership to Agent.
    // Used for --export-after-run: HTML export shows the tools panel.
    let mut tool_defs_snapshot: Vec<ion::export::ExportToolInfo> = tools
        .tool_defs()
        .into_iter()
        .map(|td| ion::export::ExportToolInfo {
            name: td.name,
            description: td.description,
            parameters: td.parameters,
        })
        .collect();
    // 按类型分组 + 组内字母序，避免 HashMap 随机顺序导致同类工具散落。
    // 优先级：内置核心(read/write/bash...) > git > skill > goal > spawn/orchestrate > wasm > mcp__
    tool_defs_snapshot.sort_by(|a, b| {
        fn group(name: &str) -> u8 {
            if name.starts_with("mcp__") {
                6
            } else if name.starts_with("wasm_") {
                5
            } else if matches!(
                name,
                "spawn_worker"
                    | "send_to_worker"
                    | "resume_worker"
                    | "await_worker"
                    | "channel_send"
                    | "kill_worker"
            ) {
                4
            } else if name.starts_with("goal_") {
                3
            } else if name == "skill" {
                2
            } else if name.starts_with("git_") {
                1
            } else {
                0
            } // 内置核心工具
        }
        (group(&a.name), &a.name).cmp(&(group(&b.name), &b.name))
    });

    // Snapshot system prompt for --export-after-run（export 时复用完整 system prompt）。
    // 注意：不再在这里预补 bash_tool_guide。agent loop 运行时会在 on_system_prompt 钩子里
    // 注入它（BashExtension），而且我们现在缓存运行时最终 prompt 到 session JSONL
    // （commit 6a8e99f），export 读的是运行时快照，不需要这里预补。
    // 之前预补会导致 guide 在最终 prompt 里出现 2 次（这里 1 次 + agent loop 每轮 1 次）。
    // 补一次全局 rules（只全局 rule 进 system prompt；路径匹配 rule 走 tool result，不进 SP）
    {
        let rules_ext = ion::rules_engine::RulesEngineExtension::new();
        let rules = rules_ext.load_rules();
        let global_rules: Vec<ion::rules_engine::Rule> = rules
            .iter()
            .filter(|r| {
                r.apply_to.is_empty() || r.apply_to.iter().any(|p| p == "**/*" || p == "**")
            })
            .cloned()
            .collect();
        if !global_rules.is_empty() {
            sys_prompt.push_str(&ion::rules_engine::RulesEngineExtension::format_rules_xml(
                &global_rules,
            ));
        }
    }
    let sys_prompt_snapshot = sys_prompt.clone();
    let mut agent =
        Agent::new(registry, model, Some(sys_prompt), tools, config).with_runtime(Box::new(rt));

    // ── 注入 session_cwd + session_id（让场景1写生命周期 entry + 更新 SessionIndex）──
    // session_cwd 让 compaction / step-snapshot 等 entry 能落盘；
    // session_id 让 increment_turn_stats/increment_compress_count 能更新索引。
    let run_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if !run_cwd.is_empty() {
        agent.set_session_cwd(Some(run_cwd.clone()));
        // 记录首次启动工作路径（CI 启动路径 / 首次选中，用实际 session_id 而非 session_id_in）
        ion::session_index::SessionIndex::set_initial_cwd(session_id, &run_cwd);
    }
    agent.set_session_id(Some(session_id.to_string()));
    // 记录权限模式快照（permissive/standard/strict/autopilot/readonly）
    ion::session_index::SessionIndex::set_security_profile(session_id, profile);

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
    let mut ext_reg = ion::agent::extension::ExtensionRunner::new();

    // Scene 1 (direct `ion "prompt"`) has no worker StreamingExtension, but
    // pre-tool Hooks can append audit entries before the final save_session().
    // Persist message checkpoints first so a rejected call is written in the
    // same order and branch as the provider conversation:
    // User -> Assistant(tool call) -> hook_event -> ToolResult(error).
    ext_reg.register(Box::new(CmdRunSessionPersistenceExtension::new(session_id)));

    // ── 注入 ctx.fs 统一文件访问能力（RuntimeFileSystem）──
    // 场景 1（直接执行）：用 LocalRuntime（本地 fs）+ allowed_roots 白名单。
    // 内置扩展通过 registry.filesystem() 拿到，WASM 扩展通过 host_read_file 拿到。
    {
        let fs_rt: std::sync::Arc<dyn ion::runtime::Runtime> =
            std::sync::Arc::new(ion::runtime::LocalRuntime::new());
        let fs_allowed_roots = ion::agent::extension::RuntimeFileSystem::default_allowed_roots(
            std::path::Path::new(&cwd),
        );
        let runtime_fs = std::sync::Arc::new(ion::agent::extension::RuntimeFileSystem::new(
            fs_rt,
            fs_allowed_roots,
        ));
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
    let storage_ctx = ion::storage_context::StorageContext::new(&cwd, &session_id, &cwd);
    ext_reg = ext_reg.with_storage(storage_ctx.clone());
    tracing::info!("[extension] StorageContext injected (data_dirs available)");

    // ── follow_up channel（cmd_run 路径专用）──
    // 让 background bash 完成通知能注入对话历史。之前 cmd_run 不设这个，
    // spawn_watcher 完成时 tx=None 直接丢弃 → bash_result 永远不进 session.jsonl。
    // 对齐 worker_rpc.rs:828/927/1113 的做法。
    let (cmd_run_follow_up_tx, cmd_run_follow_up_rx) = tokio::sync::mpsc::unbounded_channel::<(
        ion_provider::Message,
        ion::agent::agent_loop::DeliverAs,
    )>();

    // ── 注册 BashExtension（让 bash 工具的 guide + 后台进程摘要通过 on_system_prompt
    //    自动注入，跟 memory/plan/rules_engine 等扩展一致，而非内核硬编码调用）──
    // ★ 注入 follow_up_tx，让 spawn_watcher 能发完成通知
    let mut cmd_run_bash_ext = ion::agent::bash::BashExtension::new(storage_ctx.clone());
    cmd_run_bash_ext.set_follow_up_tx(cmd_run_follow_up_tx);
    ext_reg.register(Box::new(cmd_run_bash_ext));

    // ── 注册 RulesEngineExtension（扫描 <project>/.ion/rules/*.md，匹配的项目规则
    //    通过 on_system_prompt 注入 <rules> XML）──
    ext_reg.register(Box::new(ion::rules_engine::RulesEngineExtension::new()));

    // ── 注册 ContextFilesExtension（加载 AGENTS.md/CLAUDE.md，--no-context-files 关闭）
    if !ion::context_files_extension::ContextFilesExtension::is_disabled_by_env() {
        ext_reg.register(Box::new(ion::context_files_extension::ContextFilesExtension::new()));
    }

    // ── 注册 HookExtension（扫描 .ion/hooks.json，command/http/prompt/agent handler
    //    通过事件钩子触发，对齐 Claude Code hooks 系统）──
    let hooks_project_dir = std::path::PathBuf::from(&cwd);
    if ion::hooks::extension::HookExtension::has_hooks(&hooks_project_dir) {
        ext_reg.register(Box::new(ion::hooks::extension::HookExtension::new(
            hooks_project_dir,
            None,                               // runtime — 场景 1 无 host 引擎，agent handler 不可用
            Some(std::sync::Arc::clone(&registry_for_ext)), // prompt handler 调 LLM 用
            Some(model_for_ext.clone()),        // prompt handler 用当前会话模型
            None,                               // manager_bridge — 场景 1 无 MCP 转发
            None,                               // follow_up_tx — 场景 1 无 follow_up 通道
        )));
        tracing::info!("[extension] HookExtension registered (hooks.json detected)");
    }

    // Register per-turn session index extension if session is active
    if !session_id.is_empty() {
        ext_reg.register(Box::new(SessionIndexExtension::new(
            session_id,
            &eff.model,
            &eff.provider,
        )));
    }

    // ── MemoryExtension（cmd_run 路径补注册，对齐 worker_rpc:915）──
    // 之前 cmd_run 只注册了 GlobalMemoryExtension（singleton），没注册 MemoryExtension。
    // MemoryExtension 负责把 <memory_outline> XML 注入 system prompt（on_system_prompt hook），
    // 让 LLM 在后续对话中看到历史记忆。不注册 → XML 不注入 → LLM 看不到记忆。
    //
    // ★ 关键：MemoryExtension.store 必须和 MemorySaveTool.store 共享同一个 Arc，
    // 否则 memory_save 写入的数据 on_system_prompt 读不到（两个独立的 store）。
    // 之前创建了两个独立的 memory_store，导致 save 写一个、prompt 读另一个。
    // 修复：在 build_tools 之后、agent 创建之前统一创建一个 shared store。
    //（下面 Memory 工具注册时复用同一个 store）
    let cmd_run_shared_memory_store: std::sync::Arc<
        tokio::sync::Mutex<ion::agent::memory::MemoryStore>,
    > = std::sync::Arc::new(tokio::sync::Mutex::new(
        ion::agent::memory::MemoryStore::new(storage_ctx.clone()),
    ));
    {
        let mut mem_ext = ion::agent::memory::MemoryExtension::new(storage_ctx.clone());
        mem_ext.store = std::sync::Arc::clone(&cmd_run_shared_memory_store);
        ext_reg.register(Box::new(mem_ext));
        tracing::info!(
            "[extension] memory (MemoryExtension) registered — <memory_outline> XML injection enabled"
        );
    }

    // Auto-register PlanExtension if plan_enter was loaded from a WASM Extension.
    if has_plan_tools {
        ext_reg.register(Box::new(ion::agent::plan_extension::PlanExtension::new()));
        tracing::info!("[plan] PlanExtension auto-registered (plan tools detected)");
    }

    // Register WASM adapters so runtime modules participate in Extension hooks.
    for wasm_path in &loaded_wasm_paths {
        if let Some(hook_adapter) = wasm_ext_registry.create_hook_adapter(wasm_path) {
            ext_reg.register(Box::new(hook_adapter));
            tracing::info!("[wasm] registered Extension adapter for {}", wasm_path);
        }
    }

    // ── 注册 worker 级内置扩展（与 worker_rpc.rs 对齐）──
    // 这些扩展之前只在 worker_rpc（场景 2/3 host）下注册，
    // cmd_run（场景 1 --print / 单次执行）也应该有，否则 on_session_shutdown 等钩子永远不触发。
    ext_reg.register(Box::new(ion::tool_loop_detector::ToolLoopDetector::new()));
    tracing::info!("[extension] tool-loop-detector registered");

    let title_model = ion::config::IonConfig::load()
        .resolve_tier_model("fast")
        .unwrap_or_else(|| model_for_ext.clone());
    let title_api_key = ion::config::IonConfig::load()
        .resolve_provider_api_key(&title_model.provider)
        .or_else(|| eff.api_key.clone());
    ext_reg.register(Box::new(
        ion::auto_session_title::AutoSessionTitle::with_registry(
            registry_for_ext.clone(),
            title_model,
        )
        .with_api_key(title_api_key),
    ));
    tracing::info!("[extension] auto-session-title registered (fast tier + api_key)");

    let learning_ext = ion::learning_extension::LearningExtension::new()
        .with_registry_model(registry_for_ext, model_for_ext);
    ext_reg.register(Box::new(learning_ext));
    tracing::info!("[extension] learning-extension registered (with LLM distillation)");

    // ── Goal Supervisor（on_gate_check closed loop）──
    // Shares state with GoalSetTool (registered in build_tools above).
    // In scene 1, always enabled (no host config gate) — the extension is inert
    // unless goal_set is actually called by the LLM.
    let goal_ext = ion::goal_supervisor_extension::GoalSupervisorExtension::new()
        .with_shared_state(shared_goal_state.clone())
        .with_session_id(session_id);
    ext_reg.register(Box::new(goal_ext));
    tracing::info!("[extension] goal-supervisor registered (on_gate_check closed loop)");

    // ── Dev Server Detector（detect dev server ports from bash output）──
    // 对齐 worker_rpc.rs 的注册：场景 1 也需要，否则 on_tool_execution_end /
    // on_system_prompt 钩子在 ion "prompt" 单次执行时不触发。
    // 场景 1 无条件注册（同 goal-supervisor / learning-extension 约定，
    // config gate 在场景 3 的 worker_rpc.rs 做）。
    ext_reg.register(Box::new(
        ion::dev_server_detector::DevServerDetectorExtension::new(),
    ));
    tracing::info!("[extension] dev_server_detector registered");

    // ── File Snapshot + Approval（cmd_run 路径对齐 worker_rpc）──
    // Direct/offline runs previously registered the tools but not the lifecycle
    // extension, so a real write produced no parented step-snapshot in JSONL.
    let cmd_run_ion_cfg = ion::config::IonConfig::load();
    if cmd_run_ion_cfg.is_extension_enabled("file-snapshot") {
        let (snapshot_ext, snapshot_store) =
            ion::file_snapshot::FileSnapshotExtension::new_pair(storage_ctx.clone());
        ext_reg.register(Box::new(snapshot_ext));
        let approval_mgr = std::sync::Arc::new(ion::file_snapshot::approval::ApprovalManager::new(
            snapshot_store,
            storage_ctx.clone(),
        ));
        ext_reg.register(Box::new(
            ion::file_snapshot::approval::ApprovalExtension::new(approval_mgr),
        ));
        tracing::info!("[extension] file-snapshot + file-approval registered (cmd_run)");
    } else {
        tracing::info!("[extension] file-snapshot disabled by config (cmd_run)");
    }

    // ── LSP Extension（cmd_run 路径补注册，对齐 worker_rpc:967-979）──
    // LSP 是钩子驱动：on_tool_execution_end 检测 write/edit → 后台启 cargo check
    // → on_context 注入 `<diagnostics>` XML 到 messages。
    // **不应该暴露 LspCheckTool 给 LLM**（用户设计纠正：LSP 不需要 LLM 主动调）。
    if cmd_run_ion_cfg.is_extension_enabled("lsp") {
        let lsp_ext = ion::lsp_extension::LspExtension::new();
        ext_reg.register(Box::new(lsp_ext));
        tracing::info!("[extension] lsp registered (cmd_run, auto-trigger on write/edit)");
    }

    agent = agent.with_extensions(ext_reg);
    // 把 follow_up_rx 注入 agent（cmd_run_follow_up_rx 在前面 BashExtension 注册时创建）。
    // 让 outer_loop 能 drain background bash 完成通知，对齐 worker_rpc 路径。
    agent.set_follow_up_rx(cmd_run_follow_up_rx);
    // Let each extension self-describe its tools (bash/skill/etc.)
    agent.register_extension_tools();

    // ── Memory 工具（cmd_run 路径补注册，对齐 worker_rpc:367-375）──
    // ★ 复用上面 MemoryExtension 的 shared store（不是创建新的）。
    // 这样 memory_save 写入 → on_system_prompt 读取 → <memory_outline> 注入。
    {
        agent.register_tool(Box::new(ion::agent::memory::MemorySaveTool {
            store: std::sync::Arc::clone(&cmd_run_shared_memory_store),
        }));
        agent.register_tool(Box::new(ion::agent::memory::MemorySearchTool {
            store: std::sync::Arc::clone(&cmd_run_shared_memory_store),
        }));
        tracing::info!(
            "[tools] memory_save + memory_search registered (shared store with MemoryExtension)"
        );
    }

    tracing::info!("Running agent...");

    // Schema validation loop
    let max_attempts = if eff.json_schema.is_some() {
        eff.schema_retries + 1
    } else {
        1
    };
    let mut retry_prompt = message.to_string();

    // Inject session context into the WASM Extension registry so Extension data
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
                // ★ Graceful drain：捕获 background bash 完成通知，避免长任务（>30s）完成消息丢失。
                // agent.run 内部 outer_loop 等 30s，超时退出 → on_agent_end。如果后台进程
                // 还在跑（比如 sleep 35），完成时发的 follow_up 没人接收。这里再等 60s
                // 兜底，期间收到的 bash_result 等消息写入 session.jsonl。
                let drain_ms = std::env::var("ION_GRACEFUL_DRAIN_MS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(60_000);
                let drained = agent.graceful_drain_follow_ups(drain_ms, 50).await;
                if !drained.is_empty() {
                    for msg in &drained {
                        // ★ 用消息自带的 timestamp（进程完成时间），而非写入时间。
                        let msg_ts = match msg {
                            ion_provider::Message::Custom(c) => c.timestamp,
                            _ => 0,
                        };
                        let ts_iso = if msg_ts > 0 {
                            ion::session_jsonl::timestamp_iso_from_ms(msg_ts)
                        } else {
                            ion::session_jsonl::timestamp_iso()
                        };
                        let entry = serde_json::json!({
                            "id": ion::session_jsonl::generate_id(),
                            "parentId": session_id,
                            "timestamp": ts_iso,
                            "type": "message",
                            "message": msg,
                        });
                        ion::session_jsonl::append_raw_entry(&run_cwd, &entry);
                    }
                    for msg in drained {
                        agent.push_message(msg);
                    }
                    tracing::info!(
                        "[graceful-drain] cmd_run captured {} follow_up messages after agent.run()",
                        0
                    );
                }

                let output =
                    extract_assistant_text(&agent).unwrap_or_else(|| "(no response)".into());

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
                                            save_session(
                                                session_id,
                                                agent.messages(),
                                                &eff.model,
                                                &eff.provider,
                                                eff.name.as_deref(),
                                            );
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
                                        save_session(
                                            session_id,
                                            agent.messages(),
                                            &eff.model,
                                            &eff.provider,
                                            eff.name.as_deref(),
                                        );
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
                                    save_session(
                                        session_id,
                                        agent.messages(),
                                        &eff.model,
                                        &eff.provider,
                                        eff.name.as_deref(),
                                    );
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
                                save_session(
                                    session_id,
                                    agent.messages(),
                                    &eff.model,
                                    &eff.provider,
                                    eff.name.as_deref(),
                                );
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
                    save_session(
                        session_id,
                        agent.messages(),
                        &eff.model,
                        &eff.provider,
                        eff.name.as_deref(),
                    );
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
                    save_session(
                        session_id,
                        agent.messages(),
                        &eff.model,
                        &eff.provider,
                        eff.name.as_deref(),
                    );
                    // After save, run skill distillation synchronously (NOT spawned —
                    // spawned tasks die when cmd_run returns and the runtime drops).
                    // The on_session_shutdown hook fires BEFORE save_session, so it can't
                    // read the saved file; this is the cmd_run-only path.
                    let project_name = std::env::current_dir()
                        .ok()
                        .and_then(|p| {
                            p.file_name()
                                .and_then(|n| n.to_str().map(|s| s.to_string()))
                        })
                        .unwrap_or_else(|| "unknown".into());
                    let reg_clone = std::sync::Arc::clone(agent.registry());
                    let model_clone = agent.model().clone();
                    let sid_owned = session_id.to_string();
                    match ion::skill_distillation::run_skill_distillation(
                        &sid_owned,
                        &project_name,
                        &reg_clone,
                        &model_clone,
                    )
                    .await
                    {
                        Ok(Some(p)) => {
                            tracing::info!("[learning] skill distilled to {}", p.display())
                        }
                        Ok(None) => tracing::info!("[learning] no skill distilled"),
                        Err(e) => tracing::warn!("[learning] skill distillation failed: {e}"),
                    }
                    break;
                }
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("MissingApiKey") || msg.contains("API key") {
                    eprintln!("No API key found. Run: ion config set api-key <key>");
                } else if msg.contains("401")
                    || msg.contains("403")
                    || msg.contains("AuthError")
                    || msg.contains("Invalid API key")
                    || msg.contains("unauthorized")
                    || msg.contains("forbidden")
                {
                    // Auth error: key is invalid, expired, or lacks permission.
                    eprintln!(
                        "API key invalid or expired (HTTP 401/403). Run: ion config set api-key <new-key>"
                    );
                } else {
                    eprintln!("Error: {e}");
                }
                std::process::exit(1);
            }
        }
    } // end for

    // ★ 生成 session 标题，写入 SessionIndex + session.jsonl（可溯源）。
    // session.jsonl 里的 custom_message entry 是**溯源记录**——让 export 能
    // 从数据里追踪到 "title 是什么时候、由什么生成的"。
    // HTML 渲染时这个 entry 用 CSS display:none 隐藏（不污染对话流），
    // 但数据层必须存在。
    //
    // 优先级：
    //   1. SessionIndex 已有 name（AutoSessionTitle 扩展用 LLM 生成的简短标题）→ 用这个
    //   2. 否则 fallback 到首条 user message（截断到 60 字符）
    {
        // 1. 优先用 SessionIndex 已有 name（LLM 生成的）
        let existing_name = ion::session_index::SessionIndex::load()
            .sessions
            .get(session_id)
            .cloned()
            .and_then(|meta| meta.name)
            .filter(|n| !n.trim().is_empty());

        let title = if let Some(name) = existing_name {
            name
        } else {
            // 2. Fallback: 从首条 user message 提取
            agent
                .messages()
                .iter()
                .find_map(|msg| match msg {
                    Message::User(u) => {
                        let text = u
                            .content
                            .iter()
                            .filter_map(|b| match b {
                                ion::agent::messages::ContentBlock::Text(t) => {
                                    Some(t.text.as_str())
                                }
                                _ => None,
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        if text.is_empty() {
                            None
                        } else {
                            let t = text.trim();
                            let first_line = t.lines().next().unwrap_or(t);
                            let first_sentence = first_line
                                .split(['.', '。', '!', '?'])
                                .next()
                                .unwrap_or(first_line)
                                .trim();
                            if first_sentence.is_empty() {
                                None
                            } else if first_sentence.chars().count() > 60 {
                                Some(first_sentence.chars().take(57).collect::<String>() + "...")
                            } else {
                                Some(first_sentence.to_string())
                            }
                        }
                    }
                    _ => None,
                })
                .unwrap_or_else(|| "Untitled".to_string())
        };

        ion::session_index::SessionIndex::set_name(session_id, &title);
        tracing::info!("[cmd_run] set session name in index: \"{title}\"");

        // ★ 写 session_name custom entry 到 session.jsonl（可溯源）。
        // 去重：如果已有 session_name entry，更新 content + timestamp，不重复追加。
        // HTML 渲染时这个 entry 用 CSS display:none 隐藏——数据在但不污染对话流。
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let path = ion::session_jsonl::resolve_session_file(&cwd);
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let mut entries: Vec<serde_json::Value> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let now_iso = ion::session_jsonl::timestamp_iso();
        let new_content_str = format!("📝 Session title: {title}");

        // 去重：找已有的 session_name entry
        let existing_idx = entries.iter().position(|e| {
            e.get("type").and_then(|v| v.as_str()) == Some("custom_message")
                && e.get("customType").and_then(|v| v.as_str()) == Some("session_name")
        });

        if let Some(idx) = existing_idx {
            if let Some(obj) = entries[idx].as_object_mut() {
                obj.insert("content".into(), serde_json::json!(new_content_str));
                obj.insert("timestamp".into(), serde_json::json!(now_iso));
            }
        } else {
            let last_id = entries
                .iter()
                .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
                .last()
                .unwrap_or(session_id)
                .to_string();
            let entry = serde_json::json!({
                "type": "custom_message",
                "customType": "session_name",
                "content": new_content_str,
                "display": false,
                "id": ion::session_jsonl::generate_id(),
                "parentId": last_id,
                "timestamp": now_iso,
            });
            entries.push(entry);
        }

        // 重写文件
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
        {
            for (i, e) in entries.iter().enumerate() {
                if i > 0 {
                    let _ = writeln!(f);
                }
                let _ = write!(f, "{}", serde_json::to_string(e).unwrap_or_default());
            }
        }
        tracing::info!(
            "[cmd_run] wrote session_name custom entry to session.jsonl (display=false)"
        );
    }

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
        match ion::export::export_session_with_tools_and_prompt(
            session_id,
            std::path::Path::new(export_path),
            tools_opt,
            Some(sys_prompt_snapshot.clone()),
        ) {
            Ok(()) => println!("Exported to {export_path}"),
            Err(e) => {
                eprintln!("Export failed: {e}");
                std::process::exit(1);
            }
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
            eprintln!(
                "❌ Cannot connect to Host at {}\n   先启动: ion serve\n   错误: {e}",
                sock_path.display()
            );
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
    // 30 秒超时：防止 Manager 对未知 session 卡死等 oneshot。
    let mut reader = BufReader::new(stream);
    let mut attempts = 0;
    loop {
        let mut line = String::new();
        match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            reader.read_line(&mut line),
        )
        .await
        {
            Err(_) => {
                eprintln!(
                    "❌ RPC timeout (30s) — Manager did not respond. Session may not exist or agent is busy."
                );
                std::process::exit(1);
            }
            Ok(Ok(0)) => {
                eprintln!("(Manager closed connection without response)");
                break;
            }
            Ok(Ok(_)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
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
            Ok(Err(e)) => {
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

/// Subscribe to real-time events from a session or Extension.
/// Connects to Manager socket, sends subscribe, prints events line by line.
async fn cmd_subscribe(
    session: Option<&str>,
    extension: Option<&str>,
    ui: bool,
    replay: Option<usize>,
) {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let sock_path = ion::paths::host_socket_path();
    let mut stream = match tokio::net::UnixStream::connect(&sock_path).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "❌ Cannot connect to Host at {}\n   先启动: ion serve\n   错误: {e}",
                sock_path.display()
            );
            std::process::exit(1);
        }
    };

    let mut req = serde_json::json!({"method": "subscribe"});
    if let Some(sid) = session {
        req["session"] = serde_json::json!(sid);
    }
    if let Some(p) = extension {
        req["extension"] = serde_json::json!(p);
    }
    if ui {
        req["ui"] = serde_json::json!(true);
    }
    if let Some(n) = replay {
        req["replay"] = serde_json::json!(n);
    }

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
            println!(
                "{}",
                serde_json::to_string_pretty(&v).unwrap_or(line.trim().to_string())
            );
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
    let mut key_cache: std::collections::HashMap<String, String> = std::collections::HashMap::new();
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
        let sessions_json: Vec<_> = entries
            .iter()
            .map(|(id, m)| {
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
            })
            .collect();
        let project_label = if all {
            serde_json::Value::Null
        } else {
            serde_json::json!({
                "cwd": cwd,
                "projectKey": current_key,
            })
        };
        println!(
            "{}",
            serde_json::json!({
                "project": project_label,
                "sessions": sessions_json,
                "totalCount": entries.len(),
            })
            .to_string()
        );
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
        let short_id = if id.len() > 10 {
            &id[..10]
        } else {
            id.as_str()
        };
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
    let total_cache: u64 = entries
        .iter()
        .map(|(_, s)| s.token_cache_read + s.token_cache_write)
        .sum();
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
    println!(
        "View: {} | Showing {} of {} messages",
        result.view,
        result.messages.len(),
        result.total_count
    );
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
                                b.get("text")
                                    .and_then(|t| t.as_str())
                                    .map(|s| s.to_string())
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
        let suffix = if content.chars().count() > 200 {
            "..."
        } else {
            ""
        };

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
        println!(
            "\n--- {} more messages (use --limit to load more) ---",
            result.total_count - result.messages.len()
        );
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
                if line.is_empty() {
                    continue;
                }
                if let Ok(e) = serde_json::from_str::<serde_json::Value>(line) {
                    entries.push(e);
                }
            }
            if entries.is_empty() {
                None
            } else {
                Some(entries)
            }
        } else {
            load_session_entries(sid)
        }
    };

    // --checkout <name>
    if let Some(name) = &cli.checkout {
        let entries = load_entries(session_id);
        match entries {
            Some(ents) => match ion::session_tree::make_checkout(&ents, name) {
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
            },
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
            eprintln!(
                "❌ entry '{}' not found in session {}",
                rollback_to, session_id
            );
            std::process::exit(1);
        }
        // compaction 安全检查
        if let Some(c_id) = ion::session_tree::check_compaction_safety(&ents, rollback_to) {
            // XL1: --restore-code 穿越压缩点时，只恢复代码不回滚消息（消息层的压缩上下文已丢失）
            if cli.restore_code {
                eprintln!(
                    "⚠️  Cannot rollback messages to {}: it is before a compaction point ({}).",
                    rollback_to, c_id
                );
                eprintln!(
                    "   --restore-code: only restoring code files, skipping message rollback."
                );
                eprintln!("   (快照层独立于压缩，代码可以恢复；但消息无法回滚到压缩点之前)");
                // 只走代码恢复，不走消息回滚
                let target_snapshot =
                    ion::session_jsonl::resolve_step_snapshot_from_file(&cwd, rollback_to);
                match target_snapshot {
                    Some(snapshot) => {
                        let pk = ion::file_snapshot::project_key(&cwd);
                        let store = ion::file_snapshot::SnapshotStore::new(&pk);
                        let result = ion::file_snapshot::restore::restore_to_tree(
                            &store,
                            &snapshot.tree_hash,
                            &cwd,
                            false,
                        );
                        eprintln!(
                            "[restore-code] restored {} files (deleted {}, skipped {})",
                            result.summary.restored, result.summary.deleted, result.summary.skipped
                        );
                        eprintln!("[restore-code] restore_point: {}", result.restore_point_id);
                    }
                    None => {
                        eprintln!(
                            "[restore-code] ⚠️  cannot find step-snapshot for entry '{}' — skipping code restore",
                            rollback_to
                        );
                    }
                }
                return; // 不走消息回滚，直接返回
            }
            // 非 --restore-code：普通回滚穿越压缩点 → 拒绝（消息上下文会丢失）
            eprintln!(
                "❌ Cannot rollback to {}: it is before a compaction point ({}).",
                rollback_to, c_id
            );
            eprintln!("   Branching across compaction loses summarized context.");
            eprintln!(
                "   Hint: use `ion --fork-from-leaf {}/{}` instead, or add --restore-code to only restore files.",
                session_id, rollback_to
            );
            std::process::exit(1);
        }
        let old_leaf = ion::session_tree::resolve_current_leaf(&ents);

        // --restore-code：先恢复代码文件，再回滚消息
        if cli.restore_code {
            let target_snapshot =
                ion::session_jsonl::resolve_step_snapshot_from_file(&cwd, rollback_to);

            match target_snapshot {
                Some(snapshot) => {
                    let pk = ion::file_snapshot::project_key(&cwd);
                    let store = ion::file_snapshot::SnapshotStore::new(&pk);
                    // full 模式或目标落在某次快照之前时，直接使用 JSONL 里的 tree hash。
                    // 后者不能用 tool turn ID 表达，正是 pi PR #8 修复的 ID 错配场景。
                    let is_full = cli.restore_mode.as_deref() == Some("full");
                    let restored_with_delta = if !is_full && !snapshot.uses_baseline {
                        if let Some(turn_id) = snapshot.tool_snapshot_turn_id.as_deref() {
                            // delta mode（默认）：只恢复被工具快照追踪的文件改动
                            let result =
                                ion::file_snapshot::restore::restore_code_to_turn(&store, turn_id);
                            eprintln!(
                                "[restore-code] restored {} files (deleted {}, skipped {})",
                                result.summary.restored,
                                result.summary.deleted,
                                result.summary.skipped
                            );
                            eprintln!("[restore-code] restore_point: {}", result.restore_point_id);
                            true
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if !restored_with_delta {
                        let result = ion::file_snapshot::restore::restore_to_tree(
                            &store,
                            &snapshot.tree_hash,
                            &cwd,
                            false,
                        );
                        eprintln!(
                            "[restore-code:tree] restored {} files (deleted {}, skipped {})",
                            result.summary.restored, result.summary.deleted, result.summary.skipped
                        );
                        eprintln!(
                            "[restore-code:tree] restore_point: {}",
                            result.restore_point_id
                        );
                    }
                }
                None => {
                    eprintln!(
                        "[restore-code] ⚠️  cannot find step-snapshot for entry '{}' — skipping code restore",
                        rollback_to
                    );
                }
            }
        }

        let new_entries = ion::session_tree::make_rollback(
            rollback_to,
            old_leaf.as_deref(),
            cli.rollback_reason.as_deref(),
        )
        .unwrap();
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
            eprintln!(
                "❌ Cannot branch at {}: it is before a compaction point ({}).",
                from_id, c_id
            );
            eprintln!("   Branching across compaction loses summarized context.");
            eprintln!(
                "   Hint: use `ion --fork-from-leaf {}/{}` instead.",
                session_id, from_id
            );
            std::process::exit(1);
        }
        let new_entries =
            ion::session_tree::make_branch(from_id, cli.branch_name.as_deref()).unwrap();
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
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&new_path)
    {
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
    eprintln!(
        "[fork-from-leaf] new session: {} (parent: {}, path: {} entries)",
        new_id,
        src_sid,
        path.len()
    );
    Some(new_id)
}

/// 找 session 文件的绝对路径
fn find_session_file(sid: &str) -> Option<String> {
    let index = ion::session_index::SessionIndex::load();
    let meta = index.get(sid)?;
    let cwd = meta.project.as_deref()?;
    ion::session_jsonl::session_path(cwd)
        .to_str()
        .map(|s| s.to_string())
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
                            println!(
                                "{:<25} {:<15} {}",
                                name,
                                target,
                                if is_current { "*" } else { "" }
                            );
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
        if line.is_empty() {
            continue;
        }
        if let Ok(e) = serde_json::from_str::<serde_json::Value>(line) {
            entries.push(e);
        }
    }
    if entries.is_empty() {
        None
    } else {
        Some(entries)
    }
}

/// 读取 session 文件原始内容
fn load_session_raw_content(sid: &str) -> Option<String> {
    // 尝试直接路径
    if sid.contains('/') || sid.ends_with(".jsonl") {
        return std::fs::read_to_string(sid).ok();
    }
    let index = ion::session_index::SessionIndex::load();
    let meta = index.get(sid)?;
    let cwd = meta.project.as_deref()?;
    // 主 Worker 的 session.jsonl —— 校验 header.id（文件可能属于另一个 session）
    let main_path = ion::session_jsonl::session_path(cwd);
    if ion::session_jsonl::read_session_header(&main_path).is_some_and(|h| h.id == sid)
        && let Ok(content) = std::fs::read_to_string(&main_path)
    {
        return Some(content);
    }
    // 子 Worker（spawn/fork 派发）写的是 <sid>.jsonl
    std::fs::read_to_string(ion::paths::session_jsonl_path_by_id(cwd, sid)).ok()
}

/// 打印 session 的消息树（ASCII）
fn print_session_tree(entries: &[serde_json::Value], sid: &str) {
    let tree = ion::session_tree::get_tree(entries);
    let current_leaf = ion::session_tree::resolve_current_leaf(entries);
    let cwd = entries
        .iter()
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
            println!(
                "  {} → {} {}",
                name,
                target,
                if is_current { "[当前 leaf]" } else { "" }
            );
        }
    }
}

fn print_tree_node(
    node: &ion::session_tree::TreeNode,
    prefix: &str,
    is_last: bool,
    current_leaf: &Option<String>,
) {
    let entry = &node.entry;
    let id = entry.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let entry_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("?");
    // 消息摘要
    let summary = if entry_type == "message" {
        let role = entry
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            .unwrap_or("?");
        let text = entry
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let text = if text.len() > 40 { &text[..40] } else { text };
        format!("[{}] \"{}\"", role, text)
    } else {
        format!("[{}]", entry_type)
    };
    let label = node
        .label
        .as_ref()
        .map(|l| format!(" ← {}", l))
        .unwrap_or_default();
    let is_current = current_leaf.as_deref() == Some(id);
    let current_mark = if is_current { " ← [当前 leaf]" } else { "" };

    let connector = if is_last { "└─ " } else { "├─ " };
    println!(
        "{}{}{} {}{}{}",
        prefix, connector, id, summary, label, current_mark
    );

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
    println!(
        "{:<30} {:<20} {:<10} {:<20}",
        "ID", "MODEL", "RESPONSES", "CREATED"
    );
    println!("{}", "-".repeat(80));
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .collect();
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
                    meta.get("response_count")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    meta.get("created_at")
                        .and_then(|v| v.as_i64())
                        .map(|t| format!("{}s", t / 1000))
                        .unwrap_or_else(|| "?".into()),
                );
                continue;
            }
        }
        println!("{:<30} {:<20} {:<10} {:<20}", id, "?", "?", "(no meta)");
    }
}

/// Map agent color name (from frontmatter `color:` field) to ANSI escape code.
/// Used by cmd_list_agents to colorize the NAME column.
/// Public(crate) so tests in this binary's tests module can exercise it.
pub(crate) fn color_ansi(name: &Option<String>) -> &'static str {
    match name.as_deref().unwrap_or("") {
        "green" | "bright_green" => "\x1b[32m",
        "red" | "bright_red" => "\x1b[31m",
        "yellow" | "bright_yellow" => "\x1b[33m",
        "blue" | "bright_blue" => "\x1b[34m",
        "magenta" | "purple" | "bright_magenta" => "\x1b[35m",
        "cyan" | "bright_cyan" => "\x1b[36m",
        "white" | "bright_white" => "\x1b[37m",
        "orange" => "\x1b[38;5;208m", // 256-color orange
        "gray" | "grey" => "\x1b[90m",
        _ => "\x1b[0m", // 默认无色
    }
}

/// CLI handler for `ion complete` — quick-test query_tier helper.
/// Bypasses session/agent flow, directly calls IonConfig::query_tier.
async fn cmd_complete(tier: &str, system: Option<&str>, json: bool, message: &str) {
    let cfg = ion::config::IonConfig::load();
    let tier_str = cfg
        .tier_models
        .get(tier)
        .map(|s| s.clone())
        .unwrap_or_else(|| "(not configured)".to_string());
    println!("📚 tier '{tier}' → {tier_str}");

    // Build a minimal registry (only needs to route the resolved model)
    let mut registry = ion_provider::registry::ApiRegistry::new();
    registry.register_builtins();
    let registry = std::sync::Arc::new(registry);

    // Show what model was resolved (with fallback info)
    let tier_in_models = cfg.tier_models.get(tier).is_some();
    match cfg.resolve_tier_model(tier) {
        Some(m) => {
            let source = if tier_in_models {
                format!("tier_models['{tier}']")
            } else {
                format!("default (tier '{tier}' not configured, using default_model)")
            };
            println!("📍 source: {source}");
            println!(
                "🤖 model: {} ({}, base_url={}, has_api_key={})",
                m.id,
                m.provider,
                m.base_url,
                cfg.resolve_provider_api_key(&m.provider).is_some()
            );
        }
        None => {
            eprintln!(
                "❌ tier '{tier}' not resolvable AND no default_model/default_provider \
                 configured. Set tier_models['{tier}'] OR default_model+default_provider \
                 in config.json."
            );
            std::process::exit(1);
        }
    }

    let system_prompt = system.unwrap_or("You are a helpful assistant.");
    println!("📤 calling LLM (json={json})...");
    println!("{}", "-".repeat(60));

    match cfg
        .query_tier(&registry, tier, system_prompt, message, json)
        .await
    {
        Ok(text) => {
            println!("{text}");
            println!("{}", "-".repeat(60));
            println!("✅ done");
        }
        Err(e) => {
            eprintln!("❌ LLM call failed: {e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_list_agents() {
    const RESET: &str = "\x1b[0m";

    fn print_agent(a: &ion::agent_config::AgentConfig, suffix: &str) {
        let tool_count = a.tools.as_ref().map(|t| t.len()).unwrap_or(0);
        let tier = a.tier.as_deref().unwrap_or("-");
        let color = color_ansi(&a.color);
        let name_display = if color.is_empty() || color == "\x1b[0m" {
            format!("{:<16}", a.name)
        } else {
            // 颜色码占字符位（不被 {:<16} 计入显示宽度），用 pad 补齐
            format!("{color}{:<16}{}", a.name, RESET)
        };
        println!(
            "{} {:<12} {:<8}  {}{}",
            name_display,
            tier,
            tool_count,
            a.description,
            if suffix.is_empty() {
                String::new()
            } else {
                format!(" ({suffix})")
            }
        );
    }

    let agents = ion::agent_config::builtin_agents();
    println!(
        "{:<16} {:<12} {:<8}  {}",
        "NAME", "TIER", "TOOLS", "DESCRIPTION"
    );
    println!("{}", "-".repeat(90));
    for a in &agents {
        print_agent(a, "");
    }
    // Check global agents dir
    let global_dir = ion::agent_config::global_agents_dir();
    if global_dir.exists() {
        if let Ok(entries) = std::fs::read_dir(&global_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "md").unwrap_or(false) {
                    if let Some(agent) = ion::agent_config::parse_agent_file(&path) {
                        print_agent(&agent, "global");
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
                            print_agent(&agent, "project");
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

fn validate_extension_name(name: &str) -> Result<(), &'static str> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("extension name cannot be empty");
    };
    if !first.is_ascii_lowercase() {
        return Err("extension name must start with a lowercase ASCII letter");
    }
    if !chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-') {
        return Err("extension name must use lower kebab-case (a-z, 0-9, '-')");
    }
    if name.ends_with('-') || name.contains("--") {
        return Err("extension name cannot end with '-' or contain '--'");
    }
    Ok(())
}

fn extension_scaffold(name: &str) -> (String, String, String) {
    let artifact_name = name.replace('-', "_");
    let cargo_toml = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[workspace]
"#,
    );
    let lib_rs = format!(
        r##"//! {name} — ION WASM Extension.
//!
//! Build:
//!   cargo build --release --target wasm32-wasip1

#![no_std]

#[link(wasm_import_module = "env")]
extern "C" {{
    fn host_register_tool(
        name_ptr: *const u8,
        name_len: u32,
        description_ptr: *const u8,
        description_len: u32,
        schema_ptr: *const u8,
        schema_len: u32,
    );
}}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {{
    loop {{}}
}}

fn register_tool(name: &str, description: &str, schema: &str) {{
    unsafe {{
        host_register_tool(
            name.as_ptr(),
            name.len() as u32,
            description.as_ptr(),
            description.len() as u32,
            schema.as_ptr(),
            schema.len() as u32,
        );
    }}
}}

fn write_output(bytes: &[u8], out_buf: *mut u8, out_capacity: u32) -> u32 {{
    let len = bytes.len().min(out_capacity as usize);
    unsafe {{ core::ptr::copy_nonoverlapping(bytes.as_ptr(), out_buf, len) }};
    len as u32
}}

/// ION WASM Extension ABI version.
#[no_mangle]
pub extern "C" fn extension_version() -> u32 {{
    1
}}

/// Called once after the module is loaded. Register tools here.
#[no_mangle]
pub extern "C" fn extension_init() {{
    register_tool(
        "hello",
        "Return a greeting from {name}.",
        r#"{{"type":"object","properties":{{}}}}"#,
    );
}}

/// Execute a tool and write a JSON response into the host-provided buffer.
#[no_mangle]
pub extern "C" fn extension_execute_tool(
    name_ptr: *const u8,
    name_len: u32,
    _args_ptr: *const u8,
    _args_len: u32,
    out_buf: *mut u8,
    out_capacity: u32,
) -> u32 {{
    let tool_name = unsafe {{
        core::str::from_utf8_unchecked(core::slice::from_raw_parts(
            name_ptr,
            name_len as usize,
        ))
    }};

    match tool_name {{
        "hello" => write_output(br#"{{"greeting":"Hello from {name}!"}}"#, out_buf, out_capacity),
        _ => write_output(br#"{{"error":"unknown tool"}}"#, out_buf, out_capacity),
    }}
}}
"##,
    );
    let manual = format!(
        r#"# {name} 手册

> **状态：开发中** — 脚手架已生成，待补充业务能力与验证。
>
> **类型：** 运行时 WASM Extension
>
> **Extension ID：** `{artifact_name}`
>
> **ABI 版本：** `1`
> **发行版本：** `0.1.0`

## 能力与边界

当前提供 `hello` 示例工具。实现业务能力后更新本节。

## 构建与安装

```bash
cargo build --target wasm32-wasip1 --release
ion extension install ./target/wasm32-wasip1/release/{artifact_name}.wasm
```

## 工具

| 名称 | 参数 | 返回值 | 说明 |
|------|------|--------|------|
| `hello` | `{{}}` | `{{"greeting":"..."}}` | 验证 Extension ABI 与工具调用链路 |

## 验证

```bash
ion rpc --session <sid> --method call_tool \
  --params '{{"tool":"hello","args":{{}}}}'
```
"#,
    );
    (cargo_toml, lib_rs, manual)
}

/// Extension management: create / install / remove / list WASM extensions.
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
        ExtensionAction::Create { name } => {
            if let Err(message) = validate_extension_name(&name) {
                eprintln!("❌ invalid extension name '{name}': {message}");
                std::process::exit(1);
            }

            // Create scaffold: <name>/Cargo.toml + <name>/src/lib.rs + MANUAL.md
            let dir = std::path::Path::new(&name);
            if dir.exists() {
                eprintln!("❌ directory already exists: {name}");
                std::process::exit(1);
            }
            let src_dir = dir.join("src");
            if let Err(e) = std::fs::create_dir_all(&src_dir) {
                eprintln!("❌ failed to create scaffold dir: {e}");
                std::process::exit(1);
            }

            let (cargo_toml, lib_rs, manual) = extension_scaffold(&name);

            // Write files
            if let Err(e) = std::fs::write(dir.join("Cargo.toml"), cargo_toml) {
                eprintln!("❌ failed to write Cargo.toml: {e}");
                std::process::exit(1);
            }
            if let Err(e) = std::fs::write(src_dir.join("lib.rs"), lib_rs) {
                eprintln!("❌ failed to write src/lib.rs: {e}");
                std::process::exit(1);
            }
            if let Err(e) = std::fs::write(dir.join("MANUAL.md"), manual) {
                eprintln!("❌ failed to write MANUAL.md: {e}");
                std::process::exit(1);
            }

            println!("✅ scaffolded extension: {name}/");
            println!("   {name}/Cargo.toml");
            println!("   {name}/src/lib.rs");
            println!("   {name}/MANUAL.md");
            println!();
            println!("Next steps:");
            println!("  cd {name}");
            println!("  cargo build --release --target wasm32-wasip1");
            println!(
                "  ion extension install ./target/wasm32-wasip1/release/{}.wasm",
                name.replace('-', "_")
            );
        }
        ExtensionAction::List => {
            if !ext_dir.exists() {
                println!(
                    "(no extensions installed — {} does not exist)",
                    ext_dir.display()
                );
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
    use ion::worker_registry::{WorkerCreateConfig, WorkerRegistry};
    use std::sync::Arc;
    use parking_lot::Mutex;

    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    registry.lock().set_self_ref(&registry);
    tracing::info!("Submitting: {}", message);
    {
        // ⚠️ parking_lot: create_worker / send_to_worker 内部 .await，
        // 不能持锁调用。改用 prepare + register 两阶段，send_to_worker 用短锁 prepare。
        let cfg = WorkerCreateConfig {
            model: Some(eff.model.clone()),
            provider: Some(eff.provider.clone()),
            ..Default::default()
        };
        let w = match ion::worker_registry::WorkerRegistry::prepare_worker_spawn(&cfg).await {
            Ok(prepared) => registry
                .lock()
                .register_prepared_worker(prepared, &cfg, &registry)
                .unwrap_or_else(|e| panic!("{e}")),
            Err(e) => panic!("{e}"),
        };
        tracing::info!("Worker: {}", w.worker_id);

        // Send prompt
        // ⚠️ parking_lot: send_to_worker_prepare 是 async（内部 stdin write），
        // 不能持锁调用。改用 WorkerRegistry::send_async（自管锁）。
        let _ = ion::worker_registry::WorkerRegistry::send_async(
            &registry,
            &w.worker_id,
            "prompt",
            serde_json::json!({"text": message}),
        )
        .await;
    }

    // Wait for execution
    tokio::time::sleep(std::time::Duration::from_secs(15)).await;

    // Get result
    {
        let mut reg = registry.lock();
        let workers = reg.list_workers();
        if let Some(w) = workers.first() {
            match reg
                .send_to_worker(
                    &w.worker_id,
                    "get_last_assistant_text",
                    serde_json::json!({}),
                )
                .await
            {
                Ok(r) => println!(
                    "{}",
                    r.get("data")
                        .and_then(|v| v.as_str())
                        .unwrap_or("(no response)")
                ),
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

    // 安装 rustls crypto provider（必须在任何 reqwest/rustls Client build 之前）。
    // reqwest 0.13（被 rmcp 间接依赖）用 rustls-no-provider feature，若不显式 install
    // provider，build Client 时会 panic：'No rustls crypto provider is configured'。
    // 配了 HTTP MCP server（rmcp 连接）时会触发，故必须在 main 最早期执行。
    // install_default 幂等：已装过返回 Err，忽略即可。
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // FauxProvider CLI flags → env vars (so build_registry_and_model picks them up)
    if let Some(ref s) = cli.faux_script {
        unsafe {
            std::env::set_var("ION_FAUX_SCRIPT", s);
        }
    }
    if let Some(ref r) = cli.faux_reply {
        unsafe {
            std::env::set_var("ION_FAUX_REPLY", r);
        }
    }
    if let Some(rep) = cli.faux_repeat {
        unsafe {
            std::env::set_var("ION_FAUX_REPEAT", rep.to_string());
        }
    }

    // --local / --remote override: set env var before any config load
    // (IonConfig::load reads this to override runtime.default_mode)
    // Safety: this runs at the very start of main(), before any other threads exist.
    if cli.local {
        unsafe {
            std::env::set_var("ION_RUNTIME_OVERRIDE", "local");
        }
    } else if cli.remote {
        unsafe {
            std::env::set_var("ION_RUNTIME_OVERRIDE", "remote");
        }
    }

    // --no-context-files → env var (cmd_run + spawned workers both read it)
    if cli.no_context_files {
        unsafe { std::env::set_var("ION_NO_CONTEXT_FILES", "1"); }
    }

    let mut eff = resolve_effective(&cli);

    // ── --mode rpc: RPC 模式（JSON-RPC over stdin/stdout）──
    // 必须在 read_piped_stdin() 之前！否则 read_piped_stdin 会消费 stdin 内容
    // （host 持续写 RPC 命令，永远不 EOF），导致 worker 永远拿不到输入。
    // 由 host (场景 2/3) spawn 自身 (current_exe + --mode rpc) 创建 worker 子进程，
    // 对齐 pi 的 `pi --mode rpc` 设计。详见 src/worker_rpc.rs。
    if matches!(cli.mode, Some(OutputMode::Rpc)) {
        let args = ion::worker_rpc::WorkerRpcArgs::from_env_args();
        ion::worker_rpc::run_worker_rpc(args).await;
        return;
    }

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
                (_, true, _) => std::fs::read_to_string(ion::session_jsonl::last_session_path())
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
                _ => std::fs::read_to_string(ion::session_jsonl::last_session_path())
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            };
            if session_id.is_empty() {
                eprintln!("No session to export. Run a prompt first, or use --session <id>.");
                std::process::exit(1);
            } else {
                match ion::export::export_session_rich(
                    &session_id,
                    std::path::Path::new(export_path),
                ) {
                    Ok(()) => println!("Exported to {export_path}"),
                    Err(e) => {
                        eprintln!("Export failed: {e}");
                        std::process::exit(1);
                    }
                }
            }
            return;
        }
    } else {
        None
    };

    let effective_message = eff.message.clone();

    // ── --host: 临时 host 模式（快速编排）──
    if cli.host {
        let msg = if effective_message.is_empty() {
            "Hello".to_string()
        } else {
            effective_message
        };
        cmd_host(&msg, cli.agent.as_deref(), export_after_run.as_deref()).await;
        return;
    }

    if !effective_message.is_empty() {
        let (session_id, preloaded) = resolve_session_id(&cli);

        // ── Session Tree 操作：branch / checkout / rollback（在 agent.run 之前追加 leaf_pointer）──
        if !cli.no_session
            && (cli.branch.is_some() || cli.checkout.is_some() || cli.rollback.is_some())
        {
            apply_session_tree_ops(&cli, &session_id);
        }

        // ── fork-from-leaf：从某 leaf 提取新 session ──
        if let Some(spec) = &cli.fork_from_leaf {
            if let Some(new_sid) = do_fork_from_leaf(spec) {
                // 用新 session 继续
                cmd_run(
                    &eff,
                    &eff.message,
                    cli.no_tools,
                    &new_sid,
                    None,
                    &cli.messages,
                    export_after_run.as_deref(),
                )
                .await;
                return;
            } else {
                eprintln!("❌ --fork-from-leaf '{}' failed", spec);
                std::process::exit(1);
            }
        }

        cmd_run(
            &eff,
            &eff.message,
            cli.no_tools,
            &session_id,
            preloaded,
            &cli.messages,
            export_after_run.as_deref(),
        )
        .await;
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
        Some(Commands::Serve { action }) => match action {
            // `ion serve` (no subcommand) → defaults to `ion serve start`
            None => cmd_serve_start(&cli, 8080, 10, 2).await,
            Some(ServeAction::Start {
                port,
                max_workers,
                min_workers,
            }) => {
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
        Some(Commands::Rpc {
            session,
            method,
            params,
        }) => {
            cmd_rpc(session.as_deref(), method, params).await;
        }
        Some(Commands::Sessions { json, all, limit }) => cmd_sessions(*json, *all, *limit).await,
        Some(Commands::History {
            session,
            limit,
            view,
        }) => cmd_history(session, *limit, view).await,
        Some(Commands::Session { action }) => cmd_session(action.clone()).await,
        Some(Commands::Recordings) => cmd_recordings().await,
        Some(Commands::Subscribe {
            session,
            extension,
            ui,
            replay,
        }) => cmd_subscribe(session.as_deref(), extension.as_deref(), *ui, *replay).await,
        Some(Commands::ListAgents) => cmd_list_agents().await,
        Some(Commands::Query {
            tier,
            system,
            json,
            message,
        }) => {
            cmd_complete(tier, system.as_deref(), *json, message).await;
        }
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
            let _ = stream
                .write_all(format!("{}\n", serde_json::to_string(&req).unwrap()).as_bytes())
                .await;
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
    registry: &std::sync::Arc<parking_lot::Mutex<ion::worker_registry::WorkerRegistry>>,
    source: &serde_json::Value,
) -> Result<String, String> {
    use ion::worker_registry::{WorkerCreateConfig, WorkerRelation};
    let agent = source
        .get("agent")
        .and_then(|v| v.as_str())
        .unwrap_or("build")
        .to_string();
    let session_id = source
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| format!("sess_{}", &uuid::Uuid::new_v4().to_string()[..8]));
    let mut cfg = WorkerCreateConfig::default();
    cfg.session = Some(session_id.clone());
    cfg.agent = Some(agent);
    // 从 create_session 参数提取 model/provider（如果没传则用 config 默认值）
    cfg.model = source
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from);
    cfg.provider = source
        .get("provider")
        .and_then(|v| v.as_str())
        .map(String::from);
    // 参数没指定 model → 查 SessionIndex（用户 set_model 过的会话，
    // ensure_worker / auto-create 重建 worker 时应保持用户选的模型，
    // 而不是重置为默认值——治"切 4.7 → 刷新 → 变回 5.2"）
    if cfg.model.is_none() || cfg.model.as_deref() == Some("") {
        let index = ion::session_index::SessionIndex::load();
        if let Some(meta) = index.get(&session_id)
            && !meta.model.is_empty()
        {
            cfg.model = Some(meta.model.clone());
            cfg.provider = Some(meta.provider.clone());
        }
    }
    // Mark as Child relation so the worker uses an INDEPENDENT session file
    // (<session_id>.jsonl) instead of the shared session.jsonl. Without this,
    // every new session created via create_session reads/writes the same shared
    // file, causing cross-session message pollution (critic sessions inheriting
    // lyricist prompts). See docs/testing/SESSION_ISOLATION_BUG.md.
    cfg.relation = Some(WorkerRelation::Child);
    cfg.project_path = source
        .get("project_path")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| source.get("cwd").and_then(|v| v.as_str()).map(String::from))
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        });
    cfg.channels = Some(vec!["main".to_string()]);
    cfg.initial_prompt = source
        .get("initial_prompt")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Lock split: prepare (no lock) → register (short lock).
    // The old `registry.lock().create_worker(...)` held the lock for the
    // entire worktree+spawn duration, blocking ALL RPCs (including list_sessions).
    let prepared = ion::worker_registry::WorkerRegistry::prepare_worker_spawn(&cfg).await?;
    registry
        .lock()
        .register_prepared_worker(prepared, &cfg, registry)?;
    Ok(session_id)
}

/// `get_session_snapshot`：刷新/重连恢复用（设计文档 §3.2）。


/// `get_session_snapshot`：刷新/重连恢复用（设计文档 §3.2）。
/// 元数据 + workspace 信息 + worker 运行态 + 最近消息，一个接口拿全。
async fn do_get_session_snapshot(
    registry: &std::sync::Arc<parking_lot::Mutex<ion::worker_registry::WorkerRegistry>>,
    source: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use ion::session_workspace::WorkspaceStatus;

    let session_id = source
        .get("session_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or("session_id is required")?;

    // worker 运行态（null = 不在运行）
    let worker = registry.lock().workers.values().find(|w| w.session_id == session_id).map(|w| {
        serde_json::json!({
            "workerId": w.worker_id,
            "status": format!("{:?}", w.status).to_lowercase(),
        })
    });

    // workspace 元数据（含运行态合并：worker Busy → running / Idle → idle）
    let mut workspace = ion::session_workspace::WorkspaceSession::from_index(session_id);
    if let (Some(ws), Some(w)) = (&mut workspace, &worker) {
        let busy = w["status"] == "busy";
        if ws.status == WorkspaceStatus::Ready {
            ws.status = if busy { WorkspaceStatus::Running } else { WorkspaceStatus::Idle };
        }
    }

    // 最近消息：直读 session JSONL（worker 不在运行也能恢复），取最后 N 条 message 类 entry。
    // 子会话在 <sid>.jsonl，主会话在共享 session.jsonl——按 id 优先、共享兜底。
    let session_file = ion::session_index::SessionIndex::load()
        .get(session_id)
        .and_then(|meta| meta.project.clone())
        .map(|cwd| {
            let by_id = ion::paths::session_jsonl_path_by_id(&cwd, session_id);
            if by_id.exists() {
                by_id
            } else {
                ion::paths::session_jsonl_path(&cwd)
            }
        });
    let recent_messages: Vec<serde_json::Value> = session_file
        .and_then(|f| std::fs::read_to_string(f).ok())
        .map(|content| {
            content
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .filter(|e| e.get("type").and_then(|t| t.as_str()) == Some("message"))
                .collect::<Vec<_>>()
        })
        .map(|mut msgs| {
            let n = msgs.len().saturating_sub(20);
            msgs.drain(n..).collect()
        })
        .unwrap_or_default();

    Ok(serde_json::json!({
        "sessionId": session_id,
        "workspace": workspace,
        "worker": worker,
        "messageCount": recent_messages.len(),
        "recentMessages": recent_messages,
    }))
}

async fn cmd_serve_start(_cli: &Cli, _port: u16, _max_workers: usize, _min_workers: usize) {
    use ion::worker_registry::WorkerRegistry;
    use std::sync::Arc;
    use parking_lot::Mutex;

    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    registry.lock().set_self_ref(&registry);
    let event_bus = Arc::new(tokio::sync::Mutex::new(
        ion::event_bus::ExtensionEventBus::new(),
    ));

    // ── 注册单例扩展（host 级，只在 serve 模式）──
    {
        let mut reg = registry.lock();
        // Inject EventBus so Monitor/GlobalMemory singletons can broadcast events
        // (otherwise subscribe CLI cannot see monitor_triggered etc.)
        reg.set_event_bus(Arc::clone(&event_bus));
        reg.register_singleton(Box::new(
            ion::global_memory_ext::GlobalMemoryExtension::new(),
        ));
        reg.register_singleton(Box::new(ion::monitor_extension::MonitorExtension::new()));
        reg.register_singleton(Box::new(ion::rules_engine::RulesEngineExtension::new()));
        reg.init_singletons().await;
    }
    // post_init（释放 lock 后调，让单例能 create_worker spawn 系统级 agent）
    // ⚠️ 必须 tokio::spawn！post_init 内部 create_worker(memory-agent) 会调真实 LLM，
    // 如果 await 会阻塞主线程 → socket accept loop 启动不了 → RPC 全部 timeout。
    let post_init_registry = Arc::clone(&registry);
    tokio::spawn(async move {
        ion::worker_registry::WorkerRegistry::post_init_singletons(&post_init_registry).await;
    });

    // ── Host 单例检查 + Unix socket 启动 ──
    // PID 文件防重复启动；Unix socket 让外部 `ion rpc` 能连进来。
    // ⚠️ socket bind 必须在 MCP connect 之前——否则 MCP 30s 超时会阻塞 socket 创建，
    // 导致 CI 并发时 host 起不来（rpc 连不上）。
    if let Some(pid) = ion::paths::host_running() {
        eprintln!(
            "❌ Host already running (pid {pid}). Stop it first or use `ion rpc` to connect."
        );
        return;
    }
    let sock_path = ion::paths::host_socket_path();
    // 清理 stale socket 文件（上次崩溃残留）
    let _ = std::fs::remove_file(&sock_path);
    let listener = match tokio::net::UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "❌ Failed to bind Unix socket at {}: {e}",
                sock_path.display()
            );
            return;
        }
    };
    // 写 PID 文件
    let pid_path = ion::paths::host_pid_path();
    let _ = std::fs::write(&pid_path, std::process::id().to_string());
    eprintln!("🔌 Host listening on Unix socket: {}", sock_path.display());

    // ── Host 级 MCP 管理器（方案 C：host 持有连接，所有 Worker 代理调用）──
    // 放 socket bind 之后异步连，不阻塞 host 启动（CI 并发友好）。
    {
        let ion_cfg = ion::config::IonConfig::load();
        let mcp_config = ion_cfg.mcp_servers.clone();
        if !mcp_config.is_empty() {
            let mcp_manager = std::sync::Arc::new(ion::mcp::McpManager::new(mcp_config));
            eprintln!(
                "[mcp] host connecting {} server(s)...",
                mcp_manager.server_count()
            );
            // 异步连，不阻塞 socket accept loop
            let mcp_for_connect = std::sync::Arc::clone(&mcp_manager);
            let mcp_registry = Arc::clone(&registry);
            tokio::spawn(async move {
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    mcp_for_connect.connect_all(),
                )
                .await;
                eprintln!(
                    "[mcp] {} server(s) connected",
                    mcp_for_connect.connected_count().await
                );
                mcp_for_connect.spawn_reconnect_monitor();
                // set_mcp_manager 在 connect 后（短锁，不阻塞主线程）
                mcp_registry.lock().set_mcp_manager(mcp_for_connect);
            });
        }
    }

    // 自动创建一个默认 build session，让首次 RPC 不用先 create_session（修复 #1）
    // 对齐 pi：pi 启动后默认有一个 SessionManager.create 出的 session
    //
    // ⚠️ 必须用 tokio::spawn，不能直接 .await！
    // 原因：do_create_session → create_worker → 子进程 LLM 调用可能挂起（FauxProvider
    // 队列空 → auto-retry loop）。如果 .await，会阻塞 socket accept loop 启动，导致
    // serve 起来了但所有 RPC 都 timeout（"Manager did not respond"）。
    // 用 spawn 后，socket loop 立即启动，默认 session 在后台异步创建。
    let default_session_registry = Arc::clone(&registry);
    let default_session_event_bus = Arc::clone(&event_bus);
    tokio::spawn(async move {
        match do_create_session(
            &default_session_registry,
            &serde_json::json!({"agent": "build"}),
        )
        .await
        {
            Ok(sid) => {
                eprintln!("🌱 Default session ready: {sid}");
                // SessionListChanged 事件
                let mut bus = default_session_event_bus.lock().await;
                bus.broadcast_raw("host", "SessionListChanged",
                    serde_json::json!({"action": "created", "sessionId": sid}));
            }
            Err(e) => eprintln!("⚠️  Default session 创建失败（后续 RPC 会按需创建）: {e}"),
        }
    });

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
                        let read_result = reader.read_line(&mut line).await;
                        if read_result.is_ok() {
                            let line = line.trim().to_string();
                            if !line.is_empty() {
                                let cmd: serde_json::Value = match serde_json::from_str(&line) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        let resp = serde_json::json!({
                                            "type":"response","id":null,
                                            "success":false,"error":format!("invalid JSON: {e}")
                                        });
                                        let _ = write_half
                                            .write_all(format!("{resp}\n").as_bytes())
                                            .await;
                                        return;
                                    }
                                };
                                let method = cmd
                                    .get("method")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                let _session = cmd
                                    .get("session")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());

                                // ── Stream mode: subscribe ──
                                if method == "subscribe" {
                                    let extension =
                                        cmd.get("extension").and_then(|v| v.as_str()).unwrap_or("");
                                    let session = cmd
                                        .get("session")
                                        .and_then(|v| v.as_str())
                                        .map(|s| s.to_string());

                                    if extension.is_empty() && session.is_some() {
                                        // ── Instance subscribe：订阅 worker 原始事件流 ──
                                        // 无 --extension 有 --session → 收 text_delta / agent_start / agent_end 等
                                        let sid = session.as_ref().unwrap();
                                        // ⚠️ parking_lot: 把 inner_reg 限制在独立 block 内，
                                        // block 结束 guard 必然 drop，不跨下面的 write_all .await。
                                        // ── session 级订阅（D 修复）：订阅按 session 绑定而非 worker 实例 ──
                                        // ① worker 未拉起时挂起等待（60s）而非直接报错——
                                        //    此前"订阅先于 worker 建立即失效"，UI 只能 prompt 后重订绕过
                                        // ② worker 死亡/GC 后 rx 结束，自动重接新 worker（10s），
                                        //    不行才断开让客户端退避重连
                                        let replay_n = cmd
                                            .get("replay")
                                            .and_then(|v| v.as_u64())
                                            .unwrap_or(0)
                                            as usize;
                                        let first = attach_session_sub(&reg, sid, replay_n, 120).await;
                                        match first {
                                            Some((mut rx, replay_events)) => {
                                                let ack = serde_json::json!({
                                                    "type":"subscribed","session":sid,
                                                    "stream":"instance","replayed":replay_events.len()
                                                });
                                                let _ = write_half
                                                    .write_all(format!("{ack}\n").as_bytes())
                                                    .await;
                                                for evt in &replay_events {
                                                    let out = serde_json::json!({
                                                        "type": "instance_event",
                                                        "session": sid,
                                                        "event": evt.get("event").cloned().unwrap_or(evt.clone()),
                                                        "replayed": true,
                                                    });
                                                    if write_half
                                                        .write_all(format!("{out}\n").as_bytes())
                                                        .await
                                                        .is_err()
                                                    {
                                                        return;
                                                    }
                                                }
                                                let _ = write_half.flush().await;
                                                loop {
                                                    // 转发实时事件直到 rx 结束
                                                    while let Some(msg) = rx.recv().await {
                                                        let out = serde_json::json!({
                                                            "type": "instance_event",
                                                            "session": sid,
                                                            "event": msg.get("event").cloned().unwrap_or(msg),
                                                        });
                                                        if write_half
                                                            .write_all(format!("{out}\n").as_bytes())
                                                            .await
                                                            .is_err()
                                                        {
                                                            return;
                                                        }
                                                        let _ = write_half.flush().await;
                                                    }
                                                    // rx 结束：worker 死亡/被 GC → 重接新 worker（session 级）
                                                    let notice = serde_json::json!({
                                                        "type": "resubscribed", "session": sid
                                                    });
                                                    let _ = write_half
                                                        .write_all(format!("{notice}\n").as_bytes())
                                                        .await;
                                                    match attach_session_sub(&reg, sid, 0, 20).await {
                                                        Some((rx2, _)) => rx = rx2,
                                                        None => break,
                                                    }
                                                }
                                            }
                                            None => {
                                                let resp = serde_json::json!({
                                                    "type":"error",
                                                    "error":"no worker for session within 60s"
                                                });
                                                let _ = write_half
                                                    .write_all(format!("{resp}\n").as_bytes())
                                                    .await;
                                            }
                                        }
                                        return;
                                    }

                                    // ── UI subscribe：订阅 UI 事件（Ask/Confirm/Prompt/Notif/Alert）──
                                    let is_ui =
                                        cmd.get("ui").and_then(|v| v.as_bool()).unwrap_or(false);
                                    if is_ui {
                                        let mut bus = ev_bus.lock().await;
                                        let mut rx = bus.subscribe_ui();
                                        drop(bus);
                                        let ack =
                                            serde_json::json!({"type":"subscribed","stream":"ui"});
                                        let _ = write_half
                                            .write_all(format!("{ack}\n").as_bytes())
                                            .await;
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
                                                    if write_half
                                                        .write_all(format!("{msg}\n").as_bytes())
                                                        .await
                                                        .is_err()
                                                    {
                                                        break;
                                                    }
                                                    let _ = write_half.flush().await;
                                                }
                                                None => break,
                                            }
                                        }
                                        return;
                                    }

                                    // ── Extension subscribe：通过 EventBus ──
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
                                    let _ =
                                        write_half.write_all(format!("{ack}\n").as_bytes()).await;
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
                                                if write_half
                                                    .write_all(format!("{msg}\n").as_bytes())
                                                    .await
                                                    .is_err()
                                                {
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
                                    let request_id = cmd
                                        .get("params")
                                        .and_then(|p| p.get("request_id"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("")
                                        .to_string();
                                    let response = cmd
                                        .get("params")
                                        .and_then(|p| p.get("response"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("deny")
                                        .to_string();
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
                                        let _ = write_half
                                            .write_all(format!("{resp}\n").as_bytes())
                                            .await;
                                    } else {
                                        let resp = serde_json::json!({"type":"response","id":cmd.get("id"),"success":false,"error":"request not found or already expired"});
                                        let _ = write_half
                                            .write_all(format!("{resp}\n").as_bytes())
                                            .await;
                                    }
                                    return;
                                }

                                // ── Overview stream: subscribe_overview ──
                                if method == "subscribe_overview" {
                                    let (initial, rx) = {
                                        let mut reg = reg.lock();
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
                                    if write_half
                                        .write_all(format!("{ack}\n").as_bytes())
                                        .await
                                        .is_err()
                                    {
                                        return;
                                    }
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
                                                if write_half
                                                    .write_all(format!("{msg}\n").as_bytes())
                                                    .await
                                                    .is_err()
                                                {
                                                    break;
                                                }
                                                let _ = write_half.flush().await;
                                            }
                                            None => break,
                                        }
                                    }
                                    return;
                                }

                                // ── RPC mode（以下为现有逻辑：session 转发 + 等响应）──
                                let session = cmd
                                    .get("session")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                if let Some(ref sid) = session {
                                    // ⚠️ parking_lot: send_command 持 &mut self + .await（stdin write），
                                    // 不能持锁调用。改用 WorkerRegistry::send_async（自管锁）。
                                    // 先短锁查 worker_id（subscribe 在此同步完成）。
                                    let wid_opt = {
                                        let inner_reg = reg.lock();
                                        inner_reg
                                            .workers
                                            .values()
                                            .find(|w| w.session_id == *sid)
                                            .map(|w| w.worker_id.clone())
                                    };
                                    if let Some(wid) = wid_opt {
                                        // subscribe（短锁，同步）
                                        let _ = { reg.lock().subscribe(&wid) };
                                        // send_async 自管锁 + await oneshot
                                        let params = cmd.get("params").cloned().unwrap_or_default();
                                        match ion::worker_registry::WorkerRegistry::send_async(
                                            &reg, &wid, &method, params,
                                        ).await {
                                            Ok(resp) => {
                                                let mut r = resp.clone();
                                                if let Some(id) = cmd.get("id") {
                                                    r["id"] = id.clone();
                                                }
                                                let _ = write_half
                                                    .write_all(format!("{r}\n").as_bytes())
                                                    .await;
                                                let _ = write_half.flush().await;
                                            }
                                            Err(e) => {
                                                let resp = serde_json::json!({"type":"response","id":cmd.get("id"),"success":false,"error":e});
                                                let _ = write_half
                                                    .write_all(format!("{resp}\n").as_bytes())
                                                    .await;
                                            }
                                        }
                                        return;
                                    } else {
                                        // session 不存在？让 handle_manager_command 处理
                                        let resp = handle_manager_command(&reg, cmd).await;
                                        let _ = write_half
                                            .write_all(format!("{resp}\n").as_bytes())
                                            .await;
                                    }
                                } else {
                                    // 3. Manager 级命令：直接执行，不等
                                    let resp = handle_manager_command(&reg, cmd).await;
                                    let _ =
                                        write_half.write_all(format!("{resp}\n").as_bytes()).await;
                                    let _ = write_half.flush().await;
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
    let global_rx = registry.lock().subscribe_global();

    // 后台任务 1：事件 pump — 遍历所有 worker，drain_events 推送到 stdout + EventBus
    let pump_registry = Arc::clone(&registry);
    let pump_event_bus = Arc::clone(&event_bus);
    tokio::spawn(async move {
        // subscriber channels 放在 lock 外面，避免和 send_to_worker 死锁
        let mut subs: std::collections::HashMap<
            String,
            (String, tokio::sync::mpsc::Receiver<serde_json::Value>),
        > = std::collections::HashMap::new();
        loop {
            // 1. 检查新 worker（短暂锁，subscribe + drain_events）
            {
                let mut reg = pump_registry.lock();
                let current_ids: Vec<String> = reg.workers.keys().cloned().collect();
                for wid in &current_ids {
                    if !subs.contains_key(wid) {
                        let session_id = reg
                            .workers
                            .get(wid)
                            .map(|r| r.session_id.clone())
                            .unwrap_or_default();
                        if let Ok(rx) = reg.subscribe(wid) {
                            subs.insert(wid.clone(), (session_id, rx));
                        }
                    }
                    // 不调 drain_events — reader task 已实时转发 event 给 subscribers
                    // drain_events 会从 stdout_rx 偷走 send_command 等待的 response
                }
                // 清理已死的 worker
                let dead: Vec<String> = subs
                    .keys()
                    .filter(|wid| !current_ids.contains(wid))
                    .cloned()
                    .collect();
                for wid in dead {
                    subs.remove(&wid);
                }
            }
            // 2. 无锁读取 subscriber 事件（不阻塞 send_to_worker）
            for (wid, (session_id, rx)) in subs.iter_mut() {
                while let Ok(msg) = rx.try_recv() {
                    let mtype = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    // ── ExtensionEvent → 广播到 EventBus ──
                    if mtype == "extension_event" {
                        let ev = msg.clone();
                        let mut bus = pump_event_bus.lock().await;
                        let extension = ev
                            .get("extension")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let ct = ev.get("customType").and_then(|v| v.as_str()).unwrap_or("");
                        let data = ev.get("data").cloned().unwrap_or_default();
                        let ev_session = ev.get("session").and_then(|v| v.as_str());
                        let mut event =
                            ion::event_bus::ExtensionEvent::new(extension, ct).with_data(data);
                        // 审批类事件路由到 ui（让 subscribe --ui 也能收到）
                        let ui_custom_types = [
                            "ApprovalRequest",
                            "ApprovalResolved",
                            "ApprovalReset",
                            "Ask",
                            "AskResolved",
                            "AskTimedOut",
                            "Confirm",
                            "Prompt",
                            "Alert",
                            "Notif",
                        ];
                        if ui_custom_types.contains(&ct) {
                            event = event.with_route("ui");
                        }
                        if let Some(s) = ev_session {
                            event = event.with_session(s);
                        }
                        eprintln!(
                            "[debug] broadcasting extension_event: {} {} session={:?}",
                            extension, ct, ev_session
                        );
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
                        let inner_ev = msg.get("event").cloned().unwrap_or(msg.clone());
                        let inner_type = inner_ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        // 广播 worker 原始事件到全局 EventBus，让 subscribe（无参数）也能收到
                        // text_delta / agent_start / agent_end / tool_execution_* 等流式输出。
                        // 之前只 println 到 serve stdout，全局订阅收不到。对齐 pi 行为。
                        if matches!(
                            inner_type,
                            "text_delta" | "agent_start" | "agent_end" | "agent_stopped"
                                | "tool_execution_start" | "tool_execution_end"
                                | "tool_call" | "tool_call_delta"
                        ) {
                            let mut ev_obj = ion::event_bus::ExtensionEvent::new(
                                "worker", inner_type,
                            )
                            .with_data(inner_ev.clone());
                            if !session_id.is_empty() {
                                ev_obj = ev_obj.with_session(session_id);
                            }
                            let mut bus = pump_event_bus.lock().await;
                            bus.broadcast(&ev_obj);
                        }
                        let out = serde_json::json!({
                            "type": "event",
                            "worker_id": wid,
                            "session_id": session_id,
                            "event": inner_ev,
                        });
                        println!("{}", out);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
    });

    // 后台任务 2：处理 Worker 发来的 manager_command（create_worker / channel_send）
    // ⚠️ 用 try_lock：process_pending_commands 内部 create_worker 持锁较久，
    // try_lock 失败就跳过这轮，给 socket handler 的 list_sessions 留出 lock 窗口。
    //
    // ⚠️ parking_lot: process_pending_commands 是 &mut self + .await，
    // guard 不是 Send，不能直接 tokio::spawn。改在独立线程 + 单线程 runtime +
    // LocalSet 里跑（spawn_local 不要求 Send）。
    let cmd_registry = Arc::clone(&registry);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build cmd-loop runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            loop {
                {
                    if let Some(mut reg) = cmd_registry.try_lock() {
                        reg.process_pending_commands(&cmd_registry).await;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
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
            let mut reg = hb_registry.lock();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let mut changed = false;
            for record in reg.workers.values_mut() {
                match record.status {
                    ion::worker_registry::WorkerStatus::Dead
                    | ion::worker_registry::WorkerStatus::Stale => {
                        // 终态/僵尸，由后面 gc_workers 按时间清理，这里跳过
                    }
                    ion::worker_registry::WorkerStatus::Idle => {
                        // Idle 超过 180s 无心跳 → Stale。
                        // 注意：Busy 不在此列（coordinator 等长任务不能误杀），Busy 有单独超时见下。
                        if now - record.last_heartbeat > 180_000 {
                            record.set_status(ion::worker_registry::WorkerStatus::Stale);
                            changed = true;
                        }
                    }
                    ion::worker_registry::WorkerStatus::Busy => {
                        // Busy 超过 10 分钟视为僵死（agent_end 丢失 / agent.run panic / error 事件漏处理）。
                        // 10 分钟足够覆盖正常长任务（coordinator 等 5 分钟 developer 仍留余量），
                        // 但能兜住"永久卡 Busy"的死锁。转 Dead 后由 gc_workers 清理。
                        if now - record.status_since > 600_000 {
                            tracing::warn!(
                                "[heartbeat] worker {} Busy for >{}s, marking Dead (agent_end lost?)",
                                record.worker_id,
                                (now - record.status_since) / 1000
                            );
                            record.set_status(ion::worker_registry::WorkerStatus::Dead);
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
            // 定期 GC：清理 Dead 全部 + Stale 超 10 分钟的。每 tick（30s）调一次，
            // 不再依赖 monitor extension 或 stale_count > 5 阈值。
            let reaped = reg.gc_workers(600);
            if reaped > 0 {
                tracing::info!("[gc] reaped {} dead/stale workers", reaped);
                changed = true;
            }
            if changed {
                reg.broadcast_overview();
            }
        }
    });

    eprintln!(
        "Host started (async RPC, stdin/stdout + Unix socket). Commands: create_worker, create_session, list_sessions, list_workers, send, send_to_worker, kill, channel_send, channel_subscribe, get_overview, quit"
    );

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
                    if line.is_empty() {
                        continue;
                    }
                    let cmd: serde_json::Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(e) => {
                            println!(
                                r#"{{"type":"response","id":null,"success":false,"error":"{e}"}}"#
                            );
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
    registry: &Arc<parking_lot::Mutex<ion::worker_registry::WorkerRegistry>>,
    cmd: serde_json::Value,
) -> serde_json::Value {
    let id = cmd.get("id").cloned().unwrap_or(serde_json::Value::Null);
    let method = cmd
        .get("method")
        .and_then(|v| v.as_str())
        .or_else(|| cmd.get("type").and_then(|v| v.as_str()))
        .unwrap_or("");

    // Read-only commands: acquire lock only briefly to snapshot data, then
    // release before formatting the response. This prevents a slow create_worker
    // (which now uses prepare/register split) from blocking status queries.
    //
    // Write commands acquire the lock inside their own branch as needed.
    let result: Result<serde_json::Value, String> = match method {
        // ── Fast read paths (short lock, snapshot then release) ──
        "list_sessions" => {
            let sessions: Vec<_> = {
                let reg = loop {
                    if let Some(g) = registry.try_lock() {
                        break g;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                };
                reg.workers.values().map(|w| serde_json::json!({
                    "session_id": w.session_id,
                    "agent": w.agent,
                    "status": format!("{}", w.status),
                    "model": w.model,
                    "started_at": w.started_at,
                    "latest_output": w.latest_output.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                    "log_short": w.log_short,
                    "model_size": w.model_size,
                })).collect()
            };
            Ok(serde_json::json!({"sessions": sessions}))
        }
        "list_workers" => {
            let workers: Vec<_> = {
                let reg = registry.lock();
                reg.list_workers()
                    .iter()
                    .map(|w| {
                        serde_json::json!({
                            "workerId": w.worker_id,
                            "sessionId": w.session_id,
                            "project": w.project,
                            "status": format!("{}", w.status),
                            "model": w.model,
                            "agent": w.agent,
                            "parent": w.parent,
                            "channels": w.channels,
                        })
                    })
                    .collect()
            };
            Ok(serde_json::json!({"workers": workers}))
        }
        // Host 级会话直读：纯磁盘读 JSONL，不拉起 worker（UI 浏览历史会话用，毫秒级）
        "get_session_messages" => host_direct_session_read(&cmd, "messages"),
        "list_session_turns" => host_direct_session_read(&cmd, "turns"),
        _ => {
            // Fall through to write-path handling below (acquires lock per-branch).
            handle_manager_command_write(registry, cmd.clone(), id.clone(), method).await
        }
    };

    let mut resp = match result {
        Ok(data) => serde_json::json!({"type":"response","id":id,"success":true,"data":data}),
        Err(e) => serde_json::json!({"type":"response","id":id,"success":false,"error":e}),
    };
    if let Some(sid) = cmd.get("session").and_then(|v| v.as_str()) {
        resp["session"] = serde_json::json!(sid);
    }
    resp
}


/// session 级订阅挂接：等 worker 出现（wait_polls × 500ms）并 subscribe。
/// 返回 None = 超时未出现。锁在每轮短持，不跨 await。
async fn attach_session_sub(
    reg: &std::sync::Arc<parking_lot::Mutex<ion::worker_registry::WorkerRegistry>>,
    sid: &str,
    replay: usize,
    wait_polls: usize,
) -> Option<(
    tokio::sync::mpsc::Receiver<serde_json::Value>,
    Vec<serde_json::Value>,
)> {
    let polls = wait_polls.max(1);
    for i in 0..polls {
        {
            let mut inner_reg = reg.lock();
            let wid_opt = inner_reg
                .workers
                .values()
                .find(|w| w.session_id == sid)
                .map(|w| w.worker_id.clone());
            if let Some(wid) = wid_opt
                && let Ok((rx, ev)) = inner_reg.subscribe_with_replay(&wid, replay)
            {
                return Some((rx, ev));
            }
        } // guard dropped——sleep 不持锁
        if i + 1 < polls {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
    None
}


// ── B 线 fast path：FileIndex 缓存 + 路径解析（不读全文） ──
static FILE_INDEX_CACHE: std::sync::OnceLock<
    parking_lot::Mutex<std::collections::HashMap<std::path::PathBuf, Arc<ion::file_index::FileIndex>>>,
> = std::sync::OnceLock::new();

/// 获取（或增量刷新）指定路径的 FileIndex；失败返回 None
fn get_file_index(path: &std::path::Path) -> Option<Arc<ion::file_index::FileIndex>> {
    let cache = FILE_INDEX_CACHE.get_or_init(|| parking_lot::Mutex::new(std::collections::HashMap::new()));
    let mut guard = cache.lock();
    if let Some(idx) = guard.get(path) {
        // 文件没变（len+mtime 相同）→ 直接命中缓存
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() == idx.file_len && meta.modified().ok() == idx.mtime {
                return Some(Arc::clone(idx));
            }
        }
        // 文件变了：read 端优先——先给旧缓存（可能稍滞后但安全），
        // 后台重建（这里简单同步重建，后续可改 spawn）
    }
    // 全量重建
    match ion::file_index::FileIndex::build(path) {
        Ok(new_idx) => {
            let arc = Arc::new(new_idx);
            guard.insert(path.to_path_buf(), Arc::clone(&arc));
            Some(arc)
        }
        Err(_) => None,
    }
}

/// sid → 会话 JSONL 文件路径（不读全文——用 header 前几行校验）
fn resolve_session_path(sid: &str) -> Option<std::path::PathBuf> {
    if sid.contains('/') || sid.ends_with(".jsonl") {
        let p = std::path::PathBuf::from(sid);
        return p.exists().then_some(p);
    }
    let index = ion::session_index::SessionIndex::load();
    let meta = index.get(sid)?;
    let cwd = meta.project.as_deref()?;
    let main_path = ion::session_jsonl::session_path(cwd);
    if ion::session_jsonl::read_session_header(&main_path).is_some_and(|h| h.id == sid) {
        return Some(main_path);
    }
    let own = ion::paths::session_jsonl_path_by_id(cwd, sid);
    own.exists().then_some(own)
}

/// fast_messages：索引层过滤+分页，只对返回页 read_at 解析
fn fast_messages(
    path: &std::path::Path,
    params: &serde_json::Value,
) -> Option<serde_json::Value> {
    let idx = get_file_index(path)?;
    let metas: &[serde_json::Value] = &idx.metas;

    // 参数
    let view = match params.get("view").and_then(|v| v.as_str()).unwrap_or("live") {
        "since_compaction" => ion::message_retrieval::View::SinceCompaction,
        "full" => ion::message_retrieval::View::Full,
        s if s.starts_with("branch:") => {
            ion::message_retrieval::View::Branch(s[7..].to_string())
        }
        _ => ion::message_retrieval::View::Live,
    };
    let after = params.get("after").and_then(|v| v.as_str()).map(String::from);
    let before = params.get("before").and_then(|v| v.as_str()).map(String::from);
    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(50);
    let from_head = params
        .get("from")
        .and_then(|v| v.as_str())
        .map(|v| v == "head")
        .unwrap_or(false);

    // ── O(1) 快速路径：Live 视图（默认）+ 无游标过滤的简单分页 → 预计算索引直接切片 ──
    // 只有 view=Live 且无 after/before/custom 时才走（覆盖 UI 打开/翻页/head 的 90%+ 场景）
    let is_live_simple = matches!(view, ion::message_retrieval::View::Live)
        && after.is_none()
        && before.is_none();
    if is_live_simple {
        let idxs = &idx.live_message_idxs;
        let total = idxs.len();
        let (start, end) = if from_head {
            (0, limit.min(total))
        } else if limit == 0 {
            (0, total)
        } else {
            (total.saturating_sub(limit), total)
        };
        let has_more = if from_head {
            total > end
        } else {
            start > 0
        };
        let messages: Vec<serde_json::Value> = idxs[start..end]
            .iter()
            .filter_map(|&i| idx.parse_entry(i))
            .collect();
        let next_cursor = if has_more {
            if from_head {
                messages.last()?.get("id").and_then(|v| v.as_str()).map(|s| s.to_string())
            } else {
                messages.first()?.get("id").and_then(|v| v.as_str()).map(|s| s.to_string())
            }
        } else {
            None
        };
        return Some(serde_json::json!({
            "messages": messages,
            "hasMore": has_more,
            "totalCount": total,
            "nextCursor": next_cursor,
            "view": "live",
            "compactionPoints": [],
        }));
    }

    // ── 通用路径（非 Live 视图 / after/before 游标 / custom 过滤）──
    let rp = ion::message_retrieval::RetrievalParams {
        view,
        after,
        before,
        limit,
        from_head,
        complete_turn: true,
        include_custom: ion::message_retrieval::CustomFilter::None,
    };
    let result = ion::message_retrieval::retrieve_messages(metas, &rp);

    let messages: Vec<serde_json::Value> = result
        .messages
        .iter()
        .map(|m| {
            let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
            if id.is_empty() {
                return m.clone(); // branch_summary 等合成条目
            }
            match idx.id_to_idx.get(id) {
                Some(&i) => idx.parse_entry(i).unwrap_or_else(|| m.clone()),
                None => m.clone(),
            }
        })
        .collect();

    Some(serde_json::json!({
        "messages": messages,
        "hasMore": result.has_more,
        "totalCount": result.total_count,
        "nextCursor": result.next_cursor,
        "view": result.view,
        "compactionPoints": result.compaction_points,
    }))
}

/// fast_turns：索引层 turn 分组 + 概览（仅 User/Assistant 预览），返回页按需解析
fn fast_turns(
    path: &std::path::Path,
    params: &serde_json::Value,
) -> Option<serde_json::Value> {
    let idx = get_file_index(path)?;
    let metas: &[serde_json::Value] = &idx.metas;

    let full_content = params
        .get("full_content")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let limit = params
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(50);

    // turn 分组在 metas 上（只需 type + role）
    let turns = ion::message_retrieval::group_into_turns(metas);
    let total_count = turns.len();
    let has_more = total_count > limit;
    let page_turns: Vec<_> = if limit > 0 && total_count > limit {
        turns[total_count - limit..].to_vec() // 最新 N 轮
    } else {
        turns
    };
    let next_cursor = if has_more {
        page_turns
            .first()
            .and_then(|g| g.first())
            .and_then(|e| e.get("id").and_then(|v| v.as_str()))
            .map(|s| s.to_string())
    } else {
        None
    };

    let turn_json: Vec<serde_json::Value> = page_turns
        .iter()
        .map(|group| {
            let turn_id = group
                .first()
                .and_then(|e| e.get("id").and_then(|v| v.as_str()))
                .unwrap_or("");
            // 概览：从索引 heads 直接取（不用全量解析）
            let user_content = group
                .first()
                .and_then(|e| idx.id_to_idx.get(e.get("id").and_then(|v| v.as_str()).unwrap_or("")))
                .and_then(|&i| idx.heads[i].user_head.clone())
                .unwrap_or_default();
            let assistant_content = group
                .last()
                .and_then(|e| idx.id_to_idx.get(e.get("id").and_then(|v| v.as_str()).unwrap_or("")))
                .and_then(|&i| idx.heads[i].asst_head.clone())
                .unwrap_or_default();
            let tool_count = group
                .iter()
                .filter(|e| {
                    e.get("message").and_then(|m| m.get("role")).and_then(|r| r.as_str()) == Some("toolResult")
                })
                .count();

            serde_json::json!({
                "turnId": turn_id,
                "userContent": user_content,
                "assistantContent": assistant_content,
                "keySteps": [],
                "toolCallCount": tool_count,
                "tokens": {"input": 0, "output": 0},
                "status": "completed",
                "summary": serde_json::Value::Null,
                "durationMs": serde_json::Value::Null,
                "source": "index",
            })
        })
        .collect();

    Some(serde_json::json!({
        "turns": turn_json,
        "hasMore": has_more,
        "totalCount": total_count,
        "nextCursor": next_cursor,
    }))
}

/// Host 级会话直读：不拉起 worker，纯磁盘读 JSONL 后用 message_retrieval 检索。
/// JSONL 是 append-only（每条事件落盘一次），活跃 worker 写入中也安全——读到截至目前的完整条目。
/// 响应形状与 worker 级 get_messages / list_turns 完全一致，UI 无需区分两条路径。
/// `kind`: "messages"（对应 get_messages）或 "turns"（对应 list_turns）。
fn host_direct_session_read(
    cmd: &serde_json::Value,
    kind: &str,
) -> Result<serde_json::Value, String> {
    let params = cmd.get("params").cloned().unwrap_or_default();
    let sid = params
        .get("session")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())
        .or_else(|| cmd.get("session").and_then(|v| v.as_str()))
        .ok_or_else(|| "missing 'session' param".to_string())?
        .to_string();
    // ── B 线 fast path：FileIndex 索引检索（大文件下 O(page)）──
    // 先 resolve 路径（不读全文），再走 fast_messages / fast_turns
    if let Some(path) = resolve_session_path(&sid) {
        let result = if kind == "turns" {
            fast_turns(&path, &params)
        } else {
            fast_messages(&path, &params)
        };
        if let Some(data) = result {
            return Ok(data);
        }
        // fast path 失败 → 走旧全量路径兜底
    }

    let entries =
        load_session_entries(&sid).ok_or_else(|| format!("session not found on disk: {sid}"))?;

    match kind {
        "turns" => {
            let full_content = params
                .get("full_content")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let limit = params
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(50);
            let rp = ion::message_retrieval::RetrievalParams {
                limit,
                ..Default::default()
            };
            let result = ion::message_retrieval::retrieve_turns(&entries, &rp, full_content);
            Ok(serde_json::json!({
                "turns": result.turns.iter().map(|t| serde_json::json!({
                    "turnId": t.turn_id,
                    "userContent": t.user_content,
                    "assistantContent": t.assistant_content,
                    "keySteps": t.key_steps,
                    "toolCallCount": t.tool_call_count,
                    "tokens": {"input": t.tokens_input, "output": t.tokens_output},
                    "status": t.status,
                    "summary": t.summary,
                    "durationMs": t.duration_ms,
                    "source": t.source,
                })).collect::<Vec<_>>(),
                "hasMore": result.has_more,
                "totalCount": result.total_count,
                "nextCursor": result.next_cursor,
            }))
        }
        _ => {
            let view = match params.get("view").and_then(|v| v.as_str()).unwrap_or("live") {
                "since_compaction" => ion::message_retrieval::View::SinceCompaction,
                "full" => ion::message_retrieval::View::Full,
                s if s.starts_with("branch:") => {
                    ion::message_retrieval::View::Branch(s[7..].to_string())
                }
                _ => ion::message_retrieval::View::Live,
            };
            let rp = ion::message_retrieval::RetrievalParams {
                view,
                after: params
                    .get("after")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                before: params
                    .get("before")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                limit: params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(50),
                from_head: params
                    .get("from")
                    .and_then(|v| v.as_str())
                    .map(|v| v == "head")
                    .unwrap_or(false),
                complete_turn: params
                    .get("complete_turn")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true),
                include_custom: match params
                    .get("include_custom")
                    .and_then(|v| v.as_str())
                    .unwrap_or("none")
                {
                    "display_only" => ion::message_retrieval::CustomFilter::DisplayOnly,
                    "all" => ion::message_retrieval::CustomFilter::All,
                    _ => ion::message_retrieval::CustomFilter::None,
                },
            };
            let result = ion::message_retrieval::retrieve_messages(&entries, &rp);
            Ok(serde_json::json!({
                "messages": result.messages,
                "hasMore": result.has_more,
                "totalCount": result.total_count,
                "nextCursor": result.next_cursor,
                "view": result.view,
                "compactionPoints": result.compaction_points,
            }))
        }
    }
}

/// Host 级空闲会话状态合成：session 无活跃 worker 时，只读状态类 RPC
/// （get_session_info / get_settings / get_queue / get_context_usage / get_active_tools）
/// 不再 auto-create worker，而是从 SessionIndex / 全局配置合成「空闲态」响应——
/// 读一个历史会话的状态不应该拉起进程。字段形状与 worker 级一致；
/// 有活跃 worker 时调用方走原转发路径拿真实运行态。
fn host_idle_session_read(
    cmd: &serde_json::Value,
    method: &str,
) -> Result<serde_json::Value, String> {
    let params = cmd.get("params").cloned().unwrap_or_default();
    let sid = params
        .get("session")
        .or_else(|| params.get("session_id"))
        .and_then(|v| v.as_str())
        .or_else(|| cmd.get("session").and_then(|v| v.as_str()))
        .ok_or_else(|| "missing 'session' param".to_string())?
        .to_string();
    match method {
        // 与 worker 级一致：读全局配置并脱敏 api_key（空闲会话的 settings 不依赖 worker 状态）
        "get_settings" => {
            let cfg = ion::config::IonConfig::load();
            let mut cfg_json = serde_json::to_value(&cfg).unwrap_or_default();
            if cfg_json.get("api_key").is_some_and(|v| !v.is_null()) {
                cfg_json["api_key"] = serde_json::json!("***");
            }
            Ok(cfg_json)
        }
        "get_queue" => Ok(serde_json::json!({
            "steering": [], "followUp": [], "steeringCount": 0, "followUpCount": 0,
        })),
        "get_active_tools" => {
            // 空闲会话无注册工具；SessionMeta 记录了最后一次 active tools，作参考返回
            let index = ion::session_index::SessionIndex::load();
            let tools: Vec<String> = index
                .get(&sid)
                .and_then(|m| m.last_active_tools.clone())
                .unwrap_or_default();
            Ok(serde_json::json!({"tools": tools, "count": tools.len()}))
        }
        "get_session_info" | "get_context_usage" => {
            let index = ion::session_index::SessionIndex::load();
            let meta = index
                .get(&sid)
                .ok_or_else(|| format!("session not found: {sid}"))?;
            let (ctx_window, max_tokens) = {
                // 三级查找：registry(models.json) → provider/id → config.json providers
                let reg = ion_provider::registry::ModelRegistry::new();
                let with_provider = if meta.provider.is_empty() {
                    None
                } else {
                    Some(format!("{}/{}", meta.provider, meta.model))
                };
                let m = with_provider
                    .as_deref()
                    .and_then(|k| reg.find_model(k))
                    .or_else(|| reg.find_model(&meta.model));
                if let Some(m) = m {
                    (m.context_window, m.max_tokens)
                } else {
                    let cfg = ion::config::IonConfig::load();
                    cfg.providers
                        .get(&meta.provider)
                        .and_then(|p| p.models.iter().find(|m| m.id == meta.model))
                        .map(|m| {
                            (
                                m.context_window.unwrap_or(128_000),
                                m.max_tokens.unwrap_or(0),
                            )
                        })
                        .unwrap_or((0, 0))
                }
            };
            if method == "get_session_info" {
                Ok(serde_json::json!({
                    "session_id": sid,
                    "model": meta.model,
                    "provider": meta.provider,
                    "agent": meta.agent,
                    "is_running": false,
                    "is_stopped": false,
                    "message_count": meta.message_count,
                    "user_messages": meta.user_prompt_count,
                    "assistant_messages": meta.llm_request_count,
                    "tokens": {
                        "input": meta.token_input,
                        "output": meta.token_output,
                        "total": meta.token_input + meta.token_output,
                    },
                    "steering_queue": 0,
                    "follow_up_queue": 0,
                    "context_window": ctx_window,
                    "max_tokens": max_tokens,
                }))
            } else {
                // 与 worker 级同口径：优先取最后一次 LLM 调用的 usage.input
                // （最准，含 system prompt/工具 schema/tool result），无则字符/4 估算
                let estimated = load_session_entries(&sid)
                    .map(|entries| {
                        let last_input = entries.iter().rev().find_map(|e| {
                            let u = e
                                .get("message")?
                                .get("Assistant")?
                                .get("usage")?
                                .get("input")?
                                .as_u64()?;
                            (u > 0).then_some(u)
                        });
                        if let Some(u) = last_input {
                            return u;
                        }
                        let chars: usize = entries
                            .iter()
                            .filter_map(|e| {
                                let m = e.get("message")?;
                                let mut len = 0usize;
                                // User.content: string 或 Text 块数组
                                if let Some(u) = m.get("User").and_then(|u| u.get("content")) {
                                    if let Some(s) = u.as_str() {
                                        len += s.len();
                                    } else if let Some(arr) = u.as_array() {
                                        for b in arr {
                                            len += b
                                                .get("Text")
                                                .and_then(|t| t.get("text"))
                                                .and_then(|t| t.as_str())
                                                .map_or(0, str::len);
                                        }
                                    }
                                }
                                // Assistant.content: Text 块数组
                                if let Some(arr) = m
                                    .get("Assistant")
                                    .and_then(|a| a.get("content"))
                                    .and_then(|c| c.as_array())
                                {
                                    for b in arr {
                                        len += b
                                            .get("Text")
                                            .and_then(|t| t.get("text"))
                                            .and_then(|t| t.as_str())
                                            .map_or(0, str::len);
                                    }
                                }
                                (len > 0).then_some(len)
                            })
                            .sum();
                        (chars / 4) as u64
                    })
                    .unwrap_or(meta.token_input);
                let total = meta.token_input + meta.token_output;
                Ok(serde_json::json!({
                    "messageCount": meta.message_count,
                    "estimatedTokens": estimated,
                    "contextWindow": ctx_window,
                    "usagePercent": if ctx_window > 0 { (estimated * 100 / ctx_window as u64) as u32 } else { 0 },
                    "totalInputTokens": meta.token_input,
                    "totalOutputTokens": meta.token_output,
                    "autoCompaction": false,
                    "totalTokens": total,
                }))
            }
        }
        _ => Err(format!("not an idle-read method: {method}")),
    }
}

/// Write-path command handler. Each branch acquires the registry lock as needed
/// (and releases it before any long-running await where possible).
async fn handle_manager_command_write(
    registry: &Arc<parking_lot::Mutex<ion::worker_registry::WorkerRegistry>>,
    cmd: serde_json::Value,
    _id: serde_json::Value,
    method: &str,
) -> Result<serde_json::Value, String> {
    use ion::worker_registry::WorkerCreateConfig;

    // ⚠️ parking_lot: 不在顶部持锁——各分支按需 lock + 释放。
    // 之前的 `let mut reg = registry.lock();` 贯穿整个 match，跨多个 .await 编译失败
    // （parking_lot guard 不是 Send，不能跨 await 持有）。
    let result: Result<serde_json::Value, String> = match method {
        "create_worker" => {
            // 兼容两种格式：扁平（cmd 字段直接是 config）和 嵌套（cmd.params 里是 config）
            // RPC client 发的是嵌套格式 {method, params: {...}}，stdin 命令发的是扁平
            let mut cfg_source = if cmd.get("params").map(|v| v.is_object()).unwrap_or(false) {
                cmd.get("params").cloned().unwrap_or_default()
            } else {
                cmd.clone()
            };
            // message → initial_prompt fallback：用户常误用 message 字段（serde 会静默忽略），
            // 这里自动把 message 注入 initial_prompt，保持向后兼容。
            let msg_fallback: Option<String> = if cfg_source.get("initial_prompt").is_none() {
                cfg_source
                    .get("message")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
            } else {
                None
            };
            if let Some(msg) = msg_fallback {
                tracing::warn!(
                    "[rpc] create_worker: 'message' field used as initial_prompt fallback. \
                     Use 'initial_prompt' explicitly to silence this."
                );
                if let Some(obj) = cfg_source.as_object_mut() {
                    obj.insert("initial_prompt".to_string(), serde_json::Value::String(msg));
                }
            }
            let mut cfg: WorkerCreateConfig =
                serde_json::from_value(cfg_source.clone())
                    .map_err(|e| format!("invalid create_worker params: {e}"))?;
            // 支持从 params 显式传 session（重建 worker 时保留 SID）
            if cfg.session.is_none() {
                cfg.session = cfg_source
                    .get("session")
                    .or_else(|| cfg_source.get("session_id"))
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
            // 兼容旧测试脚本：如果 params 传了 cwd 但没有 project_path，映射过去
            if cfg.project_path.is_none() {
                if let Some(cwd_val) = cmd
                    .get("params")
                    .and_then(|p| p.get("cwd"))
                    .or_else(|| cmd.get("cwd"))
                {
                    if let Some(cwd) = cwd_val.as_str() {
                        cfg.project_path = Some(cwd.to_string());
                    }
                }
            }
            // ⚠️ parking_lot: create_worker 内部有 .await（spawn 子进程等），
            // 不能持锁调用。改用 prepare_worker_spawn（不持锁）+ register_prepared_worker（sync）。
            match ion::worker_registry::WorkerRegistry::prepare_worker_spawn(&cfg).await {
                Ok(prepared) => {
                    // register 拿走所有权前抓 worktree 元数据（响应带给 caller，UI 渲染卡片用）
                    let ws_meta = prepared.worktree_info.as_ref().map(|w| {
                        serde_json::json!({
                            "worktree_path": w.path,
                            "worktree_branch": w.branch,
                        })
                    });
                    match registry.lock().register_prepared_worker(prepared, &cfg, &registry) {
                        Ok(info) => {
                            let mut data = serde_json::json!({
                                "workerId": info.worker_id,
                                "sessionId": info.session_id,
                            });
                            if let Some(ws) = ws_meta
                                && let Some(obj) = data.as_object_mut()
                            {
                                obj.insert("worktree_path".to_string(), ws["worktree_path"].clone());
                                obj.insert("worktree_branch".to_string(), ws["worktree_branch"].clone());
                            }
                            Ok(data)
                        }
                        Err(e) => Err(e),
                    }
                }
                Err(e) => Err(e),
            }
        }
        "list_workers" => {
            let mut reg = registry.lock();
            let workers: Vec<_> = reg
                .list_workers()
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "workerId": w.worker_id,
                        "sessionId": w.session_id,
                        "project": w.project,
                        "status": format!("{}", w.status),
                        "model": w.model,
                        "agent": w.agent,
                        "parent": w.parent,
                        "channels": w.channels,
                    })
                })
                .collect();
            Ok(serde_json::json!({"workers": workers}))
        }
        // 对外 API：列 sessions（不暴露 worker_id）
        "list_sessions" => {
            let mut reg = registry.lock();
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
            let mut index = ion::session_index::SessionIndex::load();
            // 懒修复：messageCount=0 但有轮次的会话，用 FileIndex 快速重算
            let mut healed = 0;
            let ids_to_heal: Vec<String> = index
                .sessions
                .iter()
                .filter(|(_, m)| m.message_count == 0 && m.turn_count > 0)
                .map(|(id, _)| id.clone())
                .collect();
            for sid in ids_to_heal {
                if let Some(path) = resolve_session_path(&sid)
                    && let Some(idx) = get_file_index(&path)
                {
                    let real_count = idx.live_total as u32;
                    if real_count > 0 {
                        // 直接改内存实例（不用 patch_meta——它内部独立 load/save，
                        // 会被外层 index.save() 用旧数据覆盖）
                        if let Some(m) = index.sessions.get_mut(&sid) {
                            m.message_count = real_count;
                        }
                        healed += 1;
                    }
                }
            }
            if healed > 0 {
                index.save();
                tracing::info!("[list_all_sessions] healed messageCount for {} sessions", healed);
            }
            let sessions: Vec<_> = index
                .sessions
                .iter()
                .map(|(id, m)| {
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
                })
                .collect();
            Ok(serde_json::json!({"sessions": sessions, "totalCount": sessions.len()}))
        }
        // 对外 API：搜索 session（标题匹配 + 可选内容搜索）
        // params: { query: string, searchContent?: bool, limit?: number }
        "search_sessions" => {
            let source = if cmd.get("params").map(|v| v.is_object()).unwrap_or(false) {
                cmd.get("params").cloned().unwrap_or_default()
            } else {
                cmd.clone()
            };
            let query = source.get("query").and_then(|v| v.as_str()).unwrap_or("").to_lowercase();
            let search_content = source.get("searchContent").and_then(|v| v.as_bool()).unwrap_or(false);
            let limit = source.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
            if query.is_empty() { return Err("missing 'query'".to_string()); }
            let index = ion::session_index::SessionIndex::load();
            let mut results: Vec<serde_json::Value> = Vec::new();
            for (id, meta) in &index.sessions {
                let title_match = meta.name.as_ref().map_or(false, |n| n.to_lowercase().contains(&query))
                    || meta.first_name.as_ref().map_or(false, |n| n.to_lowercase().contains(&query))
                    || meta.project.as_ref().map_or(false, |p| p.to_lowercase().contains(&query));
                let mut content_matches: Vec<String> = Vec::new();
                let mut total_hits = 0u64;
                if search_content {
                    // 逐 session 目录查找（不依赖 cwd 字段，直接扫 sessions_dir）
                    let sdir = ion::paths::sessions_dir();
                    if let Ok(entries) = std::fs::read_dir(&sdir) {
                        for entry in entries.flatten() {
                            let f = entry.path().join(format!("{id}.jsonl"));
                            if f.exists() {
                                if let Ok(content) = std::fs::read_to_string(&f) {
                                    for line in content.lines() {
                                        if line.to_lowercase().contains(&query) {
                                            total_hits += 1;
                                            if content_matches.len() < 3 {
                                                if let Some(pos) = line.to_lowercase().find(&query) {
                                                    let s = pos.saturating_sub(30);
                                                    let e = (pos + query.len() + 30).min(line.len());
                                                    content_matches.push(line[s..e].to_string());
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            if total_hits > 0 { break; }
                        }
                    }
                }
                if title_match || total_hits > 0 {
                    results.push(serde_json::json!({
                        "id": id,
                        "name": meta.name.as_deref().unwrap_or(""),
                        "firstMessage": meta.first_name.as_deref().unwrap_or(""),
                        "project": meta.project.as_deref().unwrap_or(""),
                        "model": meta.model, "messageCount": meta.message_count,
                        "updatedAt": meta.updated_at,
                        "matchType": if title_match {"title"} else {"content"},
                        "contentHits": total_hits, "snippets": content_matches,
                    }));
                    if results.len() >= limit { break; }
                }
            }
            results.sort_by(|a, b| {
                let at = a["matchType"].as_str() == Some("title");
                let bt = b["matchType"].as_str() == Some("title");
                bt.cmp(&at)
                    .then(b["contentHits"].as_u64().cmp(&a["contentHits"].as_u64()))
                    .then(b["updatedAt"].as_u64().cmp(&a["updatedAt"].as_u64()))
            });
            Ok(serde_json::json!({"results": results, "totalMatches": results.len(), "query": query, "searchContent": search_content}))
        }
        // 对外 API：跨会话 Token 用量统计（从 SessionIndex 聚合）
        "token_usage_summary" => {
            let index = ion::session_index::SessionIndex::load();
            let mut total_input: u64 = 0;
            let mut total_output: u64 = 0;
            let mut by_model: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
            let mut by_project: std::collections::HashMap<String, (u64, u64)> = std::collections::HashMap::new();
            let mut session_count = 0u64;
            for (_id, m) in &index.sessions {
                session_count += 1;
                total_input += m.token_input;
                total_output += m.token_output;
                let mi = m.token_input;
                let mo = m.token_output;
                let model_key = m.model.clone();
                let e = by_model.entry(model_key).or_insert((0, 0));
                e.0 += mi; e.1 += mo;
                let proj = m.project.clone().unwrap_or_else(|| "unknown".to_string());
                let e2 = by_project.entry(proj).or_insert((0, 0));
                e2.0 += mi; e2.1 += mo;
            }
            Ok(serde_json::json!({
                "sessions": session_count,
                "totalInput": total_input,
                "totalOutput": total_output,
                "totalTokens": total_input + total_output,
                "byModel": by_model.into_iter().map(|(k,(i,o))| serde_json::json!({
                    "model": k, "input": i, "output": o, "total": i + o
                })).collect::<Vec<_>>(),
                "byProject": by_project.into_iter().map(|(k,(i,o))| serde_json::json!({
                    "project": k, "input": i, "output": o, "total": i + o
                })).collect::<Vec<_>>(),
            }))
        }
        // 对外 API：创建 session（自动 spawn worker，返回 session_id）
        "create_session" => {
            // 兼容嵌套格式（RPC client）和扁平（stdin）
            let source = if cmd.get("params").map(|v| v.is_object()).unwrap_or(false) {
                cmd.get("params").cloned().unwrap_or_default()
            } else {
                cmd.clone()
            };
            let agent = source
                .get("agent")
                .and_then(|v| v.as_str())
                .unwrap_or("build")
                .to_string();
            // ⚠️ parking_lot: do_create_session 内部会 lock，不持外层锁。
            match do_create_session(&registry, &source).await {
                Ok(session_id) => Ok(serde_json::json!({
                    "session_id": session_id,
                    "agent": agent,
                    "status": "created",
                })),
                Err(e) => Err(e),
            }
        }
        "get_overview" => Ok(registry.lock().get_overview()),
        "get_session_snapshot" => {
            let source = if cmd.get("params").map(|v| v.is_object()).unwrap_or(false) {
                cmd.get("params").cloned().unwrap_or_default()
            } else {
                cmd.clone()
            };
            do_get_session_snapshot(&registry, &source).await
        }
        "send" | "send_to_session" => {
            let session = cmd.get("session").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let rpc_method = cmd
                .get("rpc_method")
                .and_then(|v| v.as_str())
                .or_else(|| cmd.get("method").and_then(|v| v.as_str()))
                .unwrap_or("get_state")
                .to_string();
            let params = cmd.get("params").cloned().unwrap_or(serde_json::json!({}));
            // ⚠️ parking_lot: 检查 session 存在性 + 后续 send_to_session/do_create_session 都有 .await。
            // 先在独立 block 内短锁查 exists，drop reg 后再 await。
            let exists = {
                let reg = registry.lock();
                reg.workers.values().any(|w| w.session_id == session)
            }; // reg dropped here
            if exists {
                ion::worker_registry::WorkerRegistry::send_to_session(
                    &registry, &session, &rpc_method, params,
                )
                .await
            } else {
                tracing::info!("[send_to_session] session {session} not found, auto-creating");
                match do_create_session(
                    &registry,
                    &serde_json::json!({
                        "session_id": session,
                        "agent": "build",
                    }),
                )
                .await
                {
                    Ok(_) => {
                        // 创建后立即转发原请求（关联函数，内部自己 lock）
                        ion::worker_registry::WorkerRegistry::send_to_session(
                            &registry, &session, &rpc_method, params,
                        )
                        .await
                    }
                    Err(e) => Err(e),
                }
            }
        }
        "send_to_worker" => {
            let worker_id = cmd.get("workerId").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let rpc_method = cmd
                .get("rpc_method")
                .and_then(|v| v.as_str())
                .unwrap_or("get_state")
                .to_string();
            let params = cmd.get("params").cloned().unwrap_or(serde_json::json!({}));
            // ⚠️ parking_lot: send_command 持 &mut self + .await，改用 send_async（自管锁）。
            ion::worker_registry::WorkerRegistry::send_async(
                registry, &worker_id, &rpc_method, params,
            )
            .await
            .map(|_| serde_json::json!({"queued": true}))
        }
        "kill" | "kill_worker" => {
            // 兼容嵌套 params（RPC client）与扁平（stdin）两种格式
            let source = if cmd.get("params").map(|v| v.is_object()).unwrap_or(false) {
                cmd.get("params").cloned().unwrap_or_default()
            } else {
                cmd.clone()
            };
            let target = source
                .get("workerId")
                .and_then(|v| v.as_str())
                .or_else(|| source.get("target").and_then(|v| v.as_str()))
                .or_else(|| cmd.get("workerId").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string();
            // worktree 清理策略：默认删目录留分支（对齐 workspace 语义）
            let cleanup_worktree = source
                .get("cleanupWorktree")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let delete_branch = source
                .get("deleteBranch")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            registry
                .lock()
                .kill_worker_inner(&target, cleanup_worktree, delete_branch)
                .map(|_| serde_json::json!({"killed": true, "cleanupWorktree": cleanup_worktree, "deleteBranch": delete_branch}))
        }
        "reap_workers" | "gc_workers" => {
            // 手动清理 Dead + 超时 Stale worker，返回清理数量。
            // maxAgeSecs 控制 Stale 的清理年龄阈值（默认 600s = 10 分钟）；Dead 一律清。
            let max_age = cmd
                .get("maxAgeSecs")
                .and_then(|v| v.as_u64())
                .unwrap_or(600);
            let n = registry.lock().reap_workers(max_age);
            tracing::info!("[gc] reaped {} workers via RPC", n);
            Ok(serde_json::json!({"reaped": n}))
        }
        "channel_send" => {
            let channel = cmd
                .get("channel")
                .and_then(|v| v.as_str())
                .unwrap_or("main")
                .to_string();
            let from = cmd
                .get("from")
                .and_then(|v| v.as_str())
                .unwrap_or("manager")
                .to_string();
            let msg = cmd.get("msg").cloned().unwrap_or(serde_json::json!({}));
            // ⚠️ parking_lot: channel_send_arc 自管锁。
            ion::worker_registry::WorkerRegistry::channel_send_arc(registry, &channel, &from, msg).await;
            Ok(serde_json::json!({"sent": true}))
        }
        "channel_subscribe" => {
            let channel = cmd
                .get("channel")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let worker_id = cmd
                .get("workerId")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut reg = registry.lock();
            if let Some(record) = reg.workers.get_mut(&worker_id) {
                if !record.channels.contains(&channel) {
                    record.channels.push(channel.clone());
                }
                reg.channels
                    .entry(channel)
                    .or_default()
                    .push(worker_id.clone());
                Ok(serde_json::json!({"subscribed": true}))
            } else {
                Err("worker not found".into())
            }
        }
        "stats" => Ok(serde_json::json!({"workers": registry.lock().list_workers().len()})),
        "health" => {
            // Manager-level health check for watchdog.sh.
            // Returns immediately (<10ms): no DB, no network.
            Ok(serde_json::json!({
                "status": "ok",
                "workers": registry.lock().list_workers().len(),
                "version": env!("CARGO_PKG_VERSION"),
            }))
        }
        "request_restart" => {
            // Write sentinel file so watchdog.sh can detect and do safe upgrade.
            let restart_file = "/tmp/.ion-evolve-restart";
            match std::fs::write(
                restart_file,
                format!(
                    "{} pid={}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                    std::process::id(),
                ),
            ) {
                Ok(_) => {
                    eprintln!("[restart] Sentinel file written: {}", restart_file);
                    Ok(serde_json::json!({"notified": true, "file": restart_file}))
                }
                Err(e) => Err(format!("Failed to write restart sentinel: {}", e)),
            }
        }
        "extension_rpc" => {
            // 单例扩展的 extension_rpc：直接从 SingletonRegistry 调
            let params = cmd.get("params").cloned().unwrap_or_default();
            let extension = params
                .get("extension")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let method = params.get("method").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("args").cloned().unwrap_or_default();
            // ⚠️ parking_lot: 不持外层锁，各分支按需 lock + 释放。
            // singleton extension: 短锁取 instance Arc，drop lock 后调 on_extension_rpc（async）。
            let singleton_instance = {
                let reg = registry.lock();
                reg.singletons.get(extension).map(|e| e.instance.clone())
            };
            if let Some(instance) = singleton_instance {
                match instance.on_extension_rpc(method, args).await {
                    Ok(val) => Ok(val),
                    Err(e) => Err(format!("{:?}", e)),
                }
            } else if extension == "permission" {
                // PermissionExtension is a Worker-level extension, not a singleton.
                // Return a helpful message directing the user to use --session flag.
                Err(format!(
                    "permission is a Worker-level extension. Use: ion rpc --session <SID> --method extension_rpc --params '{{\"extension\":\"permission\",\"method\":\"list_rules\"}}'"
                ))
            } else if extension == "lsp"
                || extension == "memory"
                || extension == "bash"
                || extension == "streaming"
                || extension == "context-index"
                || extension == "file-time-guard"
                || extension == "plan"
            {
                // Worker-level extensions: forward to session's worker
                let session_id = cmd.get("session").and_then(|v| v.as_str());
                if let Some(sid) = session_id {
                    // Forward to worker via send_to_session (associated fn, locks internally)
                    ion::worker_registry::WorkerRegistry::send_to_session(
                        &registry,
                        sid,
                        "extension_rpc",
                        serde_json::json!({
                            "extension": extension,
                            "method": method,
                            "args": args,
                        }),
                    )
                    .await
                } else {
                    Err(format!(
                        "{} is a Worker-level extension. Use: ion rpc --session <SID> --method extension_rpc --params '{{\"extension\":\"{}\",\"method\":\"{}\"}}'",
                        extension, extension, method
                    ))
                }
            } else {
                Err(format!("singleton extension '{}' not found", extension))
            }
        }
        _ => {
            // 只读命令（get_messages / list_turns）走磁盘直读，不 auto-create worker——
            // 读历史会话不应该拉起一个进程（首次冷启动几百 ms + 24MB 常驻）。
            // JSONL append-only 保证读到截至目前的完整内容。session 兼容顶层和 params 两种位置。
            if matches!(
                method,
                "get_messages" | "list_turns" | "get_session_messages" | "list_session_turns"
            ) {
                let p = cmd.get("params");
                let has_session = cmd.get("session").is_some()
                    || p.and_then(|p| p.get("session")).is_some()
                    || p.and_then(|p| p.get("session_id")).is_some();
                if has_session {
                    return host_direct_session_read(
                        &cmd,
                        if matches!(method, "list_turns" | "list_session_turns") {
                            "turns"
                        } else {
                            "messages"
                        },
                    );
                }
            }
            // list_inputs / get_turn_detail：同为纯函数检索，host 直读磁盘
            // （此前无拦截 → 浏览历史会话点大纲会 auto-create worker）
            if matches!(method, "list_inputs" | "get_turn_detail") {
                let p = cmd.get("params");
                let sid = cmd
                    .get("session")
                    .and_then(|v| v.as_str())
                    .or_else(|| p.and_then(|p| p.get("session")).and_then(|v| v.as_str()))
                    .or_else(|| {
                        p.and_then(|p| p.get("session_id")).and_then(|v| v.as_str())
                    })
                    .map(|s| s.to_string());
                if let Some(sid) = sid {
                    if let Some(entries) = load_session_entries(&sid) {
                        if method == "list_inputs" {
                            let r =
                                ion::message_retrieval::retrieve_inputs(&entries, &Default::default());
                            let inputs: Vec<_> = r
                                .inputs
                                .iter()
                                .map(|i| {
                                    serde_json::json!({"turnId": i.turn_id, "entryId": i.entry_id, "text": i.text})
                                })
                                .collect();
                            return Ok(serde_json::json!({
                                "inputs": inputs, "hasMore": r.has_more,
                                "totalCount": r.total_count, "nextCursor": r.next_cursor,
                            }));
                        }
                        let turn_id =
                            p.and_then(|p| p.get("turnId")).and_then(|v| v.as_str()).unwrap_or("");
                        return match ion::message_retrieval::retrieve_turn_detail(
                            &entries,
                            turn_id,
                            &ion::message_retrieval::CustomFilter::None,
                        ) {
                            Some(d) => Ok(serde_json::json!({
                                "turnId": d.turn_id, "entries": d.entries,
                                "overview": {
                                    "userContent": d.overview.user_content,
                                    "assistantContent": d.overview.assistant_content,
                                    "keySteps": d.overview.key_steps,
                                    "toolCallCount": d.overview.tool_call_count,
                                    "tokens": {"input": d.overview.tokens_input, "output": d.overview.tokens_output},
                                    "status": d.overview.status, "durationMs": d.overview.duration_ms,
                                    "source": d.overview.source,
                                },
                            })),
                            None => Ok(serde_json::json!({"error": "turn not found", "turnId": turn_id})),
                        };
                    }
                    return Err(format!("session not found on disk: {sid}"));
                }
            }
            // 只读状态类命令：session 无活跃 worker 时合成空闲态响应，不 auto-create；
            // 有活跃 worker 则照旧转发拿真实运行态（is_running/队列/实时工具）
            if matches!(
                method,
                "get_session_info"
                    | "get_settings"
                    | "get_queue"
                    | "get_context_usage"
                    | "get_active_tools"
            ) {
                let p = cmd.get("params");
                let sid = cmd
                    .get("session")
                    .and_then(|v| v.as_str())
                    .or_else(|| p.and_then(|p| p.get("session")).and_then(|v| v.as_str()))
                    .or_else(|| {
                        p.and_then(|p| p.get("session_id")).and_then(|v| v.as_str())
                    });
                if let Some(sid) = sid {
                    let has_live = {
                        let reg = registry.lock();
                        reg.workers.values().any(|w| w.session_id == sid)
                    };
                    if !has_live {
                        return host_idle_session_read(&cmd, method);
                    }
                }
            }
            // 默认分支：如果 cmd 里有 session 字段，转发到对应 worker
            let session_id = cmd.get("session").and_then(|v| v.as_str());
            if let Some(sid) = session_id {
                let sid = sid.to_string();
                let params = cmd.get("params").cloned().unwrap_or_default();

                // 检查 session 是否存在，不存在则自动创建（修复 #2 的另一条路径）
                // 对齐 pi：pi 用 SessionManager 隐式管理，永远有 session
                // ⚠️ parking_lot: 短锁查 exists，drop 后再 await。
                let exists = {
                    let reg = registry.lock();
                    reg.workers.values().any(|w| w.session_id == sid)
                };
                if !exists {
                    tracing::info!("[forward] session {sid} not found, auto-creating");
                    // project_path 兜底：从 SessionIndex 取原项目（否则 fallback 到
                    // host 进程 cwd，消息会写进错误项目目录的同名文件）
                    let project = ion::session_index::SessionIndex::load()
                        .get(&sid)
                        .and_then(|m| m.project.clone());
                    let mut create = serde_json::json!({
                        "session_id": sid,
                        "agent": "build",
                    });
                    if let Some(p) = project {
                        create["project_path"] = serde_json::json!(p);
                    }
                    if let Err(e) = do_create_session(&registry, &create).await {
                        return Err(format!("auto-create session failed: {e}"));
                    }
                }

                // prompt/abort/steer 用 fire-and-forget(不等 oneshot)——
                // agent.run / bash sleep 会阻塞 worker 主循环很久,如果等 oneshot,
                // Manager 锁不释放,后续命令(如 abort)进不来。
                // 这些命令的 worker handler 会在 agent.run 前立刻 output_response(null)
                if method == "prompt" || method == "abort" || method == "steer" {
                    // ⚠️ parking_lot: send_command 持 &mut self + .await，改用 send_async（自管锁）。
                    ion::worker_registry::WorkerRegistry::send_async(
                        &registry, &sid, method, params,
                    )
                    .await
                    .map(|_| serde_json::json!({"status": "forwarded", "session": sid}))
                } else {
                    // 其他命令等响应(list_turns/get_messages/abort 等)
                    match ion::worker_registry::WorkerRegistry::send_to_session(
                        &registry, &sid, method, params,
                    )
                    .await
                    {
                        Ok(_) => Ok(serde_json::json!({"status": "forwarded", "session": sid})),
                        Err(e) => Err(e),
                    }
                }
            } else {
                Err(format!(
                    "unknown method: {method} (and no `session` field for forwarding)"
                ))
            }
        }
    };

    // Return Result; the caller (handle_manager_command) wraps it into a response.
    result
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

async fn cmd_host(user_message: &str, agent_name: Option<&str>, export_path: Option<&str>) {
    use ion::worker_registry::{WorkerCreateConfig, WorkerRegistry};
    use std::sync::Arc;
    use parking_lot::Mutex;

    let ion_cfg = ion::config::IonConfig::load();
    let model = ion_cfg
        .default_model
        .clone()
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());
    let provider = ion_cfg
        .default_provider
        .clone()
        .unwrap_or_else(|| "opencode".to_string());
    let agent = agent_name.unwrap_or("build").to_string();

    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    eprintln!("[host] Starting WorkerRegistry");

    let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    registry.lock().set_self_ref(&registry);

    // 1. Event pump → stdout
    let pump_registry = Arc::clone(&registry);
    eprintln!("[pump] spawning...");
    tokio::spawn(async move {
        let mut subs: std::collections::HashMap<
            String,
            tokio::sync::mpsc::Receiver<serde_json::Value>,
        > = std::collections::HashMap::new();
        // Per-worker line buffer: accumulate text_delta, flush on newline / agent_end
        let mut line_bufs: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        loop {
            {
                let mut reg = pump_registry.lock();
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
                                if delta.is_empty() {
                                    continue;
                                }
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
    // ⚠️ parking_lot: process_pending_commands 持 &mut self + .await，guard 不是 Send。
    // 在独立线程 + 单线程 runtime + LocalSet 里跑（spawn_local 不要求 Send）。
    let cmd_registry = Arc::clone(&registry);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to build cmd-loop runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            loop {
                {
                    let mut reg = cmd_registry.lock();
                    reg.process_pending_commands(&cmd_registry).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
        });
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
        let mut reg = registry.lock();

        // ── 注册单例扩展（scene 2 也需要，跟 cmd_serve_start 一致）──
        // 否则 scheduler agent 通过 extension_rpc 调 monitor validate/add 会失败。
        // 同时注入 EventBus（虽然在 scene 2 没 socket subscriber，但保持单例行为一致）。
        let host_event_bus = Arc::new(tokio::sync::Mutex::new(
            ion::event_bus::ExtensionEventBus::new(),
        ));
        reg.set_event_bus(host_event_bus);
        reg.register_singleton(Box::new(
            ion::global_memory_ext::GlobalMemoryExtension::new(),
        ));
        reg.register_singleton(Box::new(ion::monitor_extension::MonitorExtension::new()));
        reg.register_singleton(Box::new(ion::rules_engine::RulesEngineExtension::new()));
        drop(reg); // ⚠️ parking_lot: init_singletons / create_worker 内部有 .await，不能持锁
        WorkerRegistry::init_singletons_arc(&registry).await;

        // ⚠️ create_worker 持 &mut self + .await → 改用 prepare + register 两阶段。
        match ion::worker_registry::WorkerRegistry::prepare_worker_spawn(&cfg).await {
            Ok(prepared) => match registry.lock().register_prepared_worker(prepared, &cfg, &registry) {
                Ok(info) => {
                    eprintln!("[host] spawned {} ({})", &info.worker_id[..12], agent);
                    info
                }
                Err(e) => {
                    eprintln!("[host] ❌ Failed to spawn worker: {e}");
                    return;
                }
            },
            Err(e) => {
                eprintln!("[host] ❌ Failed to prepare spawn: {e}");
                return;
            }
        }
    };

    // Set entry worker for recursive idle detection
    registry.lock().set_entry_worker(&entry.worker_id);

    // 启动单例扩展的后初始化（关键：让 Monitor interval loop 真的跑起来）
    // 否则 monitor 配置加载了但不会触发，因为 on_singleton_post_init 没被调用。
    ion::worker_registry::WorkerRegistry::post_init_singletons(&registry).await;

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
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1800);
    let mut first_idle_at: Option<std::time::Instant> = None;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let all_idle = {
            let reg = registry.lock();
            match reg.entry_worker_id.as_ref() {
                Some(eid) => reg.all_workers_idle(eid).unwrap_or(false),
                None => true,
            }
        };

        if all_idle {
            // 首次进入 idle 状态，记录时间
            if first_idle_at.is_none() {
                first_idle_at = Some(std::time::Instant::now());
                eprintln!(
                    "[host] workers idle, waiting {idle_grace_secs}s grace period before cleanup..."
                );
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
        let mut reg = registry.lock();
        let wids: Vec<String> = reg.workers.keys().cloned().collect();
        for wid in &wids {
            // 发 shutdown 命令（ion_worker 收到后 break 主循环 → 执行退出前 save）
            let _ = reg
                .send_command(wid, "shutdown", serde_json::json!({}))
                .await;
        }
    }
    // 给 Worker 时间执行退出前 save_worker_session
    tokio::time::sleep(std::time::Duration::from_millis(2000)).await;
    eprintln!("[host] cleanup complete");

    // ── Export after host run (if --export was given alongside --host) ──
    // Mirrors cmd_run's export-after-run behavior: after the host finishes,
    // export the entry worker's session to HTML. Without this, the combination
    // `--host --export <path>` silently dropped the export request.
    //
    // entry.session_id is the entry worker's session (set by create_worker from
    // cfg.session or the cwd-hash default). export_session_rich rebuilds the
    // tools list from the agent config (see src/export.rs).
    if let Some(path) = export_path {
        match ion::export::export_session_rich(&entry.session_id, std::path::Path::new(path)) {
            Ok(()) => println!("Exported to {path}"),
            Err(e) => {
                eprintln!("Export failed: {e}");
                std::process::exit(1);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Session management (pi JSONL v3)
// ---------------------------------------------------------------------------

/// Append missing messages and preserve the current session branch without
/// touching SessionIndex counters. Used by incremental checkpoints.
fn save_session_messages(id: &str, messages: &[ion::agent::messages::Message]) {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    // 读已有文件，判断已写入的 message 数量 + 当前 leaf（光标）
    // Honor the per-run override (session isolation) if set, else legacy session.jsonl.
    let path = ion::session_jsonl::resolve_session_file(&cwd);
    let mut existing_entries: Vec<serde_json::Value> = Vec::new();
    let mut header_existed = false;
    if let Ok(content) = std::fs::read_to_string(&path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
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
            entry_type: "session".into(),
            version: 3,
            id: id.to_string(),
            timestamp: ion::session_jsonl::timestamp_iso(),
            cwd: cwd.clone(),
            parent_session: None,
            agent: std::env::var("ION_SESSION_AGENT").ok(),
            model: std::env::var("ION_SESSION_MODEL").ok(),
            provider: std::env::var("ION_SESSION_PROVIDER").ok(),
        };
        if let Ok(h) = serde_json::to_string(&header) {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
            {
                let _ = writeln!(f, "{}", h);
            }
        }
    }

    // 统计已有 message 数。
    //
    // 注意：磁盘上的 message 总数（包括被回滚的）≠ live message 数。
    // 用 live count 比较，避免回滚后误判"已存过"跳过新消息（issue #28）。
    let saved_msg_count = existing_entries
        .iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("message"))
        .count();
    let live_msg_count = ion::worker_rpc::count_live_messages(&existing_entries);

    // 优先用 live count 决定新增范围；如果 messages.len() 比 live 还短（罕见，理论上不该
    // 发生），把全部 messages 当新消息追加。
    let new_msgs: &[ion::agent::messages::Message] = if messages.len() > live_msg_count {
        &messages[live_msg_count..]
    } else if messages.len() < live_msg_count {
        // ROLLBACK CASE: agent.messages 比 live 短 — 全部当新消息追加
        eprintln!(
            "[save-debug] ROLLBACK CASE in save_session: msgs={} < live={} (saved_total={})",
            messages.len(),
            live_msg_count,
            saved_msg_count
        );
        messages
    } else {
        &[][..]
    };

    if !new_msgs.is_empty() {
        // parentId 从 resolve_current_leaf 取（leaf 感知，对齐 Session Tree）
        let parent_id = ion::session_tree::resolve_current_leaf(&existing_entries)
            .unwrap_or_else(|| id.to_string());
        let mut parent_id = parent_id;
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            if let Ok(meta) = f.metadata() {
                if meta.len() > 0 {
                    let _ = write!(f, "\n");
                }
            }
            for msg in new_msgs {
                let entry = ion::session_jsonl::message_to_entry(msg, &parent_id);
                if let Some(eid) = entry["id"].as_str() {
                    parent_id = eid.to_string();
                }
                let _ = writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default());
            }
        }
    }

    let _ = std::fs::write(ion::session_jsonl::last_session_path(), id);
}

/// Final session save: persist any remaining messages, then update the index
/// exactly once with the complete conversation totals.
fn save_session(
    id: &str,
    messages: &[ion::agent::messages::Message],
    model: &str,
    provider: &str,
    name: Option<&str>,
) {
    save_session_messages(id, messages);
    let total_input: u64 = messages
        .iter()
        .filter_map(|m| match m {
            ion::agent::messages::Message::Assistant(a) => Some(a.usage.input),
            _ => None,
        })
        .sum();
    let total_output: u64 = messages
        .iter()
        .filter_map(|m| match m {
            ion::agent::messages::Message::Assistant(a) => Some(a.usage.output),
            _ => None,
        })
        .sum();
    let total_cache_read: u64 = messages
        .iter()
        .filter_map(|m| match m {
            ion::agent::messages::Message::Assistant(a) => Some(a.usage.cache_read),
            _ => None,
        })
        .sum();
    let total_cache_write: u64 = messages
        .iter()
        .filter_map(|m| match m {
            ion::agent::messages::Message::Assistant(a) => Some(a.usage.cache_write),
            _ => None,
        })
        .sum();
    let user_prompt_count = messages
        .iter()
        .filter(|m| {
            matches!(
                m,
                ion::agent::messages::Message::User(u)
                    if u.source == ion_provider::types::MessageSource::Prompt
            )
        })
        .count() as u32;
    let user_message_count = messages
        .iter()
        .filter(|m| matches!(m, ion::agent::messages::Message::User(_)))
        .count() as u32;
    let assistant_count = messages
        .iter()
        .filter(|m| matches!(m, ion::agent::messages::Message::Assistant(_)))
        .count() as u32;
    let agent_name = std::env::var("ION_SESSION_AGENT").unwrap_or_else(|_| "default".into());

    // `SessionIndex::update` is additive and is appropriate for deltas. Here
    // `messages` is the complete live conversation, so write absolute totals;
    // otherwise repeated checkpoints/final saves inflate the export header.
    ion::session_index::SessionIndex::patch_meta(id, |meta| {
        meta.model = model.to_string();
        meta.provider = provider.to_string();
        meta.agent = agent_name;
        if let Some(session_name) = name {
            if meta.first_name.is_none() {
                meta.first_name = Some(session_name.to_string());
            }
            meta.name = Some(session_name.to_string());
        }
        meta.token_input = total_input;
        meta.token_output = total_output;
        meta.token_cache_read = total_cache_read;
        meta.token_cache_write = total_cache_write;
        meta.user_prompt_count = user_prompt_count;
        meta.llm_request_count = assistant_count;
        meta.message_count = messages.len() as u32;
        // A conversational turn is rooted at a real user message. Tool loops
        // may create multiple Assistant messages without creating more turns.
        meta.turn_count = user_message_count;
    });
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

    // Strategy 3: Per-run isolated file sessions/<cwd_dir>/<id>.jsonl.
    // After session isolation, each run writes its own <sid>.jsonl; scan all
    // cwd subdirs for an exact filename match. (export.rs uses the same scan.)
    {
        let target_name = format!("{id}.jsonl");
        let sessions_dir = ion::paths::sessions_dir();
        if let Ok(cwd_dirs) = std::fs::read_dir(&sessions_dir) {
            for entry in cwd_dirs.flatten() {
                let candidate = entry.path().join(&target_name);
                if candidate.exists() {
                    if let Ok(content) = std::fs::read_to_string(&candidate) {
                        return parse_jsonl_messages(&content);
                    }
                }
            }
        }
    }

    // Strategy 4: Treat id as cwd path (encoded)
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
                    if let Ok(msg) =
                        serde_json::from_value::<ion::agent::messages::Message>(msg_val.clone())
                    {
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
    if cli.no_session {
        return (String::new(), None);
    }
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
    let matches: Vec<String> = index
        .sessions
        .keys()
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
///
/// Scans ALL `*.jsonl` files under each cwd subdir (both legacy `session.jsonl`
/// and per-run `<sid>.jsonl`), so `--continue` works across the isolation
/// change: it rediscovers the most recently modified session regardless of
/// which naming scheme it used.
fn find_most_recent_session() -> Option<(String, Vec<ion::agent::messages::Message>)> {
    let sessions_dir = ion::paths::sessions_dir();
    let mut candidates: Vec<(std::path::PathBuf, std::time::SystemTime)> = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let cwd_dir = entry.path();
            if !cwd_dir.is_dir() {
                continue;
            }
            // Scan every .jsonl in this cwd's session dir (session.jsonl + <sid>.jsonl).
            if let Ok(files) = std::fs::read_dir(&cwd_dir) {
                for f in files.flatten() {
                    let fp = f.path();
                    if fp.extension().is_some_and(|e| e == "jsonl") {
                        if let Ok(mtime) = fp.metadata().and_then(|m| m.modified()) {
                            candidates.push((fp, mtime));
                        }
                    }
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
                    if !t.text.is_empty() {
                        return Some(t.text.clone());
                    }
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

struct CmdRunSessionPersistenceExtension {
    session_id: String,
}

impl CmdRunSessionPersistenceExtension {
    fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
        }
    }
}

#[async_trait::async_trait]
impl Extension for CmdRunSessionPersistenceExtension {
    fn name(&self) -> &str {
        "cmd-run-session-persistence"
    }

    async fn on_before_tool_execute(
        &self,
        _tool_name: &str,
        _args: &serde_json::Value,
        messages: &[ion::agent::messages::Message],
    ) -> ion::agent::error::AgentResult<()> {
        save_session_messages(&self.session_id, messages);
        Ok(())
    }

    async fn on_turn_end(
        &self,
        ctx: &ion::agent::extension::TurnContext,
    ) -> ion::agent::error::AgentResult<()> {
        // Persist the complete user/assistant/tool-result chain before later
        // lifecycle extensions append parented custom entries such as
        // step-snapshot. Registry hooks run in registration order.
        save_session_messages(&self.session_id, &ctx.messages);
        Ok(())
    }
}

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
    async fn on_turn_end(
        &self,
        ctx: &ion::agent::extension::TurnContext,
    ) -> ion::agent::error::AgentResult<()> {
        if self.session_id.is_empty() {
            return Ok(());
        }
        // The message tree is a current snapshot. User/LLM/token/duration
        // counters are incremented once by Agent::run/record_llm_stats.
        ion::session_index::SessionIndex::sync_message_tree(
            &self.session_id,
            &self.model,
            &self.provider,
            "default",
            ctx.messages.len() as u32,
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
        let ion_bin = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("ion"));
        // Host 的 stdout/stderr 都重定向到日志文件，不污染 TUI
        let mgr_log = ion::paths::root().join("host.log");
        let mgr_out = std::fs::File::create(&mgr_log).ok();
        match Command::new(&ion_bin)
            .arg("serve")
            .arg("start")
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
    let dashboard_dir = candidates
        .iter()
        .find(|p| p.join("src/index.ts").exists() || p.join("src/index.tsx").exists())
        .cloned()
        .unwrap_or_else(|| candidates[0].clone());

    if !dashboard_dir.join("src/index.ts").exists() && !dashboard_dir.join("src/index.tsx").exists()
    {
        eprintln!("[ion] Dashboard not found at {}", dashboard_dir.display());
        return;
    }

    let entry_file = if dashboard_dir.join("src/index.tsx").exists() {
        "src/index.tsx"
    } else {
        "src/index.ts"
    };

    // 3. 检查 node_modules，没有就 bun install
    if !dashboard_dir.join("node_modules").exists() {
        eprintln!("[ion] Installing dashboard dependencies...");
        let _ = Command::new("bun")
            .arg("install")
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
        let cli =
            Cli::try_parse_from(["ion", "--output-schema", r#"{"type":"object"}"#, "hi"]).unwrap();
        assert_eq!(cli.json_schema, Some(r#"{"type":"object"}"#.into()));
    }

    #[test]
    fn test_json_schema_still_works() {
        let cli =
            Cli::try_parse_from(["ion", "--json-schema", r#"{"type":"object"}"#, "hi"]).unwrap();
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
        let cli =
            Cli::try_parse_from(["ion", "--model", "opencode/deepseek-v4-flash", "hi"]).unwrap();
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
        let cli = Cli::try_parse_from(["ion", "--model", "opencode/deepseek-v4-flash:high", "hi"])
            .unwrap();
        assert_eq!(cli.model, Some("opencode/deepseek-v4-flash:high".into()));
    }

    #[test]
    fn test_model_thinking_takes_precedence() {
        // --thinking flag should override :thinking suffix
        let cli = Cli::try_parse_from([
            "ion",
            "--model",
            "deepseek-v4-flash:high",
            "--thinking",
            "low",
            "hi",
        ])
        .unwrap();
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
        assert!(matches!(
            cli.command,
            Some(Commands::Config {
                action: ConfigAction::List
            })
        ));
    }

    // ── ion extension create ──
    #[test]
    fn extension_name_requires_lower_kebab_case() {
        assert!(validate_extension_name("hello-extension").is_ok());
        assert!(validate_extension_name("extension2").is_ok());
        assert!(validate_extension_name("").is_err());
        assert!(validate_extension_name("Hello").is_err());
        assert!(validate_extension_name("../escape").is_err());
        assert!(validate_extension_name("hello_extension").is_err());
        assert!(validate_extension_name("hello--extension").is_err());
        assert!(validate_extension_name("hello-").is_err());
    }

    #[test]
    fn extension_scaffold_matches_wasm_abi_v1() {
        let (cargo_toml, lib_rs, manual) = extension_scaffold("hello-extension");

        assert!(cargo_toml.contains("crate-type = [\"cdylib\"]"));
        assert!(cargo_toml.contains("[workspace]"));
        assert!(lib_rs.contains("#![no_std]"));
        assert!(lib_rs.contains("fn extension_version() -> u32"));
        assert!(lib_rs.contains("fn extension_init()"));
        assert!(lib_rs.contains("fn extension_execute_tool("));
        assert!(lib_rs.contains("wasm_import_module = \"env\""));
        assert!(lib_rs.contains("out_buf: *mut u8"));
        assert!(lib_rs.contains("out_capacity: u32"));
        assert!(lib_rs.contains("wasm32-wasip1"));
        assert!(!lib_rs.contains("plugin_"));
        assert!(!lib_rs.contains("wasm32-wasi\n"));
        assert!(manual.contains("Extension ID：** `hello_extension`"));
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

    /// E2E：load_session_raw_content 在 session.jsonl 的 header.id 与目标 sid 不匹配时，
    /// 必须 fallthrough 到 <sid>.jsonl（spawned child worker 的独立 session 文件）。
    /// 这是 spawn_worker 派发的 Child/Peer worker 能被 `ion session tree <sid>` 正确读取的关键。
    #[test]
    fn load_session_raw_content_falls_through_to_by_id_path() {
        use std::fs;
        // 临时 HOME，隔离 SessionIndex
        let tmp = std::env::temp_dir().join(format!(
            "ion-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&tmp).unwrap();
        let prev_home = std::env::var("HOME").ok();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        // 项目目录 + 主 session.jsonl（header.id = main-sid，内容是 MAIN 标记）
        let project = tmp.join("proj");
        fs::create_dir_all(&project).unwrap();
        let main_sid = "sess_main_001";
        let child_sid = "sess_child_002";
        let main_path = ion::session_jsonl::session_path(project.to_str().unwrap());
        fs::create_dir_all(main_path.parent().unwrap()).unwrap();
        fs::write(
            &main_path,
            format!(
                "{{\"type\":\"session\",\"version\":1,\"id\":\"{main_sid}\",\"timestamp\":\"t\",\"cwd\":\"\"}}\n\
                 {{\"type\":\"message\",\"id\":\"m1\",\"parentId\":\"{main_sid}\",\"timestamp\":\"t\",\"message\":{{\"role\":\"user\",\"content\":\"MAIN\"}}}}\n"
            )
        ).unwrap();
        // 子 Worker 的 <child-sid>.jsonl（内容是 CHILD 标记）
        let child_path = ion::paths::session_jsonl_path_by_id(project.to_str().unwrap(), child_sid);
        fs::write(
            &child_path,
            format!(
                "{{\"type\":\"session\",\"version\":1,\"id\":\"{child_sid}\",\"timestamp\":\"t\",\"cwd\":\"\",\"parentSession\":\"{main_sid}\"}}\n\
                 {{\"type\":\"message\",\"id\":\"c1\",\"parentId\":\"{child_sid}\",\"timestamp\":\"t\",\"message\":{{\"role\":\"user\",\"content\":\"CHILD\"}}}}\n"
            )
        ).unwrap();
        // 注册到 SessionIndex
        let mut idx = ion::session_index::SessionIndex {
            sessions: std::collections::HashMap::new(),
        };
        for sid in [main_sid, child_sid] {
            idx.sessions.insert(
                sid.into(),
                ion::session_index::SessionMeta {
                    name: None,
                    first_name: None,
                    project: Some(project.to_str().unwrap().into()),
                    project_name: None,
                    worktree: false,
                    branch: None,
                    model: "".into(),
                    agent: "".into(),
                    provider: "".into(),
                    token_input: 0,
                    token_output: 0,
                    token_cache_read: 0,
                    token_cache_write: 0,
                    user_prompt_count: 0,
                    llm_request_count: 0,
                    total_duration_ms: 0,
                    compress_count: 0,
                    message_count: 0,
                    turn_count: 0,
                    created_at: 0,
                    updated_at: 0,
                    error_count: 0,
                    last_thinking_level: None,
                    last_active_tools: None,
                    last_entry_id: None,
                    parent_session: None,
                    parent_type: None,
                    initial_cwd: None,
                    last_cwd: None,
                    extra_cwds: Vec::new(),
                    tier_models: None,
                    security_profile: None,
                },
            );
        }
        idx.save();

        // 查 child_sid → 必须读到 CHILD，不是 MAIN
        let got = load_session_raw_content(child_sid).expect("应该 fallthrough 到 <sid>.jsonl");
        assert!(
            got.contains("CHILD"),
            "应该返回 child session 内容，got: {}",
            got
        );
        assert!(!got.contains("MAIN"), "不应该返回 main session 内容");

        // 查 main_sid → 应该读 session.jsonl
        let got_main = load_session_raw_content(main_sid).expect("main session 应能读到");
        assert!(got_main.contains("MAIN"));

        // 恢复 HOME
        unsafe {
            if let Some(h) = prev_home {
                std::env::set_var("HOME", h);
            } else {
                std::env::remove_var("HOME");
            }
        }
        let _ = fs::remove_dir_all(&tmp);
    }

    // ── color_ansi tests (commit d5b636a) ──
    // list-agents NAME 列颜色映射。验证所有支持的 color 名 + 默认 fallback。

    #[test]
    fn test_color_ansi_basic_8() {
        assert_eq!(color_ansi(&Some("green".into())), "\x1b[32m");
        assert_eq!(color_ansi(&Some("red".into())), "\x1b[31m");
        assert_eq!(color_ansi(&Some("yellow".into())), "\x1b[33m");
        assert_eq!(color_ansi(&Some("blue".into())), "\x1b[34m");
        assert_eq!(color_ansi(&Some("magenta".into())), "\x1b[35m");
        assert_eq!(color_ansi(&Some("cyan".into())), "\x1b[36m");
        assert_eq!(color_ansi(&Some("white".into())), "\x1b[37m");
    }

    #[test]
    fn test_color_ansi_aliases() {
        // purple 是 magenta 的别名（用户常用）
        assert_eq!(color_ansi(&Some("purple".into())), "\x1b[35m");
        // gray / grey 都映射到 bright black (90)
        assert_eq!(color_ansi(&Some("gray".into())), "\x1b[90m");
        assert_eq!(color_ansi(&Some("grey".into())), "\x1b[90m");
        // bright_ 前缀也支持
        assert_eq!(color_ansi(&Some("bright_green".into())), "\x1b[32m");
        assert_eq!(color_ansi(&Some("bright_red".into())), "\x1b[31m");
    }

    #[test]
    fn test_color_ansi_orange_256color() {
        // orange 不在基本 8 色里，用 256-color 208
        assert_eq!(color_ansi(&Some("orange".into())), "\x1b[38;5;208m");
    }

    #[test]
    fn test_color_ansi_none_and_unknown() {
        // None（agent 没设 color）→ 默认无色
        assert_eq!(color_ansi(&None), "\x1b[0m");
        // 空字符串 → 默认
        assert_eq!(color_ansi(&Some(String::new())), "\x1b[0m");
        // 未识别的颜色名 → 默认（不 panic）
        assert_eq!(color_ansi(&Some("hotpink".into())), "\x1b[0m");
        assert_eq!(color_ansi(&Some("rainbow".into())), "\x1b[0m");
    }
}

/// Build environment info string for system prompt injection.
/// Includes: time, cwd, project root, git branch, git remote.
/// 构建环境信息（cwd/git/时间/最近 commit/最近修改文件），注入 system prompt。
/// pub + 接受 cwd 参数：让 export_session_rich 也能复用（用 session header.cwd）。
pub fn build_env_info(cwd: &str) -> String {
    use std::process::Command;
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 人类可读时间（ISO 日期 + UTC 时分，比 "day 20666" 更直观）
    let now_human = {
        let days = now_epoch / 86400;
        let remain = now_epoch % 86400;
        let h = remain / 3600;
        let m = (remain % 3600) / 60;
        // 1970-01-01 + days → 粗略年月日（不处理闰年的精确算法，够用）
        let (y, mo, d) = epoch_days_to_ymd(days as i64);
        format!("{y}-{mo:02}-{d:02} {h:02}:{m:02} UTC")
    };

    let project_root = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| cwd.to_string());

    let git_branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            let b = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if b.is_empty() { None } else { Some(b) }
        });

    let git_remote = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| {
            let r = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if r.is_empty() { None } else { Some(r) }
        });

    let worktree = std::env::var("ION_WORKTREE_ROOT")
        .ok()
        .or_else(|| std::env::var("ION_WORKTREE").ok());

    // 原始工作目录（从 SessionIndex 读，如果 session_id 已知）
    // 注意：这里不直接读 index（build_env_info 是无状态函数），由调用方决定是否注入 initial_cwd。

    let mut info = String::from("\n\n--- environment ---\n## Environment\n");
    info.push_str(&format!("- **Current Time**: {}\n", now_human));
    info.push_str(&format!("- **Working Directory (cwd)**: `{}`\n", cwd));
    info.push_str(&format!("- **Project Root**: `{}`\n", project_root));
    if let Some(wt) = &worktree {
        info.push_str(&format!("- **Worktree Path**: `{}`\n", wt));
    }
    // OS / 运行环境
    let os_info = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
    info.push_str(&format!("- **Platform**: `{}`\n", os_info));
    let ion_ver = env!("CARGO_PKG_VERSION");
    info.push_str(&format!("- **ION Version**: `{}`\n", ion_ver));
    if let Some(branch) = &git_branch {
        info.push_str(&format!("- **Git Branch**: `{}`\n", branch));
    }
    if let Some(remote) = &git_remote {
        info.push_str(&format!("- **Git Remote**: `{}`\n", remote));
    }

    // 最近 commit 主题（last 5，只标题，不含文件名——压缩 token）
    let recent_subjects = Command::new("git")
        .args(["log", "--oneline", "-5"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    if let Some(subjects) = &recent_subjects {
        if !subjects.is_empty() {
            info.push_str("\n### Recent Commits (last 5)\n```\n");
            info.push_str(subjects);
            info.push_str("\n```\n");
        }
    }

    // 未提交的改动（过滤噪音后展示，与 Recently Modified 合并避免重复）
    // 噪音过滤：二进制/测试产物后缀对 LLM 无用，且容易占用大量行数
    const NOISE_SUFFIXES: &[&str] = &[
        ".png", ".jpg", ".jpeg", ".gif", ".svg", ".ico", ".bmp", ".webp", ".mp4", ".mov", ".avi",
        ".mp3", ".wav", ".zip", ".tar", ".gz", ".lock",
    ];
    let is_noise = |path: &str| -> bool {
        let lower = path.to_lowercase();
        NOISE_SUFFIXES.iter().any(|s| lower.ends_with(s))
        // 也过滤常见测试产物目录
        || lower.contains("test-results/")
        || lower.contains(".playwright-mcp/")
    };

    let uncommitted_raw = Command::new("git")
        .args(["status", "--short"])
        .current_dir(cwd)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string());
    if let Some(changes) = &uncommitted_raw {
        if !changes.is_empty() {
            // 过滤噪音行，保留代码/文档/配置文件
            let filtered: Vec<&str> = changes
                .lines()
                .filter(|line| {
                    // 从 git status 行提取路径（剥掉 XY 状态码 + 空格）
                    let path_part = line
                        .trim_start_matches(|c: char| c.is_alphabetic() || c == ' ' || c == '?')
                        .trim();
                    !is_noise(path_part)
                })
                .collect();
            if !filtered.is_empty() {
                let shown = if filtered.len() > 15 {
                    let more = filtered.len() - 15;
                    format!("\n# (and {} more...)", more)
                } else {
                    String::new()
                };
                let display = filtered
                    .iter()
                    .take(15)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("\n");
                info.push_str(&format!(
                    "\n### Workspace Changes\n```\n{display}{shown}\n```\n"
                ));
            }
        }
    }
    info
}

/// 把 epoch 天数（1970-01-01 = 0）转成 (year, month, day)。
/// 简化算法（够用，不追求闰年精确到秒级）：
/// 365.2425 天/年（格里高利历平均值）。
fn epoch_days_to_ymd(days: i64) -> (i64, u32, u32) {
    let year = 1970 + days / 365;
    // 剩余天数（粗略，闰年偏差靠 is_leap 校正月内日期）
    let mut remaining = days % 365;
    // 简单的月份表（平年）
    let month_days = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = |y: i64| (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mut month = 1u32;
    let mut day = 1u32;
    while remaining > 0 {
        let md = if month == 2 && is_leap(year) {
            29
        } else {
            month_days[(month - 1) as usize]
        };
        if remaining >= md as i64 {
            remaining -= md as i64;
            month += 1;
            if month > 12 {
                month = 1;
                // 不重新算 year（粗略够了）
            }
        } else {
            day = (remaining + 1) as u32;
            remaining = 0;
        }
    }
    (year, month, day)
}
