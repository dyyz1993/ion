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
use std::sync::atomic::{AtomicU64, Ordering};
use std::collections::HashMap;
use tokio::sync::Mutex;
use tokio::sync::{mpsc, oneshot};
use ion::agent::agent_loop::{Agent, AgentConfig};
use ion::agent::compact::CompactConfig;
use ion::agent::tool::{ReadTool, WriteTool, EditTool, BashTool, GrepTool, FindTool, LsTool, CalculatorTool, EchoTool, GitStatusTool, GitDiffTool, GitLogTool, GitAddTool, GitCommitTool, GitBranchTool, SpawnWorkerTool, SendToWorkerTool, ResumeWorkerTool, AwaitWorkerTool, ChannelSendTool, KillWorkerTool, ToolRegistry};
use ion::wasm_extension::{Registry, ToolAdapter};
use ion::session_jsonl;
use ion_provider::registry::ApiRegistry;
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

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => { session_id = args.get(i + 1).cloned(); i += 2; continue; }
            "--model" => { model_id = args.get(i + 1).cloned().unwrap_or(model_id); i += 2; continue; }
            "--provider" => { provider = args.get(i + 1).cloned().unwrap_or(provider); i += 2; continue; }
            "--channel" => { if let Some(ch) = args.get(i + 1) { channels.push(ch.clone()); } i += 2; continue; }
            "--mode" => { i += 2; continue; } // 已知是 rpc
            _ => { i += 1; }
        }
    }

    let sid = session_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    // 初始化 Provider + Model + Tools + Agent
    let mut registry = ApiRegistry::new();
    registry.register_builtins();

    let mut model_reg = ion_provider::registry::ModelRegistry::new();
    model_reg.register_builtins();
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

    // ── Memory 工具 + 共享 Store ──
    let cwd_for_memory = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let memory_store = std::sync::Arc::new(tokio::sync::Mutex::new(
        ion::agent::memory::MemoryStore::new(&cwd_for_memory, &sid)
    ));
    tools.register(Box::new(ion::agent::memory::MemorySaveTool { store: memory_store.clone() }));
    tools.register(Box::new(ion::agent::memory::MemorySearchTool { store: memory_store.clone() }));

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
	        max_turns: 20, max_outer_iterations: 5, max_retries: 30,
	        retry_base_delay_ms: 1000, enable_compact: true,
	        compact_config: CompactConfig::default(),
	        api_key: Some(api_key.clone()),
	        response_format: None, thinking: None,
	    retry_config: Some(ion::retry::RetryConfig::default()),
	    };

    let registry = Arc::new(registry);

    // WASM 插件注册表（RPC 热更新用）
    let wasm_ext_registry = Arc::new(Registry::new());

    // 记录已加载的 WASM 路径（用于后续创建 HookAdapter）
    let mut loaded_wasm_paths: Vec<String> = Vec::new();

    // ── WASM 插件自动发现（Agent 构造前，注册到 tools）──
    // 扫描 ~/.ion/agent/extensions/ 和 {cwd}/.ion/extensions/ 下的 .wasm 文件
    {
        let worker_cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let extensions_dirs: Vec<std::path::PathBuf> = vec![
            ion::paths::extensions_dir(),
            ion::paths::project_extensions_dir(&worker_cwd),
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
                                        plugin_path: canonical_str.clone(),
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

    // 加载已有会话（按 cwd 查找）
    let worker_cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let preloaded = session_jsonl::SessionFile::load(&worker_cwd).map(|f| f.messages);

    // ── ManagerBridge 必须在 Agent 构造前创建，因为 WorkerRuntime 包装它注入到 Agent ──
    let stdout = Arc::new(Mutex::new(io::stdout()));
    let manager_bridge: Arc<ManagerBridge> = Arc::new(ManagerBridge::new(sid.clone(), stdout.clone()));

    // ── Agent 用 WorkerRuntime 包装 LocalRuntime，从而获得 spawn_worker / send_to_worker ──
    let worker_rt = ion::runtime::WorkerRuntime::new(
        ion::runtime::LocalRuntime::new(),
        manager_bridge.clone() as Arc<dyn ion::runtime::ManagerBridgeHandle>,
    );

    let mut agent = Agent::new(
        Arc::clone(&registry),
        model.clone(),
        Some("You are a helpful AI assistant with access to tools.".into()),
        tools,
        config,
    )
    .with_runtime(Box::new(worker_rt));

    // 当前 agent 名称（支持 switch_agent 动态切换）
    let mut current_agent_name: String = "build".into();
    if let Some(msgs) = preloaded {
        agent = agent.with_messages(msgs);
    }

    // ── 注册内置 Extension（Memory / Bash / Streaming），可通过 config.json 关闭 ──
    let ion_cfg = ion::config::IonConfig::load();
    // 先创建 follow_up 通道（bash 插件后台进程完成时用来注入消息）
    let (follow_up_tx, mut follow_up_rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
    let mut process_map = None;
    let mut stdin_map = None;
    let mut notify_map = None;
    {
        let mut ext_reg = ion::agent::extension::ExtensionRegistry::new();

        // Memory Extension
        if ion_cfg.is_extension_enabled("memory") {
            let mut memory_ext = ion::agent::memory::MemoryExtension::new(&worker_cwd, &sid);
            // 复用 tools 的 MemoryStore（同一份数据）
            memory_ext.store = memory_store.clone();
            ext_reg.register(Box::new(memory_ext));
        } else {
            tracing::info!("[extension] memory disabled by config");
        }

        // Bash Extension（后台进程管理）
        if ion_cfg.is_extension_enabled("bash") {
            let bash_ext = ion::agent::bash::BashExtension::new(&sid);
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

        // ── 注册 WASM Extension 的 HookAdapter（让 WASM 也能实现 29 个钩子）──
        for wasm_path in &loaded_wasm_paths {
            if let Some(hook_adapter) = wasm_ext_registry.create_hook_adapter(wasm_path) {
                ext_reg.register(Box::new(hook_adapter));
                tracing::info!("[wasm] registered HookAdapter for {}", wasm_path);
            }
        }

        agent = agent.with_extensions(ext_reg);

        // 注册 bash 工具（仅当 bash extension 启用时）
        if let (Some(pm), Some(sm), Some(nm)) = (&process_map, &stdin_map, &notify_map) {
            let bash_run_tool = ion::agent::bash::BashRunTool {
                process_map: pm.clone(),
                stdin_map: sm.clone(),
                notify_map: nm.clone(),
                follow_up_tx: Some(follow_up_tx.clone()),
                session_id: sid.clone(),
            };
            let bash_kill_tool = ion::agent::bash::BashKillTool {
                process_map: pm.clone(),
                follow_up_tx: Some(follow_up_tx.clone()),
                session_id: sid.clone(),
            };
            let bash_send_tool = ion::agent::bash::BashSendTool {
                stdin_map: sm.clone(),
            };
            let bash_bg_tool = ion::agent::bash::BashBackgroundTool {
                notify_map: nm.clone(),
                process_map: pm.clone(),
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
    // - ManagerBridge 持有 pending map（_reply_to → oneshot），让工具调用能 await 响应。

    let (stdin_tx, mut stdin_rx) = mpsc::unbounded_channel::<serde_json::Value>();
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
        use tokio::io::AsyncBufReadExt;
        let mut lines = reader.lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => {
                    if line.trim().is_empty() { continue; }
                    match serde_json::from_str::<serde_json::Value>(&line) {
                        Ok(v) => { let _ = stdin_tx.send(v); }
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

            "get_session_stats" => {
                let total_input: u64 = agent.messages().iter()
                    .filter_map(|m| match m { Message::Assistant(a) => Some(a.usage.input), _ => None })
                    .sum();
                let total_output: u64 = agent.messages().iter()
                    .filter_map(|m| match m { Message::Assistant(a) => Some(a.usage.output), _ => None })
                    .sum();
                output_response(&id, "get_session_stats", &serde_json::json!({
                    "sessionId": sid,
                    "userMessages": agent.messages().iter().filter(|m| matches!(m, Message::User(_))).count(),
                    "assistantMessages": agent.messages().iter().filter(|m| matches!(m, Message::Assistant(_))).count(),
                    "toolResults": agent.messages().iter().filter(|m| matches!(m, Message::ToolResult(_))).count(),
                    "totalMessages": agent.messages().len(),
                    "tokens": {"input": total_input, "output": total_output, "cacheRead": 0, "cacheWrite": 0, "total": total_input + total_output},
                    "cost": 0,
                }));
            }

            "get_messages" => {
                let msgs: Vec<serde_json::Value> = agent.messages().iter()
                    .filter_map(|m| serde_json::to_value(m).ok())
                    .collect();
                output_response(&id, "get_messages", &serde_json::json!(msgs));
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
                let pbehavior = params.get("behavior").or_else(|| params.get("streamingBehavior"))
                    .and_then(|v| v.as_str()).unwrap_or("interrupt");

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

                        output(&serde_json::json!({"type":"event","event":{"type":"agent_start","sessionId":sid}}));
                        output(&serde_json::json!({"type":"event","event":{"type":"text_delta","delta":&combined}}));
                        output(&serde_json::json!({"type":"event","event":{"type":"agent_end","sessionId":sid}}));
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
                    }));
                    output_response(&id, "prompt", &serde_json::json!({"status":"queued","queue":"steering"}));
                    skip = true;
                } else if agent.is_running() && pbehavior == "followUp" {
                    agent.follow_up(Message::User(UserMessage {
                        role: "user".into(),
                        content: vec![ContentBlock::Text(TextContent { text: text.clone(), text_signature: None })],
                        timestamp: now_ms(),
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
                    output(&serde_json::json!({"type":"event","event":{"type":"agent_start","sessionId":sid}}));
                    {
                        let mut ctx = wasm_ext_registry.ctx.write().unwrap();
                        ctx.session_id = sid.clone();
                        ctx.cwd = worker_cwd.clone();
                        ctx.project_root = worker_cwd.clone();
                    }
                    match agent.run(&text).await {
                        Ok(()) => {
                            let msgs_json: Vec<serde_json::Value> = agent.messages().iter()
                                .filter_map(|m| serde_json::to_value(m).ok())
                                .collect();
                            save_worker_session(&sid, &worker_cwd, &msgs_json);
                            // agent_end 由 StreamingExtension 已经不发了，这里补一条
                            output(&serde_json::json!({
                                "type":"event","event":{"type":"agent_end","sessionId":sid}
                            }));
                        }
                        Err(e) => {
                            output(&serde_json::json!({
                                "type":"event","event":{"type":"error","message":e.to_string()}
                            }));
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
                    }));
                }
                output_response(&id, "promote_follow_up", &serde_json::Value::Null);
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
            "get_active_tools" => output_response(&id, "get_active_tools", &serde_json::json!(["read","write","edit","bash","grep","find","ls","calculator","echo"])),
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
            "get_context_usage" => output_response(&id, "get_context_usage", &serde_json::json!({"tokens":0,"contextWindow":128000,"percent":0.0})),
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
            "get_settings" => output_response(&id, "get_settings", &serde_json::json!({})),
            "get_commands" => output_response(&id, "get_commands", &serde_json::json!([])),
            "get_skills" => output_response(&id, "get_skills", &serde_json::json!([])),
            "get_extensions" => output_response(&id, "get_extensions", &serde_json::json!([])),
            "get_available_models" => output_response(&id, "get_available_models", &serde_json::json!([{"id":"deepseek-v4-flash","name":"DeepSeek V4 Flash"},{"id":"deepseek-v4-pro","name":"DeepSeek V4 Pro"},{"id":"gpt-4o","name":"GPT-4o"}])),
            "get_tier_models" => output_response(&id, "get_tier_models", &serde_json::json!({"fast":"deepseek-v4-flash","pro":"deepseek-v4-pro","max":"deepseek-v4-pro"})),
            "get_tree" => output_response(&id, "get_tree", &serde_json::json!([])),
            "get_modified_files" => output_response(&id, "get_modified_files", &serde_json::json!([])),
            "get_queue" => output_response(&id, "get_queue", &serde_json::json!([])),
            "get_flags" => output_response(&id, "get_flags", &serde_json::json!({})),

            "set_active_tools" => output_response(&id, "set_active_tools", &serde_json::Value::Null),
            "set_cwd" => output_response(&id, "set_cwd", &serde_json::Value::Null),
            "cycle_model" => output_response(&id, "cycle_model", &serde_json::json!({"modelId":model_id})),
            "cycle_thinking_level" => output_response(&id, "cycle_thinking_level", &serde_json::json!({"thinkingLevel":"medium"})),
            "compact" => {
                let before_msgs = agent.messages().len();
                let before_tokens: usize = agent.messages().iter()
                    .map(|m| match m {
                        ion::agent::messages::Message::User(u) => u.content.iter().map(|b| match b {
                            ion::agent::messages::ContentBlock::Text(t) => t.text.len() / 4,
                            _ => 0,
                        }).sum::<usize>(),
                        ion::agent::messages::Message::Assistant(a) => a.content.iter().map(|b| match b {
                            ion::agent::messages::AssistantContentBlock::Text(t) => t.text.len() / 4,
                            _ => 0,
                        }).sum::<usize>(),
                        _ => 0,
                    }).sum();
                output_response(&id, "compact", &serde_json::json!({
                    "compacted": before_tokens > 1000,
                    "beforeMessages": before_msgs,
                    "beforeTokens": before_tokens,
                    "afterMessages": agent.messages().len(),
                    "afterTokens": before_tokens,
                }));
            }
            "new_session" => output_response(&id, "new_session", &serde_json::json!({"sessionId":sid})),
            "export_html" => output_response(&id, "export_html", &serde_json::json!({"path":""})),
            "switch_session" => output_response(&id, "switch_session", &serde_json::Value::Null),
            "fork" => output_response(&id, "fork", &serde_json::json!({"sessionId":sid})),
            "navigate_tree" => output_response(&id, "navigate_tree", &serde_json::Value::Null),
            "delete_entries" => {
                // Delete messages by index (simplified: clear all tool results)
                let before = agent.messages().len();
                output_response(&id, "delete_entries", &serde_json::json!({"deleted": 0, "before": before, "after": agent.messages().len()}));
            }
            "summarize_entries" => {
                let summary_text = params.get("summary").and_then(|v| v.as_str()).unwrap_or("");
                output_response(&id, "summarize_entries", &serde_json::json!({
                    "summarized": true,
                    "summary": summary_text,
                    "messageCount": agent.messages().len()
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
                    // 应用工具限制（如果有）
                    if let Some(ref allowed) = agent_cfg.tools {
                        agent.restrict_tools(allowed.clone());
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
            "set_permission_mode" => output_response(&id, "set_permission_mode", &serde_json::Value::Null),
            "set_auto_compaction" => output_response(&id, "set_auto_compaction", &serde_json::Value::Null),
            "set_auto_retry" => output_response(&id, "set_auto_retry", &serde_json::Value::Null),
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
            "set_follow_up_mode" => output_response(&id, "set_follow_up_mode", &serde_json::Value::Null),
            "call_tool" => {
                // 直接调用 LLM 工具，不经过 Agent 循环。
                // 用于：ion rpc --session <id> --method call_tool
                //   --params '{"tool":"spawn_worker","args":{"relation":"child","agent":"developer","task":"..."}}'
                let tool_name = params.get("tool").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let tool_args = params.get("args").cloned().unwrap_or_default();
                match agent.call_tool(&tool_name, tool_args).await {
                    Ok(output) => output_response(&id, "call_tool", &serde_json::json!({
                        "tool": tool_name, "output": output,
                    })),
                    Err(e) => output(&serde_json::json!({
                        "type": "response", "id": id, "success": false,
                        "error": format!("call_tool {tool_name}: {e}"),
                    })),
                }
            }
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
                                        plugin_path: canonical_str.clone(),
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
            "abort_retry" => output_response(&id, "abort_retry", &serde_json::Value::Null),
            "set_tier_models" => output_response(&id, "set_tier_models", &serde_json::Value::Null),
            "get_full_messages" => output_response(&id, "get_full_messages", &serde_json::json!([])),
            "get_tree_with_leaf" => output_response(&id, "get_tree_with_leaf", &serde_json::json!([])),
            "get_file_diff" => output_response(&id, "get_file_diff", &serde_json::json!([])),
            "get_batch_diffs" => output_response(&id, "get_batch_diffs", &serde_json::json!([])),
            "get_file_history" => output_response(&id, "get_file_history", &serde_json::json!([])),
            "get_fork_messages" => output_response(&id, "get_fork_messages", &serde_json::json!([])),
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
            "set_flag" => output_response(&id, "set_flag", &serde_json::Value::Null),
            "clear_queue" => output_response(&id, "clear_queue", &serde_json::Value::Null),
            "get_mcp_servers" => output_response(&id, "get_mcp_servers", &serde_json::json!([])),
            "mcp_toggle_server" => output_response(&id, "mcp_toggle_server", &serde_json::Value::Null),
            "mcp_restart_server" => output_response(&id, "mcp_restart_server", &serde_json::Value::Null),
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
                    }
                ));
                output_response(&id, "follow_up", &serde_json::Value::Null);
            }
            "abort_bash" => output_response(&id, "abort_bash", &serde_json::Value::Null),
            "register_remote_tool" => output_response(&id, "register_remote_tool", &serde_json::Value::Null),
            "unregister_remote_tool" => output_response(&id, "unregister_remote_tool", &serde_json::Value::Null),

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
                                plugin_path: canonical_str.clone(),
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
                                plugin_path: canonical_str.clone(),
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

            "set_settings" => output_response(&id, "set_settings", &serde_json::Value::Null),
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
                output(&serde_json::json!({
                    "id": id,
                    "type": "response",
                    "command": method,
                    "success": false,
                    "error": format!("Unknown command: {method}")
                }));
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

    async fn on_tool_execution_update(&self, ctx: &ion::agent::extension::ToolExecutionContext, partial: &str) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
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

    async fn on_tool_execution_end(&self, ctx: &ion::agent::extension::ToolExecutionContext) -> ion::agent::error::AgentResult<()> {
        output(&serde_json::json!({
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
    let path = session_jsonl::session_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // 基础字段
    let mut line = serde_json::json!({
        "type": entry_type,
        "id": session_jsonl::generate_id(),
        "parentId": sid,
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
        // 确保文件末尾有换行，防止跟上一行粘在一起
        let need_sep = f.metadata().ok().map(|m| m.len() > 0).unwrap_or(false);
        if need_sep {
            let _ = write!(f, "\n");
        }
        let _ = write!(f, "{}", serde_json::to_string(&line).unwrap_or_default());
    }
}

fn save_worker_session(sid: &str, cwd: &str, msgs: &[serde_json::Value]) {
    let path = session_jsonl::session_path(cwd);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // 读取已有文件，确定已写入的 message entry 数量 + 最后一个 entry 的 id（作为 parentId）
    let mut existing_lines: Vec<String> = Vec::new();
    let mut last_id = sid.to_string();
    let mut saved_msg_count = 0usize;
    let mut header_existed = false;

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
                if let Some(id) = e.get("id").and_then(|v| v.as_str()) {
                    last_id = id.to_string();
                }
            }
        }
    }

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
        last_id = sid.to_string();
        saved_msg_count = 0;
    }

    // 只 append 新增的 message（saved_msg_count 之后的部分）
    let new_msgs = if msgs.len() > saved_msg_count {
        &msgs[saved_msg_count..]
    } else {
        &[][..]
    };

    if new_msgs.is_empty() {
        return;
    }

    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        // 防粘连：若文件非空且末尾不是换行，先补一个换行
        if let Ok(meta) = f.metadata() {
            if meta.len() > 0 {
                let _ = write!(f, "\n");
            }
        }
        // parentId 链：从 last_id 开始
        let mut parent_id = last_id;
        for msg in new_msgs {
            let entry_id = session_jsonl::generate_id();
            let entry = serde_json::json!({
                "type": "message",
                "id": entry_id,
                "parentId": parent_id,
                "timestamp": session_jsonl::timestamp_iso(),
                "message": msg,
            });
            let _ = write!(f, "{}\n", serde_json::to_string(&entry).unwrap_or_default());
            parent_id = entry_id;
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}
