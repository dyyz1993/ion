use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot, Mutex};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Worker Registry — Manager 内存状态
// ---------------------------------------------------------------------------

/// 单个 subscriber 的 event channel 容量。
///
/// 设为 4096 而非默认 256，是为了在 LLM 流式生成期间不丢事件：
/// DeepSeek/opencode 会在很短时间（< 100ms）内连续推送 30-50 个 tool_call_delta，
/// 如果 subscriber 消费稍慢（例如 host socket 转发被 lock 竞争阻塞），
/// 256 容量会被瞬间填满，try_send 开始丢事件（实际测试中观察到 26/28 被丢）。
///
/// 4096 足够容纳一个完整 LLM 流的瞬时 burst（即使每秒 1000 事件也能撑 4 秒），
/// 且每事件 ~1KB JSON，4096 个仅占 4MB 内存（per subscriber，可接受）。
const EVENT_CHANNEL_CAPACITY: usize = 4096;

pub struct WorkerRegistry {
    pub workers: HashMap<String, WorkerRecord>,
    pub channels: HashMap<String, Vec<String>>, // channel → worker_ids
    /// Path to the ion-worker binary. If None, auto-discover.
    pub worker_bin: Option<String>,
    /// Entry worker ID for recursive idle detection (set by --host mode)
    pub entry_worker_id: Option<String>,
    /// Global event subscribers (worker_created, worker_destroyed, project_changed)
    pub global_subscribers: Vec<mpsc::Sender<serde_json::Value>>,
    /// Overview snapshot subscribers (unbounded, no backpressure)
    pub overview_subscribers: Vec<mpsc::UnboundedSender<serde_json::Value>>,
    /// Singleton extensions registry（host 级单例，引用计数）
    pub singletons: std::collections::HashMap<String, SingletonEntry>,
    /// Channel for workers to send manager commands (create_worker, channel_send, etc.)
    pub manager_cmd_tx: mpsc::UnboundedSender<serde_json::Value>,
    pub manager_cmd_rx: mpsc::UnboundedReceiver<serde_json::Value>,
    /// Host 级 MCP 管理器（方案 C：所有 Worker 通过 bridge 代理调用）
    pub mcp_manager: Option<std::sync::Arc<crate::mcp::McpManager>>,
}

pub struct WorkerRecord {
    pub worker_id: String,
    pub session_id: String,
    pub project: String,
    pub project_path: String,
    pub model: String,
    pub agent: String,
    pub status: WorkerStatus,
    pub channels: Vec<String>,
    pub parent: Option<String>,
    pub children: Vec<String>,
    pub started_at: i64,
    pub last_heartbeat: i64,
    pub child_process: Option<Child>,
    pub stdin: Option<ChildStdin>,
    pub pending: HashMap<String, oneshot::Sender<serde_json::Value>>,
    pub event_subscribers: Vec<mpsc::Sender<serde_json::Value>>,
    pub parent_event_tx: Option<mpsc::Sender<serde_json::Value>>,
    pub ready_tx: Option<oneshot::Sender<serde_json::Value>>,
    /// Channel for stdout reader task to send lines back to Manager
    pub stdout_rx: Option<mpsc::UnboundedReceiver<serde_json::Value>>,
    /// Response channel: drain task sends responses here, send_to_worker reads from it
    pub response_rx: Option<mpsc::Receiver<(String, serde_json::Value)>>,
    /// Worktree info if this worker runs in an isolated git worktree
    pub worktree: Option<WorktreeInfo>,
    /// Latest output text deltas (max 5 items, each truncated to 60 chars)
    pub latest_output: VecDeque<String>,
    /// Short log snippet from latest text_delta
    pub log_short: Option<String>,
    /// Model size / context window info
    pub model_size: Option<String>,
    /// Worker 退出码（0 = 正常, 非0 = 异常退出, None = 尚未退出）
    pub exit_code: Option<i32>,
    /// 退出原因文本（stderr 最后几行摘要）
    pub exit_reason: Option<String>,
    /// stderr 日志文件路径
    pub stderr_path: Option<String>,
    /// 事件回放 ring buffer（缓存最近 N 条事件，subscribe --replay 时返回）
    pub event_history: std::collections::VecDeque<serde_json::Value>,
    /// ring buffer 容量（默认 200）
    pub event_history_cap: usize,
}

/// Worktree isolation config (specified at worker creation).
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct WorktreeConfig {
    /// Branch name the worker will work on (e.g. "feature-A")
    pub branch: String,
    /// Base branch to cut from (e.g. "main"). Defaults to HEAD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
}

/// Runtime worktree info (recorded after creation, used for cleanup).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorktreeInfo {
    /// Absolute path to the worktree directory
    pub path: String,
    /// Branch name the worker is on
    pub branch: String,
    /// Original project path (the main repo)
    pub source_repo: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatus {
    Idle,
    Busy,
    Paused,
    Dead,
    Stale,
}

impl std::fmt::Display for WorkerStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

/// 单例扩展条目（host 级，引用计数）
pub struct SingletonEntry {
    /// 唯一标识（singleton_key）
    pub key: String,
    /// 扩展实例（Arc 让 post_init 能 clone 出去在释放 lock 后调用）
    pub instance: std::sync::Arc<dyn crate::agent::extension::Extension>,
    /// 正在使用此单例的 Worker ID 集合（引用计数）
    pub users: std::collections::HashSet<String>,
    /// 是否已初始化（on_singleton_init 是否已调用）
    pub initialized: bool,
}

impl WorkerRegistry {
    pub fn new() -> Self {
        let (manager_cmd_tx, manager_cmd_rx) = mpsc::unbounded_channel();
        Self {
            workers: HashMap::new(),
            channels: HashMap::new(),
            worker_bin: None,
            entry_worker_id: None,
            global_subscribers: Vec::new(),
            overview_subscribers: Vec::new(),
            singletons: std::collections::HashMap::new(),
            manager_cmd_tx,
            manager_cmd_rx,
            mcp_manager: None,
        }
    }

    /// 设置 host 级 MCP 管理器（方案 C：host 持有连接，Worker 代理调用）
    pub fn set_mcp_manager(&mut self, mgr: std::sync::Arc<crate::mcp::McpManager>) {
        self.mcp_manager = Some(mgr);
    }

    /// Create a new WorkerRegistry with a pre-configured worker binary path.
    pub fn with_binary(bin: &str) -> Self {
        let (manager_cmd_tx, manager_cmd_rx) = mpsc::unbounded_channel();
        Self {
            workers: HashMap::new(),
            channels: HashMap::new(),
            worker_bin: Some(bin.to_string()),
            entry_worker_id: None,
            global_subscribers: Vec::new(),
            overview_subscribers: Vec::new(),
            singletons: std::collections::HashMap::new(),
            manager_cmd_tx,
            manager_cmd_rx,
            mcp_manager: None,
        }
    }

