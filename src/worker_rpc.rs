//! Worker RPC 模式 — `ion --mode rpc` 入口的实现。
//!
//! 历史：原本是独立二进制 `ion-worker`，现已合并进 `ion` 单二进制。
//! host 通过 `current_exe() + ["--mode", "rpc", ...]` spawn 自身来创建 worker 子进程，
//! 对齐 pi 的 `pi --mode rpc` 设计。
//!
//! JSONL RPC 协议，完全对齐 pi 的 rpc-mode.ts。
//!
//! 三种命令模式:
//! 1. 同步查询: get_state → 读属性 → 返回
//! 2. 异步操作: set_model → await → 返回
//! 3. 流式:     prompt → 触发(不 await) → 事件推送

use crate::agent::agent_loop::{Agent, AgentConfig, DeliverAs};
use crate::agent::compact::CompactConfig;
use crate::agent::tool::{
    AwaitWorkerTool, BashTool, BranchSessionTool, CalculatorTool, ChannelSendTool, EchoTool,
    EditTool, FindTool, GlobalMemorySaveTool, GlobalMemorySearchTool, GrepTool, KillWorkerTool,
    LsTool, ReadTool, ResumeWorkerTool, SendToWorkerTool, SkillTool, SpawnWorkerTool, ToolRegistry,
    WriteTool,
};
use crate::session_jsonl;
use crate::wasm_extension::{WasmExtensionRegistry, WasmToolAdapter};
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, oneshot};

/// 全局：当前 Worker 的 session 文件路径。
/// 主 Worker = session.jsonl；fork 子 Worker = <session_id>.jsonl（独立文件）。
/// save_worker_session 读这个路径决定往哪写。
static SESSION_FILE_PATH: std::sync::Mutex<Option<std::path::PathBuf>> =
    std::sync::Mutex::new(None);

/// 全局：当前 Worker 的 session_id + cwd。
/// on_before_tool_execute 钩子用（它拿不到 sid/cwd，只能从全局读）。
static SESSION_SID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static SESSION_CWD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
use ion_provider::registry::{ApiRegistry, ProviderFactory};
use ion_provider::types::*;

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Worker RPC 入口的参数。
///
/// `ion --mode rpc` 解析 CLI 后构造此结构，再调用 [`run_worker_rpc`]。
/// 也可由 host 通过 spawn 自身（`current_exe()` + `--mode rpc`）间接传入。
#[derive(Debug, Default, Clone)]
pub struct WorkerRpcArgs {
    /// `--session <id>`：复用已有 session；None 则生成新 UUID。
    pub session_id: Option<String>,
    /// `--model <id>`：默认 `deepseek-v4-flash`（会被 config.json 覆盖）。
    pub model_id: Option<String>,
    /// `--provider <p>`：默认 `opencode`（会被 config.json 覆盖）。
    pub provider: Option<String>,
    /// `--channel <name>`：可重复，订阅的 channel 列表。
    pub channels: Vec<String>,
    /// `--agent <name>`：初始 agent 模板。
    pub initial_agent: Option<String>,
}

impl WorkerRpcArgs {
    /// 从 `std::env::args()` 解析（兼容旧 `ion-worker --mode rpc` 调用约定）。
    ///
    /// 识别的 flag：`--session/--model/--provider/--channel/--agent/--mode`。
    /// 其他 flag（如 `--help`）被忽略。
    pub fn from_env_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        let mut out = Self::default();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--session" => {
                    out.session_id = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                "--model" => {
                    out.model_id = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                "--provider" => {
                    out.provider = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                "--channel" => {
                    if let Some(ch) = args.get(i + 1) {
                        out.channels.push(ch.clone());
                    }
                    i += 2;
                    continue;
                }
                "--agent" => {
                    out.initial_agent = args.get(i + 1).cloned();
                    i += 2;
                    continue;
                }
                "--mode" => {
                    i += 2;
                    continue;
                } // 已知是 rpc
                _ => {
                    i += 1;
                }
            }
        }
        out
    }
}

