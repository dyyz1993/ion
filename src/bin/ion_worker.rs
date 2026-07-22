//! ion worker --mode rpc
//!
//! JSONL RPC 协议，完全对齐 pi 的 rpc-mode.ts。
//!
//! 三种命令模式:
//! 1. 同步查询: get_state → 读属性 → 返回
//! 2. 异步操作: set_model → await → 返回
//! 3. 流式:     prompt → 触发(不 await) → 事件推送

use std::io::{self, Write};
use std::sync::Arc;
use std::collections::HashMap;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, oneshot};
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::compact::CompactConfig;
use ion::agent::tool::{ReadTool, WriteTool, EditTool, BashTool, GrepTool, FindTool, LsTool, CalculatorTool, EchoTool, GitStatusTool, GitDiffTool, GitLogTool, GitAddTool, GitCommitTool, GitBranchTool, SpawnWorkerTool, SendToWorkerTool, ResumeWorkerTool, AwaitWorkerTool, ChannelSendTool, KillWorkerTool, BranchSessionTool, GlobalMemorySearchTool, GlobalMemorySaveTool, SkillTool, ToolRegistry};
use ion::wasm_extension::{Registry, ToolAdapter};
use ion::session_jsonl;

/// 全局：当前 Worker 的 session 文件路径。
/// 主 Worker = session.jsonl；fork 子 Worker = <session_id>.jsonl（独立文件）。
/// save_worker_session 读这个路径决定往哪写。
static SESSION_FILE_PATH: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