    /// Create a new Worker: spawn child process, register, start IO bridge
    pub async fn create_worker(
        &mut self,
        config: WorkerCreateConfig,
        registry_arc: &Arc<Mutex<WorkerRegistry>>,
    ) -> Result<WorkerInfo, String> {
        let worker_id = format!("wkr_{}", &Uuid::new_v4().to_string()[..8]);
        let session_id = config.session.clone().unwrap_or_else(|| {
            Uuid::new_v4().to_string()
        });

        let project_path = config.project_path.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });
        let project_name = std::path::Path::new(&project_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".into());

        // Worktree isolation
        let (worktree_path, worktree_info) = if let Some(ref wt_config) = config.worktree {
            // 如果请求了 worktree 隔离，先确保项目是 git 仓库
            let repo = std::path::Path::new(&project_path);
            if !repo.join(".git").exists() {
                tracing::info!("[worktree] project is not a git repo, initializing");
                let init = std::process::Command::new("git")
                    .args(["-C", &project_path, "init", "-b", "main"])
                    .output()
                    .map_err(|e| format!("git init failed: {e}"))?;
                if !init.status.success() {
                    let stderr = String::from_utf8_lossy(&init.stderr);
                    return Err(format!("git init failed: {stderr}"));
                }
                // 初始提交
                let _add = std::process::Command::new("git")
                    .args(["-C", &project_path, "add", "."])
                    .output()
                    .map_err(|e| format!("git add failed: {e}"))?;
                let _commit = std::process::Command::new("git")
                    .args(["-C", &project_path, "commit", "-m", "ion: initial commit"])
                    .output()
                    .map_err(|e| format!("git commit failed: {e}"))?;
                tracing::info!("[worktree] git init + initial commit done");
            }
            match create_worktree_advanced(&session_id, &project_path, wt_config) {
                Ok((path, branch)) => {
                    let info = WorktreeInfo {
                        path: path.clone(),
                        branch: branch.clone(),
                        source_repo: project_path.clone(),
                    };
                    tracing::info!("[worktree] {} → {} (branch: {})", session_id, path, branch);
                    (path, Some(info))
                }
                Err(e) => {
                    // 请求了 worktree 但创建失败 → 报错（不静默）
                    return Err(format!("worktree isolation requested but creation failed: {e}"));
                }
            }
        } else {
            (project_path.clone(), None)
        };

        // Spawn child process: ion-worker --mode rpc
        // Use configured binary path, or auto-discover
        let binary = if let Some(ref configured_bin) = self.worker_bin {
            configured_bin.clone()
        } else {
            // Find ion-worker binary next to current executable
            let exe_dir = std::env::current_exe()
                .map_err(|e| e.to_string())?
                .parent()
                .ok_or("no parent dir")?
                .to_path_buf();
            let worker_bin = exe_dir.join("ion-worker");

            if worker_bin.exists() {
                worker_bin.to_string_lossy().to_string()
            } else if let Some(parent) = exe_dir.parent() {
                // Test binaries are in deps/ subdirectory; try one level up
                let parent_bin = parent.join("ion-worker");
                if parent_bin.exists() {
                    parent_bin.to_string_lossy().to_string()
                } else {
                    // Fallback: look for ion-worker in PATH
                    which::which("ion-worker").map_err(|e| e.to_string())?
                        .to_string_lossy().to_string()
                }
            } else {
                which::which("ion-worker").map_err(|e| e.to_string())?
                    .to_string_lossy().to_string()
            }
        };

        // 从 config.json 读默认 model/provider（避免硬编码 deepseek-v4-flash/opencode）
        let cfg = crate::config::IonConfig::load();
        let default_model = cfg.default_model.clone().unwrap_or_else(|| "glm-4.7".to_string());
        let default_provider = cfg.default_provider.clone().unwrap_or_else(|| "zhipuai".to_string());

        let model = config.model.clone().unwrap_or(default_model);
        let provider = config.provider.clone().unwrap_or(default_provider);
        let agent_name = config.agent.clone().unwrap_or_default();

        let mut cmd_args = vec![
            "--mode".to_string(), "rpc".to_string(),
            "--session".to_string(), session_id.clone(),
            "--model".to_string(), model.clone(),
            "--provider".to_string(), provider.clone(),
        ];
        if !agent_name.is_empty() {
            cmd_args.push("--agent".to_string());
            cmd_args.push(agent_name.clone());
        }

        let mut child_cmd = tokio::process::Command::new(&binary);
        child_cmd
            .args(&cmd_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(&worktree_path);

        // 传 ION_PROJECT_ROOT 让子进程能找到项目级 .ion/config.json
        // （worktree 目录没有 .ion/，子进程需要知道原始项目路径来读 config）
        child_cmd.env("ION_PROJECT_ROOT", &project_path);

        // 子 Worker 跳过 MCP 连接（方案 A：防止多 Worker 抢同一个 stdio MCP server 死锁）
        // 只有 LLM 通过 spawn_worker 工具创建的子 worker 才跳过（config.skip_mcp=true）。
        // host 创建的第一个入口 worker 不跳过（它持有 MCP 连接）。
        // 子 Worker 通过 spawn_worker 工具创建时设 skip_mcp=Some("stdio")（方案 B）。
        if let Some(ref mode) = config.skip_mcp {
            if !mode.is_empty() {
                child_cmd.env("ION_SKIP_MCP", mode);
            }
        }

        // ── 补丁 1（HOOKS_AND_OUTLINE_SYNC）：工具白/黑名单 + max_turns 传给子进程 ──
        // 子 Worker 启动时读这些环境变量，应用到 ToolRegistry 过滤和 Agent 循环退出条件。
        // 这让扩展/hooks 的 agent handler 能 spawn "限定工具 + 限定步数"的子 Worker，
        // 是 ION 的 agent handler 比 pi 更强的关键（pi 的 agent handler 不传 tools，退化成单轮 LLM）。
        if let Some(ref tools) = config.allowed_tools {
            if !tools.is_empty() {
                child_cmd.env("ION_ALLOWED_TOOLS", tools.join(","));
            }
        }
        if let Some(ref tools) = config.disallowed_tools {
            if !tools.is_empty() {
                child_cmd.env("ION_DISALLOWED_TOOLS", tools.join(","));
            }
        }
        if let Some(turns) = config.max_turns {
            child_cmd.env("ION_MAX_TURNS", turns.to_string());
        }

        // 同步主进程的 runtime override 到子进程（如果主进程设了 --local/--remote）
        if let Ok(rt_override) = std::env::var("ION_RUNTIME_OVERRIDE") {
            child_cmd.env("ION_RUNTIME_OVERRIDE", &rt_override);
        }

        // 传递 FauxProvider 环境变量到子 Worker（让 host 模式下的子进程也用 faux）
        for var in &["ION_FAUX_SCRIPT", "ION_FAUX_REPLY"] {
            if let Ok(val) = std::env::var(var) {
                child_cmd.env(var, &val);
            }
        }

        // 传递录制相关环境变量到子 Worker（录制模式自动传播到子进程）
        for var in &["ION_RECORD", "ION_RECORD_OVERWRITE"] {
            if let Ok(val) = std::env::var(var) {
                child_cmd.env(var, &val);
            }
        }

        // ── 传递 parent 关联信息给子进程（让子 Worker session header 能记录血缘）──
        // 从 self.workers 查 config.creator（spawn 调用者的 worker_id），
        // 拿到 parent_session_id + parent_worker_id。
        // ion_worker 读这些 env，写到 session header 的 parentSession + spawnMeta 字段。
        // 入口 Worker（无 creator）不设这些 env → parentSession=null（兼容旧行为）。
        if let Some(ref creator_wid) = config.creator {
            // config.creator 可能是 worker_id 或 session_id（ManagerBridge 传的是 session_id）。
            // 先按 worker_id 查，找不到再按 session_id 查。
            let parent_record = self.workers.get(creator_wid)
                .or_else(|| self.workers.values().find(|w| &w.session_id == creator_wid));
            if let Some(parent_record) = parent_record {
                child_cmd.env("ION_PARENT_SESSION", &parent_record.session_id);
                child_cmd.env("ION_PARENT_WORKER", &parent_record.worker_id);
            }
        }
        // 关系类型（fork/system/peer/child）— 用 config.relation
        let relation_str = match config.relation {
            Some(WorkerRelation::System) => "system",
            Some(WorkerRelation::Peer) => "peer",
            _ => "child",  // fork 也是 Child 关系
        };
        child_cmd.env("ION_SPAWN_RELATION", relation_str);
        // skill fork 标记（spawnedBy）：system_prompt_override 非空 → skill_tool fork
        if config.system_prompt_override.is_some() {
            child_cmd.env("ION_SPAWNED_BY", "skill_fork");
        } else if config.relation == Some(WorkerRelation::System) {
            child_cmd.env("ION_SPAWNED_BY", "singleton_init");
        }

        // ── hooks 递归深度传递（防 agent handler 死循环）──
        // 从 WorkerCreateConfig.hook_depth 读（hooks agent handler spawn 时设）。
        // 设了就传给子进程 ION_HOOK_DEPTH，HookExtension 读到 >= 2 就跳过 agent handler。
        // 入口 Worker（普通 spawn_worker）不设 hook_depth → 子进程没有此变量 → depth=0 → agent handler 正常。
        if let Some(depth) = config.hook_depth {
            child_cmd.env("ION_HOOK_DEPTH", depth.to_string());
        }

        // ── system prompt 覆盖（skill fork 模式用）──
        // 把 skill 内容注入 system prompt，避免被 compaction 压缩。
        if let Some(ref sp) = config.system_prompt_override {
            child_cmd.env("ION_SYSTEM_PROMPT", sp);
        }

        // ── 独立 session 文件标记 ──
        // 以下两类 Worker 用 <session_id>.jsonl 而不是共享 session.jsonl：
        // 1. fork 子 Worker（system_prompt_override 非空）：skill fork spawn 的隔离子任务
        // 2. System 关系 Worker（memory-agent 等）：常驻后台 Agent，不应污染主会话
        // 主 Worker（入口 Worker）继续用 session.jsonl（兼容现有 export/list 行为）
        let is_independent_session = config.system_prompt_override.is_some()
            || config.relation == Some(WorkerRelation::System);
        if is_independent_session {
            child_cmd.env("ION_FORK_CHILD", "1");
        }

        let mut child = child_cmd
            .spawn()
            .map_err(|e| format!("failed to spawn worker: {e}"))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;

        // ── stderr 捕获（崩溃诊断用）──
        let stderr_path = std::env::temp_dir().join(format!("ion-worker-{}.stderr", worker_id));
        let _stderr_wid = worker_id.clone();
        let stderr_path_c = stderr_path.clone();
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            use std::io::Write;
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            if let Some(parent) = stderr_path_c.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true).append(true).open(&stderr_path_c)
                {
                    let _ = writeln!(f, "{}", line);
                }
            }
        });

        let parent_tx = config.parent.as_ref().and_then(|pid| {
            self.workers.get(pid).and_then(|w| {
                // Create a channel for parent to receive child events
                Some(w.parent_event_tx.clone()).flatten()
            })
        });

        let mut record = WorkerRecord {
            worker_id: worker_id.clone(),
            session_id: session_id.clone(),
            project: project_name.clone(),
            project_path: project_path.clone(),
            model: config.model.clone().unwrap_or_default(),
            agent: config.agent.clone().unwrap_or_default(),
            status: WorkerStatus::Idle,
            channels: config.channels.clone().unwrap_or_default(),
            parent: config.parent.clone(),
            children: Vec::new(),
            started_at: now_ms(),
            last_heartbeat: now_ms(),
            ready_tx: None,
            stdout_rx: None,
            response_rx: None,
            child_process: Some(child),
            stdin: Some(stdin),
            pending: HashMap::new(),
            event_subscribers: Vec::new(),
            parent_event_tx: parent_tx,
            worktree: worktree_info,
            latest_output: VecDeque::with_capacity(5),
            log_short: None,
            model_size: None,
            exit_code: None,
            exit_reason: None,
            stderr_path: None,
            event_history: std::collections::VecDeque::with_capacity(200),
            event_history_cap: 200,
        };

        // Register in parent's children list
        if let Some(ref parent_id) = config.parent {
            if let Some(parent) = self.workers.get_mut(parent_id) {
                parent.children.push(worker_id.clone());
            }
        }

        // Register in channels
        if let Some(ref chs) = config.channels {
            for ch in chs {
                self.channels
                    .entry(ch.clone())
                    .or_default()
                    .push(worker_id.clone());
            }
        }

        let project_name_clone = project_name.clone();
        let info = WorkerInfo {
            worker_id: worker_id.clone(),
            session_id: session_id.clone(),
            project: project_name_clone,
            status: WorkerStatus::Idle,
            model: record.model.clone(),
            agent: record.agent.clone(),
            channels: record.channels.clone(),
            parent: record.parent.clone(),
            children: Vec::new(),
        };

        // Create channels for stdout reader → send_command consumer
        // unbounded: reader task 永远不阻塞，确保 response 能及时到达 send_command
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let (_response_tx, response_rx) = mpsc::channel::<(String, serde_json::Value)>(64);
        
        // Set channels on the record BEFORE inserting
        record.stdout_rx = Some(stdout_rx);
        record.response_rx = Some(response_rx);

        self.workers.insert(worker_id.clone(), record);

        // 存 stderr 日志路径到 record
        if let Some(record) = self.workers.get_mut(&worker_id) {
            record.stderr_path = Some(stderr_path.to_string_lossy().to_string());
        }

        // ── 写 SessionIndex（让 ion --resume / --rollback 能通过 SID 找到 session 文件）──
        // serve 模式的 create_session → create_worker 之前不写 index，
        // 导致 CLI 层的 --resume/--rollback 找不到 session（依赖 index 查 cwd）。
        {
            use crate::session_index::{SessionIndex, SessionMeta};
            let now = now_ms();
            let mut idx = SessionIndex::load();
            idx.upsert(
                &session_id,
                SessionMeta {
                    name: Some(session_id.clone()),
                    first_name: Some(session_id.clone()),
                    project: Some(worktree_path.clone()),
                    project_name: Some(project_name.clone()),
                    worktree: config.worktree.is_some(),
                    branch: None,
                    model: model.clone(),
                    agent: agent_name.clone(),
                    provider: provider.clone(),
                    token_input: 0,
                    token_output: 0,
                    token_cache_read: 0,
                    token_cache_write: 0,
                    compress_count: 0,
                    message_count: 0,
                    turn_count: 0,
                    created_at: now,
                    updated_at: now,
                    error_count: 0,
                    last_thinking_level: None,
                    last_active_tools: None,
                    last_entry_id: None,
                    parent_session: None,
                    parent_type: None,
                },
            );
            idx.save();
            tracing::info!("[worker] SessionIndex 写入: {} → {}", session_id, worktree_path);
        }

        // ── singleton 引用计数：新 Worker 创建后通知所有单例 ──
        // System Worker（如 memory-agent）不触发 user_join（它本身就是单例的提供者，不是用户）。
        // 只有普通用户 Worker（Child/Peer）才 join。
        if config.relation != Some(WorkerRelation::System) {
            self.singleton_user_join(&worker_id).await;
        }

			        // Start stdout reader task (小助手 + 对讲机)
		        // 持续读 worker stdout：
		        // 1. event 消息 → 直接转发给 event_subscribers（subscribe session 流）
		        // 2. 所有消息 → stdout_tx（给 send_to_worker 等 RPC 消费）
		        let wid = worker_id.clone();
		        let cmd_tx = self.manager_cmd_tx.clone();
		        let sub_registry = Arc::clone(registry_arc);
		        let sub_wid = worker_id.clone();
		        tokio::spawn(async move {
		            let reader = BufReader::new(stdout);
		            let mut lines = reader.lines();
		            while let Ok(Some(line)) = lines.next_line().await {
		                if line.trim().is_empty() { continue; }
		                match serde_json::from_str::<serde_json::Value>(&line) {
		                    Ok(msg) => {
                    let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let msg_id = msg.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();

                        // Response with ID → match pending oneshot（不经过 stdout_tx）
                        if msg_type == "response" && !msg_id.is_empty() {
                            let mut reg = sub_registry.lock().await;
                            if let Some(record) = reg.workers.get_mut(&sub_wid) {
                                if let Some(tx) = record.pending.remove(&msg_id) {
                                    let _ = tx.send(msg.clone());
                                }
                            }
                        }

                        // 关键：event 消息转发给 event_subscribers（实时流）
                        if msg_type == "event" {
				            let ev_type = msg.get("event")
				                .and_then(|e| e.get("type"))
				                .and_then(|v| v.as_str())
				                .unwrap_or("");
				            let stream_debug = std::env::var("ION_STREAM_DEBUG").ok().as_deref() == Some("1");
				            if stream_debug && ev_type == "tool_call_delta" {
				                eprintln!("[stream-debug] host forward event type=tool_call_delta");
				            }
				            let mut reg = sub_registry.lock().await;
				            if let Some(record) = reg.workers.get_mut(&sub_wid) {
				                // 转发给实时订阅者
				                for sub in &record.event_subscribers {
				                    if let Err(_) = sub.try_send(msg.clone()) {
				                        if stream_debug {
				                            eprintln!("[stream-debug] host DROP event type=tool_call_delta (subscriber channel full)");
				                        }
				                    }
				                }
				                // 写入 ring buffer（用于 subscribe --replay）
				                record.event_history.push_back(msg.clone());
				                while record.event_history.len() > record.event_history_cap {
				                    record.event_history.pop_front();
				                }
				            }
			                            // 更新 latest_output / status
		                            if ev_type == "text_delta" {
		                                if let Some(delta) = msg.get("event")
		                                    .and_then(|e| e.get("delta"))
		                                    .and_then(|v| v.as_str())
		                                {
		                                    drop(reg);
		                                    let mut reg2 = sub_registry.lock().await;
		                                    if let Some(record) = reg2.workers.get_mut(&sub_wid) {
		                                        let truncated: String = delta.chars().take(60).collect();
		                                        record.latest_output.push_back(truncated.clone());
		                                        while record.latest_output.len() > 5 {
		                                            record.latest_output.pop_front();
		                                        }
		                                        record.log_short = Some(truncated);
		                                    }
		                                }
		                            } else if ev_type == "agent_end" || ev_type == "agent_stopped" {
		                                drop(reg);
		                                let mut reg2 = sub_registry.lock().await;
		                                if let Some(record) = reg2.workers.get_mut(&sub_wid) {
		                                    record.status = WorkerStatus::Idle;
		                                }
		                                drop(reg2);
		                                let rc = Arc::clone(&sub_registry);
		                                let _w = sub_wid.clone();
		                                tokio::spawn(async move {
		                                    let mut r = rc.lock().await;
		                                    r.broadcast_overview();
		                                });
		                            }
		                        }
		                        // 所有消息也转发到 stdout_tx（给 send_to_worker）
		                        match msg_type {
		                            "manager_command" => {
		                                let _ = cmd_tx.send(msg);
		                            }
		                            _ => {
		                        if stdout_tx.send(msg).is_err() {
		                                    break;
		                                }
		                            }
		                        }
	                    }
	                    Err(_) => {
	                        tracing::warn!("[{wid}] non-JSON: {line}");
	                    }
	                }
	            }
		            // Worker exited — clean up registry
			            tracing::warn!("[{wid}] stdout closed, cleaning up");
			            let mut reg = sub_registry.lock().await;
			            // 先读 exit code
			            let exit_code = reg.workers.get_mut(&sub_wid)
			                .and_then(|r| r.child_process.as_mut())
			                .and_then(|c| c.try_wait().ok().flatten())
			                .and_then(|s| s.code());
			            if let Some(record) = reg.workers.get_mut(&sub_wid) {
			                record.exit_code = exit_code;
			            }

			            // exit_code == 0/None → 正常退出，清理（同现状）
			            // exit_code != 0 → 崩溃，标 Dead + 保留 + 通知父
			            if exit_code == Some(0) || exit_code.is_none() {
			                // 正常退出或未知 → 清理
			                if let Some(mut record) = reg.workers.remove(&sub_wid) {
			                    if let Some(ref mut child) = record.child_process {
			                        let _ = child.start_kill();
			                    }
			                    for ch in &record.channels {
			                        if let Some(subs) = reg.channels.get_mut(ch) {
			                            subs.retain(|id| id != &sub_wid);
			                        }
			                    }
			                    if let Some(ref parent_id) = record.parent {
			                        if let Some(parent) = reg.workers.get_mut(parent_id) {
			                            parent.children.retain(|id| id != &sub_wid);
			                        }
			                    }
			                }
			            } else {
			                // 非零退出 → 崩溃！标 Dead，保留 record
			                let (crash_parent, crash_session, crash_reason, crash_channels) = {
			                    if let Some(record) = reg.workers.get_mut(&sub_wid) {
			                        record.status = WorkerStatus::Dead;
			                        // 读 stderr 日志最后几行作为 exit_reason
			                        if let Some(ref stderr_path) = record.stderr_path {
			                            if let Ok(content) = std::fs::read_to_string(stderr_path) {
			                                let tail: Vec<&str> = content.lines().rev().take(10).collect::<Vec<_>>();
			                                let tail: Vec<&str> = tail.into_iter().rev().collect();
			                                let snippet = tail.join("\n");
			                                if !snippet.is_empty() {
			                                    record.exit_reason = Some(format!("exit={}: {}", exit_code.unwrap_or(-1), snippet));
			                                } else {
			                                    record.exit_reason = Some(format!("exit={}", exit_code.unwrap_or(-1)));
			                                }
			                            } else {
			                                record.exit_reason = Some(format!("exit={}", exit_code.unwrap_or(-1)));
			                            }
			                        } else {
			                            record.exit_reason = Some(format!("exit={}", exit_code.unwrap_or(-1)));
			                        }
			                        (
			                            record.parent.clone(),
			                            record.session_id.clone(),
			                            record.exit_reason.clone(),
			                            record.channels.clone(),
			                        )
			                    } else { (None, String::new(), None, Vec::new()) }
			                }; // record mutable borrow ends here

			                // 推送 child_crashed 事件到 event_subscribers
			                let crash_event = serde_json::json!({
			                    "type": "child_crashed",
			                    "worker_id": sub_wid,
			                    "session_id": crash_session,
			                    "exit_code": exit_code,
			                    "exit_reason": crash_reason,
			                });
			                // 推给 event_subscribers（需要重新 get 记录）
			                if let Some(record) = reg.workers.get(&sub_wid) {
			                    for sub in &record.event_subscribers {
			                        let _ = sub.try_send(crash_event.clone());
			                    }
			                }
			                // 也通过 parent_event_tx 通知父
			                if let Some(ref parent_id) = crash_parent {
			                    if let Some(parent) = reg.workers.get(parent_id.as_str()) {
			                        if let Some(ref tx) = parent.parent_event_tx {
			                            let _ = tx.try_send(crash_event.clone());
			                        }
			                    }
			                    // 从父的 children 列表中移除
			                    if let Some(parent) = reg.workers.get_mut(parent_id.as_str()) {
			                        parent.children.retain(|id| id != &sub_wid);
			                    }
			                }
			                // 从 channels 移除
			                for ch in &crash_channels {
			                    if let Some(subs) = reg.channels.get_mut(ch.as_str()) {
			                        subs.retain(|id| id != &sub_wid);
			                    }
			                }
			            }
				            reg.broadcast_overview();
			            drop(reg); // 释放 lock，让 singleton_user_leave 能重新获取
			            // 通知单例扩展：这个 Worker 不再使用它们（引用计数-1）
			            let mut reg2 = sub_registry.lock().await;
			            reg2.singleton_user_leave(&sub_wid).await;
			        });

		        // ── Peer 模式：内核自动追加"汇报指令段"到 initial_prompt ──
		        // 这是内核职责，不依赖 .md 自己写汇报格式。
		        let mut effective_prompt = config.initial_prompt.clone();
		        let is_peer = matches!(config.relation, Some(WorkerRelation::Peer));
		        if is_peer {
		            let creator_id = config.creator.as_deref()
		                .or(config.report_to.as_deref())
		                .unwrap_or("(unknown)");
		            let ch = config.report_channel.as_deref().unwrap_or("main");
	            let report_seg = format!(
	                "\n\n---\n## 通信约定（内核自动注入，请严格遵守）\n\
	                 你是被 {creator} 创建的同级 Worker。\n\
	                 - 任务完成后必须输出（单独一行）：`CHANNEL_SEND {ch} DONE <简短摘要>`\n\
	                 - 需要帮助时输出：`CHANNEL_SEND {ch} HELP <问题描述>`\n\
	                 - 你的创建者 worker_id：{creator}\n\
	                 - 汇报频道：{ch}\n",
	                creator = creator_id,
	                ch = ch,
	            );
	            match &mut effective_prompt {
	                Some(p) => p.push_str(&report_seg),
	                None => effective_prompt = Some(report_seg),
	            }
	        }

	        // Emit worker_created + project_changed events
	        self.emit_global(serde_json::json!({
	            "type": "worker_created",
	            "worker_id": info.worker_id,
	            "session_id": info.session_id,
	            "project": info.project,
	            "parent": info.parent,
	        }));
        self.emit_global(serde_json::json!({
            "type": "project_changed",
            "project": info.project,
            "worker_id": info.worker_id,
            "change": "created",
        }));

        // ── 注入 initial_prompt（延迟到 spawn task，避免持锁等子进程 ready 导致死锁）──
        // 之前在持锁状态下 sleep(500ms) + send_command(prompt)，
        // 导致 reader task 无法拿锁转发事件 → 子进程 stdout buffer 满 → 死锁。
        // 现在改为：创建 worker record 后立即返回（释放锁），prompt 注入放到 spawn task。
        if let Some(prompt_text) = effective_prompt {
            let wid_for_prompt = worker_id.clone();
            let prompt_registry = Arc::clone(registry_arc);
            tokio::spawn(async move {
                // 等子进程 ready（不持锁，不阻塞 reader task）
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                // 短暂持锁写 stdin（fire-and-forget，不等响应）
                let mut reg = prompt_registry.lock().await;
                if let Err(e) = reg.send_command(&wid_for_prompt, "prompt", serde_json::json!({"text": prompt_text})).await {
                    tracing::warn!("[{wid_for_prompt}] failed to inject initial_prompt: {e}");
                }
            });
        }

        // Notify overview subscribers
        self.broadcast_overview();

        Ok(info)
    }

    pub fn list_workers(&self) -> Vec<WorkerInfo> {
        self.workers.values().map(|w| WorkerInfo {
            worker_id: w.worker_id.clone(),
            session_id: w.session_id.clone(),
            project: w.project.clone(),
            status: w.status.clone(),
            model: w.model.clone(),
            agent: w.agent.clone(),
            channels: w.channels.clone(),
            parent: w.parent.clone(),
            children: w.children.clone(),
        }).collect()
    }

    pub fn list_projects(&self) -> Vec<ProjectInfo> {
        let mut projects: HashMap<String, ProjectInfo> = HashMap::new();
        for w in self.workers.values() {
            let entry = projects.entry(w.project.clone()).or_insert_with(|| {
                ProjectInfo {
                    name: w.project.clone(),
                    path: w.project_path.clone(),
                    worker_ids: Vec::new(),
                }
            });
            entry.worker_ids.push(w.worker_id.clone());
        }
        projects.into_values().collect()
    }

    pub fn kill_worker(&mut self, worker_id: &str) -> Result<(), String> {
        if let Some(mut record) = self.workers.remove(worker_id) {
            // Capture info for event emission before consuming record
            let killed_worker_id = record.worker_id.clone();
            let killed_session = record.session_id.clone();
            let killed_project = record.project.clone();
            let killed_parent = record.parent.clone();
            // Capture worktree info for cleanup
            let wt_info = record.worktree.clone();

            if let Some(ref mut child) = record.child_process {
                let _ = child.start_kill();
            }
            // Remove from channels
            for ch in &record.channels {
                if let Some(subs) = self.channels.get_mut(ch) {
                    subs.retain(|id| id != worker_id);
                }
            }
            // Remove from parent's children
            if let Some(ref parent_id) = record.parent {
                if let Some(parent) = self.workers.get_mut(parent_id) {
                    parent.children.retain(|id| id != worker_id);
                }
            }

            // Emit worker_destroyed + project_changed events
            self.emit_global(serde_json::json!({
                "type": "worker_destroyed",
                "worker_id": killed_worker_id,
                "session_id": killed_session,
                "project": killed_project,
                "parent": killed_parent,
            }));
            self.emit_global(serde_json::json!({
                "type": "project_changed",
                "project": killed_project,
                "worker_id": killed_worker_id,
                "change": "destroyed",
            }));

            // Notify overview subscribers
            self.broadcast_overview();

            // Clean up worktree directory if present (branch preserved)
            if let Some(ref wt) = wt_info {
                let _ = remove_worktree(&wt.path, &wt.source_repo);
            }

            Ok(())
        } else {
            Err(format!("worker not found: {worker_id}"))
        }
    }

    /// Reclaim a worker: kill process + clean up worktree directory.
    /// The git branch is PRESERVED (not deleted) — merge is the Agent's job.
    pub fn reclaim(&mut self, worker_id: &str) -> Result<(), String> {
        // Extract worktree info before removing the record
        let worktree_info = self.workers.get(worker_id)
            .and_then(|r| r.worktree.clone());

        // Kill the worker (removes from registry, kills process, cleans channels/parent)
        self.kill_worker(worker_id)?;

        // Clean up worktree directory (branch preserved)
        if let Some(wt) = worktree_info {
            match remove_worktree(&wt.path, &wt.source_repo) {
                Ok(_) => tracing::info!("[reclaim] worktree cleaned: {} (branch {} preserved)", wt.path, wt.branch),
                Err(e) => tracing::warn!("[reclaim] worktree cleanup failed: {e}"),
            }
        }

        Ok(())
    }

    /// Send to a session by ID. Auto-starts Worker if not running.
    pub async fn send_to_session(
        &mut self,
        session_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Find worker by session_id
        let worker_id = self.workers.iter()
            .find(|(_, w)| w.session_id == session_id)
            .map(|(id, _)| id.clone());

        match worker_id {
            Some(wid) => {
                // Worker exists → send directly
                self.send_to_worker(&wid, method, params).await
            }
            None => {
                // Worker not running → auto-start
                tracing::info!("[session] auto-starting for {session_id}");
                // 注：send_to_session 不能 auto-start（缺 registry_arc）
                return Err(format!("worker not found for session {session_id}, please create_worker first"));
            }
        }
    }

    /// Drain pending events from a worker's stdout_rx.
    /// Note: events are already forwarded to subscribers by the stdout reader task.
    /// This method only drains the buffer to prevent overflow.
    pub async fn drain_events(&mut self, worker_id: &str, timeout_ms: u64) {
        if let Some(record) = self.workers.get_mut(&worker_id.to_string()) {
            if let Some(rx) = &mut record.stdout_rx {
                let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
                while std::time::Instant::now() < deadline {
                    match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                        Ok(Some(_)) => { /* drain — stdout reader already forwards */ }
                        _ => break,
                    }
                }
            }
        }
    }

    /// Find worker by session ID
    pub fn find_by_session(&self, session_id: &str) -> Option<&WorkerRecord> {
        self.workers.values().find(|w| w.session_id == session_id)
    }

    pub fn get_worker(&self, worker_id: &str) -> Option<&WorkerRecord> {
        self.workers.get(worker_id)
    }

    /// Subscribe to a Worker's events
    pub fn subscribe(
        &mut self,
        worker_id: &str,
    ) -> Result<mpsc::Receiver<serde_json::Value>, String> {
        let record = self.workers.get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        record.event_subscribers.push(tx);
        Ok(rx)
    }

    /// 订阅 worker 事件 + 回放最近 N 条历史事件
    /// 返回 (receiver, replay_events)
    pub fn subscribe_with_replay(
        &mut self,
        worker_id: &str,
        replay_count: usize,
    ) -> Result<(mpsc::Receiver<serde_json::Value>, Vec<serde_json::Value>), String> {
        let record = self.workers.get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        record.event_subscribers.push(tx);
        // 取最近 N 条历史事件
        let history: Vec<serde_json::Value> = if replay_count > 0 {
            let total = record.event_history.len();
            let start = total.saturating_sub(replay_count);
            record.event_history.iter().skip(start).cloned().collect()
        } else {
            Vec::new()
        };
        Ok((rx, history))
    }

    /// 非阻塞发送命令（只写 stdin，返回 req_id）。
    pub async fn send_command(
        &mut self,
        worker_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<String, String> {
        let req_id = Uuid::new_v4().to_string()[..8].to_string();
        let line = serde_json::json!({"id": &req_id, "method": method, "params": params}).to_string();
        let record = self.workers.get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        if let Some(stdin) = &mut record.stdin {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(format!("{line}\n").as_bytes()).await.map_err(|e| format!("write: {e}"))?;
            stdin.flush().await.map_err(|e| format!("flush: {e}"))?;
        }
        record.status = WorkerStatus::Busy;
        Ok(req_id)
    }

    /// Register a pending oneshot for a req_id.
    pub fn register_pending(&mut self, worker_id: &str, req_id: &str) -> Option<oneshot::Receiver<serde_json::Value>> {
        let (tx, rx) = oneshot::channel();
        let record = self.workers.get_mut(worker_id)?;
        record.pending.insert(req_id.to_string(), tx);
        Some(rx)
    }

    /// Cleanup a pending oneshot (on timeout/error).
    pub fn cleanup_pending(&mut self, worker_id: &str, req_id: &str) {
        if let Some(record) = self.workers.get_mut(worker_id) {
            record.pending.remove(req_id);
        }
    }

    /// 线程安全的 send_to_worker：短暂持锁写 stdin + 注册 oneshot，然后放锁等响应。
    /// reader task 需要在锁外才能匹配 pending response，避免死锁。
    pub async fn send_async(
        registry: &Arc<tokio::sync::Mutex<Self>>,
        worker_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let (req_id, rx) = {
            let mut reg = registry.lock().await;
            let req_id = reg.send_command(worker_id, method, params).await?;
            let rx = reg.register_pending(worker_id, &req_id)
                .ok_or_else(|| format!("worker not found: {worker_id}"))?;
            (req_id, rx)
        };

        match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                if let Ok(mut reg) = registry.try_lock() {
                    reg.cleanup_pending(worker_id, &req_id);
                }
                Err("worker dropped response channel".into())
            }
            Err(_) => {
                if let Ok(mut reg) = registry.try_lock() {
                    reg.cleanup_pending(worker_id, &req_id);
                }
                Err("timeout waiting for response".into())
            }
        }
    }

    /// Send a command to a Worker via stdin, wait for response via pending oneshot.
    /// 
    /// ⚠️ 注意：此方法在 `timeout(rx).await` 阶段会释放 `&mut self`（Rust NLL 保证），
    /// 但调用方若持有 `MutexGuard`（如 `reg.lock().await`），锁会持续到 Guard drop。
    /// → 调用方必须确保 await 期间不持有锁，否则 reader task 无法匹配 response。
    /// 
    /// 安全的调用模式（与 socket handler 一致）：
    /// ```ignore
    /// let (req_id, rx) = {
    ///     let mut reg = registry.lock().await;
    ///     let req_id = reg.send_command(&wid, method, params).await?;
    ///     let rx = reg.register_pending(&wid, &req_id).unwrap();
    ///     (req_id, rx)
    /// }; // 锁在此释放
    /// let result = WorkerRegistry::await_oneshot(rx).await;
    /// ```
    pub async fn send_to_worker(
        &mut self,
        worker_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Step 1: write stdin + register oneshot (holds lock briefly)
        let req_id = Uuid::new_v4().to_string()[..8].to_string();
        let line = serde_json::json!({
            "id": req_id,
            "method": method,
            "params": params,
        }).to_string();

        let rx = {
            let record = self.workers.get_mut(worker_id)
                .ok_or_else(|| format!("worker not found: {worker_id}"))?;
            if let Some(ref mut stdin) = record.stdin {
                use tokio::io::AsyncWriteExt;
                stdin.write_all(format!("{line}\n").as_bytes()).await
                    .map_err(|e| format!("write stdin: {e}"))?;
                stdin.flush().await.map_err(|e| format!("flush: {e}"))?;
            }
            record.status = WorkerStatus::Busy;
            let (tx, rx) = oneshot::channel();
            record.pending.insert(req_id.clone(), tx);
            rx
        };
        // &mut self 在此被释放（NLL），后面的 await 不需要 self
        // 但调用方仍持锁（MutexGuard）直到 Guard 被 drop

        // Step 2: wait for oneshot via static method (no &mut self)
        Self::await_oneshot_timeout(rx).await
    }

    /// 静待方法，不持有 `&mut self`。用于在锁外等 oneshot。
    pub async fn await_oneshot(rx: oneshot::Receiver<serde_json::Value>) -> Result<serde_json::Value, String> {
        match rx.await {
            Ok(resp) => Ok(resp),
            Err(_) => Err("worker dropped response channel".into()),
        }
    }

    /// 带超时的静待方法，不持有 `&mut self`。
    pub async fn await_oneshot_timeout(rx: oneshot::Receiver<serde_json::Value>) -> Result<serde_json::Value, String> {
        match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => Err("worker dropped response channel".into()),
            Err(_) => Err("timeout waiting for response".into()),
        }
    }

    /// Send to worker with automatic retry on timeout/failure.
    ///
    /// 策略: 指数退避 → 封顶 → 固定间隔 → 30 次 → 没钱才停
    pub async fn send_to_worker_retry(
        &mut self,
        worker_id: &str,
        method: &str,
        params: serde_json::Value,
        retry_config: &crate::retry::RetryConfig,
    ) -> Result<serde_json::Value, String> {
        let mut last_error = None;

        for attempt in 0..=retry_config.max_retries {
            // 重试之前等待（首次不等待）
            if attempt > 0 {
                let delay = crate::retry::backoff_duration(attempt - 1, retry_config);
                tracing::info!(
                    "[retry] {method} attempt {}/{} waiting {:?}",
                    attempt + 1, retry_config.max_retries + 1, delay
                );
                tokio::time::sleep(delay).await;
            }

            match self.send_to_worker(worker_id, method, params.clone()).await {
                Ok(resp) => {
                    // 即使返回了 response，也可能包含业务错误
                    if resp.get("success").and_then(|v| v.as_bool()) == Some(false) {
                        let err = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown");
                        match crate::retry::should_retry(err, attempt, retry_config) {
                            crate::retry::RetryDecision::AbortPermanent => {
                                return Err(format!("[permanent] {method}: {err}"));
                            }
                            crate::retry::RetryDecision::TransientExhausted => {
                                return Err(format!(
                                    "[exhausted] {method} after {} attempts: {err}",
                                    attempt + 1
                                ));
                            }
                            _ => {
                                last_error = Some(err.to_string());
                                tracing::warn!("[retry] {method} attempt {} failed: {err}", attempt + 1);
                            }
                        }
                    } else {
                        return Ok(resp);
                    }
                }
                Err(e) => {
                    match crate::retry::should_retry(&e, attempt, retry_config) {
                        crate::retry::RetryDecision::AbortPermanent => {
                            return Err(format!("[permanent] {method}: {e}"));
                        }
                        crate::retry::RetryDecision::TransientExhausted => {
                            return Err(format!(
                                "[exhausted] {method} after {} attempts: {e}",
                                attempt + 1
                            ));
                        }
                        _ => {
                            last_error = Some(e);
                            tracing::warn!("[retry] {method} attempt {} failed", attempt + 1);
                        }
                    }
                }
            }
        }

        Err(format!(
            "[exhausted] {method} last error: {:?}",
            last_error.unwrap_or_default()
        ))
    }

    /// Forward a channel message to all subscribers
    pub async fn channel_send(
        &mut self,
        channel: &str,
        from: &str,
        msg: serde_json::Value,
    ) {
        let channel_msg = serde_json::json!({
            "type": "channel_msg",
            "channel": channel,
            "from": from,
            "msg": msg,
        });
        let line = serde_json::to_string(&channel_msg).unwrap_or_default();

        if let Some(subscribers) = self.channels.get(channel) {
            for sub_id in subscribers.clone() {
                if let Some(record) = self.workers.get_mut(&sub_id) {
                    if let Some(ref mut stdin) = record.stdin {
                        let _ = stdin.write_all(format!("{line}\n").as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                }
            }
        }
    }

    /// Subscribe to a worker (持 lock 期间拿 rx)，返回 rx 让 caller 释放 lock 后再 await。
    /// 这避免 wait_for_next_agent_end 持 lock 期间 await 导致死锁。
    fn subscribe_for_wait(&mut self, worker_id: &str) -> Result<mpsc::Receiver<serde_json::Value>, String> {
        self.subscribe(worker_id).map_err(|e| format!("subscribe failed: {e}"))
    }

    /// 排空 rx 直到 agent_end 或超时。不持任何 lock。
    /// agent_end 在 agent.run() 完全结束后触发（不是每轮 turn_end），
    /// 所以这里返回的是子 Worker 最终的完整输出。
    async fn drain_until_agent_end(
        rx: &mut mpsc::Receiver<serde_json::Value>,
        timeout_secs: u64,
    ) -> String {
        let mut acc = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        loop {
            let remaining = deadline.checked_duration_since(std::time::Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                return format!("[timeout {timeout_secs}s] partial output:\n{}", acc);
            }
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        Some(msg) => {
                            let et = msg.get("event")
                                .and_then(|e| e.get("type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            if et == "child_crashed" {
                                let exit = msg.get("event")
                                    .and_then(|e| e.get("exit_code"))
                                    .and_then(|v| v.as_i64())
                                    .unwrap_or(-1);
                                let reason = msg.get("event")
                                    .and_then(|e| e.get("exit_reason"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("unknown");
                                return format!("Worker crashed (exit={}):\n{}", exit, reason);
                            }
                            if et == "text_delta" {
                                if let Some(d) = msg.get("event")
                                    .and_then(|e| e.get("delta"))
                                    .and_then(|v| v.as_str())
                                { acc.push_str(d); }
                            }
                            if et == "agent_end" { return acc; }
                        }
                        None => return acc,
                    }
                }
                _ = tokio::time::sleep(remaining) => {
                    return format!("[timeout {timeout_secs}s] partial output:\n{}", acc);
                }
            }
        }
    }

    /// Process pending manager commands from workers.
    /// Handles: create_worker, channel_send, send_to_worker, resume_worker,
    ///          await_worker, kill_worker, peer_follow_up (internal),
    ///          wait_then_respond (internal, for non-blocking agent_end wait).
    ///
    /// 设计要点（避免死锁）：
    /// - 持 lock 期间只做"快速"操作（subscribe / send_command / write_response）
    /// - 阻塞等待 agent_end 的命令（create_worker wait=true, resume_worker, await_worker）
    ///   用 wait_then_respond 内部命令 + 独立 tokio::spawn task 处理，避免持 lock await
    pub async fn process_pending_commands(&mut self, registry_arc: &Arc<Mutex<WorkerRegistry>>) {
        while let Ok(cmd_msg) = self.manager_cmd_rx.try_recv() {
            let command = cmd_msg.get("command").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let params = cmd_msg.get("params").cloned().unwrap_or_default();
            let from_worker = params.get("_from_worker").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let reply_to = params.get("_reply_to").and_then(|v| v.as_str()).unwrap_or("").to_string();

            match command.as_str() {
                "create_worker" => {
                    let relation = params.get("relation")
                        .and_then(|v| v.as_str())
                        .map(|s| match s {
                            "peer" => WorkerRelation::Peer,
                            "system" => WorkerRelation::System,
                            _ => WorkerRelation::Child,
                        })
                        .unwrap_or(WorkerRelation::Child);
                    let wait = params.get("wait").and_then(|v| v.as_bool()).unwrap_or(true);

                    let mut config: WorkerCreateConfig = serde_json::from_value(params).unwrap_or_default();
                    // 把 from_worker（spawn 调用者）注入 config.creator 和 config.parent，
                    // 让 create_worker 内部能查到 parent_session_id 并传给子进程环境变量。
                    // 入口 Worker（host 直接 create_session 创建的）没有 from_worker → creator/parent 保持 None。
                    //
                    // 关键：parent 字段必须设，否则 create_worker 不会把子 worker 加到 parent.children 列表，
                    // 导致 all_workers_idle 的 DFS 检查漏掉子 worker，误判 entry worker idle 提前清理。
                    if !from_worker.is_empty() {
                        if config.creator.is_none() {
                            config.creator = Some(from_worker.clone());
                        }
                        if config.parent.is_none() {
                            config.parent = Some(from_worker.clone());
                        }
                    }
                    let report_channel = config.report_channel.clone().unwrap_or_else(|| "main".to_string());
                    match self.create_worker(config, registry_arc).await {
                        Ok(info) => {
                            let child_id = info.worker_id.clone();
                            let session_id = info.session_id.clone();
                            let creator_id = from_worker.clone();

                            match (relation.clone(), wait) {
                                (WorkerRelation::Child, true) => {
                                    // ── child + wait：subscribe（持 lock）后立即返回响应占位，
                                    //    真正的等待放到 wait_then_respond task 里 ──
                                    let rx_opt = self.subscribe_for_wait(&child_id).ok();
                                    // 先给 caller 一个 "running" 响应避免它死等？不，caller 期望 wait=true 时
                                    // 响应里带 first_turn_output。所以不能立即响应。
                                    // 改为：用 wait_then_respond 内部命令延迟响应。
                                    let tx = self.manager_cmd_tx.clone();
                                    let _ = tx.send(serde_json::json!({
                                        "command": "wait_then_respond",
                                        "params": {
                                            "target_worker": creator_id,
                                            "reply_to": reply_to,
                                            "wait_worker": child_id,
                                            "session_id": session_id,
                                            "relation": "child",
                                            "status": "first_turn_completed",
                                            "output_field": "first_turn_output",
                                            "rx_present": rx_opt.is_some(),
                                        }
                                    }));
                                    // 注意：rx_opt 不能跨 await 边界传给 task（lifetime），
                                    // 所以 wait_then_respond 重新 subscribe（subscribe 多次 OK，
                                    // 每个 subscriber 都能收到事件）。
                                }
                                (WorkerRelation::Child, false) => {
                                    self.write_manager_response(&from_worker, serde_json::json!({
                                        "_reply_to": reply_to,
                                        "success": true,
                                        "data": {
                                            "worker_id": child_id,
                                            "session_id": session_id,
                                            "relation": "child",
                                            "status": "running_in_background",
                                        }
                                    })).await;
                                }
                                (WorkerRelation::Peer, _) => {
                                    // ── peer：立即返回 + 后台 follow_up ──
                                    self.write_manager_response(&from_worker, serde_json::json!({
                                        "_reply_to": reply_to,
                                        "success": true,
                                        "data": {
                                            "worker_id": child_id,
                                            "session_id": session_id,
                                            "relation": "peer",
                                            "status": "running_in_background",
                                            "report_channel": report_channel.clone(),
                                        }
                                    })).await;
                                    let tx = self.manager_cmd_tx.clone();
                                    let _ = tx.send(serde_json::json!({
                                        "command": "peer_follow_up",
                                        "params": {
                                            "peer_id": child_id,
                                            "creator_id": creator_id,
                                            "report_channel": report_channel,
                                        }
                                    }));
                                }
                                (WorkerRelation::System, _) => {
                                    // ── system：host 创建的系统级 Worker（如 memory-agent），无 creator ──
                                    // 立即返回 worker_id，不注入汇报指令，不 follow_up
                                    self.write_manager_response(&from_worker, serde_json::json!({
                                        "_reply_to": reply_to,
                                        "success": true,
                                        "data": {
                                            "worker_id": child_id,
                                            "session_id": session_id,
                                            "relation": "system",
                                            "status": "running_in_background",
                                        }
                                    })).await;
                                }
                            }
                        }
                        Err(e) => {
                            self.write_manager_response(&from_worker, serde_json::json!({
                                "_reply_to": reply_to,
                                "success": false,
                                "error": e,
                            })).await;
                        }
                    }
                }
                // ── 内部命令：subscribe（持 lock）→ 释放 lock → spawn task drain → 完成后再发命令写响应 ──
                "wait_then_respond" => {
                    let target_worker = params.get("target_worker").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let reply_to = params.get("reply_to").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let wait_worker = params.get("wait_worker").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let session_id = params.get("session_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let relation = params.get("relation").and_then(|v| v.as_str()).unwrap_or("child").to_string();
                    let status = params.get("status").and_then(|v| v.as_str()).unwrap_or("first_turn_completed").to_string();
                    let output_field = params.get("output_field").and_then(|v| v.as_str()).unwrap_or("first_turn_output").to_string();

                    // subscribe（持 lock）
                    let rx_opt = self.subscribe_for_wait(&wait_worker).ok();
                    let tx = self.manager_cmd_tx.clone();
                    // 释放 lock 后 spawn task drain（不持 lock 期间 await）
                    tokio::spawn(async move {
                        let output = if let Some(mut rx) = rx_opt {
                            Self::drain_until_agent_end(&mut rx, 300).await
                        } else {
                            "[error] subscribe failed".to_string()
                        };
                        // drain 完成后，发命令回主循环写响应（主循环会重新拿 lock）
                        let _ = tx.send(serde_json::json!({
                            "command": "deliver_response",
                            "params": {
                                "target_worker": target_worker,
                                "reply_to": reply_to,
                                "data": {
                                    "worker_id": wait_worker,
                                    "session_id": session_id,
                                    "relation": relation,
                                    "status": status,
                                    output_field: output,
                                }
                            }
                        }));
                    });
                }
                "deliver_response" => {
                    // 内部命令：把预先构造好的 data 写回 target_worker
                    let target_worker = params.get("target_worker").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let reply_to = params.get("reply_to").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let data = params.get("data").cloned().unwrap_or_default();
                    self.write_manager_response(&target_worker, serde_json::json!({
                        "_reply_to": reply_to,
                        "success": true,
                        "data": data,
                    })).await;
                }
                "peer_follow_up" => {
                    // subscribe peer（持 lock），spawn task 等 agent_end，
                    // 完成后发命令回主循环调 send_command(creator, "follow_up", ...)
                    let peer_id = params.get("peer_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let creator_id = params.get("creator_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let report_channel = params.get("report_channel")
                        .and_then(|v| v.as_str()).unwrap_or("main").to_string();
                    let rx_opt = self.subscribe_for_wait(&peer_id).ok();
                    let tx = self.manager_cmd_tx.clone();
                    tokio::spawn(async move {
                        let peer_output = if let Some(mut rx) = rx_opt {
                            Self::drain_until_agent_end(&mut rx, 300).await
                        } else { "[error] subscribe failed".to_string() };
                        let follow_up_text = format!(
                            "[peer {} 完成 channel={} 汇报]\n{}",
                            &peer_id[..peer_id.len().min(12)], report_channel, peer_output
                        );
                        let _ = tx.send(serde_json::json!({
                            "command": "send_follow_up",
                            "params": {
                                "creator_id": creator_id,
                                "text": follow_up_text,
                            }
                        }));
                    });
                }
                "send_follow_up" => {
                    let creator_id = params.get("creator_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let _ = self.send_command(&creator_id, "follow_up",
                        serde_json::json!({"text": text})).await;
                }
                "channel_send" => {
                    let channel = params.get("channel").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let msg = params.get("msg").cloned().unwrap_or_default();
                    let from = params.get("from").and_then(|v| v.as_str()).unwrap_or(from_worker.as_str());
                    self.channel_send(&channel, from, msg).await;
                    if !reply_to.is_empty() {
                        self.write_manager_response(&from_worker, serde_json::json!({
                            "_reply_to": reply_to,
                            "success": true,
                            "data": {"channel": channel}
                        })).await;
                    }
                }
                "send_to_worker" => {
                    let target = params.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let send_result = self.send_command(&target, "prompt", serde_json::json!({"text": text})).await;
                    let resp = match send_result {
                        Ok(_) => serde_json::json!({
                            "_reply_to": reply_to, "success": true, "data": {"target": target}
                        }),
                        Err(e) => serde_json::json!({
                            "_reply_to": reply_to, "success": false, "error": e,
                        }),
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                "resume_worker" => {
                    // 同步 resume：先 send_command（持 lock）→ spawn task subscribe + drain → 完成发 deliver_response
                    let target = params.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let text = params.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let send_result = self.send_command(&target, "prompt", serde_json::json!({"text": text})).await;
                    match send_result {
                        Ok(_) => {
                            let rx_opt = self.subscribe_for_wait(&target).ok();
                            let tx = self.manager_cmd_tx.clone();
                            let target_clone = target.clone();
                            tokio::spawn(async move {
                                let out = if let Some(mut rx) = rx_opt {
                                    Self::drain_until_agent_end(&mut rx, 300).await
                                } else { "[error] subscribe failed".to_string() };
                                let _ = tx.send(serde_json::json!({
                                    "command": "deliver_response",
                                    "params": {
                                        "target_worker": target_clone,
                                        "reply_to": reply_to,
                                        "data": {
                                            "target": target_clone,
                                            "response_output": out,
                                        }
                                    }
                                }));
                            });
                        }
                        Err(e) => {
                            self.write_manager_response(&from_worker, serde_json::json!({
                                "_reply_to": reply_to, "success": false, "error": e,
                            })).await;
                        }
                    }
                }
                "await_worker" => {
                    let target = params.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let rx_opt = self.subscribe_for_wait(&target).ok();
                    let tx = self.manager_cmd_tx.clone();
                    let target_clone = target.clone();
                    tokio::spawn(async move {
                        let out = if let Some(mut rx) = rx_opt {
                            Self::drain_until_agent_end(&mut rx, 300).await
                        } else { "[error] subscribe failed".to_string() };
                        let _ = tx.send(serde_json::json!({
                            "command": "deliver_response",
                            "params": {
                                "target_worker": target_clone,
                                "reply_to": reply_to,
                                "data": {
                                    "target": target_clone,
                                    "first_turn_output": out,
                                }
                            }
                        }));
                    });
                }
                "kill_worker" => {
                    let target = params.get("target").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let result = self.kill_worker(&target);
                    let resp = match result {
                        Ok(_) => serde_json::json!({
                            "_reply_to": reply_to, "success": true, "data": {"target": target}
                        }),
                        Err(e) => serde_json::json!({
                            "_reply_to": reply_to, "success": false, "error": e,
                        }),
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                // ── MCP 命令（方案 C：子 Worker → host 代理调用）──
                "mcp_read_resource" => {
                    let server = params.get("server").and_then(|v| v.as_str()).unwrap_or("");
                    let uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                    let resp = if let Some(ref mgr) = self.mcp_manager {
                        match mgr.read_resource(server, uri).await {
                            Ok(content) => serde_json::json!({
                                "_reply_to": reply_to,
                                "success": true,
                                "data": {"content": content}
                            }),
                            Err(e) => serde_json::json!({
                                "_reply_to": reply_to,
                                "success": false,
                                "error": e
                            }),
                        }
                    } else {
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": false,
                            "error": "mcp not available"
                        })
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                "mcp_reload" => {
                    // 热重载 MCP 配置（重新读 config.json 的 mcp_servers）
                    let new_config = crate::config::IonConfig::load().mcp_servers;
                    let resp = if let Some(ref mgr) = self.mcp_manager {
                        mgr.reload_config(new_config.clone()).await;
                        let count = mgr.connected_count().await;
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": true,
                            "data": {"servers_loaded": new_config.len(), "connected": count}
                        })
                    } else {
                        // host 没有 mcp_manager，创建一个
                        if !new_config.is_empty() {
                            let mgr = std::sync::Arc::new(crate::mcp::McpManager::new(new_config.clone()));
                            mgr.connect_all().await;
                            mgr.spawn_reconnect_monitor();
                            let count = mgr.connected_count().await;
                            self.mcp_manager = Some(mgr);
                            serde_json::json!({
                                "_reply_to": reply_to,
                                "success": true,
                                "data": {"servers_loaded": new_config.len(), "connected": count}
                            })
                        } else {
                            serde_json::json!({
                                "_reply_to": reply_to,
                                "success": true,
                                "data": {"servers_loaded": 0, "connected": 0}
                            })
                        }
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                "mcp_call_tool" => {
                    let server = params.get("server").and_then(|v| v.as_str()).unwrap_or("");
                    let tool = params.get("tool").and_then(|v| v.as_str()).unwrap_or("");
                    let args = params.get("args").cloned().unwrap_or_default();
                    let resp = if let Some(ref mgr) = self.mcp_manager {
                        match mgr.call_tool(server, tool, args).await {
                            Ok(output) => serde_json::json!({
                                "_reply_to": reply_to,
                                "success": true,
                                "data": {"output": output}
                            }),
                            Err(e) => serde_json::json!({
                                "_reply_to": reply_to,
                                "success": false,
                                "error": e
                            }),
                        }
                    } else {
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": false,
                            "error": "mcp not available on host"
                        })
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                "mcp_list_tools" => {
                    let resp = if let Some(ref mgr) = self.mcp_manager {
                        let tools = mgr.all_discovered_tools_serialized().await;
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": true,
                            "data": {"tools": tools}
                        })
                    } else {
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": true,
                            "data": {"tools": []}
                        })
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                "mcp_get_servers" => {
                    let resp = if let Some(ref mgr) = self.mcp_manager {
                        let servers = mgr.server_list_json().await;
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": true,
                            "data": servers
                        })
                    } else {
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": true,
                            "data": []
                        })
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                "mcp_toggle_server" => {
                    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let enabled = params.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                    let resp = if let Some(ref mgr) = self.mcp_manager {
                        match mgr.toggle_server(&name, enabled).await {
                            Ok(()) => serde_json::json!({
                                "_reply_to": reply_to,
                                "success": true,
                                "data": {"name": name, "enabled": enabled}
                            }),
                            Err(e) => serde_json::json!({
                                "_reply_to": reply_to,
                                "success": false,
                                "error": e
                            }),
                        }
                    } else {
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": false,
                            "error": "mcp not available"
                        })
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                "mcp_restart_server" => {
                    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let resp = if let Some(ref mgr) = self.mcp_manager {
                        match mgr.restart_server(&name).await {
                            Ok(()) => serde_json::json!({
                                "_reply_to": reply_to,
                                "success": true,
                                "data": {"name": name, "status": "connected"}
                            }),
                            Err(e) => serde_json::json!({
                                "_reply_to": reply_to,
                                "success": false,
                                "error": e
                            }),
                        }
                    } else {
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": false,
                            "error": "mcp not available"
                        })
                    };
                    self.write_manager_response(&from_worker, resp).await;
                }
                _ => {
                    tracing::warn!("[manager] unknown command: {command}");
                    if !reply_to.is_empty() {
                        self.write_manager_response(&from_worker, serde_json::json!({
                            "_reply_to": reply_to,
                            "success": false,
                            "error": format!("unknown command: {command}"),
                        })).await;
                    }
                }
            }
        }
    }

    /// Write a manager_response back to the requesting worker's stdin.
    /// Write a response JSON line to a worker's stdin.
    /// Resolves worker by worker_id first, then by session_id.
    /// （ManagerBridge 的 _from_worker 传的是 session_id，但 registry 按 worker_id 索引）
    async fn write_manager_response(&mut self, worker_or_session: &str, resp: serde_json::Value) {
        use tokio::io::AsyncWriteExt;
        let line = format!("{}\n", serde_json::to_string(&resp).unwrap_or_default());

        let target = if self.workers.contains_key(worker_or_session) {
            Some(worker_or_session.to_string())
        } else {
            self.workers.iter()
                .find(|(_, w)| w.session_id == worker_or_session)
                .map(|(id, _)| id.clone())
        };

        match target {
            Some(wid) => {
                if let Some(record) = self.workers.get_mut(&wid) {
                    if let Some(ref mut stdin) = record.stdin {
                        let _ = stdin.write_all(line.as_bytes()).await;
                        let _ = stdin.flush().await;
                    }
                }
            }
            None => {
                tracing::warn!("[manager] cannot write response: worker/session {worker_or_session} not found");
            }
        }
    }

    pub fn subscribe_global(&mut self) -> mpsc::Receiver<serde_json::Value> {
        let (tx, rx) = mpsc::channel(256);
        self.global_subscribers.push(tx);
        rx
    }

    /// Subscribe to overview snapshots. Returns a receiver that gets the current
    /// snapshot immediately and subsequent ones on changes.
    pub fn subscribe_overview(&mut self) -> mpsc::UnboundedReceiver<serde_json::Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        // Send current state immediately
        let overview = self.get_overview();
        let _ = tx.send(overview);
        self.overview_subscribers.push(tx);
        rx
    }

    /// Emit a global event to all subscribers.
    fn emit_global(&self, event: serde_json::Value) {
        for sub in &self.global_subscribers {
            let _ = sub.try_send(event.clone());
        }
    }

    /// Broadcast overview to all overview subscribers.
    pub fn broadcast_overview(&mut self) {
        let overview = self.get_overview();
        self.overview_subscribers.retain(|tx| tx.send(overview.clone()).is_ok());
    }

    /// Get an overview of all workers, projects, and sessions.
    pub fn get_overview(&self) -> serde_json::Value {
        let workers: Vec<serde_json::Value> = self.workers.values().map(|w| {
            serde_json::json!({
                "worker_id": w.worker_id,
                "session_id": w.session_id,
                "project": w.project,
                "status": w.status,
                "exit_code": w.exit_code,
                "exit_reason": w.exit_reason,
                "model": w.model,
                "agent": w.agent,
                "channels": w.channels,
                "parent": w.parent,
                "children": w.children,
                "latest_output": w.latest_output.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
                "log_short": w.log_short,
                "model_size": w.model_size,
                "started_at": w.started_at,
            })
        }).collect();

        let projects: Vec<serde_json::Value> = self.list_projects().iter().map(|p| {
            serde_json::json!({
                "name": p.name,
                "path": p.path,
                "worker_count": p.worker_ids.len(),
            })
        }).collect();

        let sessions: Vec<serde_json::Value> = self.workers.values().map(|w| {
            serde_json::json!({
                "session_id": w.session_id,
                "worker_id": w.worker_id,
                "project": w.project,
                "created_by": w.parent,
            })
        }).collect();

        serde_json::json!({
            "workers": workers,
            "projects": projects,
            "total_workers": self.workers.values().filter(|w| w.status != WorkerStatus::Dead).count(),
            "total_projects": projects.len(),
            "total_stale": self.workers.values().filter(|w| w.status == WorkerStatus::Stale).count(),
            "total_dead": self.workers.values().filter(|w| w.status == WorkerStatus::Dead).count(),
            "sessions": sessions,
        })
    }

    /// Remove dead workers older than max_age_secs.
    pub fn gc_dead_workers(&mut self, max_age_secs: u64) {
        let now = now_ms();
        let deadline = now - (max_age_secs * 1000) as i64;
        self.workers.retain(|_id, record| {
            if record.status == WorkerStatus::Dead {
                return record.started_at >= deadline;
            }
            true
        });
    }

    // ── Singleton management（host 级单例扩展，引用计数）──

    /// 注册一个单例扩展。如果 key 已存在，返回 false（不重复创建）。
    pub fn register_singleton(&mut self, ext: Box<dyn crate::agent::extension::Extension>) -> bool {
        let key = ext.singleton_key().to_string();
        if key.is_empty() || self.singletons.contains_key(&key) {
            return false;
        }
        tracing::info!("[singleton] registered: {}", key);
        self.singletons.insert(key, SingletonEntry {
            key: ext.singleton_key().to_string(),
            instance: std::sync::Arc::from(ext),
            users: std::collections::HashSet::new(),
            initialized: false,
        });
        true
    }

    /// 初始化所有未初始化的单例（调用 on_singleton_init）。
    /// 在 host 启动后、用户 Worker 创建前调用。
    pub async fn init_singletons(&mut self) {
        let keys: Vec<String> = self.singletons.keys().cloned().collect();
        for key in keys {
            let entry = self.singletons.get_mut(&key).unwrap();
            if !entry.initialized {
                if let Err(e) = entry.instance.on_singleton_init().await {
                    tracing::error!("[singleton:{}] init failed: {:?}", key, e);
                } else {
                    entry.initialized = true;
                    tracing::info!("[singleton:{}] initialized", key);
                }
            }
        }
    }

    /// init 之后的第二步：调用每个单例的 on_singleton_post_init。
    ///
    /// post_init 拿到 registry Arc，能在其中 spawn 系统级 Worker（如 memory-agent）。
    /// **必须**在 init_singletons 释放 lock 之后调（post_init 内部会 lock registry 来 create_worker，
    /// 持 lock 调会死锁）。
    pub async fn post_init_singletons(registry: &Arc<Mutex<WorkerRegistry>>) {
        // 持 lock 时快速 clone 所有 instance 的 Arc，释放 lock 后调 post_init（避免死锁）
        let instances: Vec<Arc<dyn crate::agent::extension::Extension>> = {
            let reg = registry.lock().await;
            reg.singletons.values().map(|e| e.instance.clone()).collect()
        };
        for ext in instances {
            if let Err(e) = ext.on_singleton_post_init(registry).await {
                tracing::error!("[singleton] post_init failed: {:?}", e);
            }
        }
    }

    /// Worker 开始使用单例（引用计数 +1）。
    /// 在 create_worker 成功后调用。
    pub async fn singleton_user_join(&mut self, worker_id: &str) {
        let keys: Vec<String> = self.singletons.keys().cloned().collect();
        for key in keys {
            let entry = self.singletons.get_mut(&key).unwrap();
            if entry.users.insert(worker_id.to_string()) {
                // 新用户
                if let Err(e) = entry.instance.on_user_join(worker_id).await {
                    tracing::warn!("[singleton:{}] user_join {} failed: {:?}", key, worker_id, e);
                }
            }
        }
    }

    /// Worker 停止使用单例（引用计数 -1）。
    /// 在 Worker 清理（正常退出/崩溃）时调用。
    /// 崩溃不干掉单例——只有引用计数 == 0 才触发 on_last_user_gone。
    pub async fn singleton_user_leave(&mut self, worker_id: &str) {
        let keys: Vec<String> = self.singletons.keys().cloned().collect();
        for key in keys {
            let was_last = {
                let entry = self.singletons.get_mut(&key).unwrap();
                if entry.users.remove(worker_id) {
                    if let Err(e) = entry.instance.on_user_leave(worker_id).await {
                        tracing::warn!("[singleton:{}] user_leave {} failed: {:?}", key, worker_id, e);
                    }
                    entry.users.is_empty()
                } else {
                    false
                }
            };
            // 在 entry 的 mutable borrow 释放后才能再 borrow 调 on_last_user_gone
            if was_last {
                let entry = self.singletons.get_mut(&key).unwrap();
                if let Err(e) = entry.instance.on_last_user_gone().await {
                    tracing::warn!("[singleton:{}] last_user_gone failed: {:?}", key, e);
                }
                tracing::info!("[singleton:{}] last user gone ({})", key, worker_id);
            }
        }
    }

    /// 关闭所有单例（host shutdown 时调用）。
    pub async fn shutdown_singletons(&mut self) {
        let keys: Vec<String> = self.singletons.keys().cloned().collect();
        for key in keys {
            let entry = self.singletons.get_mut(&key).unwrap();
            if let Err(e) = entry.instance.on_singleton_shutdown().await {
                tracing::warn!("[singleton:{}] shutdown failed: {:?}", key, e);
            }
            tracing::info!("[singleton:{}] shutdown", key);
        }
        self.singletons.clear();
    }

    /// Set the entry worker for recursive idle detection.
    pub fn set_entry_worker(&mut self, worker_id: &str) {
        self.entry_worker_id = Some(worker_id.to_string());
    }

    /// Check if a worker and all its descendants are idle (DFS recursive).
    pub fn all_workers_idle(&self, entry_worker_id: &str) -> Result<bool, String> {
        let mut stack = vec![entry_worker_id.to_string()];
        let mut visited = std::collections::HashSet::new();
        while let Some(wid) = stack.pop() {
            if !visited.insert(wid.clone()) { continue; }
            let record = self.workers.get(&wid).ok_or_else(|| {
                format!("worker {wid} not found in registry")
            })?;
	            match record.status {
	                WorkerStatus::Idle | WorkerStatus::Dead => {}
                _ => return Ok(false),
            }
            for child_id in &record.children {
                stack.push(child_id.clone());
            }
        }
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// Worker stdout reader — 解析响应和事件
// ---------------------------------------------------------------------------

#[allow(dead_code)]
async fn read_worker_stdout(
    worker_id: String,
    stdout: ChildStdout,
    registry: Arc<Mutex<WorkerRegistry>>,
) {
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() { continue; }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = msg["type"].as_str().unwrap_or("");
        let msg_id = msg.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());

        match msg_type {
            // Response with ID → match pending request
            "response" => {
                if let Some(id) = msg_id {
                    let mut reg = registry.lock().await;
                    if let Some(record) = reg.workers.get_mut(&worker_id) {
                        if let Some(tx) = record.pending.remove(&id) {
                            let _ = tx.send(msg.clone());
                        }
                    }
                }
            }

            // Event (no ID) → forward to subscribers + parent
            "event" => {
                let ev_type = msg.get("event")
                    .and_then(|e| e.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                match ev_type {
                    "agent_end" => {
                        let mut reg = registry.lock().await;
                        if let Some(record) = reg.workers.get_mut(&worker_id) {
                            record.status = WorkerStatus::Idle;
                        }
                        // Forward to event subscribers
                        if let Some(record) = reg.workers.get(&worker_id) {
                            for sub in &record.event_subscribers {
                                let _ = sub.try_send(msg.clone());
                            }
                            if let Some(ref parent_tx) = record.parent_event_tx {
                                let child_event = serde_json::json!({
                                    "type": "child_event",
                                    "worker_id": worker_id,
                                    "event": msg["event"],
                                });
                                let _ = parent_tx.try_send(child_event);
                            }
                        }
                        // Broadcast overview without holding lock
                        let reg_clone = Arc::clone(&registry);
                        let _wid = worker_id.clone();
                        tokio::spawn(async move {
                            let mut r = reg_clone.lock().await;
                            r.broadcast_overview();
                        });
                    }
                    "text_delta" => {
                        let mut reg = registry.lock().await;
                        if let Some(delta) = msg.get("event")
                            .and_then(|e| e.get("delta"))
                            .and_then(|v| v.as_str())
                        {
                            if let Some(record) = reg.workers.get_mut(&worker_id) {
                                let truncated: String = delta.chars().take(60).collect();
                                record.latest_output.push_back(truncated.clone());
                                while record.latest_output.len() > 5 {
                                    record.latest_output.pop_front();
                                }
                                record.log_short = Some(truncated);
                            }
                        }
                        // Forward to event subscribers
                        if let Some(record) = reg.workers.get(&worker_id) {
                            for sub in &record.event_subscribers {
                                let _ = sub.try_send(msg.clone());
                            }
                            if let Some(ref parent_tx) = record.parent_event_tx {
                                let child_event = serde_json::json!({
                                    "type": "child_event",
                                    "worker_id": worker_id,
                                    "event": msg["event"],
                                });
                                let _ = parent_tx.try_send(child_event);
                            }
                        }
                    }
                    _ => {
                        let reg = registry.lock().await;
                        let event_json = msg.clone();

                        // Forward to event subscribers
                        if let Some(record) = reg.workers.get(&worker_id) {
                            for sub in &record.event_subscribers {
                                let _ = sub.try_send(event_json.clone());
                            }
                            // Forward to parent if exists
                            if let Some(ref parent_tx) = record.parent_event_tx {
                                let child_event = serde_json::json!({
                                    "type": "child_event",
                                    "worker_id": worker_id,
                                    "event": event_json["event"],
                                });
                                let _ = parent_tx.try_send(child_event);
                            }
                        }
                    }
                }
            }

            // Control commands (Manager intercepts)
            "create_worker" => {
                // Manager should handle this
                tracing::info!("[{worker_id}] create_worker request");
            }
            "channel_send" => {
                let channel = msg.get("channel").and_then(|v| v.as_str()).unwrap_or("");
                let channel_msg = msg.get("msg").cloned().unwrap_or(serde_json::Value::Null);
                let mut reg = registry.lock().await;
                reg.channel_send(channel, &worker_id, channel_msg).await;
            }

            // Ready signal
            "ready" => {
                tracing::info!("[{worker_id}] ready: session={}",
                    msg.get("session").and_then(|v| v.as_str()).unwrap_or("?"));
            }

            _ => {
                tracing::debug!("[{worker_id}] unknown stdout type: {msg_type}");
            }
        }
    }

    // Worker stdout closed → mark as dead
    let mut reg = registry.lock().await;
    if let Some(record) = reg.workers.get_mut(&worker_id) {
        record.status = WorkerStatus::Dead;
    }
    tracing::warn!("[{worker_id}] stdout closed, marked dead");
}

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Worker 创建时与创建者的关系。
/// - `Child`：父→子，同步语义。creator 持有 handle，可 resume、可对话。
///   `parent` 字段会被设为 creator（沿用现有父子路径）。
/// - `Peer`：creator→peer，异步语义。peer 不是 creator 的下属，只记一个"来源"。
///   `parent = None`，但 `creator` 字段被保留，内核会自动注入"汇报指令段"。
/// - `System`：host 启动时创建的系统级 Worker（如 memory-agent），无 creator。
///   parent=None，不注入汇报指令，立即返回 worker_id。
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkerRelation {
    #[default]
    Child,
    Peer,
    System,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct WorkerCreateConfig {
    /// Worktree isolation config. If Some, creates a git worktree.
    #[serde(default)]
    pub worktree: Option<WorktreeConfig>,
    pub session: Option<String>,
    pub project_path: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub agent: Option<String>,
    pub channels: Option<Vec<String>>,
    pub parent: Option<String>,
    /// 与创建者的关系。默认 Child。Peer 模式下 parent 字段会被忽略。
    #[serde(default)]
    pub relation: Option<WorkerRelation>,
    /// 创建者 worker_id。Child 模式下与 parent 等价；Peer 模式下单独保留用于注入汇报指令。
    #[serde(default)]
    pub creator: Option<String>,
    /// Peer 模式下，内核自动注入的汇报指令段使用的频道名（默认 "main"）。
    #[serde(default)]
    pub report_channel: Option<String>,
    /// Peer 模式下，汇报对象的 worker_id（用于在 prompt 里指明 creator）。
    #[serde(default)]
    pub report_to: Option<String>,
    /// 创建后立即注入的初始 prompt（由内核通过 prompt RPC 发给子进程）。
    /// Peer 模式下，汇报指令段会被追加到这个 prompt 末尾。
    #[serde(default)]
    pub initial_prompt: Option<String>,
    /// 子 Worker 的 MCP 跳过模式：
    /// - None / ""  → 不跳过（入口 Worker 持有全部 MCP 连接）
    /// - "1"        → 跳过全部 MCP（完全跳过）
    /// - "stdio"    → 只跳过 stdio，HTTP 照连（方案 B：HTTP 天然多客户端）
    #[serde(default)]
    pub skip_mcp: Option<String>,
    // ── 补丁 1 新增（HOOKS_AND_OUTLINE_SYNC）：让扩展 spawn 的子 Worker 也能限定工具/步数 ──
    /// 允许的工具白名单（None = 继承全部）。通过 ION_ALLOWED_TOOLS 环境变量传给子进程。
    #[serde(default)]
    pub allowed_tools: Option<Vec<String>>,
    /// 禁用的工具黑名单。通过 ION_DISALLOWED_TOOLS 环境变量传给子进程。
    #[serde(default)]
    pub disallowed_tools: Option<Vec<String>>,
    /// 最大 turn 数（None = 继承 host 默认）。通过 ION_MAX_TURNS 环境变量传给子进程。
    #[serde(default)]
    pub max_turns: Option<u64>,
    /// hooks 递归深度（防 agent handler 死循环）。hooks agent handler spawn 时设。
    /// Manager 传给子进程 ION_HOOK_DEPTH，HookExtension 读到 >= 2 跳过 agent handler。
    #[serde(default)]
    pub hook_depth: Option<u32>,
    /// 可选：覆盖子 Worker 的 system prompt。通过 ION_SYSTEM_PROMPT 环境变量传给子进程。
    /// 用于 skill fork 模式——把 skill 内容注入 system prompt（不被 compaction 压缩）。
    #[serde(default)]
    pub system_prompt_override: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerInfo {
    pub worker_id: String,
    pub session_id: String,
    pub project: String,
    pub status: WorkerStatus,
    pub model: String,
    pub agent: String,
    pub channels: Vec<String>,
    pub parent: Option<String>,
    pub children: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProjectInfo {
    pub name: String,
    pub path: String,
    pub worker_ids: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum WorkerEvent {
    TextDelta { worker_id: String, delta: String },
    ToolCall { worker_id: String, tool: String, args: serde_json::Value },
    Result { worker_id: String, success: bool, output: String },
    ChildEvent { worker_id: String, event: Box<WorkerEvent> },
    StatusChange { worker_id: String, status: WorkerStatus },
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// Generate a random u32 for short IDs (worktree dirs, etc.)
fn randish() -> u32 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos()
}


/// Create a git worktree using paths.rs for the path (ION_WORKTREE_ROOT aware).
/// The directory name uses a short random ID, not the full session ID.
/// Returns (worktree_path, branch_name).
pub fn create_worktree_advanced(
    session_id: &str,
    project_path: &str,
    config: &WorktreeConfig,
) -> Result<(String, String), String> {
    let project_name = std::path::Path::new(project_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    let branch_name = if config.branch.is_empty() {
        format!("ion-{session_id}")
    } else {
        config.branch.clone()
    };

    // Generate a short random ID for the worktree directory (8 hex chars)
    let wt_id = format!("{:08x}", randish());

    // Use paths.rs worktree_root (respects ION_WORKTREE_ROOT env var)
    let wt_root = crate::paths::worktree_root();
    let worktree_dir = wt_root
        .join(&wt_id)
        .join(project_name);

    // Create parent directory
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir failed: {e}"))?;
    }

    // Build git worktree add command
    let mut git_args = vec![
        "-C".to_string(),
        project_path.to_string(),
        "worktree".to_string(),
        "add".to_string(),
        worktree_dir.to_string_lossy().to_string(),
        "-b".to_string(),
        branch_name.clone(),
    ];
    // If base branch specified, append it
    if let Some(ref base) = config.base {
        git_args.push(base.clone());
    }

    let output = std::process::Command::new("git")
        .args(&git_args)
        .output()
        .map_err(|e| format!("git worktree failed: {e}"))?;

    if output.status.success() {
        tracing::info!("[worktree] created: {} (branch: {})", worktree_dir.display(), branch_name);
        Ok((worktree_dir.to_string_lossy().to_string(), branch_name))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // If worktree already exists, reuse
        if stderr.contains("already exists") || stderr.contains("already checked out") {
            tracing::info!("[worktree] reusing existing: {}", worktree_dir.display());
            return Ok((worktree_dir.to_string_lossy().to_string(), branch_name));
        }
        Err(format!("git worktree add failed: {stderr}"))
    }
}

/// Remove a git worktree directory (cleanup). Branch is preserved.
fn remove_worktree(worktree_path: &str, source_repo: &str) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .args(["-C", source_repo, "worktree", "remove", "--force", worktree_path])
        .output()
        .map_err(|e| format!("git worktree remove failed: {e}"))?;

    if output.status.success() {
        tracing::info!("[worktree] removed: {}", worktree_path);
        Ok(())
    } else {
        // Fallback: force remove the directory
        let stderr = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("[worktree] git remove failed: {stderr}, force rm");
        let _ = std::fs::remove_dir_all(worktree_path);
        Ok(())
    }
}