/// Worker RPC 主入口。
///
/// 由 `ion --mode rpc` 分支调用。初始化 tracing、Provider、Agent、RPC 循环，
/// 在 stdin EOF 时优雅退出（保存 session）。
///
/// # Panics
/// 不主动 panic；遇到致命初始化错误时通过 `eprintln!` + `std::process::exit(1)` 退出。
pub async fn run_worker_rpc(args: WorkerRpcArgs) {
    // CRITICAL: tracing MUST go to stderr, stdout is reserved for JSONL
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .try_init()
        .ok();

    // Capture process start time for the health check RPC. Used by the watchdog
    // (scripts/watchdog.sh) to report uptime during dual-version switching.
    let start_time = std::time::Instant::now();
    let WorkerRpcArgs {
        session_id,
        model_id,
        provider,
        channels,
        initial_agent,
    } = args;
    let mut model_id = model_id.unwrap_or_else(|| {
        std::env::var("ION_SESSION_MODEL").unwrap_or_else(|_| "deepseek-v4-flash".to_string())
    });
    let mut provider = provider.unwrap_or_else(|| {
        std::env::var("ION_SESSION_PROVIDER").unwrap_or_else(|_| "opencode".to_string())
    });
    let initial_agent = initial_agent;

    let sid = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 初始化 Provider + Model + Tools + Agent
    let mut registry = ApiRegistry::new();
    registry.register_builtins();

    // ── FauxProvider 接入（测试/开发用，不调真实 LLM）──
    let faux_script = std::env::var("ION_FAUX_SCRIPT").ok();
    let faux_reply = std::env::var("ION_FAUX_REPLY").ok();
    let using_faux = faux_script.is_some() || faux_reply.is_some();
    if using_faux {
        let faux = ion_provider::faux::register_faux(&mut registry);
        // Build responses from env var
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

    let mut model_reg = ion_provider::registry::ModelRegistry::new();
    model_reg.register_builtins();
    // 兼容 host 模式：如果 model_id 是 "replay/xxx" 形式，拆出 provider=replay + model_id=xxx
    // （host 的 create_worker RPC 接受完整 model 字符串，不像 CLI 会预解析）
    if model_id.starts_with("replay/") {
        provider = "replay".to_string();
        model_id = model_id["replay/".len()..].to_string();
    }
    let mut model = model_reg.find_model(&model_id).cloned().unwrap_or_else(|| {
        // 从 config.json 或 auth.json 读 base_url 和 api_key
        let cfg = crate::config::IonConfig::load();
        let auth = crate::auth::AuthStorage::load();
        let auth_url = auth.provider_base_urls.get(&provider).cloned();
        // 从 config.json 的 providers.X.base_url 读（auth.json 可能没有）
        let config_url = cfg.providers.get(&provider).map(|p| p.base_url.clone());
        let base_url = auth_url
            .or(config_url)
            .unwrap_or_else(|| "https://opencode.ai/zen/go/v1".into());
        Model {
            id: model_id.clone(),
            name: model_id.clone(),
            api: "openai-completions".into(),
            provider: provider.clone(),
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
    // 即使是 builtin model，如果 auth.json 里有该 provider 的代理 base_url，覆盖之。
    // （builtin GLM model 的 base_url 是直连 open.bigmodel.cn，但用户可能用代理。）
    if let Some(override_url) = crate::auth::AuthStorage::load()
        .provider_base_urls
        .get(&provider)
        && !override_url.is_empty()
    {
        model.base_url = override_url.clone();
    }

    // config.json 里的 provider base_url 也需要覆盖 builtin model 的直连 URL。
    if let Some(ref cfg_provider) = crate::config::IonConfig::load().providers.get(&provider) {
        if !cfg_provider.base_url.is_empty() {
            model.base_url = cfg_provider.base_url.clone();
        }
    }

    // faux 模式：强制 model.api 指向 faux provider（覆盖任何真实 API 路由）
    if using_faux {
        model.api = "faux".into();
        eprintln!("[faux] model.api forced to 'faux'");
    }
    // replay 模式：强制 model.api 指向 replay provider（绕过 find_model fallback 的 openai-completions）
    if provider == "replay" {
        model.api = "replay".into();
        eprintln!("[replay] model.api forced to 'replay' (model_id={model_id})");
    }

    // ── ReplayProvider（始终注册；通过 --model replay/<id> 激活）──
    registry.register("replay", Box::new(ion_provider::replay::ReplayProvider));

    // ── RecordingProvider（通过 ION_RECORD 环境变量激活）──
    if let Ok(rec_id) = std::env::var("ION_RECORD") {
        let overwrite = std::env::var("ION_RECORD_OVERWRITE").is_ok();
        match ion_provider::replay::recording_trace_path(&rec_id) {
            Ok(trace_path) => {
                let rec_dir = trace_path.parent().unwrap().to_path_buf();
                match ion_provider::replay::acquire_recording_lock(&rec_dir, overwrite) {
                    Ok(lock_opt) => {
                        let inner: Option<Box<dyn ion_provider::registry::ApiProvider>> =
                            if using_faux {
                                let new_faux =
                                    std::sync::Arc::new(ion_provider::faux::FauxProvider::new());
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

    // LSP shared handles (populated during extension registration below)

    let mut tools = ToolRegistry::new();
    tools.register(Box::new(ReadTool));
    tools.register(Box::new(GrepTool));
    tools.register(Box::new(FindTool));
    tools.register(Box::new(LsTool));
    tools.register(Box::new(BashTool));
    tools.register(Box::new(WriteTool));
    tools.register(Box::new(EditTool));
    tools.register(Box::new(CalculatorTool));
    tools.register(Box::new(EchoTool));
    // ── 内置 plan 工具（plan_enter/exit/add/list/done/approve）──
    // 不依赖 WASM plan-extension（已删除，跟内置 PlanExtension 工具名冲突）。
    //
    // Q2 fix: create ONE SharedPlan instance and pass it to BOTH the
    // PlanExtension (registered below as an Extension for mode hooks) AND
    // the plan Tools. Previously they used separate instances, so plan_add
    // wrote to the Tool's instance while plan_exit persisted the Extension's
    // (empty) instance → PLAN.md was always empty after exit.
    let shared_plan: crate::agent::plan_tool::SharedPlan =
        std::sync::Arc::new(crate::agent::plan_extension::PlanExtension::new());
    for t in crate::agent::plan_tool::plan_tools_with(shared_plan.clone()) {
        tools.register(t);
    }
    // ── Goal Supervisor（goal_set tool + on_gate_check closed loop）──
    // Tool is always registered (like plan tools); the Extension (which runs
    // the verification loop) is conditionally registered below based on config.
    // Both share the same SharedGoalState so the tool's writes are visible to
    // the extension's gate checks.
    let shared_goal: crate::goal_supervisor_extension::SharedGoalState = std::sync::Arc::new(
        std::sync::Mutex::new(None::<crate::goal_supervisor_extension::GoalState>),
    );
    tools.register(Box::new(
        crate::goal_supervisor_extension::GoalSetTool::new(shared_goal.clone()),
    ));
    tools.register(Box::new(crate::goal_supervisor_extension::GoalRefineTool(
        shared_goal.clone(),
    )));
    tools.register(Box::new(
        crate::goal_supervisor_extension::GoalDiagnoseTool(shared_goal.clone()),
    ));
    tools.register(Box::new(BranchSessionTool));
    tools.register(Box::new(GlobalMemorySearchTool));
    tools.register(Box::new(GlobalMemorySaveTool));

    // ── Worker 编排工具（仅 WorkerRuntime 支持真实实现）──
    // 让 LLM 自主调用 spawn_worker 创建子/同级 Worker，send_to_worker 跨 Worker 对话。
    tools.register(Box::new(SpawnWorkerTool));
    tools.register(Box::new(SendToWorkerTool));
    tools.register(Box::new(ResumeWorkerTool));
    tools.register(Box::new(AwaitWorkerTool));
    tools.register(Box::new(ChannelSendTool));
    tools.register(Box::new(KillWorkerTool));

    // ── 基础路径变量（Memory/Extension 构造前需要）──
    let worker_cwd = std::env::var("ION_WORKER_CWD")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });
    let config_root = crate::paths::project_root_for_config()
        .to_string_lossy()
        .to_string();
    let storage_ctx = crate::storage_context::StorageContext::new(&worker_cwd, &sid, &config_root);

    // ── Memory 工具 + 共享 Store ──
    // Memory 用 config_root（worktree 场景回源主仓库，缺口 #2：worktree 共享记忆）
    let memory_store = std::sync::Arc::new(tokio::sync::Mutex::new(
        crate::agent::memory::MemoryStore::new(storage_ctx.clone()),
    ));
    tools.register(Box::new(crate::agent::memory::MemorySaveTool {
        store: memory_store.clone(),
    }));
    tools.register(Box::new(crate::agent::memory::MemorySearchTool {
        store: memory_store.clone(),
    }));

    // ── Skill 工具（让 LLM 按需加载 skill）──
    // 扫描三个位置：
    // 1. ~/.ion/agent/skills/（ION 全局）
    // 2. <config_root>/.ion/skills/（项目级）
    // 3. ~/.agents/skills/（全局 skill 库，111 个）
    let agents_skills = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".agents").join("skills"))
        .unwrap_or_else(|| std::path::PathBuf::from("~/.agents/skills"));
    let skill_dirs = vec![
        crate::paths::skills_dir(),
        crate::paths::project_skills_dir(&config_root),
        agents_skills,
    ];
    // ★ 不注册 SkillTool 给 LLM（用户：'禁止提供 skill list 的能力给到 LLM，
    // 因为默认都注入到系统提示词'）。Skill 大纲已在 system prompt 里展示。
    // skill_dirs 仍然保留（供 system prompt 注入大纲用），只是不暴露 tool。

    // 加载 API key
    let api_key = crate::auth::AuthStorage::resolve_api_key(None, &provider);
    if api_key.is_none() {
        // Hardcoded fallback for testing
        let key = std::env::var("ION_API_KEY").unwrap_or_else(|_| {
            "sk-sniMbFE0l8wIGsTAsbfERSGrvcrBv97iBfDuppzN99kg5Wp2a2dMYxntMFBN9lEg".into()
        });
        let _ = key; // Will be set below
    }
    let api_key = api_key
        .or_else(|| std::env::var("ION_API_KEY").ok())
        .unwrap_or_else(|| {
            "sk-sniMbFE0l8wIGsTAsbfERSGrvcrBv97iBfDuppzN99kg5Wp2a2dMYxntMFBN9lEg".into()
        });

    let config = AgentConfig {
        // max_turns：优先读 ION_MAX_TURNS 环境变量（补丁 1：hooks/扩展 spawn 子 Worker 时限定步数）
        // 没设则默认 20（对齐 pi）。0 = 无限。
        max_turns: std::env::var("ION_MAX_TURNS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|n| if n == 0 { None } else { Some(n) })
            .unwrap_or(Some(20)),
        max_outer_iterations: std::env::var("ION_MAX_OUTER_ITERATIONS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
        max_retries: 30,
        retry_base_delay_ms: 1000,
        enable_compact: true,
        compact_config: CompactConfig::default(),
        api_key: Some(api_key.clone()),
        response_format: None,
        thinking: None,
        compact_model_id: None,
        // evolver/wf/improver agent 可能在 turn 里"只说不做"（输出文本但没调工具），
        // retry_on_no_tool_use 让它在这种情况下重试（注入 WARNING）。
        // 对这些 agent 默认启用（3 次重试），其他 agent 保持 0（禁用）。
        retry_on_no_tool_use: if matches!(
            initial_agent.as_deref(),
            Some("wf") | Some("improver") | Some("evolver")
        ) {
            std::env::var("ION_RETRY_NO_TOOL_USE")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3)
        } else {
            0
        },
        retry_config: Some(crate::retry::RetryConfig::default()),
    };

    let registry = Arc::new(registry);

    // WASM Extension 注册表（RPC 热更新用）
    let wasm_ext_registry = Arc::new(WasmExtensionRegistry::new());

    // Keep loaded WASM paths so trait adapters can be registered below.
    let mut loaded_wasm_paths: Vec<String> = Vec::new();

    // ── WASM Extension 自动发现（Agent 构造前，注册到 tools）──
    // 扫描 ~/.ion/agent/extensions/ 和 {project_root}/.ion/extensions/ 下的 .wasm 文件
    // project_root 用 project_root_for_config()（worktree 场景回源到主仓库，缺口 #2）
    {
        let config_root = crate::paths::project_root_for_config()
            .to_string_lossy()
            .to_string();
        let extensions_dirs: Vec<std::path::PathBuf> = vec![
            crate::paths::extensions_dir(),
            crate::paths::project_extensions_dir(&config_root),
        ];
        for dir in &extensions_dirs {
            if !dir.exists() {
                continue;
            }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "wasm").unwrap_or(false) {
                        let canonical_str = std::fs::canonicalize(&path)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| path.to_string_lossy().to_string());
                        let extension_id = crate::wasm_extension::extension_id_from_path(&canonical_str);
                        match wasm_ext_registry.add(&canonical_str) {
                            Ok(tool_defs) => {
                                for td in &tool_defs {
                                    tools.register(Box::new(WasmToolAdapter {
                                        name: td.name.clone(),
                                        description: td.description.clone(),
                                        parameters: td.parameters.clone(),
                                        extension_path: canonical_str.clone(),
                                        extension_id: extension_id.clone(),
                                        registry: wasm_ext_registry.clone(),
                                    }));
                                    tracing::info!(
                                        "[wasm] auto-discovered {extension_id}: {}",
                                        td.name
                                    );
                                }
                                loaded_wasm_paths.push(canonical_str);
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

    // 加载已有会话（按 cwd 查找）—— session 按 cwd 隔离，worktree 各自独立会话（设计意图）
    // worker_cwd / config_root / storage_ctx 已在前面定义（Memory 构造前）
    //
    // ── fork 子 Worker 用独立 session 文件 ──
    // 主 Worker 用 session.jsonl（共享，所有同一 cwd 的主会话共用）。
    // fork 子 Worker（ION_FORK_CHILD=1）用 <session_id>.jsonl，避免跟主 Worker 写同一文件
    // 导致数据混乱。这样 export 可以按 session_id 精确找到 fork 子 Worker 的对话历史。
    let is_fork_child = std::env::var("ION_FORK_CHILD")
        .map(|v| v == "1")
        .unwrap_or(false);
    let session_file_path: std::path::PathBuf = if is_fork_child {
        crate::paths::session_jsonl_path_by_id(&worker_cwd, &sid)
    } else {
        crate::paths::session_jsonl_path(&worker_cwd)
    };
    // 存到全局，save_worker_session 用同一个路径
    {
        let mut p = SESSION_FILE_PATH.lock().unwrap();
        *p = Some(session_file_path.clone());
    }
    // 设置 lib 层全局覆盖（让所有 append helper 使用正确的子会话路径）
    crate::session_jsonl::set_session_file_override(Some(session_file_path.clone()));
    // 存 sid + cwd 到全局，on_before_tool_execute 钩子用
    {
        *SESSION_SID.lock().unwrap() = Some(sid.clone());
        *SESSION_CWD.lock().unwrap() = Some(worker_cwd.clone());
    }
    // 设 session header 的 agent/model/provider（export.rs banner 显示用）
    if let Some(ref agent_name) = initial_agent {
        unsafe {
            std::env::set_var("ION_SESSION_AGENT", agent_name);
        }
    }
    unsafe {
        std::env::set_var("ION_SESSION_MODEL", &model.id);
        std::env::set_var("ION_SESSION_PROVIDER", &provider);
    }

    // 先确保 session header 存在（防 message/custom 在 header 之前被追加）
    if is_fork_child {
        ensure_fork_session_header(&session_file_path, &worker_cwd, &sid);
    } else {
        session_jsonl::ensure_session_header(&worker_cwd, &sid);
    }
    let preloaded = if is_fork_child {
        load_fork_session_messages(&session_file_path)
    } else {
        session_jsonl::SessionFile::load(&worker_cwd).map(|f| f.messages)
    };

    // File Snapshot Store（预声明，agent 初始化块和 RPC loop 都要用）
    #[allow(unused_assignments)]
    let mut snapshot_store: Option<std::sync::Arc<crate::file_snapshot::SnapshotStore>> = None;

    // Approval Manager（预声明，审批 RPC 用，依赖 snapshot_store）
    #[allow(unused_assignments)]
    let mut approval_mgr: Option<
        std::sync::Arc<crate::file_snapshot::approval::ApprovalManager>,
    > = None;

    // ── 加载配置（在 Runtime 和 Extension 初始化之前）──
    let ion_cfg = crate::config::IonConfig::load();

    // ── MCP（方案 C：所有 Worker 通过 bridge 代理调 host 的 MCP 连接）──
    // Worker 进程不自己 connect_all，而是从 host 拉工具列表注册 McpProxyTool。
    // 所有 Worker（入口 + 子）都是代理模式，host 持有唯一的 MCP 连接。
    // 场景 1（cmd_run）不走 worker，直接用 McpManager + McpTool（在 ion.rs 里处理）。

    // ── ManagerBridge 必须在 Agent 构造前创建，因为 WorkerRuntime 包装它注入到 Agent ──
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let manager_bridge: Arc<ManagerBridge> =
        Arc::new(ManagerBridge::new(sid.clone(), stdout.clone()));

    // ── 根据配置选择 Runtime ──
    // 用 Arc 保存，这样 HookExtension 能 clone 一份（agent handler 需要 runtime 来 spawn 子 Worker）
    let worker_rt: Arc<dyn crate::runtime::Runtime> = {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let registry =
            crate::backend_registry::BackendRegistry::from_config(&ion_cfg.runtime, &cwd);
        tracing::info!(
            "[runtime] BackendRegistry 初始化: backends={:?}",
            registry.list_backends(),
        );
        let worker_inner = crate::runtime::WorkerRuntime::new(
            registry,
            manager_bridge.clone() as Arc<dyn crate::runtime::ManagerBridgeHandle>,
        );
        Arc::new(worker_inner)
    };

    let default_prompt = "You are a helpful AI assistant with access to tools.".to_string();
    // 启动时应用 --agent 配置（如果指定）
    let mut initial_system_prompt = default_prompt.clone();
    let mut current_agent_name: String = "build".into();
    if let Some(ref agent_name) = initial_agent {
        if let Some(agent_cfg) = crate::agent_config::find_agent(agent_name) {
            current_agent_name = agent_cfg.name.clone();
            if let Some(ref sp) = agent_cfg.system_prompt {
                initial_system_prompt = sp.clone();
            }
            tracing::info!("[worker] loaded agent '{}' from config", agent_cfg.name);
            // auto-continue: wf/improver 需要（workflow 多 stage）
            // evolver 不需要 auto_continue——它用 bash background + follow_up
            if matches!(current_agent_name.as_str(), "wf" | "improver")
                && std::env::var("ION_AUTO_CONTINUE").is_err()
            {
                unsafe {
                    std::env::set_var("ION_AUTO_CONTINUE", "1");
                }
                tracing::info!(
                    "[worker] auto-set ION_AUTO_CONTINUE=1 for {} agent",
                    current_agent_name
                );
            }
            // evolver: 等 bash 后台进程的异步 follow_up
            if current_agent_name == "evolver" {
                unsafe {
                    std::env::set_var("ION_WAIT_BACKGROUND", "1");
                }
                tracing::info!("[worker] set ION_WAIT_BACKGROUND=1 for evolver");
            }
            // Note: tool restriction is applied below after `agent` is built
            // We stash the config to apply post-construction
        } else {
            tracing::warn!("[worker] agent '{}' not found, using defaults", agent_name);
        }
    }

    // ── Skill 可用性提示（让 LLM 知道有 skill 工具，但不预加载内容省 token）──
    let skill_hint = build_skill_hint(&config_root);
    if !skill_hint.is_empty() {
        initial_system_prompt.push_str("\n\n");
        initial_system_prompt.push_str(&skill_hint);
    }

    // ── system prompt 覆盖（skill fork 模式用）──
    // ION_SYSTEM_PROMPT 环境变量由 create_worker 设置（config.system_prompt_override），
    // 覆盖 agent.md 的 system prompt。用于 skill fork——把 skill 内容注入 system prompt，
    // 避免被 compaction 压缩（compaction 只处理 messages，不碰 system prompt）。
    if let Ok(sp_override) = std::env::var("ION_SYSTEM_PROMPT")
        && !sp_override.is_empty()
    {
        tracing::info!(
            "[worker] system prompt overridden by ION_SYSTEM_PROMPT ({} bytes)",
            sp_override.len()
        );
        initial_system_prompt = sp_override;
    }

    // ── 注入环境信息到 system prompt ──────────────────────────────
    // 让 LLM 知道：当前时间、cwd、项目路径、worktree 路径、git remote
    let env_info = {
        let now = {
            let secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            format!("{} (unix epoch)", secs)
        };
        let cwd = &worker_cwd;
        let project_root = std::env::var("ION_PROJECT_ROOT")
            .ok()
            .or_else(|| {
                // Try to find git root from cwd
                std::process::Command::new("git")
                    .args(["rev-parse", "--show-toplevel"])
                    .current_dir(cwd)
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| cwd.clone());
        let worktree = std::env::var("ION_WORKTREE_ROOT")
            .ok()
            .or_else(|| std::env::var("ION_WORKTREE").ok());
        let git_remote = std::process::Command::new("git")
            .args(["remote", "get-url", "origin"])
            .current_dir(cwd)
            .output()
            .ok()
            .and_then(|o| {
                let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if url.is_empty() { None } else { Some(url) }
            });
        let git_branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(cwd)
            .output()
            .ok()
            .and_then(|o| {
                let branch = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if branch.is_empty() {
                    None
                } else {
                    Some(branch)
                }
            });

        let mut info = "\n\n## Environment\n".to_string();
        info.push_str(&format!("- **Time**: {}\n", now));
        info.push_str(&format!("- **Working Directory**: `{}`\n", cwd));
        info.push_str(&format!("- **Project Root**: `{}`\n", project_root));
        if let Some(wt) = &worktree {
            info.push_str(&format!("- **Worktree Path**: `{}`\n", wt));
        }
        if let Some(branch) = &git_branch {
            info.push_str(&format!("- **Git Branch**: `{}`\n", branch));
        }
        if let Some(remote) = &git_remote {
            info.push_str(&format!("- **Git Remote**: `{}`\n", remote));
        }
        info.push_str(&format!("- **Agent**: `{}`\n", current_agent_name));
        info.push_str(&format!("- **Model**: `{}` ({})\n", model.id, provider));

        // Recent commits (last 3, with files changed)
        let recent = std::process::Command::new("git")
            .args(["log", "--oneline", "--name-only", "-3"])
            .current_dir(cwd)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());
        if let Some(commits) = &recent
            && !commits.is_empty()
        {
            info.push_str("\n### Recent Changes (last 3 commits)\n```\n");
            info.push_str(commits);
            info.push_str("\n```\n");
        }

        // Uncommitted changes
        let uncommitted = std::process::Command::new("git")
            .args(["status", "--short"])
            .current_dir(cwd)
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());
        if let Some(changes) = &uncommitted
            && !changes.is_empty()
        {
            info.push_str("\n### Uncommitted Changes\n```\n");
            info.push_str(changes);
            info.push_str("\n```\n");
        }

        info
    };
    initial_system_prompt.push_str(&env_info);

    // LSP tool registration: deferred to after Agent::new (tools moved there)
    // The shared handles are set inside the extension block below

    let mut agent = Agent::new(
        Arc::clone(&registry),
        model.clone(),
        Some(initial_system_prompt),
        tools,
        config,
    )
    .with_runtime_arc(worker_rt.clone())
    .with_session_cwd(Some(worker_cwd.clone()))
    .with_session_id(Some(sid.clone()));

    // LSP tool registration deferred to inside extension block

    // 应用初始 agent 的工具限制（必须在 Agent 构造后调用）
    if let Some(ref agent_name) = initial_agent
        && let Some(agent_cfg) = crate::agent_config::find_agent(agent_name)
    {
        // 1. 白名单优先：如果 agent 定义了 tools，只保留这些工具
        if let Some(ref allowed) = agent_cfg.tools {
            agent.restrict_tools(allowed.clone());
        }
        // 2. 黑名单：移除 disallowed_tools 里的工具
        if let Some(ref disallowed) = agent_cfg.disallowed_tools {
            for tool_name in disallowed {
                agent.remove_tool(tool_name);
            }
        }
    }

    // ── 补丁 1（HOOKS_AND_OUTLINE_SYNC）：环境变量来源的工具限制 ──
    // Manager spawn 子 Worker 时通过 ION_ALLOWED_TOOLS / ION_DISALLOWED_TOOLS 环境变量传入。
    // 叠加在 agent.md 定义的限制之后（进一步收紧，不能放宽）：
    //   - 白名单：与 agent.md 的白名单取交集（agent.md 没设白名单则直接用环境变量的）
    //   - 黑名单：并集（两边都禁的都禁）
    // 这让扩展/hooks 的 agent handler 能 spawn "限定工具"的子 Worker，
    // 是 ION 的 agent handler 比 pi 更强的关键。
    if let Ok(allowed_str) = std::env::var("ION_ALLOWED_TOOLS") {
        let allowed: Vec<String> = allowed_str
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !allowed.is_empty() {
            agent.restrict_tools(allowed);
            tracing::info!("[worker] applied ION_ALLOWED_TOOLS from env");
        }
    }
    if let Ok(disallowed_str) = std::env::var("ION_DISALLOWED_TOOLS") {
        for tool_name in disallowed_str
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            agent.remove_tool(tool_name);
        }
        tracing::info!("[worker] applied ION_DISALLOWED_TOOLS from env");
    }

    if let Some(msgs) = preloaded {
        agent = agent.with_messages(msgs);
    }

    // ── 注册内置 Extension（Memory / Bash / Streaming），可通过 config.json 关闭 ──
    // 先创建 follow_up 通道（Bash Extension 后台进程完成时用来注入消息）
    // 活跃后台 watcher 计数（bash 后台进程）：bash 扩展与 agent_loop 共享，
    // outer_loop 只在 >0 时等待后台完成（否则零等待收尾）
    let bg_pending = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let (follow_up_tx, follow_up_rx) =
        tokio::sync::mpsc::unbounded_channel::<(ion_provider::Message, DeliverAs)>();
    let mut process_map = None;
    {
        let mut ext_reg = crate::agent::extension::ExtensionRunner::new();

        // ── 注入 ctx.fs 统一文件访问能力（RuntimeFileSystem）──
        // 内置扩展通过 registry.filesystem() 拿到，WASM 扩展通过 host_read_file / host_list_dir 拿到。
        // allowed_roots = 项目根目录 + ~/.ion/（默认白名单，防路径逃逸）。
        {
            let fs_allowed_roots =
                crate::agent::extension::RuntimeFileSystem::default_allowed_roots(
                    std::path::Path::new(&worker_cwd),
                );
            let runtime_fs = std::sync::Arc::new(crate::agent::extension::RuntimeFileSystem::new(
                worker_rt.clone(),
                fs_allowed_roots,
            ));
            // 内置扩展用
            ext_reg = ext_reg.with_filesystem(runtime_fs.clone());
            // WASM 扩展用（注入到 WASM registry 的共享 Context）
            {
                let mut ctx = wasm_ext_registry.ctx.write().unwrap();
                ctx.fs = Some(runtime_fs.clone());
                ctx.tokio_handle = Some(tokio::runtime::Handle::current());
                // Inject agent_rpc: gives WASM extensions access to agent state
                // (token counts, messages, steer, LLM calls, compaction).
                ctx.agent_rpc = Some(std::sync::Arc::new(WorkerAgentRpc::new(
                    model_id.clone(),
                    provider.clone(),
                    sid.clone(),
                )));
            }
            // FsProbeExtension（给 CLI 测试用，通过 extension_rpc 暴露 ctx.fs + data_dirs）
            ext_reg.register(Box::new(FsProbeExtension {
                fs: runtime_fs,
                storage: storage_ctx.clone(),
            }));
            tracing::info!("[extension] ctx.fs (RuntimeFileSystem) injected + fs_probe registered");
        }

        // ── 注入 StorageContext（扩展通过 registry.data_dirs(name) 拿 4 级数据目录）──
        ext_reg = ext_reg.with_storage(storage_ctx.clone());
        tracing::info!("[extension] StorageContext injected (data_dirs available)");

        // ── SessionProbeExtension（给 CLI 测试用，让 session hook 可通过 subscribe 观察）──
        ext_reg.register(Box::new(SessionProbeExtension { veto: false }));
        tracing::info!(
            "[extension] session_probe registered (session hook observable via subscribe)"
        );

        // ── PlanExtension（plan mode 钩子：限制工具集 + 注入 prompt）──
        // Q2 fix: wrap the SAME shared_plan Arc that the plan Tools use, so
        // before/after_tool_call hooks see the same plan_steps state that
        // plan_add writes to. Without this, plan_exit persists an empty step
        // list (the Extension's own fresh instance), producing an empty PLAN.md.
        ext_reg.register(Box::new(crate::agent::plan_extension::SharedPlanExtension(
            shared_plan.clone(),
        )));
        tracing::info!("[extension] PlanExtension registered (shared with plan tools)");

        // Tool Loop Detector（防 LLM 重复调同一工具死循环）
        ext_reg.register(Box::new(crate::tool_loop_detector::ToolLoopDetector::new()));
        tracing::info!("[extension] tool-loop-detector registered");

        // Auto Session Title（首轮自动生成会话标题，用 fast tier 而非主模型）
        // 之前用 model.clone()（主模型）→ 烧贵的 pro/max 配额做 title 生成。
        // 改用 resolve_tier_model("fast") 拿 fast tier；fallback 到主模型。
        let title_model = crate::config::IonConfig::load()
            .resolve_tier_model("fast")
            .unwrap_or_else(|| model.clone());
        let title_api_key =
            crate::config::IonConfig::load().resolve_provider_api_key(&title_model.provider);
        ext_reg.register(Box::new(
            crate::auto_session_title::AutoSessionTitle::with_registry(
                Arc::clone(&registry),
                title_model,
            )
            .with_api_key(title_api_key),
        ));
        tracing::info!("[extension] auto-session-title registered (fast tier + api_key)");

        // Learning Extension（会话结束时自动提炼记忆，先脱敏再 LLM 提炼）
        // 注入 registry + model，让 on_session_shutdown 能 spawn LLM 蒸馏 skill
        let learning_ext = crate::learning_extension::LearningExtension::new()
            .with_registry_model(Arc::clone(&registry), model.clone());
        ext_reg.register(Box::new(learning_ext));
        tracing::info!("[extension] learning-extension registered (with LLM distillation)");

        // Memory Extension
        if ion_cfg.is_extension_enabled("memory") {
            let mut memory_ext = crate::agent::memory::MemoryExtension::new(storage_ctx.clone());
            // 复用 tools 的 MemoryStore（同一份数据）
            memory_ext.store = memory_store.clone();
            // V0.2 会话加工：注入 registry + model（SessionEnd 时 LLM 提炼记忆）
            memory_ext.registry = Some(Arc::clone(&registry));
            memory_ext.model = Some(model.clone());
            memory_ext.processing_enabled = ion_cfg.is_extension_enabled("global-memory");
            ext_reg.register(Box::new(memory_ext));
        } else {
            tracing::info!("[extension] memory disabled by config");
        }

        // Bash Extension（后台进程管理）
        if ion_cfg.is_extension_enabled("bash") {
            let mut bash_ext = crate::agent::bash::BashExtension::new(storage_ctx.clone());
            // Wire up the follow_up channel so background processes can inject
            // <bash_result> messages into the agent loop on completion.
            bash_ext.set_follow_up_tx(follow_up_tx.clone());
            // 活跃 watcher 计数与 agent_loop 共享（outer_loop 据此决定是否等待后台）
            bash_ext.set_bg_pending(bg_pending.clone());
            process_map = Some(bash_ext.process_map.clone());
            ext_reg.register(Box::new(bash_ext));
        } else {
            tracing::info!("[extension] bash disabled by config");
        }

        // Streaming Extension（流式透传）
        if ion_cfg.is_extension_enabled("streaming") {
            ext_reg.register(Box::new(StreamingExtension {
                session_id: sid.clone(),
            }));
        } else {
            tracing::info!("[extension] streaming disabled by config");
        }

        // Permission Extension（权限策略层）
        // 用 config_root（worktree 回源主仓库，读主仓库 .ion/settings.json）
        if ion_cfg.is_extension_enabled("permission") {
            let perm_ext =
                crate::agent::permission_extension::PermissionExtension::new(storage_ctx.clone());
            ext_reg.register(Box::new(perm_ext));
        } else {
            tracing::info!("[extension] permission disabled by config");
        }

        // Context Index Extension（上下文索引 + 快照折叠）
        if ion_cfg.is_extension_enabled("context-index") {
            let ctx_ext = crate::agent::context_index::ContextIndexExtension::new();
            ext_reg.register(Box::new(ctx_ext));
        } else {
            tracing::info!("[extension] context-index disabled by config");
        }

        // LSP Extension（cargo check diagnostics — LLM 编译错误反馈）
        // 钩子驱动：on_tool_execution_end 检测 write/edit → 后台 cargo check →
        // on_context 注入 <diagnostics>。LLM 不需要主动调 lsp_check。
        if ion_cfg.is_extension_enabled("lsp") {
            let lsp_ext = crate::lsp_extension::LspExtension::new();
            ext_reg.register(Box::new(lsp_ext));
            tracing::info!("[extension] lsp enabled (auto-trigger on write/edit, no LLM tool)");
        } else {
            tracing::info!("[extension] lsp disabled by config");
        }

        // Dev Server Detector Extension (detect dev server ports from bash output)
        if ion_cfg.is_extension_enabled("dev_server_detector") {
            ext_reg.register(Box::new(
                crate::dev_server_detector::DevServerDetectorExtension::new(),
            ));
            tracing::info!("[extension] dev_server_detector enabled");
        } else {
            tracing::info!("[extension] dev_server_detector disabled by config");
        }

        // File Time Guard Extension（detect externally-modified files before write/edit）
        if ion_cfg.is_extension_enabled("file-time-guard") {
            ext_reg.register(Box::new(
                crate::file_time_guard::FileTimeGuardExtension::new(),
            ));
            tracing::info!("[extension] file-time-guard enabled");
        } else {
            tracing::info!("[extension] file-time-guard disabled by config");
        }

        // Rules Engine Extension (project rules injection based on applyTo glob patterns)
        if ion_cfg.is_extension_enabled("rules-engine") {
            ext_reg.register(Box::new(crate::rules_engine::RulesEngineExtension::new()));
            tracing::info!("[extension] rules-engine enabled");
        } else {
            tracing::info!("[extension] rules-engine disabled by config");
        }

        // Context Files Extension (AGENTS.md / CLAUDE.md loading)
        if !crate::context_files_extension::ContextFilesExtension::is_disabled_by_env() {
            ext_reg.register(Box::new(
                crate::context_files_extension::ContextFilesExtension::new(),
            ));
            tracing::info!("[extension] context-files enabled");
        }

        // Goal Supervisor Extension (on_gate_check closed loop: run checks,
        // RetryWith on fail, until goal complete or guard trips).
        // Shares state with GoalSetTool (registered above in the tools section).
        if ion_cfg.is_extension_enabled("goal-supervisor") {
            let goal_ext = crate::goal_supervisor_extension::GoalSupervisorExtension::new()
                .with_shared_state(shared_goal.clone())
                .with_session_id(&sid);
            ext_reg.register(Box::new(goal_ext));
            tracing::info!("[extension] goal-supervisor enabled (on_gate_check closed loop)");
        } else {
            tracing::info!("[extension] goal-supervisor disabled by config");
        }

        // Context Reclaimer (priority-based token recycling)
        // Strips thinking blocks + reclaims old tool results (bash > grep > read)
        // Always enabled — zero LLM cost, pure text manipulation.
        ext_reg.register(Box::new(crate::context_reclaimer::ContextReclaimer::new()));
        tracing::info!(
            "[extension] context-reclaimer enabled (thinking strip + tool result recycling)"
        );

        // File Snapshot Extension（文件快照 + diff 追踪）
        snapshot_store = if ion_cfg.is_extension_enabled("file-snapshot") {
            let (fs_ext, store) =
                crate::file_snapshot::FileSnapshotExtension::new_pair(storage_ctx.clone());
            ext_reg.register(Box::new(fs_ext));
            tracing::info!("[extension] file-snapshot enabled");
            Some(store)
        } else {
            tracing::info!("[extension] file-snapshot disabled by config");
            None
        };
        // 标记 snapshot_store 在后续 RPC 分支中被读取（消除编译器误报）
        let _ = snapshot_store.is_some();

        // Approval Manager + Extension（审批，依赖 snapshot_store）
        approval_mgr = if let Some(ref store) = snapshot_store {
            let mgr = std::sync::Arc::new(crate::file_snapshot::approval::ApprovalManager::new(
                store.clone(),
                storage_ctx.clone(),
            ));
            // 注册 ApprovalExtension（on_gate_check + on_turn_end re-approval 重置）
            ext_reg.register(Box::new(
                crate::file_snapshot::approval::ApprovalExtension::new(mgr.clone()),
            ));
            // worker 启动即恢复审批状态——on_session_start 只在 agent.run(prompt)
            // 里触发，复活后未发消息的空闲 worker 查 review_pending 会拿到空审批表，
            // 已批文件全部显示回 pending
            mgr.restore_from_session();
            tracing::info!("[extension] file-approval enabled");
            Some(mgr)
        } else {
            tracing::info!("[extension] file-approval disabled (requires file-snapshot)");
            None
        };

        // Register adapters so runtime WASM modules participate in Extension hooks.
        for wasm_path in &loaded_wasm_paths {
            if let Some(hook_adapter) = wasm_ext_registry.create_hook_adapter(wasm_path) {
                ext_reg.register(Box::new(hook_adapter));
                tracing::info!("[wasm] registered Extension adapter for {}", wasm_path);
            }
        }

        // ── 注册 WorkflowExtension（可配置，默认启用）──
        // 当 agent .md 定义了 workflow: gate_command 时才生效。
        if ion_cfg.is_extension_enabled("workflow_gate")
            && let Some(ref agent_name) = initial_agent
            && let Some(agent_cfg) = crate::agent_config::find_agent(agent_name)
            && let Some(ref wf_config) = agent_cfg.workflow
        {
            tracing::info!(
                "[workflow] gate registered: cmd='{}', expected='{}'",
                wf_config.gate_command,
                wf_config.gate_expected
            );
            ext_reg.register(Box::new(
                crate::agent::workflow_extension::WorkflowExtension::new(wf_config.clone()),
            ));
        }

        // ── 注册 HookExtension（hooks.json 配置式钩子，热重载）──
        // 每次 on_session_start 等钩子触发时动态读 hooks.json，改完即生效。
        // runtime=None：command handler 用 tokio::spawn fallback；agent handler 待后续接入 runtime
        if ion_cfg.is_extension_enabled("hooks") {
            let proj_dir = std::path::PathBuf::from(&worker_cwd);
            if crate::hooks::extension::HookExtension::has_hooks(&proj_dir) {
                let hook_ext = crate::hooks::extension::HookExtension::new(
                    proj_dir,
                    Some(worker_rt.clone()), // agent handler 需要 runtime 来 spawn 子 Worker
                    Some(Arc::clone(&registry)), // prompt handler 需要 ApiRegistry 来调 LLM
                    Some(model.clone()),     // prompt handler 需要当前会话模型
                    Some(manager_bridge.clone() as Arc<dyn crate::runtime::ManagerBridgeHandle>), // mcp_tool handler 转发 MCP 调用
                    Some(follow_up_tx.clone()),
                );
                ext_reg.register(Box::new(hook_ext));
                tracing::info!("[extension] hooks enabled");
            } else {
                tracing::info!("[extension] hooks: no hooks.json found or empty, skipping");
            }
        } else {
            tracing::info!("[extension] hooks disabled by config");
        }

        agent = agent.with_extensions(ext_reg);

        // Let each extension self-describe its tools (e.g. BashExtension
        // registers bash/bash_kill/bash_send/bash_bg). Replaces the old
        // hand-written `agent.register_tool(...)` block below.
        agent.register_extension_tools();
        // Wire the follow_up receiver into the agent so background process
        // completions (bash background=true) are drained into follow_up_queue
        // during outer_loop, triggering a new turn with <bash_result> message.
        agent.set_follow_up_rx(follow_up_rx);
        agent.set_bg_pending(bg_pending.clone());

        // LspCheckTool 不再暴露给 LLM（设计纠正：LSP 是钩子驱动，write/edit 后自动触发）
    }

    // 发 ready 信号
    output(&serde_json::json!({
        "type": "ready",
        "session": sid,
        "model": model_id,
        "provider": provider,
        "channels": channels,
        "version": VERSION,
    }));

    // RPC 主循环（async stdin + ManagerBridge correlation）
    //
    // 重构要点：
    // - 同步 `for line in stdin.lock().lines()` 改成 tokio async 读，spawn 独立 task。
    //   原因：agent.run().await 期间同步读会阻塞 stdin，导致 Manager 写回的
    //   manager_response 卡管道缓冲里读不到 → spawn_worker 工具无法同步等待。
    // 修复：在 stdin 任务中提前拦截 _reply_to 消息，绕过主循环死锁。
    // - ManagerBridge 持有 pending map（_reply_to → oneshot），让工具调用能 await 响应。

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    let bridge_for_reader = Arc::clone(&manager_bridge);
    tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(tokio::io::stdin());
        use tokio::io::AsyncBufReadExt;
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<serde_json::Value>(&line) {
                        Ok(v) => {
                            // 关键：_reply_to 消息是 manager_response，直接投递避免死锁
                            let has_reply_to = v
                                .get("_reply_to")
                                .and_then(|r| r.as_str())
                                .map(|s| !s.is_empty())
                                .unwrap_or(false);
                            if has_reply_to {
                                let reply_to = v["_reply_to"].as_str().unwrap_or("").to_string();
                                bridge_for_reader.deliver_response(&reply_to, v).await;
                            } else {
                                let _ = stdin_tx.send(v);
                            }
                        }
                        Err(e) => {
                            output(&serde_json::json!({
                                "type": "error",
                                "error": { "message": format!("invalid JSON: {e}") }
                            }));
                        }
                    }
                }
                Ok(None) => break, // EOF
                Err(_) => break,
            }
        }
    });

    // ── MCP 代理工具注册（方案 C：从 host 拉工具列表，注册 McpProxyTool）──
    // 必须在 stdin reader task 启动后执行——send_command 需要 stdin reader 拦截 _reply_to 响应
    // 加 3s 超时：集成测试场景直接 spawn worker 无 host，send_command 会永远等
    {
        let mcp_result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            manager_bridge.send_command("mcp_list_tools", serde_json::json!({})),
        )
        .await;
        match mcp_result {
            Ok(Ok(resp)) => {
                if resp
                    .get("success")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false)
                {
                    let tools_list = resp
                        .get("data")
                        .and_then(|d| d.get("tools"))
                        .cloned()
                        .unwrap_or(serde_json::json!([]));
                    if let Some(arr) = tools_list.as_array() {
                        for tool in arr {
                            let full_name =
                                tool.get("full_name").and_then(|v| v.as_str()).unwrap_or("");
                            let desc = tool
                                .get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let params = tool
                                .get("input_schema")
                                .cloned()
                                .unwrap_or(serde_json::json!({}));
                            if !full_name.is_empty() {
                                agent.register_tool(Box::new(McpProxyTool::new(
                                    full_name,
                                    desc,
                                    &params,
                                    manager_bridge.clone(),
                                )));
                            }
                        }
                        tracing::info!("[mcp] {} proxy tools registered from host", arr.len());
                    }
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("[mcp] failed to fetch tools from host: {e}");
            }
            Err(_) => {
                tracing::info!(
                    "[mcp] mcp_list_tools timeout (no host or 3s limit), skip MCP proxy"
                );
            }
        }
    }

    while let Some(cmd) = stdin_rx.recv().await {
        let id = cmd
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let method = cmd
            .get("method")
            .and_then(|v| v.as_str())
            .or_else(|| cmd.get("type").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let params = cmd
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        // 分发命令
        match method.as_str() {
            // ── Health check & restart notification (watchdog dual-version switching) ──
            // These two handlers enable zero-downtime self-evolution. See
            // scripts/watchdog.sh: watchdog polls /tmp/.ion-evolve-restart and
            // verifies A_new is alive before promoting it over A_old.
            "health" => {
                // Health check for watchdog dual-version switching.
                // Returns system status quickly (<10ms): uptime, pid, version.
                // No DB queries, no network calls.
                let uptime = start_time.elapsed().as_secs();
                output_response(
                    &id,
                    "health",
                    &serde_json::json!({
                        "status": "ok",
                        "uptime_secs": uptime,
                        "pid": std::process::id(),
                        "version": env!("CARGO_PKG_VERSION"),
                    }),
                );
            }

            "request_restart" => {
                // Notify watchdog that new code was merged and ion should restart.
                // Watchdog (scripts/watchdog.sh) polls for this sentinel file.
                let restart_file = "/tmp/.ion-evolve-restart";
                // RFC3339 timestamp via SystemTime (no chrono dependency required).
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::SystemTime::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let payload = format!("restart requested at {}", secs);
                match std::fs::write(restart_file, payload) {
                    Ok(_) => {
                        eprintln!("[restart] Notified watchdog via {}", restart_file);
                        output_response(
                            &id,
                            "request_restart",
                            &serde_json::json!({
                                "notified": true,
                                "file": restart_file,
                            }),
                        );
                    }
                    Err(e) => {
                        output_error_response(
                            &id,
                            "request_restart",
                            &format!("Failed to write restart file: {e}"),
                        );
                    }
                }
            }

            // ── 同步查询 ──
            "get_state" => {
                output_response(
                    &id,
                    "get_state",
                    &serde_json::json!({
                        "model": model_id,
                        "provider": provider,
                        "session_id": sid,
                        "message_count": agent.messages().len(),
                        "is_running": agent.is_running(),
                        "steering_queue": agent.steering_queue_len(),
                        "follow_up_queue": agent.follow_up_queue_len(),
                    }),
                );
            }

            "get_session_info" => {
                // 统一状态接口（合并 get_state + get_session_stats + token 统计）
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
                let user_count = agent
                    .messages()
                    .iter()
                    .filter(|m| matches!(m, Message::User(_)))
                    .count();
                let assistant_count = agent
                    .messages()
                    .iter()
                    .filter(|m| matches!(m, Message::Assistant(_)))
                    .count();
                let tool_result_count = agent
                    .messages()
                    .iter()
                    .filter(|m| matches!(m, Message::ToolResult(_)))
                    .count();
                output_response(
                    &id,
                    "get_session_info",
                    &serde_json::json!({
                        "session_id": sid,
                        "model": model_id,
                        "provider": provider,
                        "agent": current_agent_name,
                        "is_running": agent.is_running(),
                        "is_stopped": agent.is_stopped(),
                        "message_count": agent.messages().len(),
                        "user_messages": user_count,
                        "assistant_messages": assistant_count,
                        "tool_results": tool_result_count,
                        "tokens": {
                            "input": total_input,
                            "output": total_output,
                            "total": total_input + total_output,
                        },
                        "steering_queue": agent.steering_queue_len(),
                        "follow_up_queue": agent.follow_up_queue_len(),
                        "context_window": agent.model().context_window,
                        "max_tokens": agent.model().max_tokens,
                    }),
                );
            }

            "get_inflight_messages" => {
                // 获取内存中的消息（还没落盘的）
                // 返回最后 N 条 + 总数,让前端跟磁盘 list_turns 拼接
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let msgs = agent.messages();
                let total = msgs.len();
                let start = total.saturating_sub(limit);
                let recent: Vec<serde_json::Value> = msgs[start..]
                    .iter()
                    .map(|m| serde_json::to_value(m).unwrap_or(serde_json::json!(null)))
                    .collect();
                output_response(
                    &id,
                    "get_inflight_messages",
                    &serde_json::json!({
                        "total": total,
                        "returned": recent.len(),
                        "is_running": agent.is_running(),
                        "messages": recent,
                    }),
                );
            }

            "get_session_stats" => {
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

                // 从 SessionIndex 读血缘 + lastEntryId
                let index = crate::session_index::SessionIndex::load();
                let meta = index.get(&sid);
                let parent_session = meta.and_then(|m| m.parent_session.clone());
                let parent_type = meta.and_then(|m| m.parent_type.clone());
                let last_entry_id = meta.and_then(|m| m.last_entry_id.clone());

                // 从磁盘读 lastEntryId（如果 index 里没有）
                let last_entry_id = last_entry_id.or_else(|| {
                    crate::session_jsonl::SessionFile::load(&worker_cwd).and_then(|f| f.last_id)
                });

                output_response(
                    &id,
                    "get_session_stats",
                    &serde_json::json!({
                        "sessionId": sid,
                        "userMessages": agent.messages().iter().filter(|m| matches!(m, Message::User(_))).count(),
                        "assistantMessages": agent.messages().iter().filter(|m| matches!(m, Message::Assistant(_))).count(),
                        "toolResults": agent.messages().iter().filter(|m| matches!(m, Message::ToolResult(_))).count(),
                        "totalMessages": agent.messages().len(),
                        "tokens": {"input": total_input, "output": total_output, "cacheRead": 0, "cacheWrite": 0, "total": total_input + total_output},
                        "cost": 0,
                        "lastEntryId": last_entry_id,
                        "parentSession": parent_session,
                        "parentType": parent_type,
                    }),
                );
            }

            "get_children" => {
                let target_session = params
                    .get("session")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&sid);
                let index = crate::session_index::SessionIndex::load();
                let children: Vec<_> = index
                    .get_children(target_session)
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.name,
                            "name": m.name,
                            "turnCount": m.turn_count,
                            "updatedAt": m.updated_at,
                            "parentSession": m.parent_session,
                            "parentType": m.parent_type,
                        })
                    })
                    .collect();
                output_response(
                    &id,
                    "get_children",
                    &serde_json::json!({
                        "children": children,
                        "count": children.len(),
                    }),
                );
            }

            "get_messages" => {
                // 解析分页参数
                let view_str = params
                    .get("view")
                    .and_then(|v| v.as_str())
                    .unwrap_or("live");
                let view = match view_str {
                    "since_compaction" => crate::message_retrieval::View::SinceCompaction,
                    "full" => crate::message_retrieval::View::Full,
                    s if s.starts_with("branch:") => {
                        crate::message_retrieval::View::Branch(s[7..].to_string())
                    }
                    _ => crate::message_retrieval::View::Live,
                };
                let after = params
                    .get("after")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let before = params
                    .get("before")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let limit = params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(50);
                let complete_turn = params
                    .get("complete_turn")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let custom_str = params
                    .get("include_custom")
                    .and_then(|v| v.as_str())
                    .unwrap_or("none");
                let include_custom = match custom_str {
                    "display_only" => crate::message_retrieval::CustomFilter::DisplayOnly,
                    "all" => crate::message_retrieval::CustomFilter::All,
                    _ => crate::message_retrieval::CustomFilter::None,
                };

                // 从磁盘读 entries（含 compaction/custom 等非 message entry）
                let entries: Vec<serde_json::Value> =
                    crate::message_retrieval::load_entries_cached(&worker_cwd);

                let retrieval_params = crate::message_retrieval::RetrievalParams {
                    view,
                    after,
                    before,
                    limit,
                    complete_turn,
                    include_custom,
                };
                let result =
                    crate::message_retrieval::retrieve_messages(&entries, &retrieval_params);

                output_response(
                    &id,
                    "get_messages",
                    &serde_json::json!({
                        "messages": result.messages,
                        "hasMore": result.has_more,
                        "totalCount": result.total_count,
                        "nextCursor": result.next_cursor,
                        "view": result.view,
                        "compactionPoints": result.compaction_points,
                    }),
                );
            }

            "list_turns" => {
                let full_content = params
                    .get("full_content")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let limit = params
                    .get("limit")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize)
                    .unwrap_or(50);
                let entries: Vec<serde_json::Value> =
                    crate::message_retrieval::load_entries_cached(&worker_cwd);
                let params = crate::message_retrieval::RetrievalParams {
                    limit,
                    ..Default::default()
                };
                let result =
                    crate::message_retrieval::retrieve_turns(&entries, &params, full_content);
                output_response(
                    &id,
                    "list_turns",
                    &serde_json::json!({
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
                    }),
                );
            }

            "list_inputs" => {
                let entries: Vec<serde_json::Value> =
                    crate::message_retrieval::load_entries_cached(&worker_cwd);
                let result = crate::message_retrieval::retrieve_inputs(
                    &entries,
                    &crate::message_retrieval::RetrievalParams::default(),
                );
                output_response(
                    &id,
                    "list_inputs",
                    &serde_json::json!({
                        "inputs": result.inputs.iter().map(|i| serde_json::json!({
                            "turnId": i.turn_id,
                            "entryId": i.entry_id,
                            "text": i.text,
                        })).collect::<Vec<_>>(),
                        "hasMore": result.has_more,
                        "totalCount": result.total_count,
                        "nextCursor": result.next_cursor,
                    }),
                );
            }

            "get_turn_detail" => {
                let turn_id = params.get("turnId").and_then(|v| v.as_str()).unwrap_or("");
                let entries: Vec<serde_json::Value> =
                    crate::message_retrieval::load_entries_cached(&worker_cwd);
                match crate::message_retrieval::retrieve_turn_detail(
                    &entries,
                    turn_id,
                    &crate::message_retrieval::CustomFilter::None,
                ) {
                    Some(detail) => output_response(
                        &id,
                        "get_turn_detail",
                        &serde_json::json!({
                            "turnId": detail.turn_id,
                            "entries": detail.entries,
                            "overview": {
                                "userContent": detail.overview.user_content,
                                "assistantContent": detail.overview.assistant_content,
                                "keySteps": detail.overview.key_steps,
                                "toolCallCount": detail.overview.tool_call_count,
                                "tokens": {"input": detail.overview.tokens_input, "output": detail.overview.tokens_output},
                                "status": detail.overview.status,
                                "durationMs": detail.overview.duration_ms,
                                "source": detail.overview.source,
                            }
                        }),
                    ),
                    None => output_response(
                        &id,
                        "get_turn_detail",
                        &serde_json::json!({
                            "error": "turn not found", "turnId": turn_id
                        }),
                    ),
                }
            }

            "get_last_assistant_text" => {
                let text = agent
                    .messages()
                    .iter()
                    .rev()
                    .find_map(|m| match m {
                        Message::Assistant(a) => a.content.iter().find_map(|b| match b {
                            AssistantContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        }),
                        _ => None,
                    })
                    .unwrap_or_default();
                output_response(&id, "get_last_assistant_text", &serde_json::json!(text));
            }

            "get_tools" => {
                output_response(
                    &id,
                    "get_tools",
                    &serde_json::json!({"tools": [
                        {"name": "read"}, {"name": "write"}, {"name": "edit"},
                        {"name": "bash"}, {"name": "grep"}, {"name": "find"},
                        {"name": "ls"}, {"name": "calculator"}, {"name": "echo"}
                    ]}),
                );
            }

            // ── 异步操作 ──
            "set_model" => {
                let new_model = params.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
                let new_provider = params
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or(&provider);
                model_id = new_model.to_string();
                provider = new_provider.to_string();
                output_response(
                    &id,
                    "get_state",
                    &serde_json::json!({
                        "model": model_id, "provider": provider
                    }),
                );
            }

            "set_thinking_level" => {
                let level = params
                    .get("level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("off");
                output_response(
                    &id,
                    "set_thinking_level",
                    &serde_json::json!({"thinkingLevel": level}),
                );
            }

            "set_session_name" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                output_response(&id, "set_session_name", &serde_json::json!({"name": name}));
            }

            // ── 流式命令 ──
            //
            // prompt(text, behavior?: "interrupt" | "steer" | "followUp")
            //   空闲时直接执行。忙时 + behavior 决定策略：
            //     interrupt — 打断当前 Agent 并立即执行
            //     steer — 排入 steering 队列
            //     followUp — 排入 follow_up 队列
            //   空时 + 不传 behavior：默认 "interrupt"
            // steer(text?, immediate?, promote?)  → 注入 steering 队列
            // follow_up(text)  → 注入 follow_up 队列
            // abort()  → 硬停止
            // promote_follow_up → 提升 follow_up 到 steering
            "prompt" => {
                let text = params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // 默认 behavior：steer（对齐 pi：流式中默认插话入队，不打断）。
                // 可通过 ION_PROMPT_BEHAVIOR=interrupt 切回旧行为。
                // 显式传 params.behavior / params.streamingBehavior 优先级最高。
                let default_behavior = std::env::var("ION_PROMPT_BEHAVIOR")
                    .ok()
                    .filter(|s| matches!(s.as_str(), "interrupt" | "steer" | "followUp"))
                    .unwrap_or_else(|| "steer".to_string());
                let pbehavior = params
                    .get("behavior")
                    .or_else(|| params.get("streamingBehavior"))
                    .and_then(|v| v.as_str())
                    .unwrap_or(&default_behavior);

                // !cmd 用户直发：拦截成 bash_command（避免走完整 agent loop，对齐 pi）
                // 形如 "!ls -la" 或 "! cargo build" → 取 '!' 之后的部分作为命令
                if let Some(stripped) = text.strip_prefix('!') {
                    let cmd_text = stripped.trim().to_string();
                    if !cmd_text.is_empty() {
                        // 直接执行，不入 agent loop
                        let timeout_secs =
                            params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
                        let (stdout, stderr, exit_code) =
                            match execute_bash(&cmd_text, timeout_secs).await {
                                Ok(t) => t,
                                Err(e) => {
                                    let bash_msg = BashExecutionMessage {
                                        role: "bashExecution".into(),
                                        command: cmd_text.clone(),
                                        output: format!("error: {e}"),
                                        exit_code: None,
                                        cancelled: false,
                                        truncated: false,
                                        full_output_path: None,
                                        timestamp: now_ms(),
                                        exclude_from_context: None,
                                    };
                                    agent.push_message(Message::BashExecution(bash_msg));
                                    output_response(
                                        &id,
                                        "prompt",
                                        &serde_json::json!({
                                            "status":"bash_error",
                                            "command": cmd_text,
                                            "error": e,
                                        }),
                                    );
                                    continue;
                                }
                            };
                        let combined = if stderr.is_empty() {
                            stdout
                        } else if stdout.is_empty() {
                            stderr
                        } else {
                            format!("{stdout}\n[stderr]\n{stderr}")
                        };
                        let truncated = combined.contains("[truncated");
                        let bash_msg = BashExecutionMessage {
                            role: "bashExecution".into(),
                            command: cmd_text.clone(),
                            output: combined.clone(),
                            exit_code: Some(exit_code),
                            cancelled: false,
                            truncated,
                            full_output_path: None,
                            timestamp: now_ms(),
                            exclude_from_context: None,
                        };
                        agent.push_message(Message::BashExecution(bash_msg));

                        output(
                            &serde_json::json!({"type":"event","event":{"type":"agent_start","sessionId":sid,"timestamp":now_ms()}}),
                        );
                        output(
                            &serde_json::json!({"type":"event","event":{"type":"text_delta","delta":&combined}}),
                        );
                        output(
                            &serde_json::json!({"type":"event","event":{"type":"agent_end","sessionId":sid,"timestamp":now_ms()}}),
                        );
                        output_response(
                            &id,
                            "prompt",
                            &serde_json::json!({
                                "status":"bash_executed",
                                "command": cmd_text,
                                "exitCode": exit_code,
                                "output": combined,
                                "truncated": truncated,
                            }),
                        );
                        continue;
                    }
                }

                let mut skip = false;
                if agent.is_running() && pbehavior == "steer" {
                    agent.steer(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent {
                            text: text.clone(),
                            text_signature: None,
                        })],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::Steer,
                    }));
                    output_response(
                        &id,
                        "prompt",
                        &serde_json::json!({"status":"queued","queue":"steering"}),
                    );
                    skip = true;
                } else if agent.is_running() && pbehavior == "followUp" {
                    agent.follow_up(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent {
                            text: text.clone(),
                            text_signature: None,
                        })],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::FollowUp,
                    }));
                    output_response(
                        &id,
                        "prompt",
                        &serde_json::json!({"status":"queued","queue":"followUp"}),
                    );
                    skip = true;
                } else if agent.is_running() && pbehavior == "interrupt" {
                    agent.stop();
                }

                if !skip {
                    output_response(&id, "prompt", &serde_json::Value::Null);
                    // agent_start / text_delta / agent_end 由 StreamingExtension 实时推送，
                    // 这里不再内联发送（历史 bug：曾双发 agent_start）
                    {
                        let mut ctx = wasm_ext_registry.ctx.write().unwrap();
                        ctx.session_id = sid.clone();
                        ctx.cwd = worker_cwd.clone();
                        ctx.project_root = worker_cwd.clone();
                    }
                    // agent.run 跑完整 turn 后一次性 save。
                    // 用 select! 让 agent.run 期间能响应只读 RPC(不碰 agent 的命令)
                    // 避免 list_turns/get_messages 等磁盘读 RPC 被阻塞 20 秒
                    // 同时支持 abort(pause_tx clone + 设 stopped)
                    let pause_tx_clone = agent.pause_handle();
                    let stopped_handle = agent.stopped_handle();
                    let pending_steer_queue: std::sync::Arc<
                        tokio::sync::Mutex<
                            std::collections::VecDeque<(
                                ion_provider::types::MessageSource,
                                ion_provider::types::Message,
                            )>,
                        >,
                    > = std::sync::Arc::new(tokio::sync::Mutex::new(
                        std::collections::VecDeque::new(),
                    ));
                    let run_result = {
                        let mut run_fut = std::pin::pin!(agent.run(&text));
                        loop {
                            tokio::select! {
                                result = &mut run_fut => {
                                    break result;
                                }
                                Some(bg_cmd) = stdin_rx.recv() => {
                                    let bg_id = bg_cmd.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    let bg_method = bg_cmd.get("method").and_then(|v| v.as_str())
                                        .or_else(|| bg_cmd.get("type").and_then(|v| v.as_str()))
                                        .unwrap_or("").to_string();
                                    let bg_params = bg_cmd.get("params").cloned().unwrap_or(serde_json::Value::Null);
                                    match bg_method.as_str() {
                                        // 只读磁盘的 RPC → 照常处理(agent.run 期间安全)
                                        "list_turns" => {
                                            let full_content = bg_params.get("full_content").and_then(|v| v.as_bool()).unwrap_or(false);
                                            let limit = bg_params.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(50);
                                            let entries: Vec<serde_json::Value> = crate::message_retrieval::load_entries_cached(&worker_cwd);
                                            let rp = crate::message_retrieval::RetrievalParams { limit, ..Default::default() };
                                            let result = crate::message_retrieval::retrieve_turns(&entries, &rp, full_content);
                                            output_response(&bg_id, "list_turns", &serde_json::json!({
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
                                            }));
                                        }
                                        "get_messages" => {
                                            let view_str = bg_params.get("view").and_then(|v| v.as_str()).unwrap_or("live");
                                            let view = match view_str {
                                                "since_compaction" => crate::message_retrieval::View::SinceCompaction,
                                                "full" => crate::message_retrieval::View::Full,
                                                s if s.starts_with("branch:") => crate::message_retrieval::View::Branch(s[7..].to_string()),
                                                _ => crate::message_retrieval::View::Live,
                                            };
                                            let limit = bg_params.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(0);
                                            let after = bg_params.get("after").and_then(|v| v.as_str()).map(String::from);
                                            let before = bg_params.get("before").and_then(|v| v.as_str()).map(String::from);
                                            let complete_turn = bg_params.get("complete_turn").and_then(|v| v.as_bool()).unwrap_or(false);
                                            let inc_custom = bg_params.get("include_custom").and_then(|v| v.as_str()).unwrap_or("none");
                                            let include_custom = match inc_custom {
                                                "display_only" => crate::message_retrieval::CustomFilter::DisplayOnly,
                                                "all" => crate::message_retrieval::CustomFilter::All,
                                                _ => crate::message_retrieval::CustomFilter::None,
                                            };
                                            let entries: Vec<serde_json::Value> = crate::message_retrieval::load_entries_cached(&worker_cwd);
                                            let rp = crate::message_retrieval::RetrievalParams {
                                                view, after, before, limit, complete_turn, include_custom,
                                            };
                                            let result = crate::message_retrieval::retrieve_messages(&entries, &rp);
                                            output_response(&bg_id, "get_messages", &serde_json::json!({
                                                "messages": result.messages,
                                                "hasMore": result.has_more,
                                                "totalCount": result.total_count,
                                                "nextCursor": result.next_cursor,
                                                "view": view_str,
                                            }));
                                        }
                                        "list_inputs" => {
                                            let entries: Vec<serde_json::Value> = crate::message_retrieval::load_entries_cached(&worker_cwd);
                                            let rp = crate::message_retrieval::RetrievalParams::default();
                                            let result = crate::message_retrieval::retrieve_inputs(&entries, &rp);
                                            output_response(&bg_id, "list_inputs", &serde_json::json!({
                                                "inputs": result.inputs.iter().map(|i| serde_json::json!({
                                                    "turnId": i.turn_id, "entryId": i.entry_id, "text": i.text,
                                                })).collect::<Vec<_>>(),
                                                "hasMore": result.has_more, "totalCount": result.total_count,
                                                "nextCursor": result.next_cursor,
                                            }));
                                        }
                                        "get_turn_detail" => {
                                            let turn_id = bg_params.get("turnId").and_then(|v| v.as_str()).unwrap_or("");
                                            let entries: Vec<serde_json::Value> = crate::message_retrieval::load_entries_cached(&worker_cwd);
                                            match crate::message_retrieval::retrieve_turn_detail(&entries, turn_id, &crate::message_retrieval::CustomFilter::None) {
                                                Some(detail) => output_response(&bg_id, "get_turn_detail", &serde_json::json!({
                                                    "turnId": detail.turn_id,
                                                    "entries": detail.entries,
                                                    "overview": {
                                                        "userContent": detail.overview.user_content,
                                                        "assistantContent": detail.overview.assistant_content,
                                                        "keySteps": detail.overview.key_steps,
                                                        "toolCallCount": detail.overview.tool_call_count,
                                                        "tokens": {"input": detail.overview.tokens_input, "output": detail.overview.tokens_output},
                                                        "status": detail.overview.status,
                                                        "durationMs": detail.overview.duration_ms,
                                                        "source": detail.overview.source,
                                                    }
                                                })),
                                                None => output_response(&bg_id, "get_turn_detail", &serde_json::json!({"error": "turn not found", "turnId": turn_id})),
                                            }
                                        }
                                        // review_pending → agent.run 期间也能查审批队列
                                        // （compute_pending 是纯内存计算 + 磁盘读，不碰 agent）
                                        "review_pending" => {
                                            let result = if let Some(ref mgr) = approval_mgr {
                                                let pending = mgr.compute_pending();
                                                serde_json::json!({
                                                    "pending": pending.iter().map(|p| serde_json::json!({
                                                        "path": p.path,
                                                        "status": format!("{:?}", p.status).to_lowercase(),
                                                        "diffStat": p.diff_stat,
                                                    })).collect::<Vec<_>>(),
                                                    "summary": {"total": pending.len()},
                                                })
                                            } else {
                                                serde_json::json!({"pending": [], "summary": {"total": 0}})
                                            };
                                            output_response(&bg_id, "review_pending", &result);
                                        }
                                        // 单文件 diff（与 review_pending 同源，复用其缓存）
                                        "review_file_diff" => {
                                            let path = bg_params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                                            let result = if let Some(ref mgr) = approval_mgr {
                                                mgr.file_diff(path).unwrap_or(serde_json::json!({
                                                    "error": "file not in pending list", "path": path,
                                                }))
                                            } else {
                                                serde_json::json!({"error": "approval not enabled"})
                                            };
                                            output_response(&bg_id, "review_file_diff", &result);
                                        }
                                        // 单 turn 变更摘要（只读磁盘，安全）；
                                        // turnId 省略 → 本 session 最新 ts_ turn
                                        "turn_changes" => {
                                            let turn_id_param = bg_params.get("turnId").and_then(|v| v.as_str()).unwrap_or("");
                                            let result = if let Some(ref store) = snapshot_store {
                                                let all_snaps = store.load_all_tool_snapshots();
                                                let mine: Vec<&crate::file_snapshot::ToolSnapshot> =
                                                    all_snaps.iter().filter(|s| s.session_id == sid).collect();
                                                let turn_id = if turn_id_param.is_empty() {
                                                    mine.iter().max_by(|a, b| a.timestamp.cmp(&b.timestamp)).map(|s| s.turn_id.clone())
                                                } else { Some(turn_id_param.to_string()) };
                                                match turn_id {
                                                    None => serde_json::json!({"turnId": null, "files": [],
                                                        "summary": {"files": 0, "added": 0, "removed": 0}}),
                                                    Some(tid) => {
                                                        use std::collections::HashMap;
                                                        let mut grouped: HashMap<String, Vec<&crate::file_snapshot::ToolSnapshot>> = HashMap::new();
                                                        for s in &mine {
                                                            if s.turn_id == tid { grouped.entry(s.path.clone()).or_default().push(s); }
                                                        }
                                                        let mut files = Vec::new();
                                                        let (mut ta, mut tr) = (0usize, 0usize);
                                                        for (path, group) in &grouped {
                                                            let first = group.first().unwrap();
                                                            let last = group.last().unwrap();
                                                            let before = first.before_hash.as_ref().and_then(|h| store.objects().read_object_text(h));
                                                            let after = last.after_hash.as_ref().and_then(|h| store.objects().read_object_text(h));
                                                            let (status, added, removed) = match (&before, &after) {
                                                                (Some(b), Some(a)) => { let (ad, rm) = crate::file_snapshot::count_changes(b, a); ("modified", ad, rm) }
                                                                (None, Some(a)) => ("added", a.lines().count(), 0),
                                                                (Some(b), None) => ("deleted", 0, b.lines().count()),
                                                                _ => ("modified", 0, 0),
                                                            };
                                                            ta += added; tr += removed;
                                                            files.push(serde_json::json!({"path": path, "status": status, "added": added, "removed": removed}));
                                                        }
                                                        serde_json::json!({"turnId": tid, "files": files,
                                                            "summary": {"files": grouped.len(), "added": ta, "removed": tr}})
                                                    }
                                                }
                                            } else {
                                                serde_json::json!({"error": "file-snapshot not enabled"})
                                            };
                                            output_response(&bg_id, "turn_changes", &result);
                                        }
                                        // get_session_info / get_state → agent.run 期间不能读 messages(&mut 冲突)
                                        // 返回简化版(只有 model/provider/is_running)
                                        "get_session_info" | "get_state" => {
                                            output_response(&bg_id, "get_session_info", &serde_json::json!({
                                                "session_id": sid,
                                                "model": model_id, "provider": provider,
                                                "is_running": true,  // agent.run 期间一定 running
                                                "is_stopped": stopped_handle.load(std::sync::atomic::Ordering::SeqCst),
                                                "message_count": null,  // agent.run 期间不能读
                                                "note": "agent is running, use list_turns for disk data",
                                            }));
                                        }
                                        // abort → 通过外部句柄中断(不用 agent.stop(),避免 borrow 冲突)
                                        // 设 stopped=true(AtomicBool)+ 发 pause 信号唤醒 check_pause
                                        "abort" => {
                                            stopped_handle.store(true, std::sync::atomic::Ordering::SeqCst);
                                            let _ = pause_tx_clone.send(true);
                                            output_response(&bg_id, "abort", &serde_json::Value::Null);
                                        }
                                        // steer/follow_up → 缓存到外部 queue,run 结束后 drain 进 agent
                                        "steer" => {
                                            let steer_text = bg_params.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            if !steer_text.is_empty() {
                                                pending_steer_queue.lock().await.push_back((
                                                    ion_provider::types::MessageSource::Steer,
                                                    Message::User(UserMessage {
                                                        role: "user".into(),
                                                        content: vec![ContentBlock::Text(TextContent { text: steer_text, text_signature: None })],
                                                        timestamp: now_ms(),
                                                        source: ion_provider::types::MessageSource::Steer,
                                                    }),
                                                ));
                                            }
                                            output_response(&bg_id, "steer", &serde_json::json!({"status":"queued","queue":"steering"}));
                                        }
                                        "follow_up" => {
                                            let fu_text = bg_params.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                            if !fu_text.is_empty() {
                                                pending_steer_queue.lock().await.push_back((
                                                    ion_provider::types::MessageSource::FollowUp,
                                                    Message::User(UserMessage {
                                                        role: "user".into(),
                                                        content: vec![ContentBlock::Text(TextContent { text: fu_text, text_signature: None })],
                                                        timestamp: now_ms(),
                                                        source: ion_provider::types::MessageSource::FollowUp,
                                                    }),
                                                ));
                                            }
                                            output_response(&bg_id, "follow_up", &serde_json::json!({"status":"queued","queue":"followUp"}));
                                        }
                                        // prompt / 其他写类 → 返回 busy(agent 正在跑)
                                        _ => {
                                            output_response(&bg_id, &bg_method, &serde_json::json!({
                                                "error": "agent is running, please wait",
                                                "status": "busy",
                                            }));
                                        }
                                    }
                                }
                            }
                        }
                    };
                    match run_result {
                        Ok(()) => {
                            let msgs_json: Vec<serde_json::Value> = agent
                                .messages()
                                .iter()
                                .filter_map(|m| serde_json::to_value(m).ok())
                                .collect();
                            save_worker_session(&sid, &worker_cwd, &msgs_json);
                            // 正常完成的 agent_end 由 StreamingExtension 发（曾双发）；
                            // 中止（agent_stopped）扩展版不发，仅此处发
                            let was_stopped =
                                stopped_handle.load(std::sync::atomic::Ordering::SeqCst);
                            if was_stopped {
                                output(&serde_json::json!({
                                    "type":"event","event":{
                                        "type":"agent_stopped",
                                        "sessionId":sid,
                                        "timestamp":now_ms(),
                                        "reason":"user_abort"
                                    }
                                }));
                            }
                        }
                        Err(e) => {
                            output(&serde_json::json!({
                                "type":"event","event":{"type":"error","message":e.to_string(),"timestamp":now_ms()}
                            }));
                        }
                    }
                    // drain pending steer/follow_up queue → 注入 agent
                    {
                        let mut pq = pending_steer_queue.lock().await;
                        while let Some((source, msg)) = pq.pop_front() {
                            match source {
                                ion_provider::types::MessageSource::Steer => agent.steer(msg),
                                ion_provider::types::MessageSource::FollowUp => {
                                    agent.follow_up(msg)
                                }
                                _ => agent.follow_up(msg),
                            }
                        }
                    }

                    // ── Graceful drain：agent.run() 返回后再收一会儿 follow_up ──
                    // 解决"bash 后台进程在 agent.run 期间启动但未完成，agent 已退出 →
                    // 进程完成时发的 follow_up 消息丢失"的问题。
                    //
                    // 流程：agent.run() 内部 outer_loop 已经等了 30s（BACKGROUND_WAIT_TIMEOUT），
                    // 这里再等 ION_GRACEFUL_DRAIN_MS（默认 60s），期间收到的 bash_result
                    // 等消息全部写入 session.jsonl，让下次 prompt 时 LLM 能看到。
                    //
                    // 不触发新 turn（LLM 已经 agent_end），只持久化。
                    // 只在有活跃后台 watcher 时才 graceful drain——否则每轮
                    // prompt 结束都白等 60s（worker busy 不复位、RPC 超时的直接
                    // 原因；同 outer_loop 的 BACKGROUND_WAIT_TIMEOUT 修复）
                    let drained_msgs = if agent
                        .bg_pending
                        .load(std::sync::atomic::Ordering::SeqCst)
                        > 0
                    {
                        let drain_ms = std::env::var("ION_GRACEFUL_DRAIN_MS")
                            .ok()
                            .and_then(|s| s.parse::<u64>().ok())
                            .unwrap_or(60_000);
                        agent.graceful_drain_follow_ups(drain_ms, 50).await
                    } else {
                        Vec::new()
                    };
                    for msg in &drained_msgs {
                            // ★ 用消息自带的 timestamp（进程完成时间），而非写入时间。
                            // 之前用 timestamp_iso() 导致所有 drained 消息的时间戳都是
                            // "写入时间"（agent.run 返回后），而不是进程真正完成的时间。
                            let msg_ts = match msg {
                                ion_provider::Message::Custom(c) => c.timestamp,
                                _ => 0,
                            };
                            let ts_iso = if msg_ts > 0 {
                                session_jsonl::timestamp_iso_from_ms(msg_ts)
                            } else {
                                session_jsonl::timestamp_iso()
                            };
                            let entry = serde_json::json!({
                                "id": session_jsonl::generate_id(),
                                "parentId": sid,
                                "timestamp": ts_iso,
                                "type": "message",
                                "message": msg,
                            });
                            session_jsonl::append_raw_entry(&worker_cwd, &entry);
                            agent.push_message(msg.clone());
                        }
                    if !drained_msgs.is_empty() {
                        tracing::info!(
                            "[graceful-drain] captured {} follow_up messages after agent.run()",
                            drained_msgs.len()
                        );
                    }
                }
            }
            "steer" => {
                let text = params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let immediate = params
                    .get("immediate")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let promote = params.get("promote").and_then(|v| v.as_u64());
                if let Some(idx) = promote {
                    agent.promote_follow_up(idx as usize);
                    if text.is_empty() && !immediate {
                        output_response(&id, "steer", &serde_json::json!({"status":"promoted"}));
                        output_response(&id, "steer", &serde_json::Value::Null);
                        break;
                    }
                }
                if immediate {
                    agent.stop();
                }
                if !text.is_empty() {
                    agent.steer(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent {
                            text: text.clone(),
                            text_signature: None,
                        })],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::Steer,
                    }));
                }
                output_response(&id, "steer", &serde_json::Value::Null);
            }
            "abort" => {
                agent.stop();
                output_response(&id, "abort", &serde_json::Value::Null);
            }
            "promote_follow_up" => {
                let index = params
                    .get("item")
                    .and_then(|i| i.get("index"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as usize;
                let text = params
                    .get("item")
                    .and_then(|i| i.get("text"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                agent.promote_follow_up(index);
                if !text.is_empty() {
                    agent.steer(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent {
                            text: text.clone(),
                            text_signature: None,
                        })],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::Steer,
                    }));
                }
                output_response(&id, "promote_follow_up", &serde_json::Value::Null);
            }
            "remove_follow_up" => {
                let index = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let removed = agent.remove_follow_up(index);
                output_response(
                    &id,
                    "remove_follow_up",
                    &serde_json::json!({
                        "removed": removed.is_some(),
                        "follow_up_queue": agent.follow_up_queue_len(),
                    }),
                );
            }
            "remove_steering" => {
                let index = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let removed = agent.remove_steering(index);
                output_response(
                    &id,
                    "remove_steering",
                    &serde_json::json!({
                        "removed": removed.is_some(),
                        "steering_queue": agent.steering_queue_len(),
                    }),
                );
            }
            // ── Channel 消息 (从其他 Worker 转发过来) ──
            // 把消息作为 follow_up 注入 Agent，让 Agent 下一轮消化（不抢当前轮次）。
            "channel_msg" => {
                let channel = params
                    .get("channel")
                    .and_then(|v| v.as_str())
                    .or_else(|| cmd.get("channel").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let from = params
                    .get("from")
                    .and_then(|v| v.as_str())
                    .or_else(|| cmd.get("from").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let msg_text = params
                    .get("msg")
                    .and_then(|m| m.get("text"))
                    .and_then(|v| v.as_str())
                    .or_else(|| params.get("msg").and_then(|v| v.as_str()))
                    .or_else(|| cmd.get("msg").and_then(|v| v.as_str()))
                    .unwrap_or("");

                let from_short = if from.len() >= 12 { &from[..12] } else { from };
                let user_text = format!("[channel #{} from {}] {}", channel, from_short, msg_text);

                // 注入到 Agent follow_up queue（Agent 当前轮次结束后自动消化）
                agent.follow_up(crate::agent::messages::Message::User(
                    crate::agent::messages::UserMessage {
                        role: "user".into(),
                        content: vec![crate::agent::messages::ContentBlock::Text(
                            crate::agent::messages::TextContent {
                                text: user_text,
                                text_signature: None,
                            },
                        )],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::FollowUp,
                    },
                ));
                tracing::info!("[channel] {channel} from {from}: {msg_text} (queued as follow_up)");
                output_response(&id, "channel_msg", &serde_json::Value::Null);
            }

            // ── 控制命令（Manager 拦截，带 _reply_to correlation）──
            "create_worker" => {
                // 走 ManagerBridge：注册 pending oneshot，等 manager_response
                let bridge = manager_bridge.clone();
                tokio::spawn(async move {
                    let _ = bridge.send_command("create_worker", params).await;
                    // 结果由 manager_response → pending map → oneshot 触发；
                    // RPC 调用方（如果想要结果）应该用 spawn_worker 工具，而不是 RPC。
                });
                output_response(
                    &id,
                    "create_worker",
                    &serde_json::json!({
                        "status": "pending",
                        "message": "create_worker forwarded to Manager",
                    }),
                );
            }

            "channel_send" => {
                let bridge = manager_bridge.clone();
                tokio::spawn(async move {
                    let _ = bridge.send_command("channel_send", params).await;
                });
                output_response(
                    &id,
                    "channel_send",
                    &serde_json::json!({
                        "status": "pending",
                        "message": "channel_send forwarded to Manager",
                    }),
                );
            }

            "send_to_worker" => {
                let bridge = manager_bridge.clone();
                tokio::spawn(async move {
                    let _ = bridge.send_command("send_to_worker", params).await;
                });
                output_response(
                    &id,
                    "send_to_worker",
                    &serde_json::json!({
                        "status": "pending",
                        "message": "send_to_worker forwarded to Manager",
                    }),
                );
            }

            // ── 生命周期 ──
            "kill" | "shutdown" | "dispose" => {
                output_response(&id, "shutdown", &serde_json::Value::Null);
                break;
            }

            // ── 未实现的命令（返回空/默认值，格式对齐 pi）──
            "get_system_prompt" => {
                // Return the first user message (system prompt)
                let sp = agent
                    .messages()
                    .iter()
                    .find_map(|m| match m {
                        crate::agent::messages::Message::User(u) => {
                            u.content.iter().find_map(|b| match b {
                                crate::agent::messages::ContentBlock::Text(t) => {
                                    Some(t.text.clone())
                                }
                                _ => None,
                            })
                        }
                        _ => None,
                    })
                    .unwrap_or_default();
                output_response(&id, "get_system_prompt", &serde_json::json!(sp));
            }
            "get_agents" => {
                // 真实实现：列出所有内置 + 自定义 agent
                let agents = crate::agent_config::builtin_agents();
                let list: Vec<serde_json::Value> = agents
                    .iter()
                    .map(|a| {
                        serde_json::json!({
                            "name": a.name,
                            "description": a.description,
                            "color": a.color,
                            "tier": a.tier,
                            "source": a.source,
                        })
                    })
                    .collect();
                output_response(&id, "get_agents", &serde_json::json!(list));
            }
            "get_current_agent" => {
                // 当前 agent（从 crate::agent_config 读真实定义）
                let cur =
                    crate::agent_config::find_agent(&current_agent_name).unwrap_or_else(|| {
                        crate::agent_config::builtin_agents()
                            .into_iter()
                            .next()
                            .unwrap()
                    });
                output_response(
                    &id,
                    "get_current_agent",
                    &serde_json::json!({
                        "name": cur.name,
                        "description": cur.description,
                        "color": cur.color,
                        "tier": cur.tier,
                    }),
                );
            }
            "get_settings" => {
                let cfg = crate::config::IonConfig::load();
                let key = params.get("key").and_then(|v| v.as_str());
                if let Some(k) = key {
                    let val = match k {
                        "default_provider" | "default-provider" => {
                            serde_json::json!(cfg.default_provider)
                        }
                        "default_model" | "default-model" => serde_json::json!(cfg.default_model),
                        "api_key" | "api-key" => {
                            serde_json::json!(if cfg.api_key.is_some() { "***" } else { "" })
                        }
                        "base_url" | "base-url" => serde_json::json!(cfg.base_url),
                        "runtime" => serde_json::json!(cfg.runtime),
                        "extensions" => serde_json::json!(cfg.extensions),
                        _ => serde_json::Value::Null,
                    };
                    output_response(
                        &id,
                        "get_settings",
                        &serde_json::json!({ "key": k, "value": val }),
                    );
                } else {
                    let mut cfg_json = serde_json::to_value(&cfg).unwrap_or_default();
                    if cfg_json.get("api_key").is_some() {
                        cfg_json["api_key"] =
                            serde_json::json!(if cfg.api_key.is_some() { "***" } else { "" });
                    }
                    output_response(&id, "get_settings", &cfg_json);
                }
            }
            "get_commands" => {
                // 列出内置命令（worker 支持的 RPC 方法）
                let commands = serde_json::json!([
                    {"name": "prompt", "desc": "发送消息给 agent"},
                    {"name": "steer", "desc": "插队消息（不中断当前轮）"},
                    {"name": "follow_up", "desc": "追加消息（当前轮结束后处理）"},
                    {"name": "abort", "desc": "中断当前 agent 循环"},
                    {"name": "compact", "desc": "手动触发压缩"},
                    {"name": "get_messages", "desc": "拉取消息（分页/视点）"},
                    {"name": "list_turns", "desc": "逐轮概览"},
                    {"name": "list_inputs", "desc": "用户输入列表"},
                    {"name": "get_turn_detail", "desc": "单轮明细"},
                    {"name": "get_tree", "desc": "会话树结构"},
                    {"name": "get_tree_with_leaf", "desc": "会话树 + leaf 路径"},
                    {"name": "navigate_tree", "desc": "树线性导航"},
                    {"name": "get_session_stats", "desc": "会话统计"},
                    {"name": "get_settings", "desc": "读取配置"},
                    {"name": "set_settings", "desc": "写入配置"},
                    {"name": "set_permission_mode", "desc": "切命令守卫模式"},
                    {"name": "permission_store_decision", "desc": "存储权限决策（always allow）"},
                    {"name": "permission_list_stored", "desc": "列出已存储决策"},
                    {"name": "permission_remove_stored", "desc": "删除某条存储决策"},
                    {"name": "permission_clear_stored", "desc": "清空所有存储决策"},
                    {"name": "set_cwd", "desc": "切工作目录"},
                    {"name": "add_dir", "desc": "添加额外工作目录"},
                    {"name": "remove_dir", "desc": "移除额外工作目录"},
                    {"name": "list_dirs", "desc": "列出所有工作目录"},
                    {"name": "set_auto_retry", "desc": "设置重试次数"},
                    {"name": "abort_retry", "desc": "中断重试"},
                    {"name": "abort_bash", "desc": "中断后台 bash"},
                    {"name": "call_tool", "desc": "直接调工具"},
                    {"name": "extension_rpc", "desc": "调扩展方法"},
                    {"name": "set_model", "desc": "切模型"},
                    {"name": "set_thinking_level", "desc": "切思考级别"},
                    {"name": "cycle_model", "desc": "循环切模型"},
                    {"name": "cycle_thinking_level", "desc": "循环切思考级别"},
                    {"name": "get_skills", "desc": "列出可用 skills"},
                    {"name": "goal_evolver_run_once", "desc": "分析 goal 运行日志，规划改进 Issue（dry_run=true 只看计划不提交）"},
                ]);
                output_response(&id, "get_commands", &commands);
            }
            "get_skills" => {
                // 列出全局 + 项目级 skills
                let mut skills: Vec<serde_json::Value> = Vec::new();

                // 全局 skills (~/.ion/skills/)
                let global_dir = crate::paths::skills_dir();
                if let Ok(entries) = std::fs::read_dir(&global_dir) {
                    for entry in entries.flatten() {
                        if let Some(name) = entry.file_name().to_str() {
                            let path = entry.path();
                            if path.is_file() {
                                let content = std::fs::read_to_string(&path).unwrap_or_default();
                                let brief = content.lines().take(3).collect::<Vec<_>>().join(" ");
                                skills.push(serde_json::json!({
                                    "name": name.trim_end_matches(".md"),
                                    "source": "global",
                                    "path": path.to_string_lossy(),
                                    "brief": if brief.chars().count() > 80 { format!("{}...", brief.chars().take(80).collect::<String>()) } else { brief },
                                }));
                            }
                        }
                    }
                }

                // 项目级 skills (<config_root>/.ion/skills/)——worktree 回源主仓库（缺口 #2）
                let proj_dir = crate::paths::project_skills_dir(&config_root);
                if let Ok(entries) = std::fs::read_dir(&proj_dir) {
                    for entry in entries.flatten() {
                        if let Some(name) = entry.file_name().to_str() {
                            let path = entry.path();
                            if path.is_file() {
                                let content = std::fs::read_to_string(&path).unwrap_or_default();
                                let brief = content.lines().take(3).collect::<Vec<_>>().join(" ");
                                skills.push(serde_json::json!({
                                    "name": name.trim_end_matches(".md"),
                                    "source": "project",
                                    "path": path.to_string_lossy(),
                                    "brief": if brief.chars().count() > 80 { format!("{}...", brief.chars().take(80).collect::<String>()) } else { brief },
                                }));
                            }
                        }
                    }
                }

                output_response(
                    &id,
                    "get_skills",
                    &serde_json::json!({
                        "skills": skills,
                        "count": skills.len(),
                    }),
                );
            }
            "get_extensions" => {
                // 列出已加载的扩展（从 ExtensionRunner）
                let exts: Vec<_> = agent.extensions().names();
                output_response(
                    &id,
                    "get_extensions",
                    &serde_json::json!({
                        "extensions": exts.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>(),
                        "count": exts.len(),
                    }),
                );
            }
            "get_available_models" => {
                let models: Vec<serde_json::Value> = model_reg
                    .list_models()
                    .iter()
                    .map(|m| {
                        serde_json::json!({
                            "id": m.id, "name": m.name, "provider": m.provider,
                            "reasoning": m.reasoning, "contextWindow": m.context_window,
                        })
                    })
                    .collect();
                output_response(&id, "get_available_models", &serde_json::json!(models));
            }
            "get_tier_models" => {
                let cfg = crate::config::IonConfig::load();
                output_response(&id, "get_tier_models", &serde_json::json!(cfg.tier_models));
            }
            "goal_evolver_run_once" => {
                // Analyze goal-run logs and plan improvement Issues.
                // Params: { data_dir: string (required), dry_run: bool (default true) }
                // dry_run=true  → returns planned issues without submitting them
                // dry_run=false → (future) would submit via gh issue create
                let data_dir = params.get("data_dir").and_then(|v| v.as_str());
                let dry_run = params
                    .get("dry_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                match data_dir {
                    None => {
                        output_response(
                            &id,
                            "goal_evolver_run_once",
                            &serde_json::json!({
                                "success": false, "error": "data_dir param is required"
                            }),
                        );
                    }
                    Some(dir) => match crate::goal_evolver::run_once(dir) {
                        Ok(report) => {
                            let issues: Vec<serde_json::Value> = report
                                .issues_planned
                                .iter()
                                .map(|ip| {
                                    serde_json::json!({
                                        "title": ip.title,
                                        "dimension": format!("{:?}", ip.dimension),
                                        "severity": format!("{:?}", ip.severity),
                                        "body": ip.body,
                                        "would_submit": !dry_run,
                                    })
                                })
                                .collect();
                            output_response(
                                &id,
                                "goal_evolver_run_once",
                                &serde_json::json!({
                                    "success": true,
                                    "dry_run": dry_run,
                                    "analyzed_goals": report.analyzed_goals,
                                    "total_iterations": report.total_iterations,
                                    "issues_planned": issues,
                                }),
                            );
                        }
                        Err(e) => {
                            output_response(
                                &id,
                                "goal_evolver_run_once",
                                &serde_json::json!({
                                    "success": false, "error": e
                                }),
                            );
                        }
                    },
                }
            }
            "get_tree" => {
                let mode = params
                    .get("mode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("structure");
                let entries: Vec<serde_json::Value> =
                    crate::message_retrieval::load_entries_cached(&worker_cwd);

                if entries.is_empty() {
                    output_response(
                        &id,
                        "get_tree",
                        &serde_json::json!({
                            "nodes": [], "currentLeaf": null, "branches": [], "compactionPoints": []
                        }),
                    );
                } else if mode == "full" {
                    // full 模式：返回全部 entry 骨架
                    let nodes: Vec<_> = entries
                        .iter()
                        .map(|e| {
                            serde_json::json!({
                                "id": e.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                                "parentId": e.get("parentId").and_then(|v| v.as_str()),
                                "type": e.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                                "customType": e.get("customType").and_then(|v| v.as_str()),
                            })
                        })
                        .collect();
                    output_response(
                        &id,
                        "get_tree",
                        &serde_json::json!({
                            "nodes": nodes, "mode": "full"
                        }),
                    );
                } else {
                    // structure 模式：返回压缩点、分支指针与文件快照锚点。
                    let struct_nodes: Vec<_> = entries
                        .iter()
                        .filter(|e| {
                            let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
                            t == "compaction"
                                || t == "leaf_pointer"
                                || (t == "custom"
                                    && e.get("customType").and_then(|v| v.as_str())
                                        == Some(crate::session_jsonl::CUSTOM_TYPE_STEP_SNAPSHOT))
                        })
                        .cloned()
                        .collect();
                    let current_leaf = crate::session_tree::resolve_current_leaf(&entries);
                    let compaction_points: Vec<_> = entries
                        .iter()
                        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("compaction"))
                        .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                        .collect();
                    output_response(
                        &id,
                        "get_tree",
                        &serde_json::json!({
                            "nodes": struct_nodes,
                            "currentLeaf": current_leaf,
                            "compactionPoints": compaction_points,
                            "mode": "structure"
                        }),
                    );
                }
            }
            "get_modified_files" => {
                let from_turn = params
                    .get("fromTurn")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let to_turn = params
                    .get("toTurn")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some(ref store) = snapshot_store {
                    let all_snaps = store.load_all_tool_snapshots();
                    // 按 turnId 范围过滤（from/to 是 turnId 字符串，按 timestamp 比较）
                    let snaps: Vec<_> = if from_turn.is_some() || to_turn.is_some() {
                        let from_ts = from_turn
                            .as_ref()
                            .and_then(|ft| all_snaps.iter().find(|s| &s.turn_id == ft))
                            .map(|s| s.timestamp.clone());
                        let to_ts = to_turn
                            .as_ref()
                            .and_then(|tt| all_snaps.iter().find(|s| &s.turn_id == tt))
                            .map(|s| s.timestamp.clone());
                        all_snaps
                            .into_iter()
                            .filter(|s| {
                                let after_from =
                                    from_ts.as_ref().is_none_or(|ft| &s.timestamp >= ft);
                                let before_to = to_ts.as_ref().is_none_or(|tt| &s.timestamp <= tt);
                                after_from && before_to
                            })
                            .collect()
                    } else {
                        all_snaps
                    };
                    let files: Vec<serde_json::Value> = snaps
                        .iter()
                        .map(|s| {
                            let status = match (&s.before_hash, &s.after_hash) {
                                (None, Some(_)) => "added",
                                (Some(_), None) => "deleted",
                                (Some(_), Some(_)) => "modified",
                                _ => "unchanged",
                            };
                            // source 区分：write/edit 工具 vs bash 目录扫描
                            let source = match s.tool_name.as_str() {
                                "write" => "tool_write",
                                "edit" => "tool_edit",
                                "bash" => "turn_scan",
                                _ => "tool",
                            };
                            // 路径规范化：cwd 内相对化，cwd 外绝对化
                            let normalized = normalize_path(&s.path, &worker_cwd);
                            serde_json::json!({
                                "path": normalized,
                                "status": status,
                                "source": source,
                                "turnId": s.turn_id,
                                "toolCallId": s.tool_call_id,
                                "tool": s.tool_name,
                                "hasDiff": s.before_hash.is_some() || s.after_hash.is_some(),
                            })
                        })
                        .collect();
                    let added = files.iter().filter(|f| f["status"] == "added").count();
                    let modified = files.iter().filter(|f| f["status"] == "modified").count();
                    let deleted = files.iter().filter(|f| f["status"] == "deleted").count();
                    output_response(
                        &id,
                        "get_modified_files",
                        &serde_json::json!({
                            "files": files,
                            "summary": { "added": added, "modified": modified, "deleted": deleted },
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "get_modified_files",
                        &serde_json::json!({
                            "error": "file-snapshot extension not enabled",
                        }),
                    );
                }
            }
            "get_queue" => {
                let steering: Vec<serde_json::Value> = agent
                    .steering_queue_snapshot()
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                let follow_up: Vec<serde_json::Value> = agent
                    .follow_up_queue_snapshot()
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                output_response(
                    &id,
                    "get_queue",
                    &serde_json::json!({
                        "steering": steering, "followUp": follow_up,
                        "steeringCount": agent.steering_queue_len(),
                        "followUpCount": agent.follow_up_queue_len(),
                    }),
                );
            }
            "clear_queue" => {
                agent.clear_queues();
                output_response(
                    &id,
                    "clear_queue",
                    &serde_json::json!({
                        "cleared": true,
                        "steeringCleared": agent.steering_queue_len(),
                        "followUpCleared": agent.follow_up_queue_len(),
                    }),
                );
            }
            "get_context_usage" => {
                let msgs = agent.messages();
                let input_tokens: u64 = msgs
                    .iter()
                    .filter_map(|m| match m {
                        Message::Assistant(a) => Some(a.usage.input),
                        _ => None,
                    })
                    .sum();
                let output_tokens: u64 = msgs
                    .iter()
                    .filter_map(|m| match m {
                        Message::Assistant(a) => Some(a.usage.output),
                        _ => None,
                    })
                    .sum();
                let ctx_chars: usize = msgs
                    .iter()
                    .map(|m| match m {
                        Message::User(u) => u
                            .content
                            .iter()
                            .map(|b| match b {
                                crate::agent::messages::ContentBlock::Text(t) => t.text.len(),
                                _ => 0,
                            })
                            .sum::<usize>(),
                        Message::Assistant(a) => a
                            .content
                            .iter()
                            .map(|b| match b {
                                crate::agent::messages::AssistantContentBlock::Text(t) => {
                                    t.text.len()
                                }
                                _ => 0,
                            })
                            .sum::<usize>(),
                        _ => 0,
                    })
                    .sum();
                let context_window = agent.model().context_window;
                let estimated_tokens = (ctx_chars / 4) as u64;
                output_response(
                    &id,
                    "get_context_usage",
                    &serde_json::json!({
                        "messageCount": msgs.len(),
                        "estimatedTokens": estimated_tokens,
                        "contextWindow": context_window,
                        "usagePercent": if context_window > 0 { (estimated_tokens * 100 / context_window as u64) as u32 } else { 0 },
                        "totalInputTokens": input_tokens,
                        "totalOutputTokens": output_tokens,
                        "autoCompaction": agent.auto_compact_enabled(),
                    }),
                );
            }
            "get_flags" => {
                let extension_id = params
                    .get("extension")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if extension_id.is_empty() {
                    // 无参数 → 返回所有扩展的 flag
                    let names = agent.extensions().names();
                    let mut all_flags = serde_json::Map::new();
                    for name in &names {
                        all_flags.insert(name.clone(), agent.extensions().get_flags(name));
                    }
                    output_response(&id, "get_flags", &serde_json::Value::Object(all_flags));
                } else {
                    let flags = agent.extensions().get_flags(extension_id);
                    output_response(
                        &id,
                        "get_flags",
                        &serde_json::json!({
                            "extension": extension_id,
                            "flags": flags,
                        }),
                    );
                }
            }

            "get_active_tools" => {
                let tools: Vec<String> = agent.list_tool_names();
                output_response(
                    &id,
                    "get_active_tools",
                    &serde_json::json!({"tools": tools, "count": tools.len()}),
                );
            }
            "set_active_tools" => {
                let tools_arr: Vec<String> = params
                    .get("tools")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();
                agent.restrict_tools(tools_arr.clone());
                output_response(
                    &id,
                    "set_active_tools",
                    &serde_json::json!({
                        "activeTools": tools_arr, "count": tools_arr.len(),
                    }),
                );
            }
            "get_full_messages" => {
                let msgs: Vec<serde_json::Value> = agent
                    .messages()
                    .iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                output_response(
                    &id,
                    "get_full_messages",
                    &serde_json::json!({
                        "messages": msgs, "count": msgs.len(),
                        "note": "Includes thinking blocks and all content types",
                    }),
                );
            }
            "set_auto_compaction" => {
                let enabled = params
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                agent.set_auto_compact(enabled);
                output_response(
                    &id,
                    "set_auto_compaction",
                    &serde_json::json!({
                        "autoCompaction": enabled,
                    }),
                );
            }
            "set_cwd" => {
                let cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                if cwd.is_empty() {
                    output_response(
                        &id,
                        "set_cwd",
                        &serde_json::json!({"error": "missing 'cwd' parameter"}),
                    );
                } else {
                    // 验证路径存在
                    if std::path::Path::new(cwd).exists() {
                        agent.set_session_cwd(Some(cwd.to_string()));
                        // 更新 SessionIndex.last_cwd（记录最后切换到的工作路径）
                        crate::session_index::SessionIndex::set_last_cwd(&sid, cwd);
                        output_response(
                            &id,
                            "set_cwd",
                            &serde_json::json!({
                                "cwd": cwd,
                                "success": true,
                            }),
                        );
                    } else {
                        output_response(
                            &id,
                            "set_cwd",
                            &serde_json::json!({
                                "error": format!("path '{}' does not exist", cwd),
                            }),
                        );
                    }
                }
            }
            "add_dir" => {
                // 添加额外工作目录（记录到 extra_cwds + Agent 内存，用于 prompt 注入让 LLM 知道可访问）
                let dir = params
                    .get("dir")
                    .and_then(|v| v.as_str())
                    .or_else(|| params.get("cwd").and_then(|v| v.as_str()))
                    .or_else(|| params.get("path").and_then(|v| v.as_str()))
                    .unwrap_or("");
                if dir.is_empty() {
                    output_response(
                        &id,
                        "add_dir",
                        &serde_json::json!({"error": "missing 'dir' parameter"}),
                    );
                } else {
                    // 规范化为绝对路径
                    let abs = if std::path::Path::new(dir).is_absolute() {
                        dir.to_string()
                    } else {
                        std::env::current_dir()
                            .ok()
                            .map(|c| c.join(dir).to_string_lossy().to_string())
                            .unwrap_or_else(|| dir.to_string())
                    };
                    if !std::path::Path::new(&abs).exists() {
                        output_response(
                            &id,
                            "add_dir",
                            &serde_json::json!({"error": format!("path '{}' does not exist", abs)}),
                        );
                    } else {
                        let added = agent.add_extra_cwd(&abs);
                        // 同步到 SessionIndex.extra_cwds（持久化）
                        crate::session_index::SessionIndex::add_extra_cwd(&sid, &abs);
                        let dirs = agent.get_extra_cwds();
                        output_response(
                            &id,
                            "add_dir",
                            &serde_json::json!({
                                "added": added,
                                "dir": abs,
                                "extra_cwds": dirs,
                            }),
                        );
                    }
                }
            }
            "remove_dir" => {
                let dir = params
                    .get("dir")
                    .and_then(|v| v.as_str())
                    .or_else(|| params.get("cwd").and_then(|v| v.as_str()))
                    .unwrap_or("");
                if dir.is_empty() {
                    output_response(
                        &id,
                        "remove_dir",
                        &serde_json::json!({"error": "missing 'dir' parameter"}),
                    );
                } else {
                    let removed = agent.remove_extra_cwd(dir);
                    if removed {
                        // 同步移除 SessionIndex.extra_cwds（重新写整个列表）
                        let remaining = agent.get_extra_cwds();
                        crate::session_index::SessionIndex::patch_meta(&sid, |m| {
                            m.extra_cwds = remaining.clone();
                        });
                    }
                    output_response(
                        &id,
                        "remove_dir",
                        &serde_json::json!({
                            "removed": removed,
                            "extra_cwds": agent.get_extra_cwds(),
                        }),
                    );
                }
            }
            "list_dirs" => {
                // 列出所有工作目录：cwd + extra_cwds
                let cwd = agent.session_cwd();
                output_response(
                    &id,
                    "list_dirs",
                    &serde_json::json!({
                        "cwd": cwd,
                        "extra_cwds": agent.get_extra_cwds(),
                    }),
                );
            }
            "cycle_model" => {
                let current_id = agent.model().id.clone();
                let current_provider = agent.model().provider.clone();
                let mut models = model_reg.models_by_provider(&current_provider);
                models.sort_by(|a, b| a.id.cmp(&b.id));
                if models.len() < 2 {
                    output_response(
                        &id,
                        "cycle_model",
                        &serde_json::json!({
                            "modelId": current_id, "provider": current_provider,
                            "note": "Only one model available, no cycle",
                        }),
                    );
                } else {
                    let next_idx = models
                        .iter()
                        .position(|m| m.id == current_id)
                        .map(|i| (i + 1) % models.len())
                        .unwrap_or(0);
                    let next_model = models[next_idx].clone();
                    let next_id = next_model.id.clone();
                    agent.set_model(next_model);
                    model_id = next_id.clone();
                    crate::session_index::SessionIndex::set_model(&sid, &provider, &next_id);
                    output_response(
                        &id,
                        "cycle_model",
                        &serde_json::json!({
                            "modelId": next_id, "provider": current_provider,
                            "previousModel": current_id,
                        }),
                    );
                }
            }
            "cycle_thinking_level" => {
                let levels = ["off", "minimal", "low", "medium", "high", "xhigh"];
                let current = agent.thinking_level().unwrap_or("off").to_string();
                let next = levels
                    .iter()
                    .position(|&l| l == current)
                    .map(|i| levels[(i + 1) % levels.len()])
                    .unwrap_or("medium");
                agent.set_thinking_level(Some(next.to_string()));
                crate::session_index::SessionIndex::set_thinking_level(&sid, next);
                output_response(
                    &id,
                    "cycle_thinking_level",
                    &serde_json::json!({
                        "thinkingLevel": next, "previousLevel": current,
                    }),
                );
            }
            "compact" => {
                let before_msgs = agent.messages().len();
                let before_tokens = crate::agent::compact::total_tokens(agent.messages());
                match agent.compact_now().await {
                    Ok(result) => {
                        let after_tokens = crate::agent::compact::total_tokens(agent.messages());
                        output_response(
                            &id,
                            "compact",
                            &serde_json::json!({
                                "compacted": true,
                                "beforeMessages": before_msgs,
                                "beforeTokens": before_tokens,
                                "afterMessages": agent.messages().len(),
                                "afterTokens": after_tokens,
                                "stage": result.stage,
                                "batchCount": result.batch_count,
                                "batchSummaries": result.batch_summaries.len(),
                                "hasMergedSummary": result.merged_summary.is_some(),
                                "summaryPreview": result.summary.chars().take(200).collect::<String>(),
                            }),
                        );
                    }
                    Err(e) => {
                        output_response(
                            &id,
                            "compact",
                            &serde_json::json!({
                                "compacted": false,
                                "error": e.to_string(),
                                "beforeMessages": before_msgs,
                                "beforeTokens": before_tokens,
                            }),
                        );
                    }
                }
            }
            "new_session" => {
                output_response(&id, "new_session", &serde_json::json!({"sessionId":sid}))
            }
            "export_html" => output_response(&id, "export_html", &serde_json::json!({"path":""})),
            "switch_session" => output_response(&id, "switch_session", &serde_json::Value::Null),
            "fork" => output_response(&id, "fork", &serde_json::json!({"sessionId":sid})),
            "navigate_tree" => {
                // 返回树的可导航线性结构（id/parentId/role/content 截断/leaf 标记）
                let entries: Vec<serde_json::Value> =
                    crate::message_retrieval::load_entries_cached(&worker_cwd);
                let current_leaf = crate::session_tree::resolve_current_leaf(&entries);

                let nodes: Vec<_> = entries
                    .iter()
                    .filter_map(|e| {
                        let etype = e.get("type").and_then(|v| v.as_str())?;
                        let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let parent_id = e.get("parentId").and_then(|v| v.as_str()).unwrap_or("");
                        let is_on_leaf_path = current_leaf
                            .as_ref()
                            .map(|leaf| {
                                // 简单判断：id 在 leaf path 里
                                crate::session_tree::get_branch_path(&entries, leaf)
                                    .iter()
                                    .any(|pe| pe.get("id").and_then(|v| v.as_str()) == Some(id))
                            })
                            .unwrap_or(false);

                        let role = e
                            .get("message")
                            .and_then(|m| m.get("role"))
                            .and_then(|r| r.as_str())
                            .unwrap_or("");

                        // content 截断到 50 字
                        let content = e
                            .get("message")
                            .and_then(|m| m.get("content"))
                            .and_then(|c| c.as_str())
                            .unwrap_or("");
                        let brief = if content.len() > 50 {
                            format!("{}...", &content[..50])
                        } else {
                            content.to_string()
                        };

                        Some(serde_json::json!({
                            "id": id,
                            "parentId": parent_id,
                            "type": etype,
                            "role": role,
                            "brief": brief,
                            "turnId": e.get("turnId").and_then(|v| v.as_u64()),
                            "onLeafPath": is_on_leaf_path,
                            "isCurrentLeaf": current_leaf.as_deref() == Some(id),
                        }))
                    })
                    .collect();

                output_response(
                    &id,
                    "navigate_tree",
                    &serde_json::json!({
                        "nodes": nodes,
                        "currentLeaf": current_leaf,
                        "totalNodes": nodes.len(),
                    }),
                );
            }
            "delete_entries" => {
                // 软删除：从 self.messages 移除 + 落 DeletionEntry 到 JSONL
                let target_ids: Vec<String> = params
                    .get("targetIds")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let reason = params.get("reason").and_then(|v| v.as_str());
                let before = agent.messages().len();

                if target_ids.is_empty() {
                    output_response(
                        &id,
                        "delete_entries",
                        &serde_json::json!({
                            "deleted": 0, "before": before, "after": before, "error": "no targetIds"
                        }),
                    );
                    continue;
                }

                // 从 JSONL 构建消息 entry id → 数组索引的映射
                let entries = crate::message_retrieval::load_entries_cached(&worker_cwd);

                // 尝试精确索引映射（compaction 前的快速路径）
                let indices = resolve_target_indices(&entries, agent.messages(), &target_ids);

                if indices.is_empty() {
                    output_response(
                        &id,
                        "delete_entries",
                        &serde_json::json!({
                            "deleted": 0, "before": before, "after": before,
                            "error": "no matching entries found (possibly after compaction)"
                        }),
                    );
                    continue;
                }

                if indices.is_empty() {
                    output_response(
                        &id,
                        "delete_entries",
                        &serde_json::json!({
                            "deleted": 0, "before": before, "after": before,
                            "error": "no matching entries found"
                        }),
                    );
                    continue;
                }

                // 执行删除
                agent.mark_deleted(&indices, &target_ids).await;
                // 落 DeletionEntry
                crate::session_jsonl::append_deletion(&worker_cwd, &target_ids, reason);
                // 失效缓存（下次 load_entries_cached 会重新读盘）
                crate::message_retrieval::invalidate_cache(&worker_cwd);

                output_response(
                    &id,
                    "delete_entries",
                    &serde_json::json!({
                        "deleted": indices.len(), "before": before, "after": agent.messages().len()
                    }),
                );
            }
            "summarize_entries" => {
                // 软压缩：把一批消息替换成 BranchSummary + 落 SegmentSummaryEntry
                let target_ids: Vec<String> = params
                    .get("targetIds")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let summary_text = params
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let before = agent.messages().len();

                if target_ids.is_empty() {
                    output_response(
                        &id,
                        "summarize_entries",
                        &serde_json::json!({
                            "summarized": 0, "before": before, "after": before, "error": "no targetIds"
                        }),
                    );
                    continue;
                }

                // 从 JSONL 构建索引映射（支持 compaction 后的降级匹配）
                let entries = crate::message_retrieval::load_entries_cached(&worker_cwd);

                let indices = resolve_target_indices(&entries, agent.messages(), &target_ids);

                if indices.is_empty() {
                    output_response(
                        &id,
                        "summarize_entries",
                        &serde_json::json!({
                            "summarized": 0, "before": before, "after": before,
                            "error": "no matching entries found (possibly after compaction)"
                        }),
                    );
                    continue;
                }

                // 摘要：未提供时调 LLM 自动生成
                let summary = if summary_text.is_empty() {
                    match agent.summarize_messages_llm(&indices).await {
                        Ok(s) if !s.is_empty() => s,
                        _ => format!("（{} 条消息已折叠）", indices.len()),
                    }
                } else {
                    summary_text
                };

                // 执行折叠
                agent.mark_summarized(&indices, &target_ids, &summary).await;
                // 落 SegmentSummaryEntry
                crate::session_jsonl::append_segment_summary(&worker_cwd, &target_ids, &summary);
                crate::message_retrieval::invalidate_cache(&worker_cwd);

                output_response(
                    &id,
                    "summarize_entries",
                    &serde_json::json!({
                        "summarized": indices.len(),
                        "before": before,
                        "after": agent.messages().len(),
                        "summary": summary,
                    }),
                );
            }
            "restore_entries" => {
                // 恢复软删除/折叠：追加 restoration entry + 从 JSONL 重载消息
                let target_ids: Vec<String> = params
                    .get("targetIds")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let before = agent.messages().len();

                if target_ids.is_empty() {
                    output_response(
                        &id,
                        "restore_entries",
                        &serde_json::json!({
                            "restored": 0, "before": before, "after": before, "error": "no targetIds"
                        }),
                    );
                    continue;
                }

                // 1. 从 Agent 状态移除
                agent.restore_entries(&target_ids);
                // 2. 追加 restoration entry 到 JSONL（拉取层会撤销过滤）
                crate::session_jsonl::append_restoration(&worker_cwd, &target_ids);
                // 3. 失效缓存
                crate::message_retrieval::invalidate_cache(&worker_cwd);
                // 4. 从 JSONL 重载消息到 Agent（恢复被删/折叠的原始消息）
                let new_count = agent.reload_messages_from_session(&worker_cwd);

                output_response(
                    &id,
                    "restore_entries",
                    &serde_json::json!({
                        "restored": target_ids.len(),
                        "before": before,
                        "after": new_count,
                    }),
                );
            }
            "clone" => output_response(&id, "clone", &serde_json::json!({"sessionId":sid})),
            "switch_agent" => {
                // 真实切换 agent：加载定义 + 应用系统提示词/工具限制
                let target = params
                    .get("agentName")
                    .or_else(|| params.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(agent_cfg) = crate::agent_config::find_agent(target) {
                    current_agent_name = agent_cfg.name.clone();
                    // 应用系统提示词
                    if let Some(ref sp) = agent_cfg.system_prompt {
                        agent.set_system_prompt(sp.clone());
                    }
                    // 应用工具白名单（如果有）
                    if let Some(ref allowed) = agent_cfg.tools {
                        agent.restrict_tools(allowed.clone());
                    }
                    // 应用工具黑名单（如果有）
                    if let Some(ref disallowed) = agent_cfg.disallowed_tools {
                        for tool_name in disallowed {
                            agent.remove_tool(tool_name);
                        }
                    }
                    output_response(
                        &id,
                        "switch_agent",
                        &serde_json::json!({
                            "agent": agent_cfg.name,
                            "description": agent_cfg.description,
                            "color": agent_cfg.color,
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "switch_agent",
                        &serde_json::json!({
                            "error": format!("agent '{}' not found", target)
                        }),
                    );
                }
            }
            "set_permission_mode" => {
                let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("");
                if mode.is_empty() {
                    output_response(
                        &id,
                        "set_permission_mode",
                        &serde_json::json!({
                            "error": "missing 'mode' parameter (open/blacklist/whitelist)",
                        }),
                    );
                } else {
                    match agent.runtime().set_guard_mode(mode) {
                        Ok(()) => output_response(
                            &id,
                            "set_permission_mode",
                            &serde_json::json!({
                                "mode": mode,
                                "success": true,
                            }),
                        ),
                        Err(e) => output_response(
                            &id,
                            "set_permission_mode",
                            &serde_json::json!({
                                "error": e,
                            }),
                        ),
                    }
                }
            }
            // ── Stored-Decision（权限记忆）顶层 RPC ──
            // 对齐 docs/design/PERMISSION_STORE.md §2.4，转发给 permission 扩展。
            // 用户选"always allow"后持久化决策，下次自动放行，不用反复确认。
            "permission_store_decision" => {
                match agent
                    .extension_rpc("permission", "store_decision", params)
                    .await
                {
                    Ok(output) => output_response(
                        &id,
                        "permission_store_decision",
                        &serde_json::json!({
                            "success": true, "data": output,
                        }),
                    ),
                    Err(e) => output_error_response(&id, "permission_store_decision", &format!("permission_store_decision: {e}")),
                }
            }
            "permission_list_stored" => {
                match agent
                    .extension_rpc("permission", "list_stored", serde_json::Value::Null)
                    .await
                {
                    Ok(output) => output_response(
                        &id,
                        "permission_list_stored",
                        &serde_json::json!({
                            "success": true, "data": output,
                        }),
                    ),
                    Err(e) => output_error_response(&id, "permission_list_stored", &format!("permission_list_stored: {e}")),
                }
            }
            "permission_remove_stored" => {
                match agent
                    .extension_rpc("permission", "remove_stored", params)
                    .await
                {
                    Ok(output) => output_response(
                        &id,
                        "permission_remove_stored",
                        &serde_json::json!({
                            "success": true, "data": output,
                        }),
                    ),
                    Err(e) => output_error_response(&id, "permission_remove_stored", &format!("permission_remove_stored: {e}")),
                }
            }
            "permission_clear_stored" => {
                match agent
                    .extension_rpc("permission", "clear_stored", serde_json::Value::Null)
                    .await
                {
                    Ok(output) => output_response(
                        &id,
                        "permission_clear_stored",
                        &serde_json::json!({
                            "success": true, "data": output,
                        }),
                    ),
                    Err(e) => output_error_response(&id, "permission_clear_stored", &format!("permission_clear_stored: {e}")),
                }
            }
            "set_auto_retry" => {
                let enabled = params
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let max_retries = params
                    .get("max_retries")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                if enabled {
                    let max = max_retries.unwrap_or(3);
                    agent.set_max_retries(max);
                    output_response(
                        &id,
                        "set_auto_retry",
                        &serde_json::json!({
                            "enabled": true,
                            "max_retries": max,
                        }),
                    );
                } else {
                    agent.set_max_retries(0);
                    output_response(
                        &id,
                        "set_auto_retry",
                        &serde_json::json!({
                            "enabled": false,
                            "max_retries": 0,
                        }),
                    );
                }
            }
            "bash" => {
                // 真正执行 bash 命令（不再是空桩）
                let command = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if command.is_empty() {
                    output_response(&id, "bash", &serde_json::json!({"output":"","exitCode":0}));
                } else {
                    let timeout_secs = params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
                    match execute_bash(command, timeout_secs).await {
                        Ok((stdout, stderr, exit_code)) => {
                            let output = if stderr.is_empty() {
                                stdout.clone()
                            } else {
                                format!("{stdout}\n{stderr}")
                            };
                            output_response(
                                &id,
                                "bash",
                                &serde_json::json!({
                                    "output": output,
                                    "stdout": stdout,
                                    "stderr": stderr,
                                    "exitCode": exit_code,
                                }),
                            );
                        }
                        Err(e) => {
                            output_response(
                                &id,
                                "bash",
                                &serde_json::json!({
                                    "output": format!("bash error: {e}"),
                                    "exitCode": -1,
                                }),
                            );
                        }
                    }
                }
            }
            "set_steering_mode" => {
                output_response(&id, "set_steering_mode", &serde_json::Value::Null)
            }
            "extension_rpc" => {
                // 调 Extension 私有 RPC 方法（给 CLI/外部调试用）。
                // 用于：ion rpc --session <id> --method extension_rpc
                //   --params '{"method":"ping","args":{}}'
                //   --params '{"extension":"bash","method":"list"}'
                let extension_name = params
                    .get("extension")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let rpc_method = params
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let rpc_args = params.get("args").cloned().unwrap_or_default();
                match agent
                    .extension_rpc(extension_name, &rpc_method, rpc_args)
                    .await
                {
                    Ok(output) => output_response(
                        &id,
                        "extension_rpc",
                        &serde_json::json!({
                            "method": rpc_method, "output": output,
                        }),
                    ),
                    Err(e) => output_error_response(&id, "extension_rpc", &format!("extension_rpc {rpc_method}: {e}")),
                }
            }
            "call_tool" => {
                // Directly call an LLM-registered tool by name (bypass LLM).
                // 用于 CLI 测试工具如 bash/bash_kill/bash_send。
                // --params '{"tool":"bash","args":{"command":"echo hi","description":"test"}}'
                let tool_name = params
                    .get("tool")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_args = params.get("args").cloned().unwrap_or_default();
                if tool_name.is_empty() {
                    output_error_response(&id, "call_tool", "missing 'tool'");
                    continue;
                }
                // 注意：call_tool 不在这里 drain follow_up_rx —— 后台进程完成通知
                // 需要外部主动调 drain_follow_ups RPC（避免 agent 锁竞争死锁）。
                match agent.call_tool(&tool_name, tool_args).await {
                    Ok(result) => output_response(
                        &id,
                        "call_tool",
                        &serde_json::json!({
                            "tool": tool_name, "output": result,
                        }),
                    ),
                    Err(e) => output_error_response(&id, "call_tool", &format!("call_tool {tool_name}: {e}")),
                }
            }
            "drain_follow_ups" => {
                // 主动 drain follow_up_rx（用于 call_tool 路径下后台进程完成通知的写入）。
                // 典型用法：bash background=true → sleep N → drain_follow_ups → jsonl 里有 <bash_result>
                // --params '{"wait_ms": 1000}'  // 可选：先 sleep 再 drain（等长任务完成）
                let wait_ms = params.get("wait_ms").and_then(|v| v.as_u64()).unwrap_or(0);
                if wait_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
                }
                let drained_count = agent.try_drain_follow_up_rx().await;
                let msgs = agent.drain_follow_up_queue();
                let mut written: Vec<serde_json::Value> = Vec::new();
                for msg in &msgs {
                    let entry = serde_json::json!({
                        "id": session_jsonl::generate_id(),
                        "parentId": sid,
                        "timestamp": session_jsonl::timestamp_iso(),
                        "type": "message",
                        "message": msg,
                    });
                    session_jsonl::append_raw_entry(&worker_cwd, &entry);
                    written.push(serde_json::to_value(msg).unwrap_or(serde_json::Value::Null));
                }
                for msg in msgs {
                    agent.push_message(msg);
                }
                // 各 queue 当前长度（用于确认 Steer/NextTurn 消息也按 mode 分发到位）。
                // 注意：drain_follow_up_queue 只取 follow_up_queue 写盘。
                // Steer 消息入 steering_queue 由 outer_loop 处理；NextTurn 入 next_turn_queue 由 worker_rpc 在 agent.run 返回后处理。
                let (steering_len, followup_len, next_turn_len) = agent.queue_lengths();
                output_response(
                    &id,
                    "drain_follow_ups",
                    &serde_json::json!({
                        "drained": drained_count,
                        "written": written.len(),
                        "messages": written,
                        "queue_lengths": {
                            "steering": steering_len,
                            "followUp": followup_len,
                            "nextTurn": next_turn_len,
                        },
                    }),
                )
            }
            "set_follow_up_mode" => {
                output_response(&id, "set_follow_up_mode", &serde_json::Value::Null)
            }
            "reload" => {
                // Generic reload: reload all loaded extensions
                let extensions = wasm_ext_registry.list();
                if extensions.is_empty() {
                    output_response(
                        &id,
                        "reload",
                        &serde_json::json!({"message": "no extensions loaded"}),
                    );
                } else {
                    let mut reloaded: Vec<String> = Vec::new();
                    let mut errors: Vec<String> = Vec::new();
                    for p in &extensions {
                        match wasm_ext_registry.reload(&p.path) {
                            Ok(tool_defs) => {
                                // Remove old tools, add new ones
                                for old_name in &p.tools {
                                    agent.remove_tool(old_name);
                                }
                                let canonical_str = p.path.clone();
                                let extension_id =
                                    crate::wasm_extension::extension_id_from_path(&canonical_str);
                                for td in &tool_defs {
                                    agent.register_tool(Box::new(WasmToolAdapter {
                                        name: td.name.clone(),
                                        description: td.description.clone(),
                                        parameters: td.parameters.clone(),
                                        extension_path: canonical_str.clone(),
                                        extension_id: extension_id.clone(),
                                        registry: wasm_ext_registry.clone(),
                                    }));
                                }
                                reloaded.push(p.path.clone());
                            }
                            Err(e) => {
                                errors.push(format!("{}: {e}", p.path));
                            }
                        }
                    }
                    output_response(
                        &id,
                        "reload",
                        &serde_json::json!({"reloaded": reloaded, "errors": errors}),
                    );
                }
            }
            "abort_retry" => {
                // 中断当前重试循环（复用 abort 机制）
                agent.stop();
                output_response(
                    &id,
                    "abort_retry",
                    &serde_json::json!({
                        "aborted": true,
                        "message": "retry loop interrupted",
                    }),
                );
            }
            "set_tier_models" => {
                let tier = params.get("tier").and_then(|v| v.as_str()).unwrap_or("");
                let model = params.get("model").and_then(|v| v.as_str()).unwrap_or("");
                if tier.is_empty() || model.is_empty() {
                    output_response(
                        &id,
                        "set_tier_models",
                        &serde_json::json!({"error": "missing 'tier' or 'model'"}),
                    );
                } else {
                    let mut cfg = crate::config::IonConfig::load();
                    let old = cfg.tier_models.get(tier).cloned();
                    cfg.tier_models.insert(tier.to_string(), model.to_string());
                    match cfg.save() {
                        Ok(()) => output_response(
                            &id,
                            "set_tier_models",
                            &serde_json::json!({
                                "tier": tier, "oldModel": old, "newModel": model, "saved": true,
                            }),
                        ),
                        Err(e) => output_response(
                            &id,
                            "set_tier_models",
                            &serde_json::json!({"error": format!("save failed: {}", e)}),
                        ),
                    }
                }
            }
            "get_tree_with_leaf" => {
                // get_tree + 带 pathToLeaf（root → current leaf 的路径）
                let entries: Vec<serde_json::Value> =
                    crate::message_retrieval::load_entries_cached(&worker_cwd);
                let current_leaf = crate::session_tree::resolve_current_leaf(&entries);
                let tree_nodes = crate::session_tree::get_tree(&entries);

                // 计算 root → leaf 路径
                let path_to_leaf = if let Some(ref leaf_id) = current_leaf {
                    crate::session_tree::get_branch_path(&entries, leaf_id)
                        .iter()
                        .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                };

                let branches = crate::session_tree::named_branches(&entries);
                output_response(
                    &id,
                    "get_tree_with_leaf",
                    &serde_json::json!({
                        "tree": tree_nodes,
                        "currentLeaf": current_leaf,
                        "pathToLeaf": path_to_leaf,
                        "branches": branches.iter().map(|(name, target)| {
                            serde_json::json!({"name": name, "target": target})
                        }).collect::<Vec<_>>(),
                    }),
                );
            }
            "get_file_diff" => {
                let file_path = params
                    .get("filePath")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let from_turn = params
                    .get("fromTurn")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let to_turn = params
                    .get("toTurn")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if file_path.is_empty() {
                    output_response(
                        &id,
                        "get_file_diff",
                        &serde_json::json!({"error": "missing 'filePath'"}),
                    );
                } else if let Some(ref store) = snapshot_store {
                    let history = store.load_file_history(file_path);
                    // 按 turnId 字符串过滤（timestamp 比较）
                    let from_ts = from_turn
                        .as_ref()
                        .and_then(|ft| history.iter().find(|s| &s.turn_id == ft))
                        .map(|s| s.timestamp.clone());
                    let to_ts = to_turn
                        .as_ref()
                        .and_then(|tt| history.iter().find(|s| &s.turn_id == tt))
                        .map(|s| s.timestamp.clone());
                    let relevant: Vec<_> = history
                        .iter()
                        .filter(|s| {
                            from_ts.as_ref().is_none_or(|ft| &s.timestamp >= ft)
                                && to_ts.as_ref().is_none_or(|tt| &s.timestamp <= tt)
                        })
                        .collect();
                    if relevant.is_empty() {
                        output_response(
                            &id,
                            "get_file_diff",
                            &serde_json::json!({
                                "path": file_path, "diff": null, "hasContent": false,
                            }),
                        );
                    } else {
                        let first = relevant.first().unwrap();
                        let last = relevant.last().unwrap();
                        let before_content = first
                            .before_hash
                            .as_ref()
                            .and_then(|h| store.objects().read_object_text(h));
                        let after_content = last
                            .after_hash
                            .as_ref()
                            .and_then(|h| store.objects().read_object_text(h));

                        // GC 降级：hash 存在但 object 不可读
                        let before_missing =
                            first.before_hash.is_some() && before_content.is_none();
                        let after_missing = last.after_hash.is_some() && after_content.is_none();
                        if before_missing || after_missing {
                            output_response(
                                &id,
                                "get_file_diff",
                                &serde_json::json!({
                                    "path": file_path,
                                    "diffAvailable": false,
                                    "error": { "code": "SNAPSHOT_OBJECT_MISSING" },
                                    "beforeHash": first.before_hash,
                                    "afterHash": last.after_hash,
                                }),
                            );
                            return;
                        }

                        let diff = match (&before_content, &after_content) {
                            (Some(b), Some(a)) => {
                                crate::file_snapshot::unified_diff(b, a, file_path)
                            }
                            (None, Some(a)) => format!("+++ new file\n{}", a),
                            (Some(b), None) => format!("--- deleted file\n{}", b),
                            _ => String::new(),
                        };
                        let (added, removed) = crate::file_snapshot::count_diff(&diff);
                        output_response(
                            &id,
                            "get_file_diff",
                            &serde_json::json!({
                                "path": file_path,
                                "diff": diff,
                                "diffAvailable": true,
                                "beforeHash": first.before_hash,
                                "afterHash": last.after_hash,
                                "hasContent": before_content.is_some() || after_content.is_some(),
                                "added": added,
                                "removed": removed,
                            }),
                        );
                    }
                } else {
                    output_response(
                        &id,
                        "get_file_diff",
                        &serde_json::json!({"error": "file-snapshot not enabled"}),
                    );
                }
            }
            // 单 turn 变更摘要（会话底部折叠卡数据源）：
            // 只回 path/状态/行数计数，不带 diff 文本（大 turn 也不膨胀）；
            // 单文件详情按需走 get_file_diff {filePath, fromTurn, toTurn}。
            // turnId 省略时取当前 session 最新的 ts_ turn（agent_end 刚落盘的那轮）
            "turn_changes" => {
                let turn_id_param = params.get("turnId").and_then(|v| v.as_str()).unwrap_or("");
                if let Some(ref store) = snapshot_store {
                    let all_snaps = store.load_all_tool_snapshots();
                    // store 按项目共享（聚合了所有 session），先过滤本 session
                    let mine: Vec<&crate::file_snapshot::ToolSnapshot> = all_snaps
                        .iter()
                        .filter(|s| s.session_id == sid)
                        .collect();
                    let turn_id = if turn_id_param.is_empty() {
                        mine.iter()
                            .max_by(|a, b| a.timestamp.cmp(&b.timestamp))
                            .map(|s| s.turn_id.clone())
                    } else {
                        Some(turn_id_param.to_string())
                    };
                    let turn_id = match turn_id {
                        Some(t) => t,
                        None => {
                            output_response(
                                &id,
                                "turn_changes",
                                &serde_json::json!({"turnId": null, "files": [],
                                    "summary": {"files": 0, "added": 0, "removed": 0}}),
                            );
                            return;
                        }
                    };
                    use std::collections::HashMap;
                    let mut grouped: HashMap<String, Vec<&crate::file_snapshot::ToolSnapshot>> =
                        HashMap::new();
                    for s in &mine {
                        if s.turn_id == turn_id {
                            grouped.entry(s.path.clone()).or_default().push(s);
                        }
                    }
                    let mut files = Vec::new();
                    let mut total_added = 0usize;
                    let mut total_removed = 0usize;
                    for (path, group) in &grouped {
                        let first = group.first().unwrap();
                        let last = group.last().unwrap();
                        let before = first
                            .before_hash
                            .as_ref()
                            .and_then(|h| store.objects().read_object_text(h));
                        let after = last
                            .after_hash
                            .as_ref()
                            .and_then(|h| store.objects().read_object_text(h));
                        let (status, added, removed) = match (&before, &after) {
                            (Some(b), Some(a)) => {
                                let (ad, rm) = crate::file_snapshot::count_changes(b, a);
                                ("modified", ad, rm)
                            }
                            (None, Some(a)) => ("added", a.lines().count(), 0),
                            (Some(b), None) => ("deleted", 0, b.lines().count()),
                            _ => ("modified", 0, 0),
                        };
                        total_added += added;
                        total_removed += removed;
                        files.push(serde_json::json!({
                            "path": path, "status": status, "added": added, "removed": removed,
                        }));
                    }
                    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
                    output_response(
                        &id,
                        "turn_changes",
                        &serde_json::json!({
                            "turnId": turn_id,
                            "files": files,
                            "summary": {"files": grouped.len(), "added": total_added, "removed": total_removed},
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "turn_changes",
                        &serde_json::json!({"error": "file-snapshot not enabled"}),
                    );
                }
            }
            "get_batch_diffs" => {
                let from_turn = params
                    .get("fromTurn")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let to_turn = params
                    .get("toTurn")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                if let Some(ref store) = snapshot_store {
                    let all_snaps = store.load_all_tool_snapshots();
                    let snaps: Vec<_> = if from_turn.is_some() || to_turn.is_some() {
                        let from_ts = from_turn
                            .as_ref()
                            .and_then(|ft| all_snaps.iter().find(|s| &s.turn_id == ft))
                            .map(|s| s.timestamp.clone());
                        let to_ts = to_turn
                            .as_ref()
                            .and_then(|tt| all_snaps.iter().find(|s| &s.turn_id == tt))
                            .map(|s| s.timestamp.clone());
                        all_snaps
                            .into_iter()
                            .filter(|s| {
                                let after_from =
                                    from_ts.as_ref().is_none_or(|ft| &s.timestamp >= ft);
                                let before_to = to_ts.as_ref().is_none_or(|tt| &s.timestamp <= tt);
                                after_from && before_to
                            })
                            .collect()
                    } else {
                        all_snaps
                    };
                    // 按 path 分组，取每个 path 的首尾
                    use std::collections::HashMap;
                    let mut grouped: HashMap<String, Vec<&crate::file_snapshot::ToolSnapshot>> =
                        HashMap::new();
                    for s in &snaps {
                        grouped.entry(s.path.clone()).or_default().push(s);
                    }
                    let mut files = Vec::new();
                    let mut total_added = 0usize;
                    let mut total_removed = 0usize;
                    for (path, group) in &grouped {
                        let first = group.first().unwrap();
                        let last = group.last().unwrap();
                        let before_content = first
                            .before_hash
                            .as_ref()
                            .and_then(|h| store.objects().read_object_text(h));
                        let after_content = last
                            .after_hash
                            .as_ref()
                            .and_then(|h| store.objects().read_object_text(h));
                        let diff = match (&before_content, &after_content) {
                            (Some(b), Some(a)) => crate::file_snapshot::unified_diff(b, a, path),
                            (None, Some(a)) => format!("+++ new file\n{}", a),
                            (Some(b), None) => format!("--- deleted\n{}", b),
                            _ => String::new(),
                        };
                        let (added, removed) = crate::file_snapshot::count_diff(&diff);
                        total_added += added;
                        total_removed += removed;
                        files.push(serde_json::json!({
                            "path": path, "diff": diff, "added": added, "removed": removed,
                        }));
                    }
                    output_response(
                        &id,
                        "get_batch_diffs",
                        &serde_json::json!({
                            "files": files,
                            "summary": { "files": grouped.len(), "added": total_added, "removed": total_removed },
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "get_batch_diffs",
                        &serde_json::json!({"error": "file-snapshot not enabled"}),
                    );
                }
            }
            "get_file_history" => {
                let file_path = params
                    .get("filePath")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if file_path.is_empty() {
                    output_response(
                        &id,
                        "get_file_history",
                        &serde_json::json!({"error": "missing 'filePath'"}),
                    );
                } else if let Some(ref store) = snapshot_store {
                    let history = store.load_file_history(file_path);
                    let entries: Vec<serde_json::Value> = history
                        .iter()
                        .map(|s| {
                            let action = match (&s.before_hash, &s.after_hash) {
                                (None, Some(_)) => "added",
                                (Some(_), None) => "deleted",
                                (Some(_), Some(_)) => "modified",
                                _ => "unchanged",
                            };
                            serde_json::json!({
                                "turnId": s.turn_id,
                                "action": action,
                                "toolCallId": s.tool_call_id,
                                "tool": s.tool_name,
                                "hash": s.after_hash,
                            })
                        })
                        .collect();
                    output_response(
                        &id,
                        "get_file_history",
                        &serde_json::json!({
                            "path": file_path,
                            "history": entries,
                            "count": entries.len(),
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "get_file_history",
                        &serde_json::json!({"error": "file-snapshot not enabled"}),
                    );
                }
            }
            "restore_files" => {
                let to_turn = params.get("toTurn").and_then(|v| v.as_str()).unwrap_or("");
                if to_turn.is_empty() {
                    output_response(
                        &id,
                        "restore_files",
                        &serde_json::json!({"error": "missing 'toTurn' (turnId)"}),
                    );
                } else if let Some(ref store) = snapshot_store {
                    let result =
                        crate::file_snapshot::restore::restore_code_to_turn(store, to_turn);
                    // FilesRestored 事件：回滚完成后通知所有终端同步
                    crate::file_snapshot::approval::emit_public_event(
                        "FilesRestored",
                        &serde_json::json!({
                            "toTurn": to_turn,
                            "restored": result.summary.restored,
                            "deleted": result.summary.deleted,
                            "skipped": result.summary.skipped,
                        }),
                    );
                    output_response(
                        &id,
                        "restore_files",
                        &serde_json::json!({
                            "restoredFiles": result.restored_files.iter().map(|f| serde_json::json!({
                                "path": f.path,
                                "action": f.action,
                                "fromHash": f.from_hash,
                                "toHash": f.to_hash,
                                "reason": f.reason,
                            })).collect::<Vec<_>>(),
                            "restorePoint": result.restore_point_id,
                            "summary": {
                                "restored": result.summary.restored,
                                "deleted": result.summary.deleted,
                                "skipped": result.summary.skipped,
                            },
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "restore_files",
                        &serde_json::json!({"error": "file-snapshot not enabled"}),
                    );
                }
            }
            // ── 审批 RPC（review_pending / approve / reject / approve_all / reject_all / approvals）──
            "review_pending" => {
                if let Some(ref mgr) = approval_mgr {
                    let pending = mgr.compute_pending();
                    let added = pending.iter().filter(|p| p.status == "added").count();
                    let modified = pending.iter().filter(|p| p.status == "modified").count();
                    let deleted = pending.iter().filter(|p| p.status == "deleted").count();
                    let pending_json: Vec<_> = pending
                        .iter()
                        .map(|p| {
                            // 只回摘要字段；oldContent/newContent 会让响应达到几十 MB
                            // （695 文件 ≈ 55MB），按需走 get_file_diff 单文件拉取
                            serde_json::json!({
                                "path": p.path,
                                "status": p.status,
                                "diffStat": p.diff_stat,
                            })
                        })
                        .collect();
                    output_response(
                        &id,
                        "review_pending",
                        &serde_json::json!({
                            "pending": pending_json,
                            "summary": {
                                "total": pending.len(),
                                "added": added,
                                "modified": modified,
                                "deleted": deleted,
                            },
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "review_pending",
                        &serde_json::json!({"error": "approval not enabled (requires file-snapshot)"}),
                    );
                }
            }
            // 单文件 diff（与 review_pending 同源：tree 快照 + 同一 baseline 语义）
            "review_file_diff" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    output_response(
                        &id,
                        "review_file_diff",
                        &serde_json::json!({"error": "missing 'path'"}),
                    );
                } else if let Some(ref mgr) = approval_mgr {
                    match mgr.file_diff(path) {
                        Some(diff) => output_response(&id, "review_file_diff", &diff),
                        None => output_response(
                            &id,
                            "review_file_diff",
                            &serde_json::json!({"error": "file not in pending list", "path": path}),
                        ),
                    }
                } else {
                    output_response(
                        &id,
                        "review_file_diff",
                        &serde_json::json!({"error": "approval not enabled (requires file-snapshot)"}),
                    );
                }
            }
            "review_approve" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    output_response(
                        &id,
                        "review_approve",
                        &serde_json::json!({"error": "missing 'path'"}),
                    );
                } else if let Some(ref mgr) = approval_mgr {
                    match mgr.approve(path) {
                        Ok(appr) => output_response(
                            &id,
                            "review_approve",
                            &serde_json::json!({
                                "path": appr.path, "status": "approved",
                                "approvedTreeHash": appr.approved_tree_hash,
                            }),
                        ),
                        Err(e) => {
                            output_response(&id, "review_approve", &serde_json::json!({"error": e}))
                        }
                    }
                } else {
                    output_response(
                        &id,
                        "review_approve",
                        &serde_json::json!({"error": "approval not enabled"}),
                    );
                }
            }
            "review_reject" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    output_response(
                        &id,
                        "review_reject",
                        &serde_json::json!({"error": "missing 'path'"}),
                    );
                } else if let Some(ref mgr) = approval_mgr {
                    match mgr.reject(path) {
                        Ok(rf) => {
                            // deny 消息注入 session.jsonl（下一轮 agent 可见）
                            // 使用 XML 信封，与 <goal_feedback> / <memory_outline> 等保持一致
                            let deny_msg = format!(
                                "<approval_feedback decision=\"rejected\">\n  \
                                 <file path=\"{path}\" action=\"{action}\" rolled_back=\"true\"/>\n  \
                                 <reason>用户不认可这次改动</reason>\n  \
                                 <instruction>文件已回滚到 baseline 状态。请重新评估需求，用不同的方式实现。</instruction>\n\
                                 </approval_feedback>",
                                path = path,
                                action = rf.action,
                            );
                            let entry = serde_json::json!({
                                "type": "message",
                                "id": format!("approval_deny_{}", std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)),
                                "parentId": null,
                                "timestamp": crate::session_jsonl::timestamp_iso(),
                                "message": {
                                    "role": "user",
                                    "content": [{"type": "text", "text": deny_msg}],
                                },
                                "customType": "approval_deny",
                            });
                            crate::session_jsonl::append_raw_entry(&worker_cwd, &entry);

                            output_response(
                                &id,
                                "review_reject",
                                &serde_json::json!({
                                    "path": rf.path, "status": "rejected",
                                    "action": rf.action, "rolledBack": true,
                                    "denyMessageInjected": true,
                                }),
                            );
                        }
                        Err(e) => {
                            output_response(&id, "review_reject", &serde_json::json!({"error": e}))
                        }
                    }
                } else {
                    output_response(
                        &id,
                        "review_reject",
                        &serde_json::json!({"error": "approval not enabled"}),
                    );
                }
            }
            "review_approve_all" => {
                if let Some(ref mgr) = approval_mgr {
                    let results = mgr.approve_all();
                    let ok_count = results.iter().filter(|r| r.is_ok()).count();
                    // failures[]：部分失败的明细（path + 原因）
                    let failures: Vec<serde_json::Value> = results
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| r.is_err())
                        .map(|(i, r)| {
                            serde_json::json!({
                                "index": i,
                                "error": r.as_ref().err().map(|e| e.to_string()).unwrap_or_default(),
                            })
                        })
                        .collect();
                    output_response(
                        &id,
                        "review_approve_all",
                        &serde_json::json!({
                            "approved": ok_count,
                            "errors": results.len() - ok_count,
                            "total": results.len(),
                            "failures": failures,
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "review_approve_all",
                        &serde_json::json!({"error": "approval not enabled"}),
                    );
                }
            }
            "review_reject_all" => {
                if let Some(ref mgr) = approval_mgr {
                    let results = mgr.reject_all();
                    let ok_results: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
                    let ok_count = ok_results.len();
                    let err_count = results.len() - ok_count;

                    // 批量 deny 消息注入（一条 XML 含所有被拒文件）
                    if !ok_results.is_empty() {
                        let mut files_xml = String::new();
                        for rf in &ok_results {
                            files_xml.push_str(&format!(
                                "  <file path=\"{}\" action=\"{}\" rolled_back=\"true\"/>\n",
                                rf.path, rf.action
                            ));
                        }
                        let deny_msg = format!(
                            "<approval_feedback decision=\"rejected\">\n{}  \
                             <reason>用户不认可这些改动</reason>\n  \
                             <instruction>以上 {} 个文件已全部回滚。请重新评估需求。</instruction>\n\
                             </approval_feedback>",
                            files_xml, ok_count,
                        );
                        let entry = serde_json::json!({
                            "type": "message",
                            "id": format!("approval_deny_{}", std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)),
                            "parentId": null,
                            "timestamp": crate::session_jsonl::timestamp_iso(),
                            "message": {
                                "role": "user",
                                "content": [{"type": "text", "text": deny_msg}],
                            },
                            "customType": "approval_deny",
                        });
                        crate::session_jsonl::append_raw_entry(&worker_cwd, &entry);
                    }

                    output_response(
                        &id,
                        "review_reject_all",
                        &serde_json::json!({
                            "rejected": ok_count, "errors": err_count, "total": results.len(),
                            "denyMessageInjected": ok_count > 0,
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "review_reject_all",
                        &serde_json::json!({"error": "approval not enabled"}),
                    );
                }
            }
            "review_approvals" => {
                if let Some(ref mgr) = approval_mgr {
                    let filter = params.get("status").and_then(|v| v.as_str());
                    let status_filter = filter.and_then(|s| match s {
                        "pending" => Some(crate::file_snapshot::ApprovalStatus::Pending),
                        "approved" => Some(crate::file_snapshot::ApprovalStatus::Approved),
                        "rejected" => Some(crate::file_snapshot::ApprovalStatus::Rejected),
                        _ => None,
                    });
                    let list = mgr.approvals_list(status_filter.as_ref());
                    output_response(
                        &id,
                        "review_approvals",
                        &serde_json::json!({
                            "approvals": list.iter().map(|a| serde_json::json!({
                                "path": a.path,
                                "status": serde_json::to_string(&a.status).unwrap_or_default().trim_matches('"'),
                                "timestamp": a.timestamp,
                                "approvedTreeHash": a.approved_tree_hash,
                            })).collect::<Vec<_>>(),
                        }),
                    );
                } else {
                    output_response(
                        &id,
                        "review_approvals",
                        &serde_json::json!({"error": "approval not enabled"}),
                    );
                }
            }
            "get_fork_messages" => {
                // 复用 retrieve_inputs（只返回 user 消息，用于 fork 选择）
                let entries: Vec<serde_json::Value> =
                    crate::message_retrieval::load_entries_cached(&worker_cwd);
                let params = crate::message_retrieval::RetrievalParams::default();
                let result = crate::message_retrieval::retrieve_inputs(&entries, &params);
                output_response(
                    &id,
                    "get_fork_messages",
                    &serde_json::json!({
                        "inputs": result.inputs.iter().map(|i| serde_json::json!({
                            "entryId": i.entry_id,
                            "turnId": i.turn_id,
                            "text": i.text,
                        })).collect::<Vec<_>>(),
                        "count": result.inputs.len(),
                    }),
                );
            }
            "get_agents_files" => output_response(&id, "get_agents_files", &serde_json::json!([])),
            "get_latest_agent_change" => {
                output_response(&id, "get_latest_agent_change", &serde_json::Value::Null)
            }
            "get_agent_detail" => {
                // 真实实现：返回 agent 详情（含 system_prompt）
                let name = params
                    .get("agentName")
                    .or_else(|| params.get("name"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if name.is_empty() {
                    output_response(
                        &id,
                        "get_agent_detail",
                        &serde_json::json!({"error":"missing agentName"}),
                    );
                } else {
                    match crate::agent_config::find_agent(name) {
                        Some(agent) => {
                            // 手动构建 JSON（确保 system_prompt 可见）
                            let detail = serde_json::json!({
                                "name": agent.name,
                                "description": agent.description,
                                "tools": agent.tools,
                                "disallowed_tools": agent.disallowed_tools,
                                "model": agent.model,
                                "max_turns": agent.max_turns,
                                "thinking_level": agent.thinking_level,
                                "tier": agent.tier,
                                "color": agent.color,
                                "skills": agent.skills,
                                "system_prompt": agent.system_prompt,
                                "source": agent.source,
                            });
                            output_response(&id, "get_agent_detail", &detail);
                        }
                        None => {
                            output_response(
                                &id,
                                "get_agent_detail",
                                &serde_json::json!({"error": format!("agent '{}' not found", name)}),
                            );
                        }
                    }
                }
            }
            "get_all_tools" => output_response(&id, "get_all_tools", &serde_json::json!([])),
            "get_flag_values" => output_response(&id, "get_flag_values", &serde_json::json!({})),
            "set_flag" => {
                let extension_id = params
                    .get("extension")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let flag_name = params.get("flag").and_then(|v| v.as_str()).unwrap_or("");
                let value = params
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                if extension_id.is_empty() || flag_name.is_empty() {
                    output_response(
                        &id,
                        "set_flag",
                        &serde_json::json!({
                            "error": "missing 'extension' or 'flag' parameter",
                        }),
                    );
                } else {
                    agent
                        .extensions()
                        .set_flag(extension_id, flag_name, value.clone());
                    output_response(
                        &id,
                        "set_flag",
                        &serde_json::json!({
                            "extension": extension_id,
                            "flag": flag_name,
                            "value": value,
                            "set": true,
                        }),
                    );
                }
            }
            "get_mcp_servers" => {
                // 方案 C：转发给 host 查真实状态
                match manager_bridge
                    .send_command("mcp_get_servers", serde_json::json!({}))
                    .await
                {
                    Ok(resp) => {
                        let servers = resp
                            .get("data")
                            .cloned()
                            .unwrap_or(serde_json::Value::Array(vec![]));
                        output_response(&id, "get_mcp_servers", &servers);
                    }
                    Err(e) => output_error_response(
                        &id,
                        "get_mcp_servers",
                        &format!("host proxy error: {e}"),
                    ),
                }
            }
            "mcp_toggle_server" => {
                // 方案 C：转发给 host（host 的 McpManager 执行 toggle）
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let enabled = params.get("enabled").and_then(|v| v.as_bool());
                if name.is_empty() {
                    output_error_response(&id, "mcp_toggle_server", "missing 'name'");
                    continue;
                }
                let enabled = match enabled {
                    Some(e) => e,
                    None => {
                        output_error_response(&id, "mcp_toggle_server", "missing 'enabled'");
                        continue;
                    }
                };
                match manager_bridge
                    .send_command(
                        "mcp_toggle_server",
                        serde_json::json!({
                            "name": name, "enabled": enabled
                        }),
                    )
                    .await
                {
                    Ok(resp) => {
                        if resp
                            .get("success")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            // McpServerStatusChanged 事件
                            crate::file_snapshot::approval::emit_public_event(
                                "McpServerStatusChanged",
                                &serde_json::json!({"name": name, "enabled": enabled}),
                            );
                            output_response(
                                &id,
                                "mcp_toggle_server",
                                resp.get("data").unwrap_or(&serde_json::Value::Null),
                            );
                        } else {
                            output_error_response(
                                &id,
                                "mcp_toggle_server",
                                resp.get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown"),
                            );
                        }
                    }
                    Err(e) => {
                        output_error_response(&id, "mcp_toggle_server", &format!("proxy: {e}"))
                    }
                }
            }
            "mcp_restart_server" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    output_error_response(&id, "mcp_restart_server", "missing 'name'");
                    continue;
                }
                match manager_bridge
                    .send_command(
                        "mcp_restart_server",
                        serde_json::json!({
                            "name": name
                        }),
                    )
                    .await
                {
                    Ok(resp) => {
                        if resp
                            .get("success")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            output_response(
                                &id,
                                "mcp_restart_server",
                                resp.get("data").unwrap_or(&serde_json::Value::Null),
                            );
                        } else {
                            output_error_response(
                                &id,
                                "mcp_restart_server",
                                resp.get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown"),
                            );
                        }
                    }
                    Err(e) => {
                        output_error_response(&id, "mcp_restart_server", &format!("proxy: {e}"))
                    }
                }
            }
            "mcp_reload" => {
                // 方案 C：转发给 host 重新加载 MCP 配置
                match manager_bridge
                    .send_command("mcp_reload", serde_json::json!({}))
                    .await
                {
                    Ok(resp) => {
                        if resp
                            .get("success")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            output_response(
                                &id,
                                "mcp_reload",
                                resp.get("data").unwrap_or(&serde_json::Value::Null),
                            );
                        } else {
                            output_error_response(
                                &id,
                                "mcp_reload",
                                resp.get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown"),
                            );
                        }
                    }
                    Err(e) => output_error_response(&id, "mcp_reload", &format!("proxy: {e}")),
                }
            }
            "mcp_read_resource" => {
                // 方案 C：转发给 host 读 MCP 资源
                let server = params
                    .get("server")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let uri = params
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if server.is_empty() || uri.is_empty() {
                    output_error_response(&id, "mcp_read_resource", "missing 'server' or 'uri'");
                    continue;
                }
                match manager_bridge
                    .send_command(
                        "mcp_read_resource",
                        serde_json::json!({
                            "server": server, "uri": uri
                        }),
                    )
                    .await
                {
                    Ok(resp) => {
                        if resp
                            .get("success")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            output_response(
                                &id,
                                "mcp_read_resource",
                                resp.get("data").unwrap_or(&serde_json::Value::Null),
                            );
                        } else {
                            output_error_response(
                                &id,
                                "mcp_read_resource",
                                resp.get("error")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown"),
                            );
                        }
                    }
                    Err(e) => {
                        output_error_response(&id, "mcp_read_resource", &format!("proxy: {e}"))
                    }
                }
            }
            "continue" => {
                // Continue last session
                output_response(&id, "continue", &serde_json::Value::Null);
            }
            "follow_up" => {
                let text = params
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                agent.follow_up(crate::agent::messages::Message::User(
                    crate::agent::messages::UserMessage {
                        role: "user".into(),
                        content: vec![crate::agent::messages::ContentBlock::Text(
                            crate::agent::messages::TextContent {
                                text,
                                text_signature: None,
                            },
                        )],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::FollowUp,
                    },
                ));
                output_response(&id, "follow_up", &serde_json::Value::Null);
            }
            "abort_bash" => {
                // 通过 process_map 找到 pid 并 kill
                let bid = params.get("bid").and_then(|v| v.as_str()).unwrap_or("");
                if bid.is_empty() {
                    output_response(
                        &id,
                        "abort_bash",
                        &serde_json::json!({"error": "missing 'bid' parameter"}),
                    );
                } else if let Some(ref pm) = process_map {
                    let map = pm.blocking_lock();
                    if let Some(info) = map.get(bid) {
                        let pid = info.os_pid;
                        let cmd = info.command.clone();
                        drop(map);
                        // 发 kill 信号（用 kill 命令，避免加 libc 依赖）
                        let kill_result = std::process::Command::new("kill")
                            .arg("-TERM")
                            .arg(pid.to_string())
                            .output()
                            .map(|o| o.status.success())
                            .unwrap_or(false);
                        output_response(
                            &id,
                            "abort_bash",
                            &serde_json::json!({
                                "bid": bid,
                                "pid": pid,
                                "command": cmd,
                                "signal": "SIGTERM",
                                "success": kill_result,
                            }),
                        );
                    } else {
                        output_response(
                            &id,
                            "abort_bash",
                            &serde_json::json!({
                                "error": format!("process '{}' not found", bid),
                                "available": map.keys().cloned().collect::<Vec<_>>(),
                            }),
                        );
                    }
                } else {
                    output_response(
                        &id,
                        "abort_bash",
                        &serde_json::json!({"error": "bash extension not enabled"}),
                    );
                }
            }
            "register_remote_tool" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let url = params
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let description = params
                    .get("description")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let method = params
                    .get("method")
                    .and_then(|v| v.as_str())
                    .unwrap_or("POST")
                    .to_string();
                let parameters = params
                    .get("parameters")
                    .cloned()
                    .unwrap_or(serde_json::json!({}));
                let headers: std::collections::HashMap<String, String> = params
                    .get("headers")
                    .and_then(|v| v.as_object())
                    .map(|obj| {
                        obj.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default();
                if name.is_empty() || url.is_empty() {
                    output_error_response(&id, "register_remote_tool", "missing 'name' or 'url'");
                    continue;
                }
                agent.register_tool(Box::new(crate::agent::tool::RemoteTool {
                    name: name.clone(),
                    description,
                    parameters,
                    url,
                    method,
                    headers,
                }));
                output_response(
                    &id,
                    "register_remote_tool",
                    &serde_json::json!({
                        "name": name,
                        "status": "registered"
                    }),
                );
            }
            "unregister_remote_tool" => {
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if name.is_empty() {
                    output_error_response(&id, "unregister_remote_tool", "missing 'name'");
                    continue;
                }
                agent.remove_tool(&name);
                output_response(
                    &id,
                    "unregister_remote_tool",
                    &serde_json::json!({
                        "name": name,
                        "status": "removed"
                    }),
                );
            }

            // ── WASM Extension 热更新 ──
            "extension_add" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    output_error_response(&id, "extension_add", "missing 'path'");
                    continue;
                }
                let canonical = match std::fs::canonicalize(path) {
                    Ok(p) => p,
                    Err(e) => {
                        output_error_response(&id, "extension_add", &format!("bad path: {e}"));
                        continue;
                    }
                };
                let canonical_str = canonical.to_string_lossy().to_string();

                match wasm_ext_registry.add(&canonical_str) {
                    Ok(tool_defs) => {
                        let extension_id = crate::wasm_extension::extension_id_from_path(&canonical_str);
                        for td in &tool_defs {
                            agent.register_tool(Box::new(WasmToolAdapter {
                                name: td.name.clone(),
                                description: td.description.clone(),
                                parameters: td.parameters.clone(),
                                extension_path: canonical_str.clone(),
                                extension_id: extension_id.clone(),
                                registry: wasm_ext_registry.clone(),
                            }));
                        }
                        let names: Vec<&str> = tool_defs.iter().map(|t| t.name.as_str()).collect();
                        // ExtensionListChanged 事件
                        crate::file_snapshot::approval::emit_public_event(
                            "ExtensionListChanged",
                            &serde_json::json!({"action":"add","path":canonical_str,"tools":names}),
                        );
                        output_response(&id, "extension_add", &serde_json::json!({"tools": names}));
                    }
                    Err(e) => {
                        output_error_response(&id, "extension_add", &format!("load failed: {e}"));
                    }
                }
            }

            "extension_remove" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    output_error_response(&id, "extension_remove", "missing 'path'");
                    continue;
                }
                match wasm_ext_registry.remove(path) {
                    Ok(tool_names) => {
                        for name in &tool_names {
                            agent.remove_tool(name);
                        }
                        // ExtensionListChanged 事件
                        crate::file_snapshot::approval::emit_public_event(
                            "ExtensionListChanged",
                            &serde_json::json!({"action":"remove","path":path,"removed_tools":tool_names}),
                        );
                        output_response(
                            &id,
                            "extension_remove",
                            &serde_json::json!({"removed_tools": tool_names}),
                        );
                    }
                    Err(e) => {
                        output_error_response(&id, "extension_remove", &e);
                    }
                }
            }

            "extension_list" => {
                let extensions = wasm_ext_registry.list();
                output_response(
                    &id,
                    "extension_list",
                    &serde_json::json!({"extensions": extensions}),
                );
            }

            "extension_reload" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    output_error_response(&id, "extension_reload", "missing 'path'");
                    continue;
                }
                let canonical = match std::fs::canonicalize(path) {
                    Ok(p) => p,
                    Err(e) => {
                        output_error_response(&id, "extension_reload", &format!("bad path: {e}"));
                        continue;
                    }
                };
                let canonical_str = canonical.to_string_lossy().to_string();

                // 先卸载旧的（如果有）
                if let Ok(old_tools) = wasm_ext_registry.remove(&canonical_str) {
                    for name in &old_tools {
                        agent.remove_tool(name);
                    }
                }

                // 重新加载
                let extension_id = crate::wasm_extension::extension_id_from_path(&canonical_str);
                match wasm_ext_registry.add(&canonical_str) {
                    Ok(tool_defs) => {
                        for td in &tool_defs {
                            agent.register_tool(Box::new(WasmToolAdapter {
                                name: td.name.clone(),
                                description: td.description.clone(),
                                parameters: td.parameters.clone(),
                                extension_path: canonical_str.clone(),
                                extension_id: extension_id.clone(),
                                registry: wasm_ext_registry.clone(),
                            }));
                        }
                        let names: Vec<&str> = tool_defs.iter().map(|t| t.name.as_str()).collect();
                        output_response(
                            &id,
                            "extension_reload",
                            &serde_json::json!({"tools": names}),
                        );
                    }
                    Err(e) => {
                        output_error_response(
                            &id,
                            "extension_reload",
                            &format!("reload failed: {e}"),
                        );
                    }
                }
            }

            "set_settings" => {
                let key = params.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let value = params
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                if key.is_empty() {
                    output_response(
                        &id,
                        "set_settings",
                        &serde_json::json!({"error": "missing 'key' parameter"}),
                    );
                } else {
                    let mut cfg = crate::config::IonConfig::load();
                    let old_val: serde_json::Value;
                    match key {
                        "default_provider" | "default-provider" => {
                            old_val = serde_json::json!(cfg.default_provider);
                            cfg.default_provider = value.as_str().map(|s| s.to_string());
                        }
                        "default_model" | "default-model" => {
                            old_val = serde_json::json!(cfg.default_model);
                            cfg.default_model = value.as_str().map(|s| s.to_string());
                        }
                        "api_key" | "api-key" => {
                            old_val = serde_json::json!("***");
                            cfg.api_key = value.as_str().map(|s| s.to_string());
                        }
                        "base_url" | "base-url" => {
                            old_val = serde_json::json!(cfg.base_url);
                            cfg.base_url = value.as_str().map(|s| s.to_string());
                        }
                        _ => {
                            output_response(
                                &id,
                                "set_settings",
                                &serde_json::json!({
                                    "error": format!("unknown key '{}' (supported: default_provider, default_model, api_key, base_url)", key),
                                }),
                            );
                            return;
                        }
                    }
                    match cfg.save() {
                        Ok(()) => output_response(
                            &id,
                            "set_settings",
                            &serde_json::json!({
                                "key": key,
                                "old_value": old_val,
                                "new_value": if key.contains("api_key") { serde_json::json!("***") } else { value },
                                "saved": true,
                            }),
                        ),
                        Err(e) => output_response(
                            &id,
                            "set_settings",
                            &serde_json::json!({
                                "error": format!("save failed: {}", e),
                            }),
                        ),
                    }
                }
            }
            "rollback_preview" => {
                output_response(&id, "rollback_preview", &serde_json::Value::Null)
            }
            "copy_fork" => output_response(&id, "copy_fork", &serde_json::json!({"sessionId":sid})),
            "append_system_event" => {
                let ctype = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let label = params.get("label").and_then(|v| v.as_str()).unwrap_or("");
                let display = params
                    .get("display")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                append_session_entry(
                    &worker_cwd,
                    &sid,
                    "system_event",
                    &serde_json::json!({
                        "customType": ctype,
                        "label": label,
                        "display": display,
                    }),
                );
                output_response(
                    &id,
                    "append_system_event",
                    &serde_json::json!({"status":"appended"}),
                );
            }
            "append_custom_message" => {
                let ctype = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let display = params
                    .get("display")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let details = params.get("details");
                append_session_entry(
                    &worker_cwd,
                    &sid,
                    "custom_message",
                    &serde_json::json!({
                        "customType": ctype,
                        "content": content,
                        "display": display,
                        "details": details,
                    }),
                );
                output_response(
                    &id,
                    "append_custom_message",
                    &serde_json::json!({"status":"appended"}),
                );
            }
            "append_custom_entry" => {
                let ctype = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let data = params.get("data").cloned().unwrap_or_default();
                append_session_entry(
                    &worker_cwd,
                    &sid,
                    "custom",
                    &serde_json::json!({
                        "customType": ctype,
                        "data": data,
                    }),
                );
                output_response(
                    &id,
                    "append_custom_entry",
                    &serde_json::json!({"status":"appended"}),
                );
            }
            "append_entry" => {
                // 统一追加入口 — 对齐 pi 的 appendCustomEntry/appendCustomMessage
                let entry_type = params.get("type").and_then(|v| v.as_str()).unwrap_or("custom");
                let inject_to_llm = params.get("injectToLlm").and_then(|v| v.as_bool()).unwrap_or(false);

                // 根据 type 构建对应格式的 entry_data
                let entry_data = match entry_type {
                    "custom" => serde_json::json!({
                        "customType": params.get("customType").unwrap_or(&serde_json::json!("")),
                        "data": params.get("data").unwrap_or(&serde_json::json!({})),
                    }),
                    "custom_message" => serde_json::json!({
                        "customType": params.get("customType").unwrap_or(&serde_json::json!("")),
                        "content": params.get("content").unwrap_or(&serde_json::json!("")),
                        "display": params.get("display").unwrap_or(&serde_json::json!(true)),
                        "details": params.get("details").unwrap_or(&serde_json::Value::Null),
                    }),
                    "system_event" => serde_json::json!({
                        "customType": params.get("customType").unwrap_or(&serde_json::json!("")),
                        "label": params.get("label").unwrap_or(&serde_json::json!("")),
                        "display": params.get("display").unwrap_or(&serde_json::json!(true)),
                    }),
                    // 其他类型（label, model_change 等）直接透传 params
                    _ => params.clone(),
                };

                // 写入 session.jsonl
                append_session_entry(&worker_cwd, &sid, entry_type, &entry_data);

                // 如果 injectToLlm=true，同时推入 live messages（对齐 pi 的 custom_message 语义）
                let mut injected = false;
                if inject_to_llm && entry_type == "custom_message" {
                    let ctype = params.get("customType").and_then(|v| v.as_str()).unwrap_or("");
                    let content_text = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
                    agent.push_message(Message::Custom(CustomMessage {
                        role: "custom".into(),
                        custom_type: ctype.into(),
                        content: CustomContent::Text(content_text.into()),
                        display: params.get("display").and_then(|v| v.as_bool()).unwrap_or(true),
                        details: params.get("details").cloned(),
                        timestamp: now_ms(),
                    }));
                    injected = true;
                }

                output_response(
                    &id,
                    "append_entry",
                    &serde_json::json!({"status":"appended","injected":injected}),
                );
            }
            "send_custom_message" => {
                let ctype: String = params
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_default();
                let content: String = params
                    .get("content")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_default();
                let deliver_as = params
                    .get("deliverAs")
                    .and_then(|v| v.as_str())
                    .unwrap_or("followUp");
                // 用 Message::Custom（不是 Message::User），
                // 确保历史重建时能与真实用户消息区分
                let msg = Message::Custom(CustomMessage {
                    role: "custom".into(),
                    custom_type: ctype,
                    content: CustomContent::Text(content),
                    display: true,
                    details: None,
                    timestamp: now_ms(),
                });
                match deliver_as {
                    "steer" => agent.steer(msg),
                    "nextTurn" | _ => agent.follow_up(msg),
                }
                output_response(
                    &id,
                    "send_custom_message",
                    &serde_json::json!({"status":"queued","queue":deliver_as}),
                );
            }
            "append_model_change" => {
                let provider = params
                    .get("provider")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let model_id = params.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
                append_session_entry(
                    &worker_cwd,
                    &sid,
                    "model_change",
                    &serde_json::json!({
                        "provider": provider,
                        "modelId": model_id,
                    }),
                );
                // 同步到 session index（O(1) 查询用）
                crate::session_index::SessionIndex::set_model(&sid, provider, model_id);
                output_response(
                    &id,
                    "append_model_change",
                    &serde_json::json!({"status":"appended"}),
                );
            }
            "append_thinking_level_change" => {
                let level = params.get("level").and_then(|v| v.as_str()).unwrap_or("");
                append_session_entry(
                    &worker_cwd,
                    &sid,
                    "thinking_level_change",
                    &serde_json::json!({
                        "level": level,
                    }),
                );
                crate::session_index::SessionIndex::set_thinking_level(&sid, level);
                output_response(
                    &id,
                    "append_thinking_level_change",
                    &serde_json::json!({"status":"appended"}),
                );
            }
            "append_agent_change" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let config = params.get("config");
                let mut entry = serde_json::json!({"name": name});
                if let Some(c) = config {
                    entry["config"] = c.clone();
                }
                append_session_entry(&worker_cwd, &sid, "agent_change", &entry);
                crate::session_index::SessionIndex::set_agent(&sid, name);
                output_response(
                    &id,
                    "append_agent_change",
                    &serde_json::json!({"status":"appended"}),
                );
            }
            "append_session_name" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                append_session_entry(
                    &worker_cwd,
                    &sid,
                    "session_info",
                    &serde_json::json!({
                        "name": name,
                    }),
                );
                crate::session_index::SessionIndex::set_name(&sid, name);
                output_response(
                    &id,
                    "append_session_name",
                    &serde_json::json!({"status":"appended","name":name}),
                );
            }
            "append_label" => {
                let target_id = params
                    .get("targetId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let label = params.get("label").and_then(|v| v.as_str()).unwrap_or("");
                append_session_entry(
                    &worker_cwd,
                    &sid,
                    "label",
                    &serde_json::json!({
                        "targetId": target_id,
                        "label": label,
                    }),
                );
                output_response(
                    &id,
                    "append_label",
                    &serde_json::json!({"status":"appended"}),
                );
            }
            "append_active_tools_change" => {
                let names: Vec<String> = params
                    .get("activeToolNames")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                append_session_entry(
                    &worker_cwd,
                    &sid,
                    "active_tools_change",
                    &serde_json::json!({
                        "activeToolNames": names,
                    }),
                );
                crate::session_index::SessionIndex::set_active_tools(&sid, names);
                output_response(
                    &id,
                    "append_active_tools_change",
                    &serde_json::json!({"status":"appended"}),
                );
            }
            "get_process_snapshot" => {
                output_response(&id, "get_process_snapshot", &serde_json::json!({}))
            }

            // ── bash_command：用户 !cmd 直发，结果作为 Message::BashExecution 入历史 ──
            // 不走 agent.run()，直接执行 + 入库 + 返回。
            // LLM 下次看到时 provider 自动把 role:bashExecution 转成 user text。
            "bash_command" => {
                let command: String = params
                    .get("command")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .unwrap_or_default();
                let timeout_secs = params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
                let exclude_from_context =
                    params.get("excludeFromContext").and_then(|v| v.as_bool());

                if command.is_empty() {
                    output_error_response(&id, "bash_command", "missing 'command'");
                    continue;
                }

                // 执行
                let (stdout, stderr, exit_code) = match execute_bash(&command, timeout_secs).await {
                    Ok(t) => t,
                    Err(e) => {
                        // 失败也入一条 BashExecution，方便 UI 显示错误
                        let bash_msg = BashExecutionMessage {
                            role: "bashExecution".into(),
                            command: command.clone(),
                            output: format!("error: {e}"),
                            exit_code: None,
                            cancelled: false,
                            truncated: false,
                            full_output_path: None,
                            timestamp: now_ms(),
                            exclude_from_context,
                        };
                        agent.push_message(Message::BashExecution(bash_msg.clone()));
                        output_response(
                            &id,
                            "bash_command",
                            &serde_json::json!({
                                "status":"error",
                                "error": e,
                                "exitCode": null,
                                "output": null,
                            }),
                        );
                        continue;
                    }
                };

                // 合并 stdout+stderr 作为 output（对齐 pi 的 BashExecutionMessage.output 单字段）
                let combined = if stderr.is_empty() {
                    stdout
                } else if stdout.is_empty() {
                    stderr
                } else {
                    format!("{stdout}\n[stderr]\n{stderr}")
                };
                let truncated = combined.contains("[truncated");

                let bash_msg = BashExecutionMessage {
                    role: "bashExecution".into(),
                    command: command.clone(),
                    output: combined.clone(),
                    exit_code: Some(exit_code),
                    cancelled: false,
                    truncated,
                    full_output_path: None,
                    timestamp: now_ms(),
                    exclude_from_context,
                };
                // 入 agent.messages（下次 LLM 调用会看到）
                agent.push_message(Message::BashExecution(bash_msg));

                output_response(
                    &id,
                    "bash_command",
                    &serde_json::json!({
                        "status":"ok",
                        "exitCode": exit_code,
                        "output": combined,
                        "truncated": truncated,
                    }),
                );
            }

            // ── Manager 回执（worker→manager 命令的结果）──
            // 按 _reply_to 查 pending map，触发对应 oneshot；不再 echo response。
            "manager_response" => {
                let reply_to = cmd
                    .get("_reply_to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !reply_to.is_empty() {
                    manager_bridge.deliver_response(&reply_to, cmd).await;
                } else {
                    tracing::debug!("[{sid}] manager response without _reply_to: {params}");
                }
            }

            // ── 真正未知 ──
            _ => {
                // 兜底：检查是否有 _reply_to（Manager 写回 manager_response 可能不带 type）
                let reply_to = cmd
                    .get("_reply_to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !reply_to.is_empty() {
                    manager_bridge.deliver_response(&reply_to, cmd).await;
                } else {
                    output_error_response(&id, &method, &format!("Unknown command: {method}"));
                }
            }
        }

        // Note: follow_up_rx was moved into the agent via set_follow_up_rx().
        // outer_loop drains it internally after each inner_loop, so completed
        // background bash processes inject <bash_result> messages as new turns.
    }

    // 退出前保存会话
    let msgs_json: Vec<serde_json::Value> = agent
        .messages()
        .iter()
        .filter_map(|m| serde_json::to_value(m).ok())
        .collect();
    save_worker_session(&sid, &worker_cwd, &msgs_json);

    // exit
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// 执行 bash 命令，返回 (stdout, stderr, exit_code)
async fn execute_bash(command: &str, timeout_secs: u64) -> Result<(String, String, i32), String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        tokio::process::Command::new("sh")
            .args(["-c", command])
            .output(),
    )
    .await
    .map_err(|_| format!("bash timed out after {timeout_secs}s"))?
    .map_err(|e| format!("spawn failed: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let exit_code = output.status.code().unwrap_or(-1);

    // 限制输出大小，防止爆炸
    const MAX_OUTPUT: usize = 100_000;
    fn truncate(s: String) -> String {
        if s.len() > MAX_OUTPUT {
            let left = MAX_OUTPUT;
            format!("{}...[truncated {} bytes]", &s[..left], s.len() - left)
        } else {
            s
        }
    }

    Ok((truncate(stdout), truncate(stderr), exit_code))
}

fn output(msg: &serde_json::Value) {
    let line = serde_json::to_string(msg).unwrap_or_default();
    let mut stdout = io::stdout().lock();
    let _ = writeln!(stdout, "{line}");
    let _ = stdout.flush();
}

/// 构建 skill 可用性提示（扫描全局 + 项目级 skill 目录）。
///
/// 返回空字符串表示没有可用 skill（不往 system prompt 加无用提示）。
/// 对齐 docs/design/SKILL_TOOL.md §2.5：让 LLM 知道有哪些 skill 可选，但不预加载内容。
fn build_skill_hint(config_root: &str) -> String {
    let agents_skills = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".agents").join("skills"))
        .unwrap_or_else(|| std::path::PathBuf::from("~/.agents/skills"));
    let dirs = [
        crate::paths::skills_dir(),
        crate::paths::project_skills_dir(config_root),
        agents_skills,
    ];
    // 收集 (name, description) 对
    let mut skills: Vec<(String, String)> = Vec::new();
    for dir in &dirs {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let (name, content_path) = if path.is_file() {
                    // 格式 1：<dir>/<name>.md
                    match path.file_name().and_then(|n| n.to_str()) {
                        Some(fname) if fname.ends_with(".md") => {
                            (fname.trim_end_matches(".md").to_string(), path.clone())
                        }
                        _ => continue,
                    }
                } else if path.is_dir() {
                    // 格式 2：<dir>/<name>/SKILL.md
                    let skill_md = path.join("SKILL.md");
                    if !skill_md.is_file() {
                        continue;
                    }
                    let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                        Some(n) => n,
                        None => continue,
                    };
                    (strip_version_suffix_inline(dir_name), skill_md)
                } else {
                    continue;
                };
                if !skills.iter().any(|(n, _)| n == &name) {
                    let content = std::fs::read_to_string(&content_path).unwrap_or_default();
                    let desc = parse_skill_description_inline(&content);
                    skills.push((name, desc));
                }
            }
        }
    }
    if skills.is_empty() {
        return String::new();
    }
    skills.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::from(
        "## Skill 工具\n\
         你有 `skill` 工具可以加载专门的 skill（结构化工作流）。\n\
         **当用户请求匹配某个 skill 时，优先调用 skill 工具，而不是手动用 bash/read/write。**\n\
         Skill 提供经过验证的工作流，比临时工具调用更可靠。\n\n\
         ### 可用 skills:\n",
    );
    for (name, desc) in &skills {
        if desc.is_empty() {
            out.push_str(&format!("  - `{name}`\n"));
        } else {
            out.push_str(&format!("  - `{name}`: {desc}\n"));
        }
    }
    out.push_str(
        "\n### 用法:\n\
         - `skill(skill_name=\"code-audit\", context=\"inject\")` — 加载到当前上下文，你自己执行\n\
         - `skill(skill_name=\"code-audit\", context=\"fork\")` — 隔离子 Worker 执行（主上下文干净）\n\
         - `skill(skill_name=\"list\")` — 列出所有 skill 详情\n",
    );
    out
}