/// 全局：当前 Worker 的 session_id + cwd。
/// on_before_tool_execute 钩子用（它拿不到 sid/cwd，只能从全局读）。
static SESSION_SID: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static SESSION_CWD: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
use ion_provider::registry::{ApiRegistry, ProviderFactory};
use ion_provider::types::*;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[tokio::main]
async fn main() {
    // CRITICAL: tracing MUST go to stderr, stdout is reserved for JSONL
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_target(false)
        .try_init().ok();

    let args: Vec<String> = std::env::args().collect();
    let mut session_id: Option<String> = None;
    let mut model_id = "deepseek-v4-flash".to_string();
    let mut provider = "opencode".to_string();
    let mut channels: Vec<String> = Vec::new();
    let mut initial_agent: Option<String> = None;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => { session_id = args.get(i + 1).cloned(); i += 2; continue; }
            "--model" => { model_id = args.get(i + 1).cloned().unwrap_or(model_id); i += 2; continue; }
            "--provider" => { provider = args.get(i + 1).cloned().unwrap_or(provider); i += 2; continue; }
            "--channel" => { if let Some(ch) = args.get(i + 1) { channels.push(ch.clone()); } i += 2; continue; }
            "--agent" => { initial_agent = args.get(i + 1).cloned(); i += 2; continue; }
            "--mode" => { i += 2; continue; } // 已知是 rpc
            _ => { i += 1; }
        }
    }

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
        // 从 auth.json 读 base_url 和 api_key
        let auth_url = ion::auth::AuthStorage::load().provider_base_urls.get(&provider).cloned();
        Model {
            id: model_id.clone(), name: model_id.clone(),
            api: "openai-completions".into(), provider: provider.clone(),
            base_url: auth_url.clone().unwrap_or_else(|| "https://opencode.ai/zen/go/v1".into()),
            reasoning: false, input: vec!["text".into()],
            cost: Cost { input: 0.0, output: 0.0, cache_read: 0.0, cache_write: 0.0 },
            context_window: 128000, max_tokens: 8192, compat: None, headers: None,
        }
    });
    // 即使是 builtin model，如果 auth.json 里有该 provider 的代理 base_url，覆盖之。
    // （builtin GLM model 的 base_url 是直连 open.bigmodel.cn，但用户可能用代理。）
    if let Some(override_url) = ion::auth::AuthStorage::load().provider_base_urls.get(&provider) {
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
                        let inner: Option<Box<dyn ion_provider::registry::ApiProvider>> = if using_faux {
                            let new_faux = std::sync::Arc::new(ion_provider::faux::FauxProvider::new());
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
    tools.register(Box::new(BranchSessionTool));
    tools.register(Box::new(GlobalMemorySearchTool));
    tools.register(Box::new(GlobalMemorySaveTool));
    tools.register(Box::new(GitStatusTool));
    tools.register(Box::new(GitDiffTool));
    tools.register(Box::new(GitLogTool));
    tools.register(Box::new(GitAddTool));
    tools.register(Box::new(GitCommitTool));
    tools.register(Box::new(GitBranchTool));
    // ── Worker 编排工具（仅 WorkerRuntime 支持真实实现）──
    // 让 LLM 自主调用 spawn_worker 创建子/同级 Worker，send_to_worker 跨 Worker 对话。
    tools.register(Box::new(SpawnWorkerTool));
    tools.register(Box::new(SendToWorkerTool));
    tools.register(Box::new(ResumeWorkerTool));
    tools.register(Box::new(AwaitWorkerTool));
    tools.register(Box::new(ChannelSendTool));
    tools.register(Box::new(KillWorkerTool));

    // ── 基础路径变量（Memory/Extension 构造前需要）──
    let worker_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let config_root = ion::paths::project_root_for_config()
        .to_string_lossy().to_string();
    let storage_ctx = ion::storage_context::StorageContext::new(
        &worker_cwd, &sid, &config_root,
    );

    // ── Memory 工具 + 共享 Store ──
    // Memory 用 config_root（worktree 场景回源主仓库，缺口 #2：worktree 共享记忆）
    let memory_store = std::sync::Arc::new(tokio::sync::Mutex::new(
        ion::agent::memory::MemoryStore::new(storage_ctx.clone())
    ));
    tools.register(Box::new(ion::agent::memory::MemorySaveTool { store: memory_store.clone() }));
    tools.register(Box::new(ion::agent::memory::MemorySearchTool { store: memory_store.clone() }));

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
        ion::paths::skills_dir(),
        ion::paths::project_skills_dir(&config_root),
        agents_skills,
    ];
    tools.register(Box::new(SkillTool { skill_dirs }));

    // 加载 API key
    let api_key = ion::auth::AuthStorage::resolve_api_key(None, &provider);
    if api_key.is_none() {
        // Hardcoded fallback for testing
        let key = std::env::var("ION_API_KEY").unwrap_or_else(|_| {
            "sk-sniMbFE0l8wIGsTAsbfERSGrvcrBv97iBfDuppzN99kg5Wp2a2dMYxntMFBN9lEg".into()
        });
        let _ = key; // Will be set below
    }
    let api_key = api_key.or_else(|| {
        std::env::var("ION_API_KEY").ok()
    }).unwrap_or_else(|| {
        "sk-sniMbFE0l8wIGsTAsbfERSGrvcrBv97iBfDuppzN99kg5Wp2a2dMYxntMFBN9lEg".into()
    });

	    let config = AgentConfig {
	        // max_turns：优先读 ION_MAX_TURNS 环境变量（补丁 1：hooks/扩展 spawn 子 Worker 时限定步数）
	        // 没设则默认 20（对齐 pi）。0 = 无限。
	        max_turns: std::env::var("ION_MAX_TURNS").ok()
	            .and_then(|s| s.parse::<u64>().ok())
	            .map(|n| if n == 0 { None } else { Some(n) })
	            .unwrap_or(Some(20)),
	        max_outer_iterations: std::env::var("ION_MAX_OUTER_ITERATIONS")
	            .ok().and_then(|s| s.parse().ok())
	            .unwrap_or(5),
	        max_retries: 30,
	        retry_base_delay_ms: 1000, enable_compact: true,
	        compact_config: CompactConfig::default(),
	        api_key: Some(api_key.clone()),
		        response_format: None, thinking: None,
			    compact_model_id: None,
		    // evolver/wf/improver agent 可能在 turn 里"只说不做"（输出文本但没调工具），
		    // retry_on_no_tool_use 让它在这种情况下重试（注入 WARNING）。
		    // 对这些 agent 默认启用（3 次重试），其他 agent 保持 0（禁用）。
            retry_on_no_tool_use: if matches!(initial_agent.as_deref(), Some("wf") | Some("improver") | Some("evolver")) {
		        std::env::var("ION_RETRY_NO_TOOL_USE")
		            .ok().and_then(|s| s.parse().ok())
		            .unwrap_or(3)
		    } else {
		        0
		    },
			    retry_config: Some(ion::retry::RetryConfig::default()),
	    };

    let registry = Arc::new(registry);

    // WASM 插件注册表（RPC 热更新用）
    let wasm_ext_registry = Arc::new(Registry::new());

    // 记录已加载的 WASM 路径（用于后续创建 HookAdapter）
    let mut loaded_wasm_paths: Vec<String> = Vec::new();

    // ── WASM 插件自动发现（Agent 构造前，注册到 tools）──
    // 扫描 ~/.ion/agent/extensions/ 和 {project_root}/.ion/extensions/ 下的 .wasm 文件
    // project_root 用 project_root_for_config()（worktree 场景回源到主仓库，缺口 #2）
    {
        let config_root = ion::paths::project_root_for_config()
            .to_string_lossy().to_string();
        let extensions_dirs: Vec<std::path::PathBuf> = vec![
            ion::paths::extensions_dir(),
            ion::paths::project_extensions_dir(&config_root),
        ];
        for dir in &extensions_dirs {
            if !dir.exists() { continue; }
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().map(|e| e == "wasm").unwrap_or(false) {
                        let canonical_str = std::fs::canonicalize(&path)
                            .map(|p| p.to_string_lossy().to_string())
                            .unwrap_or_else(|_| path.to_string_lossy().to_string());
                        let ext_name = ion::wasm_extension::ext_name_from_path(&canonical_str);
                        match wasm_ext_registry.add(&canonical_str) {
                            Ok(tool_defs) => {
                                for td in &tool_defs {
                                    tools.register(Box::new(ToolAdapter {
                                        name: td.name.clone(),
                                        description: td.description.clone(),
                                        parameters: td.parameters.clone(),
                                        extension_path: canonical_str.clone(),
                                        ext_name: ext_name.clone(),
                                        registry: wasm_ext_registry.clone(),
                                    }));
                                    tracing::info!("[wasm] auto-discovered {ext_name}: {}", td.name);
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
    let is_fork_child = std::env::var("ION_FORK_CHILD").map(|v| v == "1").unwrap_or(false);
    let session_file_path: std::path::PathBuf = if is_fork_child {
        ion::paths::session_jsonl_path_by_id(&worker_cwd, &sid)
    } else {
        ion::paths::session_jsonl_path(&worker_cwd)
    };
    // 存到全局，save_worker_session 用同一个路径
    {
        let mut p = SESSION_FILE_PATH.lock().unwrap();
        *p = Some(session_file_path.clone());
    }
    // 设置 lib 层全局覆盖（让 append_raw_entry / append_turn_summary 也用正确路径）
    // 这样 fork 子 Worker 的 turn_summary 不会写到主 session.jsonl
    ion::session_jsonl::set_session_file_override(Some(session_file_path.clone()));
    // 存 sid + cwd 到全局，on_before_tool_execute 钩子用
    {
        *SESSION_SID.lock().unwrap() = Some(sid.clone());
        *SESSION_CWD.lock().unwrap() = Some(worker_cwd.clone());
    }
    // 设 session header 的 agent/model/provider（export.rs banner 显示用）
    if let Some(ref agent_name) = initial_agent {
        unsafe { std::env::set_var("ION_SESSION_AGENT", agent_name); }
    }
    unsafe {
        std::env::set_var("ION_SESSION_MODEL", &model.id);
        std::env::set_var("ION_SESSION_PROVIDER", &provider);
    }

    // 先确保 session header 存在（防 turn_summary 在 header 之前被追加，导致文件第一行不是 header）
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
    let mut snapshot_store: Option<std::sync::Arc<ion::file_snapshot::SnapshotStore>> = None;

    // Approval Manager（预声明，审批 RPC 用，依赖 snapshot_store）
    #[allow(unused_assignments)]
    let mut approval_mgr: Option<std::sync::Arc<ion::file_snapshot::approval::ApprovalManager>> = None;

    // ── 加载配置（在 Runtime 和 Extension 初始化之前）──
    let ion_cfg = ion::config::IonConfig::load();

    // ── MCP（方案 C：所有 Worker 通过 bridge 代理调 host 的 MCP 连接）──
    // Worker 进程不自己 connect_all，而是从 host 拉工具列表注册 McpProxyTool。
    // 所有 Worker（入口 + 子）都是代理模式，host 持有唯一的 MCP 连接。
    // 场景 1（cmd_run）不走 worker，直接用 McpManager + McpTool（在 ion.rs 里处理）。

    // ── ManagerBridge 必须在 Agent 构造前创建，因为 WorkerRuntime 包装它注入到 Agent ──
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let manager_bridge: Arc<ManagerBridge> = Arc::new(ManagerBridge::new(sid.clone(), stdout.clone()));

    // ── 根据配置选择 Runtime ──
    // 用 Arc 保存，这样 HookExtension 能 clone 一份（agent handler 需要 runtime 来 spawn 子 Worker）
    let worker_rt: Arc<dyn ion::runtime::Runtime> = {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let registry = ion::backend_registry::BackendRegistry::from_config(&ion_cfg.runtime, &cwd);
        tracing::info!(
            "[runtime] BackendRegistry 初始化: backends={:?}",
            registry.list_backends(),
        );
        let worker_inner = ion::runtime::WorkerRuntime::new(
            registry,
            manager_bridge.clone() as Arc<dyn ion::runtime::ManagerBridgeHandle>,
        );
        Arc::new(worker_inner)
    };

    let default_prompt = "You are a helpful AI assistant with access to tools.".to_string();
    // 启动时应用 --agent 配置（如果指定）
    let mut initial_system_prompt = default_prompt.clone();
    let mut current_agent_name: String = "build".into();
    if let Some(ref agent_name) = initial_agent {
        if let Some(agent_cfg) = ion::agent_config::find_agent(agent_name) {
            current_agent_name = agent_cfg.name.clone();
            if let Some(ref sp) = agent_cfg.system_prompt {
                initial_system_prompt = sp.clone();
            }
            tracing::info!("[worker] loaded agent '{}' from config", agent_cfg.name);
            // auto-continue: wf/improver 需要（workflow 多 stage）
            // evolver 不需要 auto_continue——它用 bash_run background + follow_up
            if matches!(current_agent_name.as_str(), "wf" | "improver") {
                if std::env::var("ION_AUTO_CONTINUE").is_err() {
                    unsafe { std::env::set_var("ION_AUTO_CONTINUE", "1"); }
                    tracing::info!("[worker] auto-set ION_AUTO_CONTINUE=1 for {} agent", current_agent_name);
                }
            }
            // evolver: 等 bash_run 后台进程的异步 follow_up
            if current_agent_name == "evolver" {
                unsafe { std::env::set_var("ION_WAIT_BACKGROUND", "1"); }
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
    if let Ok(sp_override) = std::env::var("ION_SYSTEM_PROMPT") {
        if !sp_override.is_empty() {
            tracing::info!("[worker] system prompt overridden by ION_SYSTEM_PROMPT ({} bytes)", sp_override.len());
            initial_system_prompt = sp_override;
        }
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
        let project_root = std::env::var("ION_PROJECT_ROOT").ok()
            .or_else(|| {
                // Try to find git root from cwd
                std::process::Command::new("git")
                    .args(&["rev-parse", "--show-toplevel"])
                    .current_dir(cwd)
                    .output().ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .map(|s| s.trim().to_string())
            })
            .unwrap_or_else(|| cwd.clone());
        let worktree = std::env::var("ION_WORKTREE_ROOT").ok()
            .or_else(|| {
                std::env::var("ION_WORKTREE").ok()
            });
        let git_remote = std::process::Command::new("git")
            .args(&["remote", "get-url", "origin"])
            .current_dir(cwd)
            .output().ok()
            .and_then(|o| {
                let url = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if url.is_empty() { None } else { Some(url) }
            });
        let git_branch = std::process::Command::new("git")
            .args(&["rev-parse", "--abbrev-ref", "HEAD"])
            .current_dir(cwd)
            .output().ok()
            .and_then(|o| {
                let branch = String::from_utf8_lossy(&o.stdout).trim().to_string();
                if branch.is_empty() { None } else { Some(branch) }
            });

        let mut info = format!("\n\n## Environment\n");
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
            .args(&["log", "--oneline", "--name-only", "-3"])
            .current_dir(cwd)
            .output().ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());
        if let Some(commits) = &recent {
            if !commits.is_empty() {
                info.push_str("\n### Recent Changes (last 3 commits)\n```\n");
                info.push_str(commits);
                info.push_str("\n```\n");
            }
        }

        // Uncommitted changes
        let uncommitted = std::process::Command::new("git")
            .args(&["status", "--short"])
            .current_dir(cwd)
            .output().ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .map(|s| s.trim().to_string());
        if let Some(changes) = &uncommitted {
            if !changes.is_empty() {
                info.push_str("\n### Uncommitted Changes\n```\n");
                info.push_str(changes);
                info.push_str("\n```\n");
            }
        }

        info
    };
    initial_system_prompt.push_str(&env_info);

    let mut agent = Agent::new(
        Arc::clone(&registry),
        model.clone(),
        Some(initial_system_prompt),
        tools,
        config,
    )
        .with_runtime_arc(worker_rt.clone())
        .with_session_cwd(Some(worker_cwd.clone()));

    // 应用初始 agent 的工具限制（必须在 Agent 构造后调用）
    if let Some(ref agent_name) = initial_agent {
        if let Some(agent_cfg) = ion::agent_config::find_agent(agent_name) {
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
    }

    // ── 补丁 1（HOOKS_AND_OUTLINE_SYNC）：环境变量来源的工具限制 ──
    // Manager spawn 子 Worker 时通过 ION_ALLOWED_TOOLS / ION_DISALLOWED_TOOLS 环境变量传入。
    // 叠加在 agent.md 定义的限制之后（进一步收紧，不能放宽）：
    //   - 白名单：与 agent.md 的白名单取交集（agent.md 没设白名单则直接用环境变量的）
    //   - 黑名单：并集（两边都禁的都禁）
    // 这让扩展/hooks 的 agent handler 能 spawn "限定工具"的子 Worker，
    // 是 ION 的 agent handler 比 pi 更强的关键。
    if let Ok(allowed_str) = std::env::var("ION_ALLOWED_TOOLS") {
        let allowed: Vec<String> = allowed_str.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !allowed.is_empty() {
            agent.restrict_tools(allowed);
            tracing::info!("[worker] applied ION_ALLOWED_TOOLS from env");
        }
    }
    if let Ok(disallowed_str) = std::env::var("ION_DISALLOWED_TOOLS") {
        for tool_name in disallowed_str.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            agent.remove_tool(tool_name);
        }
        tracing::info!("[worker] applied ION_DISALLOWED_TOOLS from env");
    }

    if let Some(msgs) = preloaded {
        agent = agent.with_messages(msgs);
    }

    // ── 注册内置 Extension（Memory / Bash / Streaming），可通过 config.json 关闭 ──
    // 先创建 follow_up 通道（bash 插件后台进程完成时用来注入消息）
    let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let mut process_map = None;
    let mut stdin_map = None;
    let mut notify_map = None;
    {
        let mut ext_reg = ion::agent::extension::ExtensionRegistry::new();

        // ── 注入 ctx.fs 统一文件访问能力（RuntimeFileSystem）──
        // 内置扩展通过 registry.filesystem() 拿到，WASM 扩展通过 host_read_file / host_list_dir 拿到。
        // allowed_roots = 项目根目录 + ~/.ion/（默认白名单，防路径逃逸）。
        {
            let fs_allowed_roots =
                ion::agent::extension::RuntimeFileSystem::default_allowed_roots(
                    std::path::Path::new(&worker_cwd),
                );
            let runtime_fs = std::sync::Arc::new(
                ion::agent::extension::RuntimeFileSystem::new(
                    worker_rt.clone(),
                    fs_allowed_roots,
                ),
            );
            // 内置扩展用
            ext_reg = ext_reg.with_filesystem(runtime_fs.clone());
            // WASM 扩展用（注入到 WASM registry 的共享 Context）
            {
                let mut ctx = wasm_ext_registry.ctx.write().unwrap();
                ctx.fs = Some(runtime_fs.clone());
                ctx.tokio_handle = Some(tokio::runtime::Handle::current());
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
        tracing::info!("[extension] session_probe registered (session hook observable via subscribe)");

        // Memory Extension
        if ion_cfg.is_extension_enabled("memory") {
            let mut memory_ext = ion::agent::memory::MemoryExtension::new(storage_ctx.clone());
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
            let bash_ext = ion::agent::bash::BashExtension::new(storage_ctx.clone());
            process_map = Some(bash_ext.process_map.clone());
            stdin_map = Some(bash_ext.stdin_map.clone());
            notify_map = Some(bash_ext.notify_map.clone());
            ext_reg.register(Box::new(bash_ext));
        } else {
            tracing::info!("[extension] bash disabled by config");
        }

        // Streaming Extension（流式透传）
        if ion_cfg.is_extension_enabled("streaming") {
            ext_reg.register(Box::new(StreamingExtension));
        } else {
            tracing::info!("[extension] streaming disabled by config");
        }

        // Permission Extension（权限策略层）
        // 用 config_root（worktree 回源主仓库，读主仓库 .ion/settings.json）
        if ion_cfg.is_extension_enabled("permission") {
            let perm_ext = ion::agent::permission_extension::PermissionExtension::new(storage_ctx.clone());
            ext_reg.register(Box::new(perm_ext));
        } else {
            tracing::info!("[extension] permission disabled by config");
        }

        // Context Index Extension（上下文索引 + 快照折叠）
        if ion_cfg.is_extension_enabled("context-index") {
            let ctx_ext = ion::agent::context_index::ContextIndexExtension::new();
            ext_reg.register(Box::new(ctx_ext));
        } else {
            tracing::info!("[extension] context-index disabled by config");
        }

        // File Snapshot Extension（文件快照 + diff 追踪）
        snapshot_store =
            if ion_cfg.is_extension_enabled("file-snapshot") {
                let (fs_ext, store) = ion::file_snapshot::FileSnapshotExtension::new_pair(storage_ctx.clone());
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
            let mgr = std::sync::Arc::new(
                ion::file_snapshot::approval::ApprovalManager::new(store.clone(), storage_ctx.clone())
            );
            // 注册 ApprovalExtension（on_gate_check + on_turn_end re-approval 重置）
            ext_reg.register(Box::new(
                ion::file_snapshot::approval::ApprovalExtension::new(mgr.clone())
            ));
            tracing::info!("[extension] file-approval enabled");
            Some(mgr)
        } else {
            tracing::info!("[extension] file-approval disabled (requires file-snapshot)");
            None
        };

        // ── 注册 WASM Extension 的 HookAdapter（让 WASM 也能实现 29 个钩子）──
        for wasm_path in &loaded_wasm_paths {
            if let Some(hook_adapter) = wasm_ext_registry.create_hook_adapter(wasm_path) {
                ext_reg.register(Box::new(hook_adapter));
                tracing::info!("[wasm] registered HookAdapter for {}", wasm_path);
            }
        }

        // ── 注册 WorkflowExtension（可配置，默认启用）──
        // 当 agent .md 定义了 workflow: gate_command 时才生效。
        if ion_cfg.is_extension_enabled("workflow_gate") {
            if let Some(ref agent_name) = initial_agent {
                if let Some(agent_cfg) = ion::agent_config::find_agent(agent_name) {
                    if let Some(ref wf_config) = agent_cfg.workflow {
                        tracing::info!("[workflow] gate registered: cmd='{}', expected='{}'",
                            wf_config.gate_command, wf_config.gate_expected);
                        ext_reg.register(Box::new(
                            ion::agent::workflow_extension::WorkflowExtension::new(wf_config.clone())
                        ));
                    }
                }
            }
        }

        // ── 注册 HookExtension（hooks.json 配置式钩子，热重载）──
        // 每次 on_session_start 等钩子触发时动态读 hooks.json，改完即生效。
        // runtime=None：command handler 用 tokio::spawn fallback；agent handler 待后续接入 runtime
        if ion_cfg.is_extension_enabled("hooks") {
            let proj_dir = std::path::PathBuf::from(&worker_cwd);
            if ion::hooks::extension::HookExtension::has_hooks(&proj_dir) {
                let hook_ext = ion::hooks::extension::HookExtension::new(
                    proj_dir,
                    Some(worker_rt.clone()),     // agent handler 需要 runtime 来 spawn 子 Worker
                    Some(Arc::clone(&registry)), // prompt handler 需要 ApiRegistry 来调 LLM
                    Some(model.clone()),         // prompt handler 需要当前会话模型
                    Some(manager_bridge.clone() as Arc<dyn ion::runtime::ManagerBridgeHandle>), // mcp_tool handler 转发 MCP 调用
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

        // 注册 bash 工具（仅当 bash extension 启用时）
        if let (Some(pm), Some(sm), Some(nm)) = (&process_map, &stdin_map, &notify_map) {
            let bash_run_tool = ion::agent::bash::BashRunTool {
                process_map: pm.clone(),
                stdin_map: sm.clone(),
                notify_map: nm.clone(),
                follow_up_tx: Some(follow_up_tx.clone()),
                storage: storage_ctx.clone(),
            };
            let bash_kill_tool = ion::agent::bash::BashKillTool {
                process_map: pm.clone(),
                follow_up_tx: Some(follow_up_tx.clone()),
                storage: storage_ctx.clone(),
            };
            let bash_send_tool = ion::agent::bash::BashSendTool {
                stdin_map: sm.clone(),
            };
            let bash_bg_tool = ion::agent::bash::BashBackgroundTool {
                notify_map: nm.clone(),
                process_map: pm.clone(),
                storage: storage_ctx.clone(),
            };
            agent.register_tool(Box::new(bash_run_tool));
            agent.register_tool(Box::new(bash_kill_tool));
            agent.register_tool(Box::new(bash_send_tool));
            agent.register_tool(Box::new(bash_bg_tool));
        }
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
                    if line.trim().is_empty() { continue; }
                    match serde_json::from_str::<serde_json::Value>(&line) {
                        Ok(v) => {
                            // 关键：_reply_to 消息是 manager_response，直接投递避免死锁
                            let has_reply_to = v.get("_reply_to").and_then(|r| r.as_str()).map(|s| !s.is_empty()).unwrap_or(false);
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
        ).await;
        match mcp_result {
            Ok(Ok(resp)) => {
                if resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                    let tools_list = resp
                        .get("data")
                        .and_then(|d| d.get("tools"))
                        .cloned()
                        .unwrap_or(serde_json::json!([]));
                    if let Some(arr) = tools_list.as_array() {
                        for tool in arr {
                            let full_name = tool.get("full_name").and_then(|v| v.as_str()).unwrap_or("");
                            let desc = tool.get("description").and_then(|v| v.as_str()).unwrap_or("");
                            let params = tool.get("input_schema").cloned().unwrap_or(serde_json::json!({}));
                            if !full_name.is_empty() {
                                agent.register_tool(Box::new(McpProxyTool::new(
                                    full_name, desc, &params, manager_bridge.clone(),
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
                tracing::info!("[mcp] mcp_list_tools timeout (no host or 3s limit), skip MCP proxy");
            }
        }
    }

    while let Some(cmd) = stdin_rx.recv().await {
        let id = cmd.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        let method = cmd.get("method").and_then(|v| v.as_str())
            .or_else(|| cmd.get("type").and_then(|v| v.as_str()))
            .unwrap_or("").to_string();
        let params = cmd.get("params").cloned().unwrap_or(serde_json::Value::Null);

        // 分发命令
        match method.as_str() {
            // ── 同步查询 ──
            "get_state" => {
                output_response(&id, "get_state", &serde_json::json!({
                    "model": model_id,
                    "provider": provider,
                    "session_id": sid,
                    "message_count": agent.messages().len(),
                    "is_running": agent.is_running(),
                    "steering_queue": agent.steering_queue_len(),
                    "follow_up_queue": agent.follow_up_queue_len(),
                }));
            }

            "get_session_info" => {
                // 统一状态接口（合并 get_state + get_session_stats + token 统计）
                let total_input: u64 = agent.messages().iter()
                    .filter_map(|m| match m { Message::Assistant(a) => Some(a.usage.input), _ => None })
                    .sum();
                let total_output: u64 = agent.messages().iter()
                    .filter_map(|m| match m { Message::Assistant(a) => Some(a.usage.output), _ => None })
                    .sum();
                let user_count = agent.messages().iter()
                    .filter(|m| matches!(m, Message::User(_))).count();
                let assistant_count = agent.messages().iter()
                    .filter(|m| matches!(m, Message::Assistant(_))).count();
                let tool_result_count = agent.messages().iter()
                    .filter(|m| matches!(m, Message::ToolResult(_))).count();
                output_response(&id, "get_session_info", &serde_json::json!({
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
                }));
            }

            "get_inflight_messages" => {
                // 获取内存中的消息（还没落盘的）
                // 返回最后 N 条 + 总数,让前端跟磁盘 list_turns 拼接
                let limit = params.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
                let msgs = agent.messages();
                let total = msgs.len();
                let start = total.saturating_sub(limit);
                let recent: Vec<serde_json::Value> = msgs[start..].iter()
                    .map(|m| serde_json::to_value(m).unwrap_or(serde_json::json!(null)))
                    .collect();
                output_response(&id, "get_inflight_messages", &serde_json::json!({
                    "total": total,
                    "returned": recent.len(),
                    "is_running": agent.is_running(),
                    "messages": recent,
                }));
            }

            "get_session_stats" => {
                let total_input: u64 = agent.messages().iter()
                    .filter_map(|m| match m { Message::Assistant(a) => Some(a.usage.input), _ => None })
                    .sum();
                let total_output: u64 = agent.messages().iter()
                    .filter_map(|m| match m { Message::Assistant(a) => Some(a.usage.output), _ => None })
                    .sum();

                // 从 SessionIndex 读血缘 + lastEntryId
                let index = ion::session_index::SessionIndex::load();
                let meta = index.get(&sid);
                let parent_session = meta.and_then(|m| m.parent_session.clone());
                let parent_type = meta.and_then(|m| m.parent_type.clone());
                let last_entry_id = meta.and_then(|m| m.last_entry_id.clone());

                // 从磁盘读 lastEntryId（如果 index 里没有）
                let last_entry_id = last_entry_id.or_else(|| {
                    ion::session_jsonl::SessionFile::load(&worker_cwd)
                        .and_then(|f| f.last_id)
                });

                output_response(&id, "get_session_stats", &serde_json::json!({
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
                }));
            }

            "get_children" => {
                let target_session = params.get("session").and_then(|v| v.as_str()).unwrap_or(&sid);
                let index = ion::session_index::SessionIndex::load();
                let children: Vec<_> = index.get_children(target_session).iter().map(|m| {
                    serde_json::json!({
                        "id": m.name,
                        "name": m.name,
                        "turnCount": m.turn_count,
                        "updatedAt": m.updated_at,
                        "parentSession": m.parent_session,
                        "parentType": m.parent_type,
                    })
                }).collect();
                output_response(&id, "get_children", &serde_json::json!({
                    "children": children,
                    "count": children.len(),
                }));
            }

            "get_messages" => {
                // 解析分页参数
                let view_str = params.get("view").and_then(|v| v.as_str()).unwrap_or("live");
                let view = match view_str {
                    "since_compaction" => ion::message_retrieval::View::SinceCompaction,
                    "full" => ion::message_retrieval::View::Full,
                    s if s.starts_with("branch:") => {
                        ion::message_retrieval::View::Branch(s[7..].to_string())
                    }
                    _ => ion::message_retrieval::View::Live,
                };
                let after = params.get("after").and_then(|v| v.as_str()).map(|s| s.to_string());
                let before = params.get("before").and_then(|v| v.as_str()).map(|s| s.to_string());
                let limit = params.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(50);
                let complete_turn = params.get("complete_turn").and_then(|v| v.as_bool()).unwrap_or(true);
                let custom_str = params.get("include_custom").and_then(|v| v.as_str()).unwrap_or("none");
                let include_custom = match custom_str {
                    "display_only" => ion::message_retrieval::CustomFilter::DisplayOnly,
                    "all" => ion::message_retrieval::CustomFilter::All,
                    _ => ion::message_retrieval::CustomFilter::None,
                };

                // 从磁盘读 entries（含 turn_summary/compaction 等非 message entry）
                let entries: Vec<serde_json::Value> =
                    ion::message_retrieval::load_entries_cached(&worker_cwd);

                let retrieval_params = ion::message_retrieval::RetrievalParams {
                    view,
                    after,
                    before,
                    limit,
                    complete_turn,
                    include_custom,
                };
                let result = ion::message_retrieval::retrieve_messages(&entries, &retrieval_params);

                output_response(&id, "get_messages", &serde_json::json!({
                    "messages": result.messages,
                    "hasMore": result.has_more,
                    "totalCount": result.total_count,
                    "nextCursor": result.next_cursor,
                    "view": result.view,
                    "compactionPoints": result.compaction_points,
                }));
            }

            "list_turns" => {
                let full_content = params.get("full_content").and_then(|v| v.as_bool()).unwrap_or(false);
                let limit = params.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(50);
                let entries: Vec<serde_json::Value> =
                    ion::message_retrieval::load_entries_cached(&worker_cwd);
                let params = ion::message_retrieval::RetrievalParams {
                    limit,
                    ..Default::default()
                };
                let result = ion::message_retrieval::retrieve_turns(&entries, &params, full_content);
                output_response(&id, "list_turns", &serde_json::json!({
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

            "list_inputs" => {
                let entries: Vec<serde_json::Value> =
                    ion::message_retrieval::load_entries_cached(&worker_cwd);
                let result = ion::message_retrieval::retrieve_inputs(
                    &entries,
                    &ion::message_retrieval::RetrievalParams::default(),
                );
                output_response(&id, "list_inputs", &serde_json::json!({
                    "inputs": result.inputs.iter().map(|i| serde_json::json!({
                        "turnId": i.turn_id,
                        "entryId": i.entry_id,
                        "text": i.text,
                    })).collect::<Vec<_>>(),
                    "hasMore": result.has_more,
                    "totalCount": result.total_count,
                    "nextCursor": result.next_cursor,
                }));
            }

            "get_turn_detail" => {
                let turn_id = params.get("turnId").and_then(|v| v.as_str()).unwrap_or("");
                let entries: Vec<serde_json::Value> =
                    ion::message_retrieval::load_entries_cached(&worker_cwd);
                match ion::message_retrieval::retrieve_turn_detail(
                    &entries,
                    turn_id,
                    &ion::message_retrieval::CustomFilter::None,
                ) {
                    Some(detail) => output_response(&id, "get_turn_detail", &serde_json::json!({
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
                    None => output_response(&id, "get_turn_detail", &serde_json::json!({
                        "error": "turn not found", "turnId": turn_id
                    })),
                }
            }

            "get_last_assistant_text" => {
                let text = agent.messages().iter().rev()
                    .find_map(|m| match m {
                        Message::Assistant(a) => a.content.iter().find_map(|b| match b {
                            AssistantContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        }),
                        _ => None,
                    }).unwrap_or_default();
                output_response(&id, "get_last_assistant_text", &serde_json::json!(text));
            }

            "get_tools" => {
                output_response(&id, "get_tools", &serde_json::json!({"tools": [
                    {"name": "read"}, {"name": "write"}, {"name": "edit"},
                    {"name": "bash"}, {"name": "grep"}, {"name": "find"},
                    {"name": "ls"}, {"name": "calculator"}, {"name": "echo"}
                ]}));
            }

            // ── 异步操作 ──
            "set_model" => {
                let new_model = params.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
                let new_provider = params.get("provider").and_then(|v| v.as_str()).unwrap_or(&provider);
                model_id = new_model.to_string();
                provider = new_provider.to_string();
                output_response(&id, "get_state", &serde_json::json!({
                    "model": model_id, "provider": provider
                }));
            }

            "set_thinking_level" => {
                let level = params.get("level").and_then(|v| v.as_str()).unwrap_or("off");
                output_response(&id, "set_thinking_level", &serde_json::json!({"thinkingLevel": level}));
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
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                // 默认 behavior：steer（对齐 pi：流式中默认插话入队，不打断）。
                // 可通过 ION_PROMPT_BEHAVIOR=interrupt 切回旧行为。
                // 显式传 params.behavior / params.streamingBehavior 优先级最高。
                let default_behavior = std::env::var("ION_PROMPT_BEHAVIOR")
                    .ok().filter(|s| matches!(s.as_str(), "interrupt" | "steer" | "followUp"))
                    .unwrap_or_else(|| "steer".to_string());
                let pbehavior = params.get("behavior").or_else(|| params.get("streamingBehavior"))
                    .and_then(|v| v.as_str()).unwrap_or(&default_behavior);

                // !cmd 用户直发：拦截成 bash_command（避免走完整 agent loop，对齐 pi）
                // 形如 "!ls -la" 或 "! cargo build" → 取 '!' 之后的部分作为命令
                if let Some(stripped) = text.strip_prefix('!') {
                    let cmd_text = stripped.trim().to_string();
                    if !cmd_text.is_empty() {
                        // 直接执行，不入 agent loop
                        let timeout_secs = params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
                        let (stdout, stderr, exit_code) = match execute_bash(&cmd_text, timeout_secs).await {
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
                                output_response(&id, "prompt", &serde_json::json!({
                                    "status":"bash_error",
                                    "command": cmd_text,
                                    "error": e,
                                }));
                                continue;
                            }
                        };
                        let combined = if stderr.is_empty() { stdout }
                            else if stdout.is_empty() { stderr }
                            else { format!("{stdout}\n[stderr]\n{stderr}") };
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

                        output(&serde_json::json!({"type":"event","event":{"type":"agent_start","sessionId":sid,"timestamp":now_ms()}}));
                        output(&serde_json::json!({"type":"event","event":{"type":"text_delta","delta":&combined}}));
                        output(&serde_json::json!({"type":"event","event":{"type":"agent_end","sessionId":sid,"timestamp":now_ms()}}));
                        output_response(&id, "prompt", &serde_json::json!({
                            "status":"bash_executed",
                            "command": cmd_text,
                            "exitCode": exit_code,
                            "output": combined,
                            "truncated": truncated,
                        }));
                        continue;
                    }
                }

                let mut skip = false;
                if agent.is_running() && pbehavior == "steer" {
                    agent.steer(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent { text: text.clone(), text_signature: None })],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::Steer,
                    }));
                    output_response(&id, "prompt", &serde_json::json!({"status":"queued","queue":"steering"}));
                    skip = true;
                } else if agent.is_running() && pbehavior == "followUp" {
                    agent.follow_up(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent { text: text.clone(), text_signature: None })],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::FollowUp,
                    }));
                    output_response(&id, "prompt", &serde_json::json!({"status":"queued","queue":"followUp"}));
                    skip = true;
                } else if agent.is_running() && pbehavior == "interrupt" {
                    agent.stop();
                }

                if !skip {
                    output_response(&id, "prompt", &serde_json::Value::Null);
                    // agent_start / text_delta / agent_end 由 StreamingExtension 实时推送，
                    // 不需要这里再发（避免重复）
                    output(&serde_json::json!({"type":"event","event":{"type":"agent_start","sessionId":sid,"timestamp":now_ms()}}));
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
                    let pending_steer_queue: std::sync::Arc<tokio::sync::Mutex<std::collections::VecDeque<(ion_provider::types::MessageSource, ion_provider::types::Message)>>> =
                        std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::new()));
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
                                            let entries: Vec<serde_json::Value> = ion::message_retrieval::load_entries_cached(&worker_cwd);
                                            let rp = ion::message_retrieval::RetrievalParams { limit, ..Default::default() };
                                            let result = ion::message_retrieval::retrieve_turns(&entries, &rp, full_content);
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
                                                "since_compaction" => ion::message_retrieval::View::SinceCompaction,
                                                "full" => ion::message_retrieval::View::Full,
                                                s if s.starts_with("branch:") => ion::message_retrieval::View::Branch(s[7..].to_string()),
                                                _ => ion::message_retrieval::View::Live,
                                            };
                                            let limit = bg_params.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize).unwrap_or(0);
                                            let after = bg_params.get("after").and_then(|v| v.as_str()).map(String::from);
                                            let before = bg_params.get("before").and_then(|v| v.as_str()).map(String::from);
                                            let complete_turn = bg_params.get("complete_turn").and_then(|v| v.as_bool()).unwrap_or(false);
                                            let inc_custom = bg_params.get("include_custom").and_then(|v| v.as_str()).unwrap_or("none");
                                            let include_custom = match inc_custom {
                                                "display_only" => ion::message_retrieval::CustomFilter::DisplayOnly,
                                                "all" => ion::message_retrieval::CustomFilter::All,
                                                _ => ion::message_retrieval::CustomFilter::None,
                                            };
                                            let entries: Vec<serde_json::Value> = ion::message_retrieval::load_entries_cached(&worker_cwd);
                                            let rp = ion::message_retrieval::RetrievalParams {
                                                view, after, before, limit, complete_turn, include_custom,
                                            };
                                            let result = ion::message_retrieval::retrieve_messages(&entries, &rp);
                                            output_response(&bg_id, "get_messages", &serde_json::json!({
                                                "messages": result.messages,
                                                "hasMore": result.has_more,
                                                "totalCount": result.total_count,
                                                "nextCursor": result.next_cursor,
                                                "view": view_str,
                                            }));
                                        }
                                        "list_inputs" => {
                                            let entries: Vec<serde_json::Value> = ion::message_retrieval::load_entries_cached(&worker_cwd);
                                            let rp = ion::message_retrieval::RetrievalParams::default();
                                            let result = ion::message_retrieval::retrieve_inputs(&entries, &rp);
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
                                            let entries: Vec<serde_json::Value> = ion::message_retrieval::load_entries_cached(&worker_cwd);
                                            match ion::message_retrieval::retrieve_turn_detail(&entries, turn_id, &ion::message_retrieval::CustomFilter::None) {
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
                            let msgs_json: Vec<serde_json::Value> = agent.messages().iter()
                                .filter_map(|m| serde_json::to_value(m).ok())
                                .collect();
                            save_worker_session(&sid, &worker_cwd, &msgs_json);
                            // 区分正常完成 vs 被中止
                            let was_stopped = stopped_handle.load(std::sync::atomic::Ordering::SeqCst);
                            let (evt_type, reason) = if was_stopped {
                                ("agent_stopped", "user_abort")
                            } else {
                                ("agent_end", "completed")
                            };
                            output(&serde_json::json!({
                                "type":"event","event":{
                                    "type":evt_type,
                                    "sessionId":sid,
                                    "timestamp":now_ms(),
                                    "reason":reason
                                }
                            }));
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
                                ion_provider::types::MessageSource::FollowUp => agent.follow_up(msg),
                                _ => agent.follow_up(msg),
                            }
                        }
                    }
                }
            }
            "steer" => {
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let immediate = params.get("immediate").and_then(|v| v.as_bool()).unwrap_or(false);
                let promote = params.get("promote").and_then(|v| v.as_u64());
                if let Some(idx) = promote {
                    agent.promote_follow_up(idx as usize);
                    if text.is_empty() && !immediate {
                        output_response(&id, "steer", &serde_json::json!({"status":"promoted"}));
                        output_response(&id, "steer", &serde_json::Value::Null);
                        break;
                    }
                }
                if immediate { agent.stop(); }
                if !text.is_empty() {
                    agent.steer(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent { text: text.clone(), text_signature: None })],
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
                let index = params.get("item")
                    .and_then(|i| i.get("index")).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let text = params.get("item")
                    .and_then(|i| i.get("text")).and_then(|v| v.as_str()).unwrap_or("").to_string();
                agent.promote_follow_up(index);
                if !text.is_empty() {
                    agent.steer(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent { text: text.clone(), text_signature: None })],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::Steer,
                    }));
                }
                output_response(&id, "promote_follow_up", &serde_json::Value::Null);
            }
            "remove_follow_up" => {
                let index = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let removed = agent.remove_follow_up(index);
                output_response(&id, "remove_follow_up", &serde_json::json!({
                    "removed": removed.is_some(),
                    "follow_up_queue": agent.follow_up_queue_len(),
                }));
            }
            "remove_steering" => {
                let index = params.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                let removed = agent.remove_steering(index);
                output_response(&id, "remove_steering", &serde_json::json!({
                    "removed": removed.is_some(),
                    "steering_queue": agent.steering_queue_len(),
                }));
            }
// ── Channel 消息 (从其他 Worker 转发过来) ──
            // 把消息作为 follow_up 注入 Agent，让 Agent 下一轮消化（不抢当前轮次）。
            "channel_msg" => {
                let channel = params.get("channel").and_then(|v| v.as_str())
                    .or_else(|| cmd.get("channel").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let from = params.get("from").and_then(|v| v.as_str())
                    .or_else(|| cmd.get("from").and_then(|v| v.as_str()))
                    .unwrap_or("");
                let msg_text = params.get("msg")
                    .and_then(|m| m.get("text")).and_then(|v| v.as_str())
                    .or_else(|| params.get("msg").and_then(|v| v.as_str()))
                    .or_else(|| cmd.get("msg").and_then(|v| v.as_str()))
                    .unwrap_or("");

                let from_short = if from.len() >= 12 { &from[..12] } else { from };
                let user_text = format!("[channel #{} from {}] {}", channel, from_short, msg_text);

                // 注入到 Agent follow_up queue（Agent 当前轮次结束后自动消化）
                agent.follow_up(ion::agent::messages::Message::User(
                    ion::agent::messages::UserMessage {
                        role: "user".into(),
                        content: vec![ion::agent::messages::ContentBlock::Text(
                            ion::agent::messages::TextContent { text: user_text, text_signature: None }
                        )],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::FollowUp,
                    }
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
                output_response(&id, "create_worker", &serde_json::json!({
                    "status": "pending",
                    "message": "create_worker forwarded to Manager",
                }));
            }

            "channel_send" => {
                let bridge = manager_bridge.clone();
                tokio::spawn(async move {
                    let _ = bridge.send_command("channel_send", params).await;
                });
                output_response(&id, "channel_send", &serde_json::json!({
                    "status": "pending",
                    "message": "channel_send forwarded to Manager",
                }));
            }

            "send_to_worker" => {
                let bridge = manager_bridge.clone();
                tokio::spawn(async move {
                    let _ = bridge.send_command("send_to_worker", params).await;
                });
                output_response(&id, "send_to_worker", &serde_json::json!({
                    "status": "pending",
                    "message": "send_to_worker forwarded to Manager",
                }));
            }

            // ── 生命周期 ──
            "kill" | "shutdown" | "dispose" => {
                output_response(&id, "shutdown", &serde_json::Value::Null);
                break;
            }

            // ── 未实现的命令（返回空/默认值，格式对齐 pi）──
            "get_system_prompt" => {
                // Return the first user message (system prompt)
                let sp = agent.messages().iter()
                    .find_map(|m| match m {
                        ion::agent::messages::Message::User(u) => u.content.iter().find_map(|b| match b {
                            ion::agent::messages::ContentBlock::Text(t) => Some(t.text.clone()),
                            _ => None,
                        }),
                        _ => None,
                    }).unwrap_or_default();
                output_response(&id, "get_system_prompt", &serde_json::json!(sp));
            },
            "get_agents" => {
                // 真实实现：列出所有内置 + 自定义 agent
                let agents = ion::agent_config::builtin_agents();
                let list: Vec<serde_json::Value> = agents.iter().map(|a| {
                    serde_json::json!({
                        "name": a.name,
                        "description": a.description,
                        "color": a.color,
                        "tier": a.tier,
                        "source": a.source,
                    })
                }).collect();
                output_response(&id, "get_agents", &serde_json::json!(list));
            },
            "get_current_agent" => {
                // 当前 agent（从 ion::agent_config 读真实定义）
                let cur = ion::agent_config::find_agent(&current_agent_name)
                    .unwrap_or_else(|| {
                        ion::agent_config::builtin_agents().into_iter()
                            .next().unwrap()
                    });
                output_response(&id, "get_current_agent", &serde_json::json!({
                    "name": cur.name,
                    "description": cur.description,
                    "color": cur.color,
                    "tier": cur.tier,
                }));
            },
            "get_settings" => {
                let cfg = ion::config::IonConfig::load();
                let key = params.get("key").and_then(|v| v.as_str());
                if let Some(k) = key {
                    let val = match k {
                        "default_provider" | "default-provider" => serde_json::json!(cfg.default_provider),
                        "default_model" | "default-model" => serde_json::json!(cfg.default_model),
                        "api_key" | "api-key" => serde_json::json!(if cfg.api_key.is_some() { "***" } else { "" }),
                        "base_url" | "base-url" => serde_json::json!(cfg.base_url),
                        "runtime" => serde_json::json!(cfg.runtime),
                        "extensions" => serde_json::json!(cfg.extensions),
                        _ => serde_json::Value::Null,
                    };
                    output_response(&id, "get_settings", &serde_json::json!({ "key": k, "value": val }));
                } else {
                    let mut cfg_json = serde_json::to_value(&cfg).unwrap_or_default();
                    if cfg_json.get("api_key").is_some() {
                        cfg_json["api_key"] = serde_json::json!(if cfg.api_key.is_some() { "***" } else { "" });
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
                ]);
                output_response(&id, "get_commands", &commands);
            }
            "get_skills" => {
                // 列出全局 + 项目级 skills
                let mut skills: Vec<serde_json::Value> = Vec::new();

                // 全局 skills (~/.ion/skills/)
                let global_dir = ion::paths::skills_dir();
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
                let proj_dir = ion::paths::project_skills_dir(&config_root);
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

                output_response(&id, "get_skills", &serde_json::json!({
                    "skills": skills,
                    "count": skills.len(),
                }));
            }
            "get_extensions" => {
                // 列出已加载的扩展（从 ExtensionRegistry）
                let exts: Vec<_> = agent.extensions().names();
                output_response(&id, "get_extensions", &serde_json::json!({
                    "extensions": exts.iter().map(|n| serde_json::json!({"name": n})).collect::<Vec<_>>(),
                    "count": exts.len(),
                }));
            }
            "get_available_models" => {
                let models: Vec<serde_json::Value> = model_reg.list_models().iter()
                    .map(|m| serde_json::json!({
                        "id": m.id, "name": m.name, "provider": m.provider,
                        "reasoning": m.reasoning, "contextWindow": m.context_window,
                    }))
                    .collect();
                output_response(&id, "get_available_models", &serde_json::json!(models));
            },
            "get_tier_models" => {
                let cfg = ion::config::IonConfig::load();
                output_response(&id, "get_tier_models", &serde_json::json!(cfg.tier_models));
            }
            "get_tree" => {
                let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("structure");
                let entries: Vec<serde_json::Value> =
                    ion::message_retrieval::load_entries_cached(&worker_cwd);

                if entries.is_empty() {
                    output_response(&id, "get_tree", &serde_json::json!({
                        "nodes": [], "currentLeaf": null, "branches": [], "compactionPoints": []
                    }));
                } else if mode == "full" {
                    // full 模式：返回全部 entry 骨架
                    let nodes: Vec<_> = entries.iter().map(|e| serde_json::json!({
                        "id": e.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                        "parentId": e.get("parentId").and_then(|v| v.as_str()),
                        "type": e.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                        "turnId": e.get("turnId").and_then(|v| v.as_u64()),
                    })).collect();
                    output_response(&id, "get_tree", &serde_json::json!({
                        "nodes": nodes, "mode": "full"
                    }));
                } else {
                    // structure 模式：只返回 compaction + leaf_pointer + 分支末端
                    let struct_nodes: Vec<_> = entries.iter().filter(|e| {
                        let t = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        t == "compaction" || t == "leaf_pointer" || t == "turn_summary"
                    }).cloned().collect();
                    let current_leaf = ion::session_tree::resolve_current_leaf(&entries);
                    let compaction_points: Vec<_> = entries.iter()
                        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("compaction"))
                        .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                        .collect();
                    output_response(&id, "get_tree", &serde_json::json!({
                        "nodes": struct_nodes,
                        "currentLeaf": current_leaf,
                        "compactionPoints": compaction_points,
                        "mode": "structure"
                    }));
                }
            }
            "get_modified_files" => {
                let from_turn = params.get("fromTurn").and_then(|v| v.as_str()).map(|s| s.to_string());
                let to_turn = params.get("toTurn").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let Some(ref store) = snapshot_store {
                    let all_snaps = store.load_all_tool_snapshots();
                    // 按 turnId 范围过滤（from/to 是 turnId 字符串，按 timestamp 比较）
                    let snaps: Vec<_> = if from_turn.is_some() || to_turn.is_some() {
                        let from_ts = from_turn.as_ref()
                            .and_then(|ft| all_snaps.iter().find(|s| &s.turn_id == ft))
                            .map(|s| s.timestamp.clone());
                        let to_ts = to_turn.as_ref()
                            .and_then(|tt| all_snaps.iter().find(|s| &s.turn_id == tt))
                            .map(|s| s.timestamp.clone());
                        all_snaps.into_iter().filter(|s| {
                            let after_from = from_ts.as_ref().map_or(true, |ft| &s.timestamp >= ft);
                            let before_to = to_ts.as_ref().map_or(true, |tt| &s.timestamp <= tt);
                            after_from && before_to
                        }).collect()
                    } else {
                        all_snaps
                    };
                    let files: Vec<serde_json::Value> = snaps.iter().map(|s| {
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
                    }).collect();
                    let added = files.iter().filter(|f| f["status"] == "added").count();
                    let modified = files.iter().filter(|f| f["status"] == "modified").count();
                    let deleted = files.iter().filter(|f| f["status"] == "deleted").count();
                    output_response(&id, "get_modified_files", &serde_json::json!({
                        "files": files,
                        "summary": { "added": added, "modified": modified, "deleted": deleted },
                    }));
                } else {
                    output_response(&id, "get_modified_files", &serde_json::json!({
                        "error": "file-snapshot extension not enabled",
                    }));
                }
            }
            "get_queue" => {
                let steering: Vec<serde_json::Value> = agent.steering_queue_snapshot().iter()
                    .filter_map(|m| serde_json::to_value(m).ok()).collect();
                let follow_up: Vec<serde_json::Value> = agent.follow_up_queue_snapshot().iter()
                    .filter_map(|m| serde_json::to_value(m).ok()).collect();
                output_response(&id, "get_queue", &serde_json::json!({
                    "steering": steering, "followUp": follow_up,
                    "steeringCount": agent.steering_queue_len(),
                    "followUpCount": agent.follow_up_queue_len(),
                }));
            },
            "clear_queue" => {
                agent.clear_queues();
                output_response(&id, "clear_queue", &serde_json::json!({
                    "cleared": true,
                    "steeringCleared": agent.steering_queue_len(),
                    "followUpCleared": agent.follow_up_queue_len(),
                }));
            },
            "get_context_usage" => {
                let msgs = agent.messages();
                let input_tokens: u64 = msgs.iter()
                    .filter_map(|m| match m { Message::Assistant(a) => Some(a.usage.input), _ => None })
                    .sum();
                let output_tokens: u64 = msgs.iter()
                    .filter_map(|m| match m { Message::Assistant(a) => Some(a.usage.output), _ => None })
                    .sum();
                let ctx_chars: usize = msgs.iter()
                    .map(|m| match m {
                        Message::User(u) => u.content.iter().map(|b| match b {
                            ion::agent::messages::ContentBlock::Text(t) => t.text.len(),
                            _ => 0,
                        }).sum::<usize>(),
                        Message::Assistant(a) => a.content.iter().map(|b| match b {
                            ion::agent::messages::AssistantContentBlock::Text(t) => t.text.len(),
                            _ => 0,
                        }).sum::<usize>(),
                        _ => 0,
                    }).sum();
                let context_window = agent.model().context_window;
                let estimated_tokens = (ctx_chars / 4) as u64;
                output_response(&id, "get_context_usage", &serde_json::json!({
                    "messageCount": msgs.len(),
                    "estimatedTokens": estimated_tokens,
                    "contextWindow": context_window,
                    "usagePercent": if context_window > 0 { (estimated_tokens * 100 / context_window as u64) as u32 } else { 0 },
                    "totalInputTokens": input_tokens,
                    "totalOutputTokens": output_tokens,
                    "autoCompaction": agent.auto_compact_enabled(),
                }));
            },
            "get_flags" => {
                let ext_name = params.get("extension").and_then(|v| v.as_str()).unwrap_or("");
                if ext_name.is_empty() {
                    // 无参数 → 返回所有扩展的 flag
                    let names = agent.extensions().names();
                    let mut all_flags = serde_json::Map::new();
                    for name in &names {
                        all_flags.insert(name.clone(), agent.extensions().get_flags(name));
                    }
                    output_response(&id, "get_flags", &serde_json::Value::Object(all_flags));
                } else {
                    let flags = agent.extensions().get_flags(ext_name);
                    output_response(&id, "get_flags", &serde_json::json!({
                        "extension": ext_name,
                        "flags": flags,
                    }));
                }
            }

            "get_active_tools" => {
                let tools: Vec<String> = agent.list_tool_names();
                output_response(&id, "get_active_tools", &serde_json::json!({"tools": tools, "count": tools.len()}));
            },
            "set_active_tools" => {
                let tools_arr: Vec<String> = params.get("tools")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                agent.restrict_tools(tools_arr.clone());
                output_response(&id, "set_active_tools", &serde_json::json!({
                    "activeTools": tools_arr, "count": tools_arr.len(),
                }));
            },
            "get_full_messages" => {
                let msgs: Vec<serde_json::Value> = agent.messages().iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                output_response(&id, "get_full_messages", &serde_json::json!({
                    "messages": msgs, "count": msgs.len(),
                    "note": "Includes thinking blocks and all content types",
                }));
            },
            "set_auto_compaction" => {
                let enabled = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                agent.set_auto_compact(enabled);
                output_response(&id, "set_auto_compaction", &serde_json::json!({
                    "autoCompaction": enabled,
                }));
            },
            "set_cwd" => {
                let cwd = params.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                if cwd.is_empty() {
                    output_response(&id, "set_cwd", &serde_json::json!({"error": "missing 'cwd' parameter"}));
                } else {
                    // 验证路径存在
                    if std::path::Path::new(cwd).exists() {
                        agent.set_session_cwd(Some(cwd.to_string()));
                        output_response(&id, "set_cwd", &serde_json::json!({
                            "cwd": cwd,
                            "success": true,
                        }));
                    } else {
                        output_response(&id, "set_cwd", &serde_json::json!({
                            "error": format!("path '{}' does not exist", cwd),
                        }));
                    }
                }
            }
            "cycle_model" => {
                let current_id = agent.model().id.clone();
                let current_provider = agent.model().provider.clone();
                let mut models = model_reg.models_by_provider(&current_provider);
                models.sort_by(|a, b| a.id.cmp(&b.id));
                if models.len() < 2 {
                    output_response(&id, "cycle_model", &serde_json::json!({
                        "modelId": current_id, "provider": current_provider,
                        "note": "Only one model available, no cycle",
                    }));
                } else {
                    let next_idx = models.iter().position(|m| m.id == current_id)
                        .map(|i| (i + 1) % models.len())
                        .unwrap_or(0);
                    let next_model = models[next_idx].clone();
                    let next_id = next_model.id.clone();
                    agent.set_model(next_model);
                    model_id = next_id.clone();
                    ion::session_index::SessionIndex::set_model(&sid, &provider, &next_id);
                    output_response(&id, "cycle_model", &serde_json::json!({
                        "modelId": next_id, "provider": current_provider,
                        "previousModel": current_id,
                    }));
                }
            },
            "cycle_thinking_level" => {
                let levels = ["off", "minimal", "low", "medium", "high", "xhigh"];
                let current = agent.thinking_level().unwrap_or("off").to_string();
                let next = levels.iter().position(|&l| l == current)
                    .map(|i| levels[(i + 1) % levels.len()])
                    .unwrap_or("medium");
                agent.set_thinking_level(Some(next.to_string()));
                ion::session_index::SessionIndex::set_thinking_level(&sid, next);
                output_response(&id, "cycle_thinking_level", &serde_json::json!({
                    "thinkingLevel": next, "previousLevel": current,
                }));
            },
            "compact" => {
                let before_msgs = agent.messages().len();
                let before_tokens = ion::agent::compact::total_tokens(agent.messages());
                match agent.compact_now().await {
                    Ok(result) => {
                        let after_tokens = ion::agent::compact::total_tokens(agent.messages());
                        output_response(&id, "compact", &serde_json::json!({
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
                        }));
                    }
                    Err(e) => {
                        output_response(&id, "compact", &serde_json::json!({
                            "compacted": false,
                            "error": e.to_string(),
                            "beforeMessages": before_msgs,
                            "beforeTokens": before_tokens,
                        }));
                    }
                }
            }
            "new_session" => output_response(&id, "new_session", &serde_json::json!({"sessionId":sid})),
            "export_html" => output_response(&id, "export_html", &serde_json::json!({"path":""})),
            "switch_session" => output_response(&id, "switch_session", &serde_json::Value::Null),
            "fork" => output_response(&id, "fork", &serde_json::json!({"sessionId":sid})),
            "navigate_tree" => {
                // 返回树的可导航线性结构（id/parentId/role/content 截断/leaf 标记）
                let entries: Vec<serde_json::Value> =
                    ion::message_retrieval::load_entries_cached(&worker_cwd);
                let current_leaf = ion::session_tree::resolve_current_leaf(&entries);

                let nodes: Vec<_> = entries.iter().filter_map(|e| {
                    let etype = e.get("type").and_then(|v| v.as_str())?;
                    let id = e.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let parent_id = e.get("parentId").and_then(|v| v.as_str()).unwrap_or("");
                    let is_on_leaf_path = current_leaf.as_ref().map(|leaf| {
                        // 简单判断：id 在 leaf path 里
                        ion::session_tree::get_branch_path(&entries, leaf)
                            .iter()
                            .any(|pe| pe.get("id").and_then(|v| v.as_str()) == Some(id))
                    }).unwrap_or(false);

                    let role = e.get("message")
                        .and_then(|m| m.get("role"))
                        .and_then(|r| r.as_str())
                        .unwrap_or("");

                    // content 截断到 50 字
                    let content = e.get("message")
                        .and_then(|m| m.get("content"))
                        .and_then(|c| c.as_str())
                        .unwrap_or("");
                    let brief = if content.len() > 50 { format!("{}...", &content[..50]) } else { content.to_string() };

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
                }).collect();

                output_response(&id, "navigate_tree", &serde_json::json!({
                    "nodes": nodes,
                    "currentLeaf": current_leaf,
                    "totalNodes": nodes.len(),
                }));
            }
            "delete_entries" => {
                // 软删除：从 self.messages 移除 + 落 DeletionEntry 到 JSONL
                let target_ids: Vec<String> = params.get("targetIds")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let reason = params.get("reason").and_then(|v| v.as_str());
                let before = agent.messages().len();

                if target_ids.is_empty() {
                    output_response(&id, "delete_entries", &serde_json::json!({
                        "deleted": 0, "before": before, "after": before, "error": "no targetIds"
                    }));
                    continue;
                }

                // 从 JSONL 构建消息 entry id → 数组索引的映射
                let entries = ion::message_retrieval::load_entries_cached(&worker_cwd);

                // 尝试精确索引映射（compaction 前的快速路径）
                let indices = resolve_target_indices(
                    &entries,
                    agent.messages(),
                    &target_ids,
                );

                if indices.is_empty() {
                    output_response(&id, "delete_entries", &serde_json::json!({
                        "deleted": 0, "before": before, "after": before,
                        "error": "no matching entries found (possibly after compaction)"
                    }));
                    continue;
                }

                if indices.is_empty() {
                    output_response(&id, "delete_entries", &serde_json::json!({
                        "deleted": 0, "before": before, "after": before,
                        "error": "no matching entries found"
                    }));
                    continue;
                }

                // 执行删除
                agent.mark_deleted(&indices, &target_ids).await;
                // 落 DeletionEntry
                ion::session_jsonl::append_deletion(&worker_cwd, &target_ids, reason);
                // 失效缓存（下次 load_entries_cached 会重新读盘）
                ion::message_retrieval::invalidate_cache(&worker_cwd);

                output_response(&id, "delete_entries", &serde_json::json!({
                    "deleted": indices.len(), "before": before, "after": agent.messages().len()
                }));
            }
            "summarize_entries" => {
                // 软压缩：把一批消息替换成 BranchSummary + 落 SegmentSummaryEntry
                let target_ids: Vec<String> = params.get("targetIds")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let summary_text = params.get("summary").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let before = agent.messages().len();

                if target_ids.is_empty() {
                    output_response(&id, "summarize_entries", &serde_json::json!({
                        "summarized": 0, "before": before, "after": before, "error": "no targetIds"
                    }));
                    continue;
                }

                // 从 JSONL 构建索引映射（支持 compaction 后的降级匹配）
                let entries = ion::message_retrieval::load_entries_cached(&worker_cwd);

                let indices = resolve_target_indices(
                    &entries,
                    agent.messages(),
                    &target_ids,
                );

                if indices.is_empty() {
                    output_response(&id, "summarize_entries", &serde_json::json!({
                        "summarized": 0, "before": before, "after": before,
                        "error": "no matching entries found (possibly after compaction)"
                    }));
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
                ion::session_jsonl::append_segment_summary(&worker_cwd, &target_ids, &summary);
                ion::message_retrieval::invalidate_cache(&worker_cwd);

                output_response(&id, "summarize_entries", &serde_json::json!({
                    "summarized": indices.len(),
                    "before": before,
                    "after": agent.messages().len(),
                    "summary": summary,
                }));
            }
            "restore_entries" => {
                // 恢复软删除/折叠：追加 restoration entry + 从 JSONL 重载消息
                let target_ids: Vec<String> = params.get("targetIds")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                let before = agent.messages().len();

                if target_ids.is_empty() {
                    output_response(&id, "restore_entries", &serde_json::json!({
                        "restored": 0, "before": before, "after": before, "error": "no targetIds"
                    }));
                    continue;
                }

                // 1. 从 Agent 状态移除
                agent.restore_entries(&target_ids);
                // 2. 追加 restoration entry 到 JSONL（拉取层会撤销过滤）
                ion::session_jsonl::append_restoration(&worker_cwd, &target_ids);
                // 3. 失效缓存
                ion::message_retrieval::invalidate_cache(&worker_cwd);
                // 4. 从 JSONL 重载消息到 Agent（恢复被删/折叠的原始消息）
                let new_count = agent.reload_messages_from_session(&worker_cwd);

                output_response(&id, "restore_entries", &serde_json::json!({
                    "restored": target_ids.len(),
                    "before": before,
                    "after": new_count,
                }));
            }
            "clone" => output_response(&id, "clone", &serde_json::json!({"sessionId":sid})),
            "switch_agent" => {
                // 真实切换 agent：加载定义 + 应用系统提示词/工具限制
                let target = params.get("agentName").or_else(|| params.get("name"))
                    .and_then(|v| v.as_str()).unwrap_or("");
                if let Some(agent_cfg) = ion::agent_config::find_agent(target) {
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
                    output_response(&id, "switch_agent", &serde_json::json!({
                        "agent": agent_cfg.name,
                        "description": agent_cfg.description,
                        "color": agent_cfg.color,
                    }));
                } else {
                    output_response(&id, "switch_agent", &serde_json::json!({
                        "error": format!("agent '{}' not found", target)
                    }));
                }
            },
            "set_permission_mode" => {
                let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("");
                if mode.is_empty() {
                    output_response(&id, "set_permission_mode", &serde_json::json!({
                        "error": "missing 'mode' parameter (open/blacklist/whitelist)",
                    }));
                } else {
                    match agent.runtime().set_guard_mode(mode) {
                        Ok(()) => output_response(&id, "set_permission_mode", &serde_json::json!({
                            "mode": mode,
                            "success": true,
                        })),
                        Err(e) => output_response(&id, "set_permission_mode", &serde_json::json!({
                            "error": e,
                        })),
                    }
                }
            }
            // ── Stored-Decision（权限记忆）顶层 RPC ──
            // 对齐 docs/design/PERMISSION_STORE.md §2.4，转发给 permission 扩展。
            // 用户选"always allow"后持久化决策，下次自动放行，不用反复确认。
            "permission_store_decision" => {
                match agent.extension_rpc("permission", "store_decision", params).await {
                    Ok(output) => output_response(&id, "permission_store_decision", &serde_json::json!({
                        "success": true, "data": output,
                    })),
                    Err(e) => output(&serde_json::json!({
                        "type": "response", "id": id, "success": false,
                        "error": format!("permission_store_decision: {e}"),
                    })),
                }
            }
            "permission_list_stored" => {
                match agent.extension_rpc("permission", "list_stored", serde_json::Value::Null).await {
                    Ok(output) => output_response(&id, "permission_list_stored", &serde_json::json!({
                        "success": true, "data": output,
                    })),
                    Err(e) => output(&serde_json::json!({
                        "type": "response", "id": id, "success": false,
                        "error": format!("permission_list_stored: {e}"),
                    })),
                }
            }
            "permission_remove_stored" => {
                match agent.extension_rpc("permission", "remove_stored", params).await {
                    Ok(output) => output_response(&id, "permission_remove_stored", &serde_json::json!({
                        "success": true, "data": output,
                    })),
                    Err(e) => output(&serde_json::json!({
                        "type": "response", "id": id, "success": false,
                        "error": format!("permission_remove_stored: {e}"),
                    })),
                }
            }
            "permission_clear_stored" => {
                match agent.extension_rpc("permission", "clear_stored", serde_json::Value::Null).await {
                    Ok(output) => output_response(&id, "permission_clear_stored", &serde_json::json!({
                        "success": true, "data": output,
                    })),
                    Err(e) => output(&serde_json::json!({
                        "type": "response", "id": id, "success": false,
                        "error": format!("permission_clear_stored: {e}"),
                    })),
                }
            }
            "set_auto_retry" => {
                let enabled = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                let max_retries = params.get("max_retries").and_then(|v| v.as_u64()).map(|v| v as u32);
                if enabled {
                    let max = max_retries.unwrap_or(3);
                    agent.set_max_retries(max);
                    output_response(&id, "set_auto_retry", &serde_json::json!({
                        "enabled": true,
                        "max_retries": max,
                    }));
                } else {
                    agent.set_max_retries(0);
                    output_response(&id, "set_auto_retry", &serde_json::json!({
                        "enabled": false,
                        "max_retries": 0,
                    }));
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
                            let output = if stderr.is_empty() { stdout.clone() }
                                else { format!("{stdout}\n{stderr}") };
                            output_response(&id, "bash", &serde_json::json!({
                                "output": output,
                                "stdout": stdout,
                                "stderr": stderr,
                                "exitCode": exit_code,
                            }));
                        }
                        Err(e) => {
                            output_response(&id, "bash", &serde_json::json!({
                                "output": format!("bash error: {e}"),
                                "exitCode": -1,
                            }));
                        }
                    }
                }
            }
            "set_steering_mode" => output_response(&id, "set_steering_mode", &serde_json::Value::Null),
            "extension_rpc" => {
                // 调插件私有 RPC 方法（给 CLI/外部调试用）。
                // 用于：ion rpc --session <id> --method extension_rpc
                //   --params '{"method":"ping","args":{}}'
                //   --params '{"extension":"bash","method":"list"}'
                let extension_name = params.get("extension").and_then(|v| v.as_str()).unwrap_or("");
                let rpc_method = params.get("method").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let rpc_args = params.get("args").cloned().unwrap_or_default();
                match agent.extension_rpc(extension_name, &rpc_method, rpc_args).await {
                    Ok(output) => output_response(&id, "extension_rpc", &serde_json::json!({
                        "method": rpc_method, "output": output,
                    })),
                    Err(e) => output(&serde_json::json!({
                        "type": "response", "id": id, "success": false,
                        "error": format!("extension_rpc {rpc_method}: {e}"),
                    })),
                }
            }
            "call_tool" => {
                // Directly call an LLM-registered tool by name (bypass LLM).
                // 用于 CLI 测试工具如 bash_run/bash_kill/bash_send。
                // --params '{"tool":"bash_run","args":{"command":"echo hi","description":"test"}}'
                let tool_name = params.get("tool").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let tool_args = params.get("args").cloned().unwrap_or_default();
                if tool_name.is_empty() {
                    output_error_response(&id, "call_tool", "missing 'tool'");
                    continue;
                }
                match agent.call_tool(&tool_name, tool_args).await {
                    Ok(result) => output_response(&id, "call_tool", &serde_json::json!({
                        "tool": tool_name, "output": result,
                    })),
                    Err(e) => output(&serde_json::json!({
                        "type": "response", "id": id, "success": false,
                        "error": format!("call_tool {tool_name}: {e}"),
                    })),
                }
            }
            "set_follow_up_mode" => output_response(&id, "set_follow_up_mode", &serde_json::Value::Null),
            "reload" => {
                // Generic reload: reload all loaded extensions
                let extensions = wasm_ext_registry.list();
                if extensions.is_empty() {
                    output_response(&id, "reload", &serde_json::json!({"message": "no extensions loaded"}));
                } else {
                    let mut reloaded: Vec<String> = Vec::new();
                    let mut errors: Vec<String> = Vec::new();
                    for p in &extensions {
                        match wasm_ext_registry.reload(&p.path) {
                            Ok(tool_defs) => {
                                // Remove old tools, add new ones
                                for old_name in &p.tools { agent.remove_tool(old_name); }
                                let canonical_str = p.path.clone();
                                let ext_name = ion::wasm_extension::ext_name_from_path(&canonical_str);
                                for td in &tool_defs {
                                    agent.register_tool(Box::new(ToolAdapter {
                                        name: td.name.clone(),
                                        description: td.description.clone(),
                                        parameters: td.parameters.clone(),
                                        extension_path: canonical_str.clone(),
                                        ext_name: ext_name.clone(),
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
                    output_response(&id, "reload", &serde_json::json!({"reloaded": reloaded, "errors": errors}));
                }
            }
            "abort_retry" => {
                // 中断当前重试循环（复用 abort 机制）
                agent.stop();
                output_response(&id, "abort_retry", &serde_json::json!({
                    "aborted": true,
                    "message": "retry loop interrupted",
                }));
            }
            "set_tier_models" => {
                let tier = params.get("tier").and_then(|v| v.as_str()).unwrap_or("");
                let model = params.get("model").and_then(|v| v.as_str()).unwrap_or("");
                if tier.is_empty() || model.is_empty() {
                    output_response(&id, "set_tier_models", &serde_json::json!({"error": "missing 'tier' or 'model'"}));
                } else {
                    let mut cfg = ion::config::IonConfig::load();
                    let old = cfg.tier_models.get(tier).cloned();
                    cfg.tier_models.insert(tier.to_string(), model.to_string());
                    match cfg.save() {
                        Ok(()) => output_response(&id, "set_tier_models", &serde_json::json!({
                            "tier": tier, "oldModel": old, "newModel": model, "saved": true,
                        })),
                        Err(e) => output_response(&id, "set_tier_models", &serde_json::json!({"error": format!("save failed: {}", e)})),
                    }
                }
            }
            "get_tree_with_leaf" => {
                // get_tree + 带 pathToLeaf（root → current leaf 的路径）
                let entries: Vec<serde_json::Value> =
                    ion::message_retrieval::load_entries_cached(&worker_cwd);
                let current_leaf = ion::session_tree::resolve_current_leaf(&entries);
                let tree_nodes = ion::session_tree::get_tree(&entries);

                // 计算 root → leaf 路径
                let path_to_leaf = if let Some(ref leaf_id) = current_leaf {
                    ion::session_tree::get_branch_path(&entries, leaf_id)
                        .iter()
                        .filter_map(|e| e.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
                        .collect::<Vec<_>>()
                } else {
                    vec![]
                };

                let branches = ion::session_tree::named_branches(&entries);
                output_response(&id, "get_tree_with_leaf", &serde_json::json!({
                    "tree": tree_nodes,
                    "currentLeaf": current_leaf,
                    "pathToLeaf": path_to_leaf,
                    "branches": branches.iter().map(|(name, target)| {
                        serde_json::json!({"name": name, "target": target})
                    }).collect::<Vec<_>>(),
                }));
            }
            "get_file_diff" => {
                let file_path = params.get("filePath").and_then(|v| v.as_str()).unwrap_or("");
                let from_turn = params.get("fromTurn").and_then(|v| v.as_str()).map(|s| s.to_string());
                let to_turn = params.get("toTurn").and_then(|v| v.as_str()).map(|s| s.to_string());
                if file_path.is_empty() {
                    output_response(&id, "get_file_diff", &serde_json::json!({"error": "missing 'filePath'"}));
                } else if let Some(ref store) = snapshot_store {
                    let history = store.load_file_history(file_path);
                    // 按 turnId 字符串过滤（timestamp 比较）
                    let from_ts = from_turn.as_ref()
                        .and_then(|ft| history.iter().find(|s| &s.turn_id == ft))
                        .map(|s| s.timestamp.clone());
                    let to_ts = to_turn.as_ref()
                        .and_then(|tt| history.iter().find(|s| &s.turn_id == tt))
                        .map(|s| s.timestamp.clone());
                    let relevant: Vec<_> = history.iter()
                        .filter(|s| {
                            from_ts.as_ref().map_or(true, |ft| &s.timestamp >= ft)
                                && to_ts.as_ref().map_or(true, |tt| &s.timestamp <= tt)
                        })
                        .collect();
                    if relevant.is_empty() {
                        output_response(&id, "get_file_diff", &serde_json::json!({
                            "path": file_path, "diff": null, "hasContent": false,
                        }));
                    } else {
                        let first = relevant.first().unwrap();
                        let last = relevant.last().unwrap();
                        let before_content = first.before_hash.as_ref()
                            .and_then(|h| store.objects().read_object_text(h));
                        let after_content = last.after_hash.as_ref()
                            .and_then(|h| store.objects().read_object_text(h));

                        // GC 降级：hash 存在但 object 不可读
                        let before_missing = first.before_hash.is_some() && before_content.is_none();
                        let after_missing = last.after_hash.is_some() && after_content.is_none();
                        if before_missing || after_missing {
                            output_response(&id, "get_file_diff", &serde_json::json!({
                                "path": file_path,
                                "diffAvailable": false,
                                "error": { "code": "SNAPSHOT_OBJECT_MISSING" },
                                "beforeHash": first.before_hash,
                                "afterHash": last.after_hash,
                            }));
                            return;
                        }

                        let diff = match (&before_content, &after_content) {
                            (Some(b), Some(a)) => ion::file_snapshot::unified_diff(b, a, file_path),
                            (None, Some(a)) => format!("+++ new file\n{}", a),
                            (Some(b), None) => format!("--- deleted file\n{}", b),
                            _ => String::new(),
                        };
                        let (added, removed) = ion::file_snapshot::count_diff(&diff);
                        output_response(&id, "get_file_diff", &serde_json::json!({
                            "path": file_path,
                            "diff": diff,
                            "diffAvailable": true,
                            "beforeHash": first.before_hash,
                            "afterHash": last.after_hash,
                            "hasContent": before_content.is_some() || after_content.is_some(),
                            "added": added,
                            "removed": removed,
                        }));
                    }
                } else {
                    output_response(&id, "get_file_diff", &serde_json::json!({"error": "file-snapshot not enabled"}));
                }
            }
            "get_batch_diffs" => {
                let from_turn = params.get("fromTurn").and_then(|v| v.as_str()).map(|s| s.to_string());
                let to_turn = params.get("toTurn").and_then(|v| v.as_str()).map(|s| s.to_string());
                if let Some(ref store) = snapshot_store {
                    let all_snaps = store.load_all_tool_snapshots();
                    let snaps: Vec<_> = if from_turn.is_some() || to_turn.is_some() {
                        let from_ts = from_turn.as_ref()
                            .and_then(|ft| all_snaps.iter().find(|s| &s.turn_id == ft))
                            .map(|s| s.timestamp.clone());
                        let to_ts = to_turn.as_ref()
                            .and_then(|tt| all_snaps.iter().find(|s| &s.turn_id == tt))
                            .map(|s| s.timestamp.clone());
                        all_snaps.into_iter().filter(|s| {
                            let after_from = from_ts.as_ref().map_or(true, |ft| &s.timestamp >= ft);
                            let before_to = to_ts.as_ref().map_or(true, |tt| &s.timestamp <= tt);
                            after_from && before_to
                        }).collect()
                    } else {
                        all_snaps
                    };
                    // 按 path 分组，取每个 path 的首尾
                    use std::collections::HashMap;
                    let mut grouped: HashMap<String, Vec<&ion::file_snapshot::ToolSnapshot>> = HashMap::new();
                    for s in &snaps {
                        grouped.entry(s.path.clone()).or_default().push(s);
                    }
                    let mut files = Vec::new();
                    let mut total_added = 0usize;
                    let mut total_removed = 0usize;
                    for (path, group) in &grouped {
                        let first = group.first().unwrap();
                        let last = group.last().unwrap();
                        let before_content = first.before_hash.as_ref()
                            .and_then(|h| store.objects().read_object_text(h));
                        let after_content = last.after_hash.as_ref()
                            .and_then(|h| store.objects().read_object_text(h));
                        let diff = match (&before_content, &after_content) {
                            (Some(b), Some(a)) => ion::file_snapshot::unified_diff(b, a, path),
                            (None, Some(a)) => format!("+++ new file\n{}", a),
                            (Some(b), None) => format!("--- deleted\n{}", b),
                            _ => String::new(),
                        };
                        let (added, removed) = ion::file_snapshot::count_diff(&diff);
                        total_added += added;
                        total_removed += removed;
                        files.push(serde_json::json!({
                            "path": path, "diff": diff, "added": added, "removed": removed,
                        }));
                    }
                    output_response(&id, "get_batch_diffs", &serde_json::json!({
                        "files": files,
                        "summary": { "files": grouped.len(), "added": total_added, "removed": total_removed },
                    }));
                } else {
                    output_response(&id, "get_batch_diffs", &serde_json::json!({"error": "file-snapshot not enabled"}));
                }
            }
            "get_file_history" => {
                let file_path = params.get("filePath").and_then(|v| v.as_str()).unwrap_or("");
                if file_path.is_empty() {
                    output_response(&id, "get_file_history", &serde_json::json!({"error": "missing 'filePath'"}));
                } else if let Some(ref store) = snapshot_store {
                    let history = store.load_file_history(file_path);
                    let entries: Vec<serde_json::Value> = history.iter().map(|s| {
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
                    }).collect();
                    output_response(&id, "get_file_history", &serde_json::json!({
                        "path": file_path,
                        "history": entries,
                        "count": entries.len(),
                    }));
                } else {
                    output_response(&id, "get_file_history", &serde_json::json!({"error": "file-snapshot not enabled"}));
                }
            }
            "restore_files" => {
                let to_turn = params.get("toTurn").and_then(|v| v.as_str()).unwrap_or("");
                if to_turn.is_empty() {
                    output_response(&id, "restore_files", &serde_json::json!({"error": "missing 'toTurn' (turnId)"}));
                } else if let Some(ref store) = snapshot_store {
                    let result = ion::file_snapshot::restore::restore_code_to_turn(store, to_turn);
                    output_response(&id, "restore_files", &serde_json::json!({
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
                    }));
                } else {
                    output_response(&id, "restore_files", &serde_json::json!({"error": "file-snapshot not enabled"}));
                }
            }
            // ── 审批 RPC（review_pending / approve / reject / approve_all / reject_all / approvals）──
            "review_pending" => {
                if let Some(ref mgr) = approval_mgr {
                    let pending = mgr.compute_pending();
                    let added = pending.iter().filter(|p| p.status == "added").count();
                    let modified = pending.iter().filter(|p| p.status == "modified").count();
                    let deleted = pending.iter().filter(|p| p.status == "deleted").count();
                    let pending_json: Vec<_> = pending.iter().map(|p| serde_json::json!({
                        "path": p.path,
                        "status": p.status,
                        "diffStat": p.diff_stat,
                        "oldContent": p.old_content,
                        "newContent": p.new_content,
                    })).collect();
                    output_response(&id, "review_pending", &serde_json::json!({
                        "pending": pending_json,
                        "summary": {
                            "total": pending.len(),
                            "added": added,
                            "modified": modified,
                            "deleted": deleted,
                        },
                    }));
                } else {
                    output_response(&id, "review_pending", &serde_json::json!({"error": "approval not enabled (requires file-snapshot)"}));
                }
            }
            "review_approve" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    output_response(&id, "review_approve", &serde_json::json!({"error": "missing 'path'"}));
                } else if let Some(ref mgr) = approval_mgr {
                    match mgr.approve(path) {
                        Ok(appr) => output_response(&id, "review_approve", &serde_json::json!({
                            "path": appr.path, "status": "approved",
                            "approvedTreeHash": appr.approved_tree_hash,
                        })),
                        Err(e) => output_response(&id, "review_approve", &serde_json::json!({"error": e})),
                    }
                } else {
                    output_response(&id, "review_approve", &serde_json::json!({"error": "approval not enabled"}));
                }
            }
            "review_reject" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    output_response(&id, "review_reject", &serde_json::json!({"error": "missing 'path'"}));
                } else if let Some(ref mgr) = approval_mgr {
                    match mgr.reject(path) {
                        Ok(rf) => {
                            // deny 消息注入 session.jsonl（下一轮 agent 可见）
                            let deny_msg = format!(
                                "📋 审批拒绝：文件 {} 已回滚（action: {}）。用户不认可这次改动，请重新处理。",
                                path, rf.action
                            );
                            let entry = serde_json::json!({
                                "type": "message",
                                "id": format!("approval_deny_{}", std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)),
                                "parentId": null,
                                "timestamp": ion::session_jsonl::timestamp_iso(),
                                "message": {
                                    "role": "user",
                                    "content": [{"type": "text", "text": deny_msg}],
                                },
                                "customType": "approval_deny",
                            });
                            ion::session_jsonl::append_raw_entry(&worker_cwd, &entry);

                            output_response(&id, "review_reject", &serde_json::json!({
                                "path": rf.path, "status": "rejected",
                                "action": rf.action, "rolledBack": true,
                                "denyMessageInjected": true,
                            }));
                        }
                        Err(e) => output_response(&id, "review_reject", &serde_json::json!({"error": e})),
                    }
                } else {
                    output_response(&id, "review_reject", &serde_json::json!({"error": "approval not enabled"}));
                }
            }
            "review_approve_all" => {
                if let Some(ref mgr) = approval_mgr {
                    let results = mgr.approve_all();
                    let ok_count = results.iter().filter(|r| r.is_ok()).count();
                    let err_count = results.len() - ok_count;
                    output_response(&id, "review_approve_all", &serde_json::json!({
                        "approved": ok_count, "errors": err_count, "total": results.len(),
                    }));
                } else {
                    output_response(&id, "review_approve_all", &serde_json::json!({"error": "approval not enabled"}));
                }
            }
            "review_reject_all" => {
                if let Some(ref mgr) = approval_mgr {
                    let results = mgr.reject_all();
                    let ok_count = results.iter().filter(|r| r.is_ok()).count();
                    let err_count = results.len() - ok_count;
                    output_response(&id, "review_reject_all", &serde_json::json!({
                        "rejected": ok_count, "errors": err_count, "total": results.len(),
                    }));
                } else {
                    output_response(&id, "review_reject_all", &serde_json::json!({"error": "approval not enabled"}));
                }
            }
            "review_approvals" => {
                if let Some(ref mgr) = approval_mgr {
                    let filter = params.get("status").and_then(|v| v.as_str());
                    let status_filter = filter.and_then(|s| match s {
                        "pending" => Some(ion::file_snapshot::ApprovalStatus::Pending),
                        "approved" => Some(ion::file_snapshot::ApprovalStatus::Approved),
                        "rejected" => Some(ion::file_snapshot::ApprovalStatus::Rejected),
                        _ => None,
                    });
                    let list = mgr.approvals_list(status_filter.as_ref());
                    output_response(&id, "review_approvals", &serde_json::json!({
                        "approvals": list.iter().map(|a| serde_json::json!({
                            "path": a.path,
                            "status": serde_json::to_string(&a.status).unwrap_or_default().trim_matches('"'),
                            "timestamp": a.timestamp,
                            "approvedTreeHash": a.approved_tree_hash,
                        })).collect::<Vec<_>>(),
                    }));
                } else {
                    output_response(&id, "review_approvals", &serde_json::json!({"error": "approval not enabled"}));
                }
            }
            "get_fork_messages" => {
                // 复用 retrieve_inputs（只返回 user 消息，用于 fork 选择）
                let entries: Vec<serde_json::Value> =
                    ion::message_retrieval::load_entries_cached(&worker_cwd);
                let params = ion::message_retrieval::RetrievalParams::default();
                let result = ion::message_retrieval::retrieve_inputs(&entries, &params);
                output_response(&id, "get_fork_messages", &serde_json::json!({
                    "inputs": result.inputs.iter().map(|i| serde_json::json!({
                        "entryId": i.entry_id,
                        "turnId": i.turn_id,
                        "text": i.text,
                    })).collect::<Vec<_>>(),
                    "count": result.inputs.len(),
                }));
            }
            "get_agents_files" => output_response(&id, "get_agents_files", &serde_json::json!([])),
            "get_latest_agent_change" => output_response(&id, "get_latest_agent_change", &serde_json::Value::Null),
            "get_agent_detail" => {
                // 真实实现：返回 agent 详情（含 system_prompt）
                let name = params.get("agentName").or_else(|| params.get("name"))
                    .and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    output_response(&id, "get_agent_detail", &serde_json::json!({"error":"missing agentName"}));
                } else {
                    match ion::agent_config::find_agent(name) {
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
                        },
                        None => {
                            output_response(&id, "get_agent_detail", &serde_json::json!({"error": format!("agent '{}' not found", name)}));
                        }
                    }
                }
            },
            "get_all_tools" => output_response(&id, "get_all_tools", &serde_json::json!([])),
            "get_flag_values" => output_response(&id, "get_flag_values", &serde_json::json!({})),
            "set_flag" => {
                let ext_name = params.get("extension").and_then(|v| v.as_str()).unwrap_or("");
                let flag_name = params.get("flag").and_then(|v| v.as_str()).unwrap_or("");
                let value = params.get("value").cloned().unwrap_or(serde_json::Value::Null);
                if ext_name.is_empty() || flag_name.is_empty() {
                    output_response(&id, "set_flag", &serde_json::json!({
                        "error": "missing 'extension' or 'flag' parameter",
                    }));
                } else {
                    agent.extensions().set_flag(ext_name, flag_name, value.clone());
                    output_response(&id, "set_flag", &serde_json::json!({
                        "extension": ext_name,
                        "flag": flag_name,
                        "value": value,
                        "set": true,
                    }));
                }
            }
            "get_mcp_servers" => {
                // 方案 C：转发给 host 查真实状态
                match manager_bridge.send_command("mcp_get_servers", serde_json::json!({})).await {
                    Ok(resp) => {
                        let servers = resp.get("data").cloned().unwrap_or(serde_json::Value::Array(vec![]));
                        output_response(&id, "get_mcp_servers", &servers);
                    }
                    Err(e) => output_error_response(&id, "get_mcp_servers", &format!("host proxy error: {e}")),
                }
            }
            "mcp_toggle_server" => {
                // 方案 C：转发给 host（host 的 McpManager 执行 toggle）
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
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
                match manager_bridge.send_command("mcp_toggle_server", serde_json::json!({
                    "name": name, "enabled": enabled
                })).await {
                    Ok(resp) => {
                        if resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                            output_response(&id, "mcp_toggle_server", resp.get("data").unwrap_or(&serde_json::Value::Null));
                        } else {
                            output_error_response(&id, "mcp_toggle_server",
                                resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown"));
                        }
                    }
                    Err(e) => output_error_response(&id, "mcp_toggle_server", &format!("proxy: {e}")),
                }
            }
            "mcp_restart_server" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if name.is_empty() {
                    output_error_response(&id, "mcp_restart_server", "missing 'name'");
                    continue;
                }
                match manager_bridge.send_command("mcp_restart_server", serde_json::json!({
                    "name": name
                })).await {
                    Ok(resp) => {
                        if resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                            output_response(&id, "mcp_restart_server", resp.get("data").unwrap_or(&serde_json::Value::Null));
                        } else {
                            output_error_response(&id, "mcp_restart_server",
                                resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown"));
                        }
                    }
                    Err(e) => output_error_response(&id, "mcp_restart_server", &format!("proxy: {e}")),
                }
            }
            "mcp_reload" => {
                // 方案 C：转发给 host 重新加载 MCP 配置
                match manager_bridge.send_command("mcp_reload", serde_json::json!({})).await {
                    Ok(resp) => {
                        if resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                            output_response(&id, "mcp_reload", resp.get("data").unwrap_or(&serde_json::Value::Null));
                        } else {
                            output_error_response(&id, "mcp_reload",
                                resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown"));
                        }
                    }
                    Err(e) => output_error_response(&id, "mcp_reload", &format!("proxy: {e}")),
                }
            }
            "mcp_read_resource" => {
                // 方案 C：转发给 host 读 MCP 资源
                let server = params.get("server").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if server.is_empty() || uri.is_empty() {
                    output_error_response(&id, "mcp_read_resource", "missing 'server' or 'uri'");
                    continue;
                }
                match manager_bridge.send_command("mcp_read_resource", serde_json::json!({
                    "server": server, "uri": uri
                })).await {
                    Ok(resp) => {
                        if resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
                            output_response(&id, "mcp_read_resource", resp.get("data").unwrap_or(&serde_json::Value::Null));
                        } else {
                            output_error_response(&id, "mcp_read_resource",
                                resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown"));
                        }
                    }
                    Err(e) => output_error_response(&id, "mcp_read_resource", &format!("proxy: {e}")),
                }
            }
            "continue" => {
                // Continue last session
                output_response(&id, "continue", &serde_json::Value::Null);
            }
            "follow_up" => {
                let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                agent.follow_up(ion::agent::messages::Message::User(
                    ion::agent::messages::UserMessage {
                        role: "user".into(),
                        content: vec![ion::agent::messages::ContentBlock::Text(
                            ion::agent::messages::TextContent { text, text_signature: None }
                        )],
                        timestamp: now_ms(),
                        source: ion_provider::types::MessageSource::FollowUp,
                    }
                ));
                output_response(&id, "follow_up", &serde_json::Value::Null);
            }
            "abort_bash" => {
                // 通过 process_map 找到 pid 并 kill
                let bid = params.get("bid").and_then(|v| v.as_str()).unwrap_or("");
                if bid.is_empty() {
                    output_response(&id, "abort_bash", &serde_json::json!({"error": "missing 'bid' parameter"}));
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
                        output_response(&id, "abort_bash", &serde_json::json!({
                            "bid": bid,
                            "pid": pid,
                            "command": cmd,
                            "signal": "SIGTERM",
                            "success": kill_result,
                        }));
                    } else {
                        output_response(&id, "abort_bash", &serde_json::json!({
                            "error": format!("process '{}' not found", bid),
                            "available": map.keys().cloned().collect::<Vec<_>>(),
                        }));
                    }
                } else {
                    output_response(&id, "abort_bash", &serde_json::json!({"error": "bash extension not enabled"}));
                }
            }
            "register_remote_tool" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let url = params.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let description = params.get("description").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let method = params.get("method").and_then(|v| v.as_str()).unwrap_or("POST").to_string();
                let parameters = params.get("parameters").cloned().unwrap_or(serde_json::json!({}));
                let headers: std::collections::HashMap<String, String> = params.get("headers")
                    .and_then(|v| v.as_object())
                    .map(|obj| obj.iter().filter_map(|(k, v)| {
                        v.as_str().map(|s| (k.clone(), s.to_string()))
                    }).collect())
                    .unwrap_or_default();
                if name.is_empty() || url.is_empty() {
                    output_error_response(&id, "register_remote_tool", "missing 'name' or 'url'");
                    continue;
                }
                agent.register_tool(Box::new(ion::agent::tool::RemoteTool {
                    name: name.clone(),
                    description,
                    parameters,
                    url,
                    method,
                    headers,
                }));
                output_response(&id, "register_remote_tool", &serde_json::json!({
                    "name": name,
                    "status": "registered"
                }));
            }
            "unregister_remote_tool" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if name.is_empty() {
                    output_error_response(&id, "unregister_remote_tool", "missing 'name'");
                    continue;
                }
                agent.remove_tool(&name);
                output_response(&id, "unregister_remote_tool", &serde_json::json!({
                    "name": name,
                    "status": "removed"
                }));
            }

            // ── WASM 插件热更新 ──
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
                        let ext_name = ion::wasm_extension::ext_name_from_path(&canonical_str);
                        for td in &tool_defs {
                            agent.register_tool(Box::new(ToolAdapter {
                                name: td.name.clone(),
                                description: td.description.clone(),
                                parameters: td.parameters.clone(),
                                extension_path: canonical_str.clone(),
                                ext_name: ext_name.clone(),
                                registry: wasm_ext_registry.clone(),
                            }));
                        }
                        let names: Vec<&str> = tool_defs.iter().map(|t| t.name.as_str()).collect();
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
                        output_response(&id, "extension_remove", &serde_json::json!({"removed_tools": tool_names}));
                    }
                    Err(e) => {
                        output_error_response(&id, "extension_remove", &e);
                    }
                }
            }

            "extension_list" => {
                let extensions = wasm_ext_registry.list();
                output_response(&id, "extension_list", &serde_json::json!({"extensions": extensions}));
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
                    for name in &old_tools { agent.remove_tool(name); }
                }

                // 重新加载
                let ext_name = ion::wasm_extension::ext_name_from_path(&canonical_str);
                match wasm_ext_registry.add(&canonical_str) {
                    Ok(tool_defs) => {
                        for td in &tool_defs {
                            agent.register_tool(Box::new(ToolAdapter {
                                name: td.name.clone(),
                                description: td.description.clone(),
                                parameters: td.parameters.clone(),
                                extension_path: canonical_str.clone(),
                                ext_name: ext_name.clone(),
                                registry: wasm_ext_registry.clone(),
                            }));
                        }
                        let names: Vec<&str> = tool_defs.iter().map(|t| t.name.as_str()).collect();
                        output_response(&id, "extension_reload", &serde_json::json!({"tools": names}));
                    }
                    Err(e) => {
                        output_error_response(&id, "extension_reload", &format!("reload failed: {e}"));
                    }
                }
            }

            "set_settings" => {
                let key = params.get("key").and_then(|v| v.as_str()).unwrap_or("");
                let value = params.get("value").cloned().unwrap_or(serde_json::Value::Null);
                if key.is_empty() {
                    output_response(&id, "set_settings", &serde_json::json!({"error": "missing 'key' parameter"}));
                } else {
                    let mut cfg = ion::config::IonConfig::load();
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
                            output_response(&id, "set_settings", &serde_json::json!({
                                "error": format!("unknown key '{}' (supported: default_provider, default_model, api_key, base_url)", key),
                            }));
                            return;
                        }
                    }
                    match cfg.save() {
                        Ok(()) => output_response(&id, "set_settings", &serde_json::json!({
                            "key": key,
                            "old_value": old_val,
                            "new_value": if key.contains("api_key") { serde_json::json!("***") } else { value },
                            "saved": true,
                        })),
                        Err(e) => output_response(&id, "set_settings", &serde_json::json!({
                            "error": format!("save failed: {}", e),
                        })),
                    }
                }
            }
            "rollback_preview" => output_response(&id, "rollback_preview", &serde_json::Value::Null),
            "copy_fork" => output_response(&id, "copy_fork", &serde_json::json!({"sessionId":sid})),
            "append_system_event" => {
                let ctype = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let label = params.get("label").and_then(|v| v.as_str()).unwrap_or("");
                let display = params.get("display").and_then(|v| v.as_bool()).unwrap_or(true);
                append_session_entry(&worker_cwd, &sid, "system_event", &serde_json::json!({
                    "customType": ctype,
                    "label": label,
                    "display": display,
                }));
                output_response(&id, "append_system_event", &serde_json::json!({"status":"appended"}));
            }
            "append_custom_message" => {
                let ctype = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
                let display = params.get("display").and_then(|v| v.as_bool()).unwrap_or(true);
                let details = params.get("details");
                append_session_entry(&worker_cwd, &sid, "custom_message", &serde_json::json!({
                    "customType": ctype,
                    "content": content,
                    "display": display,
                    "details": details,
                }));
                output_response(&id, "append_custom_message", &serde_json::json!({"status":"appended"}));
            }
            "append_custom_entry" => {
                let ctype = params.get("type").and_then(|v| v.as_str()).unwrap_or("");
                let data = params.get("data").cloned().unwrap_or_default();
                append_session_entry(&worker_cwd, &sid, "custom", &serde_json::json!({
                    "customType": ctype,
                    "data": data,
                }));
                output_response(&id, "append_custom_entry", &serde_json::json!({"status":"appended"}));
            }
            "send_custom_message" => {
                let ctype: String = params.get("type").and_then(|v| v.as_str()).map(String::from).unwrap_or_default();
                let content: String = params.get("content").and_then(|v| v.as_str()).map(String::from).unwrap_or_default();
                let deliver_as = params.get("deliverAs").and_then(|v| v.as_str()).unwrap_or("followUp");
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
                output_response(&id, "send_custom_message", &serde_json::json!({"status":"queued","queue":deliver_as}));
            }
            "append_model_change" => {
                let provider = params.get("provider").and_then(|v| v.as_str()).unwrap_or("");
                let model_id = params.get("modelId").and_then(|v| v.as_str()).unwrap_or("");
                append_session_entry(&worker_cwd, &sid, "model_change", &serde_json::json!({
                    "provider": provider,
                    "modelId": model_id,
                }));
                // 同步到 session index（O(1) 查询用）
                ion::session_index::SessionIndex::set_model(&sid, provider, model_id);
                output_response(&id, "append_model_change", &serde_json::json!({"status":"appended"}));
            }
            "append_thinking_level_change" => {
                let level = params.get("level").and_then(|v| v.as_str()).unwrap_or("");
                append_session_entry(&worker_cwd, &sid, "thinking_level_change", &serde_json::json!({
                    "level": level,
                }));
                ion::session_index::SessionIndex::set_thinking_level(&sid, level);
                output_response(&id, "append_thinking_level_change", &serde_json::json!({"status":"appended"}));
            }
            "append_agent_change" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                let config = params.get("config");
                let mut entry = serde_json::json!({"name": name});
                if let Some(c) = config { entry["config"] = c.clone(); }
                append_session_entry(&worker_cwd, &sid, "agent_change", &entry);
                ion::session_index::SessionIndex::set_agent(&sid, name);
                output_response(&id, "append_agent_change", &serde_json::json!({"status":"appended"}));
            }
            "append_session_name" => {
                let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
                append_session_entry(&worker_cwd, &sid, "session_info", &serde_json::json!({
                    "name": name,
                }));
                ion::session_index::SessionIndex::set_name(&sid, name);
                output_response(&id, "append_session_name", &serde_json::json!({"status":"appended","name":name}));
            }
            "append_label" => {
                let target_id = params.get("targetId").and_then(|v| v.as_str()).unwrap_or("");
                let label = params.get("label").and_then(|v| v.as_str()).unwrap_or("");
                append_session_entry(&worker_cwd, &sid, "label", &serde_json::json!({
                    "targetId": target_id,
                    "label": label,
                }));
                output_response(&id, "append_label", &serde_json::json!({"status":"appended"}));
            }
            "append_active_tools_change" => {
                let names: Vec<String> = params.get("activeToolNames")
                    .and_then(|v| v.as_array())
                    .map(|arr| arr.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect())
                    .unwrap_or_default();
                append_session_entry(&worker_cwd, &sid, "active_tools_change", &serde_json::json!({
                    "activeToolNames": names,
                }));
                ion::session_index::SessionIndex::set_active_tools(&sid, names);
                output_response(&id, "append_active_tools_change",
                    &serde_json::json!({"status":"appended"}));
            }
            "get_process_snapshot" => output_response(&id, "get_process_snapshot", &serde_json::json!({})),

            // ── bash_command：用户 !cmd 直发，结果作为 Message::BashExecution 入历史 ──
            // 不走 agent.run()，直接执行 + 入库 + 返回。
            // LLM 下次看到时 provider 自动把 role:bashExecution 转成 user text。
            "bash_command" => {
                let command: String = params.get("command").and_then(|v| v.as_str()).map(String::from).unwrap_or_default();
                let timeout_secs = params.get("timeout").and_then(|v| v.as_u64()).unwrap_or(30);
                let exclude_from_context = params.get("excludeFromContext").and_then(|v| v.as_bool());

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
                        output_response(&id, "bash_command", &serde_json::json!({
                            "status":"error",
                            "error": e,
                            "exitCode": null,
                            "output": null,
                        }));
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

                output_response(&id, "bash_command", &serde_json::json!({
                    "status":"ok",
                    "exitCode": exit_code,
                    "output": combined,
                    "truncated": truncated,
                }));
            }

            // ── Manager 回执（worker→manager 命令的结果）──
            // 按 _reply_to 查 pending map，触发对应 oneshot；不再 echo response。
            "manager_response" => {
                let reply_to = cmd.get("_reply_to").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if !reply_to.is_empty() {
                    manager_bridge.deliver_response(&reply_to, cmd).await;
                } else {
                    tracing::debug!("[{sid}] manager response without _reply_to: {params}");
                }
            }

            // ── 真正未知 ──
            _ => {
                // 兜底：检查是否有 _reply_to（Manager 写回 manager_response 可能不带 type）
                let reply_to = cmd.get("_reply_to")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !reply_to.is_empty() {
                    manager_bridge.deliver_response(&reply_to, cmd).await;
                } else {
                    output(&serde_json::json!({
                        "id": id,
                        "type": "response",
                        "command": method,
                        "success": false,
                        "error": format!("Unknown command: {method}")
                    }));
                }
            }
        }

        // Drain bash follow_up messages (background process completions)
        while let Ok(msg) = follow_up_rx.try_recv() {
            agent.follow_up(msg);
        }
    }

    // 退出前保存会话
    let msgs_json: Vec<serde_json::Value> = agent.messages().iter()
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
    ).await.map_err(|_| format!("bash timed out after {timeout_secs}s"))?
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
        } else { s }
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
        ion::paths::skills_dir(),
        ion::paths::project_skills_dir(config_root),
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
                    if !skill_md.is_file() { continue; }
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
    if let Some(rest) = trimmed.strip_prefix("---") {
        if let Some(end) = rest.find("\n---") {
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
impl ion::agent::tool::Tool for McpProxyTool {
    fn name(&self) -> &str { &self.full_name }
    fn description(&self) -> &str { &self.description }
    fn parameters(&self) -> serde_json::Value { self.parameters.clone() }

    async fn execute(
        &self,
        args: serde_json::Value,
        _rt: &dyn ion::runtime::Runtime,
    ) -> ion::agent::error::AgentResult<String> {
        let resp = self.bridge
            .send_command("mcp_call_tool", serde_json::json!({
                "server": self.server_name,
                "tool": self.tool_name,
                "args": args,
            }))
            .await
            .map_err(|e| ion::agent::error::AgentError::Tool(e))?;

        if resp.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
            Ok(resp
                .get("data")
                .and_then(|d| d.get("output"))
                .and_then(|o| o.as_str())
                .unwrap_or("")
                .to_string())
        } else {
            Err(ion::agent::error::AgentError::Tool(
                resp.get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("mcp proxy error")
                    .into(),
            ))
        }
    }
}

// ── StreamingExtension: 透传 text_delta + tool_execution 到 stdout ──
struct StreamingExtension;

#[async_trait::async_trait]
impl ion::agent::extension::Extension for StreamingExtension {
    fn name(&self) -> &str { "streaming" }

    async fn on_message_delta(&self, delta: &str, role: &str) -> ion::agent::error::AgentResult<()> {
        if role == "assistant" && !delta.is_empty() {
            output(&serde_json::json!({
                "type": "event",
                "event": {"type": "text_delta", "delta": delta}
            }));
        }
        Ok(())
    }


    /// agent_start 事件（对齐 pi）
    async fn on_agent_start(&self, _ctx: &ion::agent::agent_loop::AgentContext) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "agent_start",
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// agent_end 事件（对齐 pi — 含消息数）
    async fn on_agent_end(&self, ctx: &ion::agent::agent_loop::AgentContext) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "agent_end",
                "willRetry": false,
                "messages": ctx.message_count,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// message_start 事件（对齐 pi）
    async fn on_message_start(&self, role: &str, content: &str) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_start",
                "role": role,
                "content_length": content.len(),
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// message_end 事件（对齐 pi — 含 token 用量）
    async fn on_message_end(&self, role: &str, _full_content: &str, usage: &ion_provider::types::Usage) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "message_end",
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
    async fn on_tool_call_delta(&self, delta: &str, name: &str) -> ion::agent::error::AgentResult<()> {
        if !delta.is_empty() {
            if std::env::var("ION_STREAM_DEBUG").ok().as_deref() == Some("1") {
                eprintln!("[stream-debug] worker emit tool_call_delta name={name} len={}", delta.len());
            }
            output(&serde_json::json!({
                "type": "event",
                "event": {
                    "type": "tool_call_delta",
                    "delta": delta,
                    "toolName": name,
                    "timestamp": now_ms(),
                }
            }));
        }
        Ok(())
    }


    /// 自动重试开始事件：让前端显示 "重试中 (N/M)..."（对齐 pi auto_retry_start）
    async fn on_auto_retry_start(&self, attempt: u32, max_retries: u32) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "auto_retry_start",
                "attempt": attempt,
                "maxRetries": max_retries,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    /// 自动重试结束事件（success=false 表示所有重试用完仍失败）
    async fn on_auto_retry_end(&self, success: bool, attempt: u32) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "auto_retry_end",
                "success": success,
                "attempt": attempt,
                "timestamp": now_ms(),
            }
        }));
        Ok(())
    }

    async fn on_tool_execution_start(&self, ctx: &ion::agent::extension::ToolExecutionContext) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_start",
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
        messages: &[ion::agent::messages::Message],
    ) -> ion::agent::error::AgentResult<()> {
        let msgs_json: Vec<serde_json::Value> = messages.iter()
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

    async fn on_tool_execution_update(&self, ctx: &ion::agent::extension::ToolExecutionContext, partial: &str) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_update",
                "toolCallId": ctx.tool_call_id,
                "toolName": ctx.tool_name,
                "args": ctx.args,
                "partialResult": partial,
            }
        }));
        Ok(())
    }

    async fn on_tool_execution_end(&self, ctx: &ion::agent::extension::ToolExecutionContext) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
            "type": "event",
            "event": {
                "type": "tool_execution_end",
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

// ── FsProbeExtension: ctx.fs 探针扩展（给 CLI 测试用）──────────────────────
// 通过 extension_rpc 暴露 ctx.fs 的 read_file / list_dir / path_exists / glob，
// 以及 data_dirs（4 级数据目录），让 tests/extension_fs_ci.sh 能验证注入。
struct FsProbeExtension {
    fs: std::sync::Arc<ion::agent::extension::RuntimeFileSystem>,
    storage: ion::storage_context::StorageContext,
}

#[async_trait::async_trait]
impl ion::agent::extension::Extension for FsProbeExtension {
    fn name(&self) -> &str { "fs_probe" }

    async fn on_extension_rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> ion::agent::error::AgentResult<serde_json::Value> {
        use ion::agent::error::AgentError;
        use ion::agent::extension::FileSystemCapability;
        let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
        match method {
            "read_file" => {
                let content = self.fs.read_file(path).await
                    .map_err(AgentError::Tool)?;
                Ok(serde_json::json!({"content": content}))
            }
            "write_file" => {
                let content = params.get("content").and_then(|v| v.as_str()).unwrap_or("");
                self.fs.write_file(path, content).await
                    .map_err(AgentError::Tool)?;
                Ok(serde_json::json!({"written": true}))
            }
            "list_dir" => {
                let entries = self.fs.list_dir(path).await
                    .map_err(AgentError::Tool)?;
                let arr: Vec<serde_json::Value> = entries.iter().map(|e| serde_json::json!({
                    "name": e.name, "is_dir": e.is_dir, "size": e.size,
                })).collect();
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
                let matches = self.fs.glob(pattern).await
                    .map_err(AgentError::Tool)?;
                Ok(serde_json::json!({"matches": matches}))
            }
            "data_dirs" => {
                // 返回 4 级数据目录（验证 StorageContext 注入）
                let ext_name = params.get("ext_name").and_then(|v| v.as_str()).unwrap_or("fs_probe");
                let dirs = ion::agent::extension::ExtensionDataDirs {
                    global: self.storage.global_dir(ext_name),
                    project: self.storage.project_dir(ext_name),
                    cwd: self.storage.cwd_dir(ext_name),
                    session: self.storage.session_dir(ext_name),
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
impl ion::agent::extension::Extension for SessionProbeExtension {
    fn name(&self) -> &str { "session_probe" }

    async fn on_session_before_switch(
        &self,
        ctx: &ion::agent::extension::SessionSwitchContext,
    ) -> ion::agent::error::AgentResult<()> {
        self.emit_seen(&ctx.action, &ctx.target_leaf_id, &ctx.branch_name);
        if self.veto {
            Err(ion::agent::error::AgentError::Tool("vetoed by session_probe".into()))
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
}

fn output_error_response(id: &str, command: &str, error: &str) {
    output(&serde_json::json!({
        "id": id,
        "type": "response",
        "command": command,
        "success": false,
        "error": error,
    }));
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
impl ion::runtime::ManagerBridgeHandle for ManagerBridge {
    async fn send_command(
        &self,
        command: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        ManagerBridge::send_command(self, command, params).await
    }
}

#[async_trait::async_trait]
impl ion::worker_api::BridgeHandle for ManagerBridge {
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
    let path = SESSION_FILE_PATH.lock().unwrap()
        .clone()
        .unwrap_or_else(|| session_jsonl::session_path(cwd));
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // parentId：从文件现有 entries 解析当前 leaf（修 bug：原来硬编码 sid）
    let parent_id = (|| {
        let content = std::fs::read_to_string(&path).ok()?;
        let entries: Vec<serde_json::Value> = content.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        ion::session_tree::resolve_current_leaf(&entries)
    })().unwrap_or_else(|| sid.to_string());

    let mut line = serde_json::json!({
        "type": entry_type,
        "id": session_jsonl::generate_id(),
        "parentId": parent_id,
        "timestamp": session_jsonl::timestamp_iso(),
    });
    // 合并 entry_data 的字段到顶层（不嵌套在 data 里），对齐 pi JSONL 格式
    if let Some(obj) = entry_data.as_object() {
        if let Some(m) = line.as_object_mut() {
            for (k, v) in obj {
                m.insert(k.clone(), v.clone());
            }
        }
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let need_sep = f.metadata().ok().map(|m| m.len() > 0).unwrap_or(false);
        if need_sep {
            let _ = write!(f, "\n");
        }
        let _ = write!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
}

/// Ensure the fork sub-worker session header exists at the given path.
/// Unlike ensure_session_header (which writes to session.jsonl shared by cwd),
/// this writes to <session_id>.jsonl — a fork sub-worker's private session file.
fn ensure_fork_session_header(path: &std::path::Path, cwd: &str, sid: &str) {
    if path.exists() {
        return; // 已存在，不覆盖
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // ── 读 parent 关联信息（ION_FORK_CHILD 子 Worker 都会设这些 env）──
    let parent_session = std::env::var("ION_PARENT_SESSION").ok().filter(|s| !s.is_empty());
    let parent_worker = std::env::var("ION_PARENT_WORKER").ok().filter(|s| !s.is_empty());
    let spawn_relation = std::env::var("ION_SPAWN_RELATION").ok().filter(|s| !s.is_empty());
    let spawned_by = std::env::var("ION_SPAWNED_BY").ok().filter(|s| !s.is_empty());

    // 构造 spawnMeta（ION 扩展，详细血缘信息）
    let has_spawn_meta = parent_worker.is_some() || spawn_relation.is_some() || spawned_by.is_some();
    let mut header = serde_json::json!({
        "type": "session",
        "version": 3,
        "id": sid,
        "timestamp": session_jsonl::timestamp_iso(),
        "cwd": cwd,
        "parentSession": parent_session.clone(),
    });
    if has_spawn_meta {
        let mut spawn_meta = serde_json::json!({});
        if let Some(ref pw) = parent_worker { spawn_meta["parentWorker"] = serde_json::Value::String(pw.clone()); }
        if let Some(ref rel) = spawn_relation { spawn_meta["relation"] = serde_json::Value::String(rel.clone()); }
        if let Some(ref sb) = spawned_by { spawn_meta["spawnedBy"] = serde_json::Value::String(sb.clone()); }
        if let Some(ref ps) = parent_session { spawn_meta["parentSession"] = serde_json::Value::String(ps.clone()); }
        header["spawnMeta"] = spawn_meta;
    }

    let json = serde_json::to_string(&header).unwrap_or_default();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).write(true).truncate(true).open(path) {
        let _ = f.write_all(format!("{json}\n").as_bytes());
    }

    // fork 子 Worker：把 system_prompt（含 skill 内容）作为 custom entry 写到第二行
    // 这样 export HTML 时能恢复 systemPrompt 字段，让用户看到 skill 注入的内容
    if let Ok(sp) = std::env::var("ION_SYSTEM_PROMPT") {
        if !sp.is_empty() {
            let sp_entry = serde_json::json!({
                "type": "custom",
                "id": session_jsonl::generate_id(),
                "parentId": sid,
                "timestamp": session_jsonl::timestamp_iso(),
                "customType": "system_prompt",
                "data": { "systemPrompt": sp },
            });
            let sp_json = serde_json::to_string(&sp_entry).unwrap_or_default();
            if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(path) {
                let _ = f.write_all(format!("{sp_json}\n").as_bytes());
            }
        }
    }
}

/// Load messages from a fork sub-worker's session file.
fn load_fork_session_messages(path: &std::path::Path) -> Option<Vec<ion::agent::messages::Message>> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut messages = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        if let Ok(e) = serde_json::from_str::<serde_json::Value>(line) {
            if e.get("type").and_then(|v| v.as_str()) == Some("message") {
                if let Some(m) = e.get("message").and_then(|m| serde_json::from_value(m.clone()).ok()) {
                    messages.push(m);
                }
            }
        }
    }
    Some(messages)
}

fn save_worker_session(sid: &str, cwd: &str, msgs: &[serde_json::Value]) {
    // 优先用全局 SESSION_FILE_PATH（fork 子 Worker 设的 <session_id>.jsonl）
    // fallback 到 session_path(cwd)（主 Worker 的 session.jsonl）
    let path = SESSION_FILE_PATH.lock().unwrap()
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
            if line.is_empty() { continue; }
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
    let last_id = ion::session_tree::resolve_current_leaf(&all_entries)
        .unwrap_or_else(|| sid.to_string());

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
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            // 文件之前不存在，写 header
            if existing_lines.is_empty() {
                let _ = write!(f, "{header_line}\n");
            }
        }
        // 全新会话：leaf 就是 session id（resolve_current_leaf 此时返回 None，
        // 已被 unwrap_or_else(sid) 处理；saved_msg_count 本就是 0）
    }

    // 只 append 新增的 message（saved_msg_count 之后的部分）
    let new_msgs = if msgs.len() > saved_msg_count {
        eprintln!("[save-debug] msgs={} saved={} new={}", msgs.len(), saved_msg_count, msgs.len() - saved_msg_count);
        &msgs[saved_msg_count..]
    } else {
        &[][..]
    };

    if new_msgs.is_empty() {
        return;
    }

    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
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
    agent_messages: &[ion::agent::messages::Message],
    target_ids: &[String],
) -> Vec<usize> {
    let msg_entries: Vec<&serde_json::Value> = entries.iter()
        .filter(|e| e.get("type").and_then(|v| v.as_str()) == Some("message"))
        .collect();

    // 路径 1：精确索引映射（计数一致时）
    if msg_entries.len() == agent_messages.len() {
        let entry_ids: Vec<&str> = msg_entries.iter()
            .filter_map(|e| e.get("id").and_then(|v| v.as_str()))
            .collect();
        return target_ids.iter()
            .filter_map(|tid| entry_ids.iter().position(|eid| *eid == tid))
            .collect();
    }

    // 路径 2：内容匹配降级（compaction 后，计数不一致）
    // 用 target_id 从 JSONL 找到对应的 message 内容，
    // 然后在 agent 的内存消息里按序列化内容查找
    tracing::info!(
        "[soft-delete] entry/index mismatch (jsonl={} agent={}), falling back to content matching",
        msg_entries.len(), agent_messages.len()
    );

    // 构建 entry_id → 序列化 message 文本 的映射
    let id_to_content: std::collections::HashMap<&str, String> = msg_entries.iter()
        .filter_map(|e| {
            let id = e.get("id").and_then(|v| v.as_str())?;
            let msg_val = e.get("message")?;
            // 用 message 的 JSON 序列化做内容指纹
            Some((id, serde_json::to_string(msg_val).unwrap_or_default()))
        })
        .collect();

    // 构建 agent 内存消息的序列化文本列表
    let agent_contents: Vec<String> = agent_messages.iter()
        .map(|m| serde_json::to_string(m).unwrap_or_default())
        .collect();

    target_ids.iter()
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