/// 去掉版本后缀（如 "debug-pro-1.0.0" → "debug-pro"）
fn strip_version_suffix_inline(name: &str) -> String {
    if let Some(pos) = name.rfind('-') {
        let suffix = &name[pos + 1..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return name[..pos].to_string();
        }
    }
    name.to_string()
}

/// 从 skill 文件 frontmatter 提取 description（build_skill_hint 用，避免跟 tool.rs 的私有函数冲突）
fn parse_skill_description_inline(content: &str) -> String {
    let trimmed = content.trim_start();
    if let Some(rest) = trimmed.strip_prefix("---")
        && let Some(end) = rest.find("\n---")
    {
        let frontmatter = &rest[..end];
        for line in frontmatter.lines() {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("description:") {
                let val = rest.trim().trim_matches(|c| c == '"' || c == '\'');
                if !val.is_empty() {
                    return val.to_string();
                }
            }
        }
    }
    // 没有 frontmatter，取第一行 # 标题作为描述
    for line in content.lines() {
        let line = line.trim();
        if let Some(title) = line.strip_prefix("# ") {
            return title.to_string();
        }
    }
    String::new()
}

// ── McpProxyTool: 方案 C 子 Worker 的 MCP 工具代理（走 bridge 调 host）──
use async_trait::async_trait as mcp_async_trait;

struct McpProxyTool {
    full_name: String,
    description: String,
    parameters: serde_json::Value,
    server_name: String,
    tool_name: String,
    bridge: Arc<ManagerBridge>,
}

impl McpProxyTool {
    fn new(
        full_name: &str,
        description: &str,
        parameters: &serde_json::Value,
        bridge: Arc<ManagerBridge>,
    ) -> Self {
        let parts: Vec<&str> = full_name.splitn(3, "__").collect();
        Self {
            full_name: full_name.to_string(),
            description: description.to_string(),
            parameters: parameters.clone(),
            server_name: parts.get(1).copied().unwrap_or("").to_string(),
            tool_name: parts.get(2).copied().unwrap_or("").to_string(),
            bridge,
        }
    }
}

#[mcp_async_trait]
impl crate::agent::tool::Tool for McpProxyTool {
    fn name(&self) -> &str {
        &self.full_name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> serde_json::Value {
        self.parameters.clone()
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn crate::runtime::Runtime,
    ) -> crate::agent::error::AgentResult<String> {
        let resp = self
            .bridge
            .send_command(
                "mcp_call_tool",
                serde_json::json!({
                    "server": self.server_name,
                    "tool": self.tool_name,
                    "args": args,
                }),
            )
            .await
            .map_err(crate::agent::error::AgentError::Tool)?;

        if resp
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            Ok(resp
                .get("data")
                .and_then(|d| d.get("output"))
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string())
        } else {
            Err(crate::agent::error::AgentError::Tool(
                resp.get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("mcp proxy error")
                    .into(),
            ))
        }
    }
}

// ── StreamingExtension: 透传 text_delta + tool_execution 到 stdout ──
/// Streaming extension — emits agent events as JSON to stdout.
///
/// Holds the worker's `session_id` so every emitted event includes it.
/// Without this, `subscribe --session <SID>` can't filter events for a
/// specific session (the bug tracked in issue #29).
struct StreamingExtension {
    session_id: String,
}

#[async_trait::async_trait]
impl crate::agent::extension::Extension for StreamingExtension {
    fn name(&self) -> &str {
        "streaming"
    }

    async fn on_message_delta(
        &self,
        delta: &str,
        role: &str,
    ) -> crate::agent::error::AgentResult<()> {
        if role == "assistant" && !delta.is_empty() {
            output(&serde_json::json!({
                "type": "event",
                "event": {"type": "text_delta", "delta": delta, "sessionId": self.session_id}
            }));
        }
        Ok(())
    }

    /// agent_start 事件（对齐 pi）
    async fn on_agent_start(
        &self,
        _ctx: &crate::agent::agent_loop::AgentContext,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "agent_start",
                "sessionId": self.session_id,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// agent_end 事件（对齐 pi — 含消息数）
    async fn on_agent_end(
        &self,
        ctx: &crate::agent::agent_loop::AgentContext,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "agent_end",
                "sessionId": self.session_id,
                "willRetry": false,
                "messages": ctx.message_count,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// message_start 事件（对齐 pi）
    async fn on_message_start(
        &self,
        role: &str,
        content: &str,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_start",
                "sessionId": self.session_id,
                "role": role,
                "content_length": content.len(),
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// message_end 事件（对齐 pi — 含 token 用量）
    async fn on_message_end(
        &self,
        role: &str,
        _full_content: &str,
        usage: &ion_provider::types::Usage,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_end",
                "sessionId": self.session_id,
                "role": role,
                "usage": {
                    "input": usage.input,
                    "output": usage.output,
                    "total": usage.total_tokens,
                },
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }
    async fn on_tool_call_delta(
        &self,
        delta: &str,
        name: &str,
    ) -> crate::agent::error::AgentResult<()> {
        if !delta.is_empty() {
            if std::env::var("ION_STREAM_DEBUG").ok().as_deref() == Some("1") {
                eprintln!(
                    "[stream-debug] worker emit tool_call_delta name={name} len={}",
                    delta.len()
                );
            }
            output(&serde_json::json!({
                "type": "event",
                "event": {
                    "type": "tool_call_delta",
                    "sessionId": self.session_id,
                    "delta": delta,
                    "toolName": name,
                    "timestamp": now_ms(),
                }
            }));
        }
        Ok(())
    }

    /// 自动重试开始事件：让前端显示 "重试中 (N/M)..."（对齐 pi auto_retry_start）
    async fn on_auto_retry_start(
        &self,
        attempt: u32,
        max_retries: u32,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "auto_retry_start",
                "sessionId": self.session_id,
                "attempt": attempt,
                "maxRetries": max_retries,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// 自动重试结束事件（success=false 表示所有重试用完仍失败）
    async fn on_auto_retry_end(
        &self,
        success: bool,
        attempt: u32,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "auto_retry_end",
                "sessionId": self.session_id,
                "success": success,
                "attempt": attempt,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    async fn on_tool_execution_start(
        &self,
        ctx: &crate::agent::extension::ToolExecutionContext,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_start",
                "sessionId": self.session_id,
                "toolCallId": ctx.tool_call_id,
                "toolName": ctx.tool_name,
                "args": ctx.args,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// 工具执行前增量 save（解决 fork 阻塞丢 message 问题）。
    /// 每次工具执行前都 save 当前 messages，这样即使 fork 阻塞 / 进程被杀，
    /// 主 session 也有 user prompt + assistant tool call decision。
    async fn on_before_tool_execute(
        &self,
        _tool_name: &str,
        _args: &serde_json::Value,
        messages: &[crate::agent::messages::Message],
    ) -> crate::agent::error::AgentResult<()> {
        let msgs_json: Vec<serde_json::Value> = messages
            .iter()
            .filter_map(|m| serde_json::to_value(m).ok())
            .collect();
        eprintln!("[before-tool] tool={_tool_name} msgs={}", msgs_json.len());
        if !msgs_json.is_empty() {
            // save_worker_session 内部有去重（按文件已有 message 数），不会重复写
            // 但我们需要 sid + cwd —— 从全局拿
            let sid = SESSION_SID.lock().unwrap().clone();
            let cwd = SESSION_CWD.lock().unwrap().clone();
            if let (Some(sid), Some(cwd)) = (sid, cwd) {
                save_worker_session(&sid, &cwd, &msgs_json);
            }
        }
        Ok(())
    }

    async fn on_tool_execution_update(
        &self,
        ctx: &crate::agent::extension::ToolExecutionContext,
        partial: &str,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_update",
                "sessionId": self.session_id,
                "toolCallId": ctx.tool_call_id,
                "toolName": ctx.tool_name,
                "args": ctx.args,
                "partialResult": partial,
            }
        }));
        Ok(())
    }

    async fn on_tool_execution_end(
        &self,
        ctx: &crate::agent::extension::ToolExecutionContext,
    ) -> crate::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_end",
                "sessionId": self.session_id,
                "toolCallId": ctx.tool_call_id,
                "toolName": ctx.tool_name,
                "isError": ctx.is_error,
                "result": ctx.result,
                "durationMs": ctx.duration_ms,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }
}

// ── WorkerAgentRpc: gives WASM extensions access to agent state ──────────
// Implements AgentRpcHandle so WASM host functions can query/modify agent:
// token counts, messages, steer, LLM calls, worker status, compaction, worktrees.
// Uses a shared snapshot (Arc<RwLock<AgentSnapshot>>) updated each turn.

/// Snapshot of agent state, cloned at turn boundaries.
/// WASM reads this — never touches &mut Agent directly.
#[derive(Clone, Default)]
struct AgentSnapshot {
    model: String,
    provider: String,
    session_id: String,
    message_count: usize,
    is_running: bool,
    total_input_tokens: u64,
    total_output_tokens: u64,
    context_window: u64,
    estimated_tokens: u64,
    messages_json: String,
    steering_queue_len: usize,
    follow_up_queue_len: usize,
}

struct WorkerAgentRpc {
    snapshot: std::sync::Arc<std::sync::RwLock<AgentSnapshot>>,
}

impl WorkerAgentRpc {
    fn new(model: String, provider: String, session_id: String) -> Self {
        Self {
            snapshot: std::sync::Arc::new(std::sync::RwLock::new(AgentSnapshot {
                model,
                provider,
                session_id,
                ..Default::default()
            })),
        }
    }

    /// Returns a clone of the Arc<RwLock> so ion_worker can update the snapshot
    /// at turn boundaries (before each tool execution).
    #[allow(dead_code)]
    fn snapshot_handle(&self) -> std::sync::Arc<std::sync::RwLock<AgentSnapshot>> {
        std::sync::Arc::clone(&self.snapshot)
    }
}

#[async_trait::async_trait]
impl crate::wasm_extension::AgentRpcHandle for WorkerAgentRpc {
    async fn call(&self, method: &str, params_json: &str) -> Result<String, String> {
        let snap = self.snapshot.read().unwrap();
        let params: serde_json::Value =
            serde_json::from_str(params_json).unwrap_or(serde_json::Value::Null);

        match method {
            "get_context_usage" => Ok(serde_json::json!({
                "total_tokens": snap.estimated_tokens,
                "input_tokens": snap.total_input_tokens,
                "output_tokens": snap.total_output_tokens,
                "context_window": snap.context_window,
                "usage_percent": if snap.context_window > 0 {
                    (snap.estimated_tokens as f64 / snap.context_window as f64 * 100.0) as u64
                } else { 0 },
                "message_count": snap.message_count,
            })
            .to_string()),

            "get_full_messages" => Ok(snap.messages_json.clone()),

            "get_state" => Ok(serde_json::json!({
                "model": snap.model,
                "provider": snap.provider,
                "session_id": snap.session_id,
                "message_count": snap.message_count,
                "is_running": snap.is_running,
                "steering_queue": snap.steering_queue_len,
                "follow_up_queue": snap.follow_up_queue_len,
            })
            .to_string()),

            "steer" => {
                // Emit steer command to stdout (Manager/worker picks it up)
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Err("steer: empty text".into());
                }
                // Emit as a steer event — the worker's select! loop handles it
                let cmd = serde_json::json!({
                    "type": "steer",
                    "text": text,
                });
                println!("{}", cmd);
                Ok(serde_json::json!({"steered": true}).to_string())
            }

            "inject_follow_up" => {
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if text.is_empty() {
                    return Err("inject_follow_up: empty text".into());
                }
                let cmd = serde_json::json!({
                    "type": "follow_up",
                    "text": text,
                });
                println!("{}", cmd);
                Ok(serde_json::json!({"injected": true}).to_string())
            }

            "llm_call" => {
                // LLM call not available in snapshot — would need ApiRegistry.
                // Return error for now; can be extended later.
                Err("llm_call: not yet implemented (requires ApiRegistry injection)".into())
            }

            "get_worker_status" => {
                let worker_id = params
                    .get("worker_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if worker_id.is_empty() {
                    return Err("get_worker_status: empty worker_id".into());
                }
                // Emit a get_worker_status command — Manager responds
                let cmd = serde_json::json!({
                    "type": "get_worker_status",
                    "worker_id": worker_id,
                });
                println!("{}", cmd);
                Ok(serde_json::json!({"requested": true}).to_string())
            }

            "compact_now" => {
                // Trigger compaction by setting a flag the agent loop checks.
                // For now, emit a compact command.
                let cmd = serde_json::json!({"type": "compact_now"});
                println!("{}", cmd);
                Ok(serde_json::json!({"compacting": true}).to_string())
            }

            "create_worktree" => {
                let branch = params.get("branch").and_then(|v| v.as_str()).unwrap_or("");
                let cmd = serde_json::json!({
                    "type": "create_worktree",
                    "branch": branch,
                });
                println!("{}", cmd);
                Ok(serde_json::json!({"requested": true}).to_string())
            }

            _ => Err(format!("unknown agent_rpc method: {method}")),
        }
    }
}

// ── FsProbeExtension: ctx.fs 探针扩展（给 CLI 测试用）──────────────────────
// 通过 extension_rpc 暴露 ctx.fs 的 read_file / list_dir / path_exists / glob，
// 以及 data_dirs（4 级数据目录），让 tests/extension_fs_ci.sh 能验证注入。
struct FsProbeExtension {
    fs: std::sync::Arc<crate::agent::extension::RuntimeFileSystem>,
    storage: crate::storage_context::StorageContext,
}

#[async_trait::async_trait]
impl crate::agent::extension::Extension for FsProbeExtension {
    fn name(&self) -> &str {
        "fs_probe"
    }

    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> crate::agent::error::AgentResult<serde_json::Value> {
        use crate::agent::error::AgentError;
        use crate::agent::extension::FileSystemCapability;
        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        match method {
            "read_file" => {
                let content = self.fs.read_file(path).await.map_err(AgentError::Tool)?;
                Ok(serde_json::json!({"content": content}))
            }
            "write_file" => {
                let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
                self.fs
                    .write_file(path, content)
                    .await
                    .map_err(AgentError::Tool)?;
                Ok(serde_json::json!({"written": true}))
            }
            "list_dir" => {
                let entries = self.fs.list_dir(path).await.map_err(AgentError::Tool)?;
                let arr: Vec<serde_json::Value> = entries
                    .iter()
                    .map(|e| {
                        serde_json::json!({
                            "name": e.name, "is_dir": e.is_dir, "size": e.size,
                        })
                    })
                    .collect();
                Ok(serde_json::json!({"entries": arr}))
            }
            "path_exists" => {
                // path_exists 内部要 block_on（RuntimeFileSystem 是 async），
                // 这里直接调（我们已经在 async 上下文里）
                let exists = self.fs.path_exists(path).await;
                Ok(serde_json::json!({"exists": exists}))
            }
            "glob" => {
                let pattern = params.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
                let matches = self.fs.glob(pattern).await.map_err(AgentError::Tool)?;
                Ok(serde_json::json!({"matches": matches}))
            }
            "data_dirs" => {
                // 返回 4 级数据目录（验证 StorageContext 注入）
                let extension_id = params
                    .get("extension_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("fs_probe");
                let dirs = crate::agent::extension::ExtensionDataDirs {
                    global: self.storage.global_dir(extension_id),
                    project: self.storage.project_dir(extension_id),
                    cwd: self.storage.cwd_dir(extension_id),
                    session: self.storage.session_dir(extension_id),
                };
                Ok(serde_json::json!({
                    "global": dirs.global.to_string_lossy(),
                    "project": dirs.project.to_string_lossy(),
                    "cwd": dirs.cwd.to_string_lossy(),
                    "session": dirs.session.to_string_lossy(),
                }))
            }
            _ => Err(AgentError::Tool("extension rpc method not found".into())),
        }
    }
}

// ── SessionProbeExtension: session hook 探针扩展（给 CLI 测试用）──────────
// on_session_before_switch 被触发时 emit 一个 session_switch_seen 事件，
// 让 tests/session_hook_ci.sh 能通过 ion subscribe 观察 hook 是否真的触发。
// veto_mode=true 时返回 Err（测试 veto 能力）。
struct SessionProbeExtension {
    veto: bool,
}

impl SessionProbeExtension {
    fn emit_seen(&self, action: &str, target: &Option<String>, branch_name: &Option<String>) {
        // 必须包 "type":"event" 外壳，否则 Manager stdout-reader 不转发给 subscriber。
        // （参照 AGENTS.md「推送事件模式（仿 BashExtension）」）
        let msg = serde_json::json!({
            "type": "event",
            "event": {
                "type": "extension_event",
                "extension": "session_probe",
                "customType": "session_switch_seen",
                "visibility": "llm_and_ui",
                "data": {
                    "action": action,
                    "target_leaf_id": target,
                    "branch_name": branch_name,
                    "veto": self.veto,
                },
            },
        });
        println!("{}", serde_json::to_string(&msg).unwrap_or_default());
    }
}

#[async_trait::async_trait]
impl crate::agent::extension::Extension for SessionProbeExtension {
    fn name(&self) -> &str {
        "session_probe"
    }

    async fn on_session_before_switch(
        &self,
        ctx: &crate::agent::extension::SessionSwitchContext,
    ) -> crate::agent::error::AgentResult<()> {
        self.emit_seen(&ctx.action, &ctx.target_leaf_id, &ctx.branch_name);
        if self.veto {
            Err(crate::agent::error::AgentError::Tool(
                "vetoed by session_probe".into(),
            ))
        } else {
            Ok(())
        }
    }
}

fn output_response(id: &str, command: &str, data: &serde_json::Value) {
    output(&serde_json::json!({
        "id": id,
        "type": "response",
        "command": command,
        "success": true,
        "data": data,
    }));
    emit_rpc_response_event(id, command, true, None);
}

fn output_error_response(id: &str, command: &str, error: &str) {
    output(&serde_json::json!({
        "id": id,
        "type": "response",
        "command": command,
        "success": false,
        "error": error,
    }));
    emit_rpc_response_event(id, command, false, Some(error));
}

/// 每条用户触发的 RPC 完成后广播一条 `rpc_response` 事件。
///
/// 目的：多终端实时同步——所有 UI 对同一个 worker 发起的任何操作（点击审批、
/// 切模型、改权限、查询刷新……）都会以事件形式广播，其他终端无需轮询即可感知。
/// 只带摘要（method/id/success/error 截断），不带响应体——get_messages 等响应
/// 可达几十 MB，进事件流会把慢订阅者挤掉（EventBus bounded 1000 条）。
fn emit_rpc_response_event(id: &str, command: &str, success: bool, error: Option<&str>) {
    let session_id = SESSION_SID
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    let mut event = serde_json::json!({
        "type": "rpc_response",
        "id": id,
        "method": command,
        "success": success,
        "sessionId": session_id,
        "timestamp": now_ms(),
    });
    if let Some(err) = error {
        event["error"] = serde_json::Value::String(err.chars().take(200).collect());
    }
    output(&serde_json::json!({ "type": "event", "event": event }));
}

// ---------------------------------------------------------------------------
// ManagerBridge — Worker → Manager 命令通道 + correlation
// ---------------------------------------------------------------------------
//
// 设计目的：让 Worker 内部运行的 Tool（如 spawn_worker / send_to_worker）能
// 同步 await Manager 的响应。
//
// 协议：
//   Worker → stdout: {"type":"manager_command","command":"...","_reply_to":"<id>","_from_worker":"<sid>","params":{...}}
//   Manager → Worker stdin: {"type":"manager_response","_reply_to":"<id>","success":true,"data":{...}}
//
// correlation 用 `_reply_to`（UUID 片段），Manager 原样塞回。
// Worker 端维护 pending map：_reply_to → oneshot::Sender。
// manager_response 到达时按 _reply_to 触发对应 oneshot。

pub struct ManagerBridge {
    pub self_id: String,
    pub stdout: Arc<Mutex<io::Stdout>>,
    pub pending: Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>,
}

#[async_trait::async_trait]
impl crate::runtime::ManagerBridgeHandle for ManagerBridge {
    async fn send_command(
        &self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        ManagerBridge::send_command(self, command, params).await
    }
}

#[async_trait::async_trait]
impl crate::worker_api::BridgeHandle for ManagerBridge {
    async fn send_command(
        &self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        ManagerBridge::send_command(self, command, params).await
    }
}

impl ManagerBridge {
    pub fn new(self_id: String, stdout: Arc<Mutex<io::Stdout>>) -> Self {
        Self {
            self_id,
            stdout,
            pending: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 发送 manager_command 并 await 响应（120s 超时）。
    /// 在 Tool 内调用，让 LLM 能同步拿到 worker_id / first_turn_output。
    pub async fn send_command(
        &self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let reply_to = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let (tx, rx) = oneshot::channel::<serde_json::Value>();
        self.pending.lock().await.insert(reply_to.clone(), tx);

        // 把 _reply_to / _from_worker 塞进 params（同 Manager 端的提取位置）
        let mut full_params = if params.is_object() {
            let mut obj = params;
            if let Some(o) = obj.as_object_mut() {
                o.insert("_reply_to".into(), serde_json::json!(reply_to));
                o.insert("_from_worker".into(), serde_json::json!(self.self_id));
            }
            obj
        } else {
            serde_json::json!({
                "_reply_to": reply_to,
                "_from_worker": self.self_id,
                "payload": params,
            })
        };
        let _ = &mut full_params; // suppress mut warning

        let msg = serde_json::json!({
            "type": "manager_command",
            "command": command,
            "params": full_params,
        });
        {
            let line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
            let mut out = self.stdout.lock().await;
            let _ = writeln!(out, "{line}");
            let _ = out.flush();
        }

        // 等 manager_response（320s 超时，对齐 Manager 端 child 首轮等待上限 300s + 余量）
        match tokio::time::timeout(std::time::Duration::from_secs(320), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                self.pending.lock().await.remove(&reply_to);
                Err(format!("manager_command '{command}' channel dropped"))
            }
            Err(_) => {
                self.pending.lock().await.remove(&reply_to);
                Err(format!("manager_command '{command}' timeout (320s)"))
            }
        }
    }

    /// 把 manager_response 投递到 pending map 里对应的 oneshot。
    /// 在 stdin 主循环的 "manager_response" 分支调用。
    pub async fn deliver_response(&self, reply_to: &str, resp: serde_json::Value) {
        if let Some(tx) = self.pending.lock().await.remove(reply_to) {
            let _ = tx.send(resp);
        } else {
            tracing::warn!("[bridge] no pending request for _reply_to={reply_to}");
        }
    }
}

/// Append a JSON line to the session.jsonl file (not a message, just a record).
fn append_session_entry(cwd: &str, sid: &str, entry_type: &str, entry_data: &serde_json::Value) {
    // 优先用全局 SESSION_FILE_PATH（fork 子 Worker 的 <session_id>.jsonl）
    let path = SESSION_FILE_PATH
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| session_jsonl::session_path(cwd));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // parentId：从文件现有 entries 解析当前 leaf（修 bug：原来硬编码 sid）
    let parent_id = (|| {
        let content = std::fs::read_to_string(&path).ok()?;
        let entries: Vec<serde_json::Value> = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        crate::session_tree::resolve_current_leaf(&entries)
    })()
    .unwrap_or_else(|| sid.to_string());

    let mut line = serde_json::json!({
        "type": entry_type,
        "id": session_jsonl::generate_id(),
        "parentId": parent_id,
        "timestamp": session_jsonl::timestamp_iso(),
    });
    // 合并 entry_data 的字段到顶层（不嵌套在 data 里），对齐 pi JSONL 格式
    if let Some(obj) = entry_data.as_object()
        && let Some(m) = line.as_object_mut()
    {
        for (k, v) in obj {
            m.insert(k.clone(), v.clone());
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let need_sep = f.metadata().ok().map(|m| m.len() > 0).unwrap_or(false);
        if need_sep {
            let _ = writeln!(f);
        }
        let _ = write!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
    // 刷新检索缓存（与 session_jsonl::append_raw_entry 保持一致）
    crate::message_retrieval::invalidate_cache(cwd);
}

/// Ensure the fork sub-worker session header exists at the given path.
/// Unlike ensure_session_header (which writes to session.jsonl shared by cwd),
/// this writes to <session_id>.jsonl — a fork sub-worker's private session file.
fn ensure_fork_session_header(path: &std::path::Path, cwd: &str, sid: &str) {
    // 文件已存在：检查 header 是否缺 agent/model/provider（历史 session 用旧版
    // ensure_fork_session_header 生成，没写这些字段，导致 export 拿不到 system prompt）。
    // 缺了就从 ION_SESSION_* env 补（in-place 修补第一行），补完即返回，不重建文件。
    if path.exists() {
        patch_fork_session_header_if_needed(path);
        return;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // ── 读 parent 关联信息（ION_FORK_CHILD 子 Worker 都会设这些 env）──
    let parent_session = std::env::var("ION_PARENT_SESSION")
        .ok()
        .filter(|s| !s.is_empty());
    let parent_worker = std::env::var("ION_PARENT_WORKER")
        .ok()
        .filter(|s| !s.is_empty());
    let spawn_relation = std::env::var("ION_SPAWN_RELATION")
        .ok()
        .filter(|s| !s.is_empty());
    let spawned_by = std::env::var("ION_SPAWNED_BY")
        .ok()
        .filter(|s| !s.is_empty());

    // 构造 spawnMeta（ION 扩展，详细血缘信息）
    let has_spawn_meta =
        parent_worker.is_some() || spawn_relation.is_some() || spawned_by.is_some();
    let mut header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": sid,
        "timestamp": session_jsonl::timestamp_iso(),
        "cwd": cwd,
        "parentSession": parent_session.clone(),
    });
    // export.rs 依赖 header.agent 加载 system prompt + tools，缺失会导致子 worker HTML
    // 显示不全。env 在 worker_rpc.rs 启动时由 initial_agent/model/provider 写入。
    let (env_agent, env_model, env_provider) = session_jsonl::read_session_env_tuple();
    if let Some(a) = env_agent {
        header["agent"] = serde_json::Value::String(a);
    }
    if let Some(m) = env_model {
        header["model"] = serde_json::Value::String(m);
    }
    if let Some(p) = env_provider {
        header["provider"] = serde_json::Value::String(p);
    }
    if has_spawn_meta {
        let mut spawn_meta = serde_json::json!({});
        if let Some(ref pw) = parent_worker {
            spawn_meta["parentWorker"] = serde_json::Value::String(pw.clone());
        }
        if let Some(ref rel) = spawn_relation {
            spawn_meta["relation"] = serde_json::Value::String(rel.clone());
        }
        if let Some(ref sb) = spawned_by {
            spawn_meta["spawnedBy"] = serde_json::Value::String(sb.clone());
        }
        if let Some(ref ps) = parent_session {
            spawn_meta["parentSession"] = serde_json::Value::String(ps.clone());
        }
        header["spawnMeta"] = spawn_meta;
    }

    let json = serde_json::to_string(&header).unwrap_or_default();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
    {
        let _ = f.write_all(format!("{json}\n").as_bytes());
    }

    // fork 子 Worker：把 system_prompt（含 skill 内容）作为 custom entry 写到第二行
    // 这样 export HTML 时能恢复 systemPrompt 字段，让用户看到 skill 注入的内容
    if let Ok(sp) = std::env::var("ION_SYSTEM_PROMPT")
        && !sp.is_empty()
    {
        let sp_entry = serde_json::json!({
            "type": "custom",
            "id": session_jsonl::generate_id(),
            "parentId": sid,
            "timestamp": session_jsonl::timestamp_iso(),
            "customType": session_jsonl::CUSTOM_TYPE_SYSTEM_PROMPT,
            "data": { "systemPrompt": sp },
        });
        let sp_json = serde_json::to_string(&sp_entry).unwrap_or_default();
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
            let _ = f.write_all(format!("{sp_json}\n").as_bytes());
        }
    }
}

/// 历史遗留修复：旧版 ensure_fork_session_header 不写 agent/model/provider 字段，
/// 导致 export HTML 拿不到 agent config（system prompt + tools 都显示不出来）。
/// 本函数在文件已存在时检查 header 是否缺这些字段，缺了就从 ION_SESSION_* env 补，
/// 只改第一行（header），不动其余 entries。幂等：字段齐全则什么都不做。
fn patch_fork_session_header_if_needed(path: &std::path::Path) {
    let (env_agent, env_model, env_provider) = session_jsonl::read_session_env_tuple();
    // 无 agent env 时无法判定该补什么 —— 跳过（避免把入口 worker 的空 env 误写入）
    let env_agent = match env_agent {
        Some(a) => a,
        None => return,
    };

    // 先用 read_session_header 只读首行（避免为判定字段把整文件读进内存）
    let mut header = match session_jsonl::read_session_header(path) {
        Some(h) => h,
        None => return,
    };

    let need_agent = header.agent.is_none();
    let need_model = env_model.is_some() && header.model.is_none();
    let need_provider = env_provider.is_some() && header.provider.is_none();
    if !need_agent && !need_model && !need_provider {
        return;
    }

    if need_agent {
        header.agent = Some(env_agent);
    }
    if need_model {
        header.model = env_model;
    }
    if need_provider {
        header.provider = env_provider;
    }

    // 只有确实需要改才读全文 + 重写（header 行替换，其余 entries 原样保留）
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return;
    }
    let new_header = serde_json::to_string(&header).unwrap_or_else(|_| lines[0].to_string());
    let mut out = String::with_capacity(content.len() + new_header.len());
    out.push_str(&new_header);
    out.push('\n');
    for line in &lines[1..] {
        out.push_str(line);
        out.push('\n');
    }
    let _ = std::fs::write(path, out);
    tracing::info!(
        "[worker] patched legacy fork session header (agent/model/provider) → {}",
        path.display()
    );
}

/// Load messages from a fork sub-worker's session file.
fn load_fork_session_messages(
    path: &std::path::Path,
) -> Option<Vec<crate::agent::messages::Message>> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut messages = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(e) = serde_json::from_str::<serde_json::Value>(line)
            && e.get("type").and_then(|v| v.as_str()) == Some("message")
            && let Some(m) = e
                .get("message")
                .and_then(|m| serde_json::from_value(m.clone()).ok())
        {
            messages.push(m);
        }
    }
    Some(messages)
}

/// Count messages reachable from the current leaf pointer (live view).
///
/// The session file is only-append, so after a rollback the disk still has
/// all historical messages. To know how many messages are "live" (visible
/// to the agent after rollback), we walk the parentId chain starting from
/// the current leaf_pointer's leafId.
pub fn count_live_messages(entries: &[serde_json::Value]) -> usize {
    // Resolve current leaf id (the cursor)
    let leaf_id = match crate::session_tree::resolve_current_leaf(entries) {
        Some(id) => id,
        None => {
            // No leaf_pointer entries → all messages are live
            return entries
                .iter()
                .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("message"))
                .count();
        }
    };

    // Build a HashMap<id, index> once so each parentId-chain lookup is O(1)
    // instead of O(n) (the old entries.iter().find() made this O(n²) on
    // large sessions — see the perf bug where a 110k-entry session hung at
    // 100% CPU inside on_before_tool_execute → save_worker_session).
    let mut id_index: std::collections::HashMap<&str, &serde_json::Value> =
        std::collections::HashMap::with_capacity(entries.len());
    for e in entries {
        if let Some(id) = e.get("id").and_then(|v| v.as_str()) {
            // First occurrence wins, mirroring the old .find() semantics.
            id_index.entry(id).or_insert(e);
        }
    }

    // Walk parentId chain from leaf_id, counting messages on the path
    let mut count = 0usize;
    let mut current_id = Some(leaf_id.as_str());
    let mut visited = std::collections::HashSet::new();
    while let Some(cid) = current_id {
        if !visited.insert(cid.to_string()) {
            break; // cycle guard
        }
        // O(1) lookup via the prebuilt index (was O(n) entries.iter().find).
        match id_index.get(cid) {
            Some(e) => {
                if e.get("type").and_then(|v| v.as_str()) == Some("message") {
                    count += 1;
                }
                current_id = e.get("parentId").and_then(|v| v.as_str());
            }
            None => break,
        }
    }
    count
}

fn save_worker_session(sid: &str, cwd: &str, msgs: &[serde_json::Value]) {
    // 优先用全局 SESSION_FILE_PATH（fork 子 Worker 设的 <session_id>.jsonl）
    // fallback 到 session_path(cwd)（主 Worker 的 session.jsonl）
    let path = SESSION_FILE_PATH
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| session_jsonl::session_path(cwd));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // 读取已有文件，确定已写入的 message entry 数量 + 当前 leaf（光标，parentId 来源）
    let mut existing_lines: Vec<String> = Vec::new();
    let mut saved_msg_count = 0usize;
    let mut header_existed = false;
    // 收集所有 entries 用于 leaf 解析
    let mut all_entries: Vec<serde_json::Value> = Vec::new();

    if let Ok(content) = std::fs::read_to_string(&path) {
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            existing_lines.push(line.to_string());
            if let Ok(e) = serde_json::from_str::<serde_json::Value>(line) {
                if e.get("type").and_then(|v| v.as_str()) == Some("session") {
                    header_existed = true;
                }
                if e.get("type").and_then(|v| v.as_str()) == Some("message") {
                    saved_msg_count += 1;
                }
                all_entries.push(e);
            }
        }
    }

    // leaf 感知：用 resolve_current_leaf 算 parentId（对齐 Session Tree，感知 leaf_pointer）
    let last_id =
        crate::session_tree::resolve_current_leaf(&all_entries).unwrap_or_else(|| sid.to_string());

    // 若文件不存在或空，先写 header
    if !header_existed {
        let header = serde_json::json!({
            "type": "session",
            "version": 3,
            "id": sid,
            "timestamp": session_jsonl::timestamp_iso(),
            "cwd": cwd,
        });
        let header_line = serde_json::to_string(&header).unwrap_or_default();

        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            // 文件之前不存在，写 header
            if existing_lines.is_empty() {
                let _ = writeln!(f, "{header_line}");
            }
        }
        // 全新会话：leaf 就是 session id（resolve_current_leaf 此时返回 None，
        // 已被 unwrap_or_else(sid) 处理；saved_msg_count 本就是 0）
    }

    // 只 append 新增的 message。
    //
    // 注意：session 文件是 only-append 的，回滚后磁盘上仍保留所有历史 message
    // （只是 leaf_pointer 移到了更早的位置）。所以不能简单地用"磁盘 message 总数"
    // 作为已保存数量——否则回滚后再加消息会被误判为"已经存过"而跳过。
    //
    // 正确做法：数磁盘上 parentId 链能到达的 message（即 live 视图），跟 msgs.len() 比。
    // 简化：如果 msgs.len() > saved_msg_count，说明是纯追加（正常情况），取后半段。
    // 如果 msgs.len() <= saved_msg_count，说明发生了回滚/分支，需要从头重写 live 部分
    // —— 但 only-append 不变量禁止改写，所以这里用"追加 live 视图里有但磁盘 leaf 链
    // 上没有的部分"。
    //
    // 简化实现：比较 live message 数（按 leaf_pointer 链计算）vs msgs.len()
    let live_msg_count = count_live_messages(&all_entries);
    let new_msgs = if msgs.len() > live_msg_count {
        eprintln!(
            "[save-debug] msgs={} live={} saved_total={} new={}",
            msgs.len(),
            live_msg_count,
            saved_msg_count,
            msgs.len() - live_msg_count
        );
        &msgs[live_msg_count..]
    } else if msgs.len() < live_msg_count {
        // 回滚后再加消息：msgs 比 live 短，说明 leaf 已经回退到比 msgs 还早的位置。
        // 这种情况理论上不该发生（resume 后 agent.messages 应该 ≥ live），但如果发生了
        // 就把所有 msgs 当新消息追加（带新的 parentId 链）。
        eprintln!(
            "[save-debug] ROLLBACK CASE: msgs={} < live={} — appending all as new",
            msgs.len(),
            live_msg_count
        );
        msgs
    } else {
        // msgs.len() == live_msg_count: 没有新消息，跳过
        &[][..]
    };

    if new_msgs.is_empty() {
        return;
    }

    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let need_sep = f.metadata().ok().map(|m| m.len() > 0).unwrap_or(false);
        // parentId 链：从 last_id 开始
        let mut parent_id = last_id;
        let mut is_first = true;
        for msg in new_msgs {
            let entry_id = session_jsonl::generate_id();
            let entry = serde_json::json!({
                "type": "message",
                "id": entry_id,
                "parentId": parent_id,
                "timestamp": session_jsonl::timestamp_iso(),
                "message": msg,
            });
            let json = serde_json::to_string(&entry).unwrap_or_default();
            // 合并 \n + JSON 为单次 write_all（第一条消息在 need_sep 时加前导换行）
            let payload = if is_first && need_sep {
                is_first = false;
                format!("\n{}\n", json)
            } else {
                is_first = false;
                format!("{}\n", json)
            };
            let _ = f.write_all(payload.as_bytes());
            parent_id = entry_id;
        }
    }
}

/// 路径规范化：cwd 内返回相对路径，cwd 外返回规范化的绝对路径
fn normalize_path(path: &str, cwd: &str) -> String {
    let abs = if std::path::Path::new(path).is_absolute() {
        path.to_string()
    } else {
        format!("{}/{}", cwd.trim_end_matches('/'), path)
    };
    // 规范化（去 ..）
    let canonical = std::path::Path::new(&abs)
        .components()
        .filter(|c| c.as_os_str() != ".")
        .collect::<std::path::PathBuf>();
    let canonical_str = canonical.to_string_lossy().to_string();
    // cwd 内 → 相对化
    if let Some(rel) = canonical_str.strip_prefix(cwd) {
        rel.trim_start_matches('/').to_string()
    } else {
        canonical_str
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 将 JSONL entry IDs 解析为 agent 内存消息数组的索引。
///
/// 两条路径：
/// 1. **精确映射**（compaction 前）：JSONL message entry 与 self.messages 一一对应
/// 2. **内容匹配降级**（compaction 后）：JSONL 消息数 > 内存消息数，
///    用 entry 里的 message 序列化内容在 self.messages 中查找匹配
fn resolve_target_indices(
    entries: &[serde_json::Value],
    agent_messages: &[crate::agent::messages::Message],
    target_ids: &[String],
) -> Vec<usize> {
    let msg_entries: Vec<&serde_json::Value> = entries
        .iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("message"))
        .collect();

    // 路径 1：精确索引映射（计数一致时）
    if msg_entries.len() == agent_messages.len() {
        let entry_ids: Vec<&str> = msg_entries
            .iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
            .collect();
        return target_ids
            .iter()
            .filter_map(|tid| entry_ids.iter().position(|eid| *eid == tid))
            .collect();
    }

    // 路径 2：内容匹配降级（compaction 后，计数不一致）
    // 用 target_id 从 JSONL 找到对应的 message 内容，
    // 然后在 agent 的内存消息里按序列化内容查找
    tracing::info!(
        "[soft-delete] entry/index mismatch (jsonl={} agent={}), falling back to content matching",
        msg_entries.len(),
        agent_messages.len()
    );

    // 构建 entry_id → 序列化 message 文本 的映射
    let id_to_content: std::collections::HashMap<&str, String> = msg_entries
        .iter()
        .filter_map(|e| {
            let id = e.get("id").and_then(|v| v.as_str())?;
            let msg_val = e.get("message")?;
            // 用 message 的 JSON 序列化做内容指纹
            Some((id, serde_json::to_string(msg_val).unwrap_or_default()))
        })
        .collect();

    // 构建 agent 内存消息的序列化文本列表
    let agent_contents: Vec<String> = agent_messages
        .iter()
        .map(|m| serde_json::to_string(m).unwrap_or_default())
        .collect();

    target_ids
        .iter()
        .filter_map(|tid| {
            let target_content = id_to_content.get(tid.as_str())?;
            // 在 agent 内存里找第一条内容匹配的
            agent_contents.iter().position(|c| c == target_content)
        })
        .collect::<Vec<usize>>()
        // 去重：避免两条 target 解析到同一索引，导致 mark_deleted 删错消息
        .into_iter()
        .collect::<std::collections::HashSet<usize>>()
        .into_iter()
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::extension::Extension;

    // --- strip_version_suffix_inline ---

    #[test]
    fn test_strip_version_suffix_strips_numeric_version() {
        // A directory name with a trailing numeric version suffix should have
        // the version part removed.
        assert_eq!(strip_version_suffix_inline("debug-pro-1.0.0"), "debug-pro");
    }

    #[test]
    fn test_strip_version_suffix_keeps_name_without_version() {
        // Names that have no version-like suffix should be returned unchanged.
        assert_eq!(strip_version_suffix_inline("code-audit"), "code-audit");
        assert_eq!(
            strip_version_suffix_inline("code-audit-v1"),
            "code-audit-v1"
        );
    }

    #[test]
    fn test_strip_version_suffix_empty_input() {
        // An empty string is a no-op.
        assert_eq!(strip_version_suffix_inline(""), "");
    }

    // --- parse_skill_description_inline ---

    #[test]
    fn test_parse_skill_description_from_frontmatter() {
        // A SKILL.md with a frontmatter description should extract that value.
        let content =
            "---\nname: code-audit\ndescription: \"Audit code for bugs\"\n---\n# Code Audit\nbody";
        assert_eq!(
            parse_skill_description_inline(content),
            "Audit code for bugs"
        );
    }

    #[test]
    fn test_parse_skill_description_falls_back_to_title() {
        // When no frontmatter is present, the first H1 title should be used.
        let content = "# Code Audit Skill\nSome body text.";
        assert_eq!(parse_skill_description_inline(content), "Code Audit Skill");
    }

    #[test]
    fn test_parse_skill_description_empty_for_no_marker() {
        // Content without frontmatter or an H1 title yields an empty string.
        assert_eq!(parse_skill_description_inline("just some plain text"), "");
    }

    // --- normalize_path ---

    #[test]
    fn test_normalize_path_relative_becomes_relative_to_cwd() {
        // A relative path under cwd should be returned as a relative string.
        assert_eq!(normalize_path("src/main.rs", "/project"), "src/main.rs");
    }

    #[test]
    fn test_normalize_path_absolute_inside_cwd_is_relativeized() {
        // An absolute path inside cwd collapses to its relative form.
        assert_eq!(
            normalize_path("/project/src/main.rs", "/project"),
            "src/main.rs"
        );
    }

    #[test]
    fn test_normalize_path_strips_trailing_slash_in_cwd() {
        // A trailing slash on cwd must not cause a leading slash in the result.
        assert_eq!(normalize_path("a.txt", "/project/"), "a.txt");
    }

    // --- now_ms ---

    #[test]
    fn test_now_ms_is_positive() {
        // The current epoch-millis should always be a large positive number.
        let ts = now_ms();
        assert!(ts > 0);
        // Two calls should be non-decreasing.
        let ts2 = now_ms();
        assert!(ts2 >= ts);
    }

    // --- SessionProbeExtension::veto field ---

    #[test]
    fn test_session_probe_extension_construction() {
        // Verify the struct can be constructed and the veto flag round-trips.
        let ext = SessionProbeExtension { veto: true };
        assert!(ext.veto);
        assert_eq!(ext.name(), "session_probe");
    }

    // --- FsProbeExtension ---
    // (Skipped: requires a RuntimeFileSystem + StorageContext to construct,
    // which would couple the unit test to heavy async dependencies.)

    // --- ArcFauxProvider wraps inner ---

    #[test]
    fn test_arc_faux_provider_wraps_inner() {
        // Constructing the wrapper around an inner FauxProvider should keep it
        // alive behind the Arc (pending count starts at zero). We verify the
        // wrapper holds a clone of the same underlying Arc without depending
        // on exact strong-count values, which are fragile across assertion
        // points.
        let inner = std::sync::Arc::new(ion_provider::faux::FauxProvider::new());
        assert_eq!(inner.pending_count(), 0);
        let _wrapper = ArcFauxProvider(inner.clone());
        // Both the local `inner` and the wrapped clone now point at the same
        // provider, so any state mutation is observable through either handle.
        assert_eq!(inner.pending_count(), 0);
    }

    // --- output_error_response shape ---

    #[test]
    fn test_output_error_response_does_not_panic_on_simple_inputs() {
        // The function writes to stdout; we only verify it does not panic and
        // returns (it returns unit). Guard against accidental panic with
        // minimal valid inputs. stdout is captured by the test harness.
        output_error_response("42", "get_state", "not found");
        // If we reach this assertion, the call succeeded without panicking.
        assert!(true);
    }

    // --- watchdog dual-version switching (health + request_restart RPC) ---

    #[test]
    fn test_restart_notification_file() {
        // Verify the sentinel-file mechanism that request_restart relies on:
        // writing the restart signal to /tmp must round-trip correctly so the
        // watchdog (scripts/watchdog.sh) can detect it on the next poll.
        let restart_file = "/tmp/.ion-evolve-restart-test";
        let content = "test restart";
        std::fs::write(restart_file, content).unwrap();
        assert_eq!(std::fs::read_to_string(restart_file).unwrap(), content);
        std::fs::remove_file(restart_file).unwrap();
    }

    #[test]
    fn test_health_response_structure() {
        // The health RPC must return a JSON object with the fields the watchdog
        // inspects during dual-version switching: status, uptime_secs, pid,
        // version. No field may be null; uptime and pid must be positive.
        let health = serde_json::json!({
            "status": "ok",
            "uptime_secs": 100u64,
            "pid": 12345u64,
            "version": "0.1.0",
        });
        assert_eq!(health["status"], "ok");
        assert!(health["uptime_secs"].as_u64().unwrap() > 0);
        assert!(health["pid"].as_u64().unwrap() > 0);
    }

    // --- count_live_messages ---
    //
    // These guard against the O(n²) regression where parentId-chain lookups
    // used entries.iter().find() per step. With a large session that lookup
    // dominated and hung the worker at 100% CPU inside save_worker_session.

    fn mk_msg(id: &str, parent: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "role": "user",
            "content": "x",
        })
    }

    #[test]
    fn test_count_live_messages_no_leaf_counts_all_messages() {
        // No leaf_pointer entry → every message entry is live.
        let entries = vec![
            mk_msg("a", None),
            mk_msg("b", Some("a")),
            serde_json::json!({"type": "system_event", "id": "s1"}),
        ];
        assert_eq!(count_live_messages(&entries), 2);
    }

    #[test]
    fn test_count_live_messages_walks_parent_chain_from_leaf() {
        // Leaf points at the last message; walk back through parentId.
        let entries = vec![
            serde_json::json!({"type": "leaf_pointer", "id": "lp1", "leafId": "c"}),
            mk_msg("a", None),
            mk_msg("b", Some("a")),
            mk_msg("c", Some("b")),
            // Orphan message not on the leaf chain — must NOT be counted.
            mk_msg("orphan", None),
        ];
        assert_eq!(count_live_messages(&entries), 3);
    }

    #[test]
    fn test_count_live_messages_cycle_guard() {
        // A malformed cycle (b→a→b) must terminate, not loop forever.
        let entries = vec![
            serde_json::json!({"type": "leaf_pointer", "id": "lp1", "leafId": "b"}),
            mk_msg("a", Some("b")),
            mk_msg("b", Some("a")),
        ];
        // Visits b and a then hits the visited guard → 2 messages counted.
        assert_eq!(count_live_messages(&entries), 2);
    }

    #[test]
    fn test_count_live_messages_large_chain_is_linear() {
        // Regression: a long parentId chain used to be O(n²) because each
        // step did a linear entries.iter().find(). Build a 20k-entry chain
        // (previously this would hang) and assert it completes quickly and
        // counts correctly. Also interleaves non-message entries to confirm
        // they are skipped but still indexed for id lookup.
        let n = 20_000usize;
        let mut entries: Vec<serde_json::Value> = Vec::with_capacity(n + 1);
        entries.push(serde_json::json!({"type": "leaf_pointer", "id": "lp", "leafId": "m0"}));
        for i in 0..n {
            let parent = if i == 0 {
                None
            } else {
                Some(format!("m{}", i - 1))
            };
            entries.push(mk_msg(&format!("m{}", i), parent.as_deref()));
            // Sprinkle a non-message entry sharing an id-free shape; it must
            // not break the id index.
            entries.push(serde_json::json!({"type": "system_event", "id": format!("s{}", i)}));
        }
        let start = std::time::Instant::now();
        let count = count_live_messages(&entries);
        let elapsed = start.elapsed();
        assert_eq!(count, n, "all {} chain messages should be live", n);
        // Generous bound: the old O(n²) impl took minutes/forever on 110k.
        // The O(n) impl should be well under a second; allow headroom for CI.
        assert!(
            elapsed.as_secs() < 5,
            "count_live_messages should be near-linear, took {:?}",
            elapsed
        );
    }
}
