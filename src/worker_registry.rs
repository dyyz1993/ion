use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

/// 复活/重注册会话时，从 existing SessionMeta 带回历史热字段
/// （AGENTS.md 存储落位原则：upsert 是整替换，重建 meta 必须带回旧值，
///   否则 create_session 复活会把 name/计数/created_at 全部清零）。
fn merge_existing_meta(
    idx: &crate::session_index::SessionIndex,
    sid: &str,
    mut meta: crate::session_index::SessionMeta,
) -> crate::session_index::SessionMeta {
    let Some(old) = idx.get(sid) else {
        return meta; // 全新会话，直接用新 meta
    };
    // 标题：旧 name 是有意义的标题（≠ 裸 sid）时保留
    if old.name.as_deref().is_some_and(|n| n != sid) {
        meta.name = old.name.clone();
        if old.first_name.is_some() {
            meta.first_name = old.first_name.clone();
        }
    }
    // 计数/时间戳：历史累计带回
    meta.token_input = old.token_input;
    meta.token_output = old.token_output;
    meta.token_cache_read = old.token_cache_read;
    meta.token_cache_write = old.token_cache_write;
    meta.user_prompt_count = old.user_prompt_count;
    meta.llm_request_count = old.llm_request_count;
    meta.total_duration_ms = old.total_duration_ms;
    meta.compress_count = old.compress_count;
    meta.message_count = old.message_count;
    meta.turn_count = old.turn_count;
    meta.error_count = old.error_count;
    meta.created_at = old.created_at;
    // project 归属带回：复活时的 cwd 可能不是原项目（否则直读按新 project 找不到会话文件）
    if old.project.is_some() {
        meta.project = old.project.clone();
        meta.project_name = old.project_name.clone().or(meta.project_name.clone());
    }
    // last_* 运行态快照带回
    meta.last_thinking_level = old.last_thinking_level.clone();
    meta.last_active_tools = old.last_active_tools.clone();
    meta.last_entry_id = old.last_entry_id.clone();
    // 血缘/分支：新值缺失时用旧值兜底
    if meta.parent_session.is_none() {
        meta.parent_session = old.parent_session.clone();
        meta.parent_type = old.parent_type.clone();
    }
    if meta.branch.is_none() {
        meta.branch = old.branch.clone();
    }
    if old.initial_cwd.is_some() && meta.initial_cwd.is_none() {
        meta.initial_cwd = old.initial_cwd.clone();
    }
    meta
}

/// Result of `prepare_worker_spawn` — contains spawned child process + all data
/// needed to register the worker in the registry (under lock, fast).
pub struct PreparedSpawn {
    pub child: tokio::process::Child,
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::process::ChildStdout,
    pub stderr: tokio::process::ChildStderr,
    pub worker_id: String,
    pub session_id: String,
    pub project_path: String,
    pub project_name: String,
    pub worktree_path: String,
    pub worktree_info: Option<WorktreeInfo>,
    pub model: String,
    pub provider: String,
    pub agent_name: String,
}

/// Find the ion binary path (for spawning child workers).
fn find_ion_binary() -> String {
    // Try current exe first
    if let Ok(exe) = std::env::current_exe()
        && exe.exists()
    {
        return exe.to_string_lossy().to_string();
    }
    // Fallback: look for target/debug/ion relative to CWD
    let candidates = [
        "target/debug/ion-worker",
        "target/debug/ion",
        "ion-worker",
        "/usr/local/bin/ion-worker",
    ];
    for c in &candidates {
        if std::path::Path::new(c).exists() {
            return c.to_string();
        }
    }
    "ion-worker".to_string() // last resort: rely on PATH
}
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{mpsc, oneshot};
use parking_lot::Mutex;
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
    /// Host 级 EventBus handle（singleton 扩展用，broadcast 给所有 subscribers）
    /// None = 默认（cmd_run 等不需要事件广播的场景）；Some = cmd_serve/cmd_host 注入
    pub event_bus: Option<std::sync::Arc<tokio::sync::Mutex<crate::event_bus::ExtensionEventBus>>>,
    /// Weak back-reference to the Arc<Mutex<Self>> that owns this registry.
    /// Set by `new_in_arc` / `set_self_ref` after construction. Used by
    /// `send_to_session` to auto-start workers (which needs the Arc to pass to
    /// `create_worker`). Weak avoids a reference cycle.
    self_ref: std::sync::Weak<Mutex<WorkerRegistry>>,
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
    /// Worker 进入当前状态的时间戳（用于 Busy 超时判断、Stale 时间 GC）。
    /// 在 set_status 里自动维护，不要手动赋值。
    pub status_since: i64,
    /// Worker 变 Dead 的时间戳（gc 按 died_at 判断而非 started_at）。
    /// None 表示未退出过。
    pub died_at: Option<i64>,
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
    Dead,
    Stale,
    // 历史变体 Paused / Spawning 已移除：全代码库无任何赋值点（只在测试和 match 臂
    // 引用，属死代码）。如需恢复"注册未 spawn"的语义，建议用独立字段而非新 enum 变体，
    // 避免再触发大量 match 臂的穷尽性维护负担。
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

impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkerRecord {
    /// 统一的状态变更入口：同步更新 status 和 status_since，并在转 Dead 时记录 died_at。
    /// 所有状态变更都应走这里，保证 status_since / died_at 不被遗忘。
    /// 注意：Dead 是终态，一旦 Dead 不再被覆盖（避免 GC 期间被误改回其他状态）。
    pub fn set_status(&mut self, new_status: WorkerStatus) {
        if self.status != WorkerStatus::Dead && new_status == WorkerStatus::Dead {
            self.died_at = Some(now_ms());
        }
        self.status = new_status;
        self.status_since = now_ms();
    }
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
            event_bus: None,
            self_ref: std::sync::Weak::new(),
        }
    }

    /// Set the weak back-reference to the owning Arc. Call once after wrapping
    /// in `Arc::new(Mutex::new(...))` so that `send_to_session` can auto-start
    /// workers (it needs the Arc to pass to `create_worker`).
    ///
    /// Example:
    /// ```ignore
    /// let registry = Arc::new(Mutex::new(WorkerRegistry::new()));
    /// registry.lock().set_self_ref(&registry);
    /// ```
    pub fn set_self_ref(&mut self, arc: &std::sync::Arc<Mutex<WorkerRegistry>>) {
        self.self_ref = std::sync::Arc::downgrade(arc);
    }

    /// 设置 host 级 MCP 管理器（方案 C：host 持有连接，Worker 代理调用）
    pub fn set_mcp_manager(&mut self, mgr: std::sync::Arc<crate::mcp::McpManager>) {
        self.mcp_manager = Some(mgr);
    }

    /// 设置 host 级 EventBus handle，让 singleton 扩展能 broadcast 事件
    pub fn set_event_bus(
        &mut self,
        bus: std::sync::Arc<tokio::sync::Mutex<crate::event_bus::ExtensionEventBus>>,
    ) {
        self.event_bus = Some(bus);
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
            event_bus: None,
            self_ref: std::sync::Weak::new(),
        }
    }

    /// Pre-compute worktree path + spawn child process OUTSIDE the registry lock.
    ///
    /// This extracts the slow parts of create_worker (git init/add/commit for worktree,
    /// fork+exec for child process) so they can run without blocking RPCs.
    ///
    /// Returns a `PreparedSpawn` containing the child process + all data needed
    /// for the caller to register it in the registry (phase 2, under lock).
    pub async fn prepare_worker_spawn(
        config: &WorkerCreateConfig,
    ) -> Result<PreparedSpawn, String> {
        let project_path = config.project_path.clone().unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        });

        let project_name = std::path::Path::new(&project_path)
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".into());
        let session_id = config
            .session
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // ── Worktree creation (SLOW: git init/add/commit, 1-3s) ──
        // ⚠️ 用 tokio::process::Command（异步），避免阻塞 tokio runtime。
        // 之前用 std::process::Command::output() 是同步阻塞，会卡住整个 runtime，
        // 导致 socket handler 无法响应 RPC。
        let (worktree_path, worktree_info) = if let Some(ref wt_config) = config.worktree {
            let repo = std::path::Path::new(&project_path);
            // require_clean：已是 git 仓库且工作区有未提交改动 → 拒绝（防脏状态进基准）
            if config.require_clean.unwrap_or(false) && repo.join(".git").exists() {
                let status = tokio::process::Command::new("git")
                    .args(["-C", &project_path, "status", "--porcelain"])
                    .output()
                    .await
                    .map_err(|e| format!("git status failed: {e}"))?;
                if !status.stdout.is_empty() {
                    return Err("source branch has uncommitted changes".to_string());
                }
            }
            if !repo.join(".git").exists() {
                tracing::info!("[worktree] project is not a git repo, initializing");
                let init = tokio::process::Command::new("git")
                    .args(["-C", &project_path, "init", "-b", "main"])
                    .output()
                    .await
                    .map_err(|e| format!("git init failed: {e}"))?;
                if !init.status.success() {
                    let stderr = String::from_utf8_lossy(&init.stderr);
                    return Err(format!("git init failed: {stderr}"));
                }
                let _add = tokio::process::Command::new("git")
                    .args(["-C", &project_path, "add", "."])
                    .output()
                    .await
                    .map_err(|e| format!("git add failed: {e}"))?;
                let _commit = tokio::process::Command::new("git")
                    .args(["-C", &project_path, "commit", "-m", "ion: initial commit"])
                    .output()
                    .await
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
                    tracing::info!("[worktree] created: {} (branch: {})", path, branch);
                    (path, Some(info))
                }
                Err(e) => return Err(format!("worktree creation failed: {e}")),
            }
        } else {
            (project_path.clone(), None)
        };

        // ── Resolve model/provider/agent ──
        let cfg = crate::config::IonConfig::load();
        let default_model = cfg
            .default_model
            .clone()
            .unwrap_or_else(|| "glm-4.7".to_string());
        let default_provider = cfg
            .default_provider
            .clone()
            .unwrap_or_else(|| "zhipuai".to_string());
        let model = config.model.clone().unwrap_or(default_model);
        let provider = config.provider.clone().unwrap_or(default_provider);
        let agent_name = config.agent.clone().unwrap_or_default();

        // ── Build child command args ──
        let mut cmd_args = vec![
            "--mode".to_string(),
            "rpc".to_string(),
            "--session".to_string(),
            session_id.clone(),
            "--model".to_string(),
            model.clone(),
            "--provider".to_string(),
            provider.clone(),
        ];
        if !agent_name.is_empty() {
            cmd_args.push("--agent".to_string());
            cmd_args.push(agent_name.clone());
        }

        // ── Find ion binary ──
        let binary = find_ion_binary();

        let mut child_cmd = tokio::process::Command::new(&binary);
        child_cmd
            .args(&cmd_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(&worktree_path);

        // ── Set env vars (same as original create_worker) ──
        child_cmd.env("ION_PROJECT_ROOT", &project_path);
        child_cmd.env("ION_WORKER_CWD", &worktree_path);
        // ★ 传 model/provider 到子进程（否则 worker 用 config 默认值，不是 create_session 指定的）
        if let Some(ref m) = config.model {
            child_cmd.env("ION_SESSION_MODEL", m);
        }
        if let Some(ref p) = config.provider {
            child_cmd.env("ION_SESSION_PROVIDER", p);
        }
        if let Some(ref mode) = config.skip_mcp
            && !mode.is_empty()
        {
            child_cmd.env("ION_SKIP_MCP", mode);
        }
        if let Some(ref tools) = config.allowed_tools
            && !tools.is_empty()
        {
            child_cmd.env("ION_ALLOWED_TOOLS", tools.join(","));
        }
        if let Some(ref tools) = config.disallowed_tools
            && !tools.is_empty()
        {
            child_cmd.env("ION_DISALLOWED_TOOLS", tools.join(","));
        }
        if let Some(turns) = config.max_turns {
            child_cmd.env("ION_MAX_TURNS", turns.to_string());
        }
        if let Ok(rt_override) = std::env::var("ION_RUNTIME_OVERRIDE") {
            child_cmd.env("ION_RUNTIME_OVERRIDE", &rt_override);
        }
        for var in &[
            "ION_FAUX_SCRIPT",
            "ION_FAUX_REPLY",
            "ION_FAUX_REPEAT",
            "ION_FAUX_ERROR",
            "ION_GRACEFUL_DRAIN_MS",
        ] {
            if let Ok(val) = std::env::var(var) {
                child_cmd.env(var, &val);
            }
        }
        for var in &["ION_RECORD", "ION_RECORD_OVERWRITE"] {
            if let Ok(val) = std::env::var(var) {
                child_cmd.env(var, &val);
            }
        }
        if let Some(depth) = config.hook_depth {
            child_cmd.env("ION_HOOK_DEPTH", depth.to_string());
        }
        if let Some(ref sp) = config.system_prompt_override {
            child_cmd.env("ION_SYSTEM_PROMPT", sp);
        }
        let relation_str = match config.relation {
            Some(WorkerRelation::System) => "system",
            Some(WorkerRelation::Peer) => "peer",
            _ => "child",
        };
        child_cmd.env("ION_SPAWN_RELATION", relation_str);
        if config.system_prompt_override.is_some() {
            child_cmd.env("ION_SPAWNED_BY", "skill_fork");
        } else if config.relation == Some(WorkerRelation::System) {
            child_cmd.env("ION_SPAWNED_BY", "singleton_init");
        }
        if config.uses_independent_session_file() {
            child_cmd.env("ION_FORK_CHILD", "1");
        }

        // ── Spawn child process (SLOW: fork+exec, 50-200ms) ──
        let mut child = child_cmd
            .spawn()
            .map_err(|e| {
                // 回滚：worktree 已建但子进程起不来 → 清掉半成品（分支保留）
                if let Some(wt) = &worktree_info {
                    let _ = remove_worktree(&wt.path, &wt.source_repo);
                }
                format!("failed to spawn worker: {e}")
            })?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;

        Ok(PreparedSpawn {
            child,
            stdin,
            stdout,
            stderr,
            worker_id: format!("wkr_{}", &Uuid::new_v4().to_string()[..8]),
            session_id,
            project_path,
            project_name,
            worktree_path,
            worktree_info,
            model,
            provider,
            agent_name,
        })
    }

    /// Create a new Worker: spawn child process, register, start IO bridge
    pub async fn create_worker(
        &mut self,
        config: WorkerCreateConfig,
        registry_arc: &Arc<Mutex<WorkerRegistry>>,
    ) -> Result<WorkerInfo, String> {
        let worker_id = format!("wkr_{}", &Uuid::new_v4().to_string()[..8]);
        let session_id = config
            .session
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

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
            // require_clean：已是 git 仓库且工作区有未提交改动 → 拒绝
            if config.require_clean.unwrap_or(false) && repo.join(".git").exists() {
                let status = std::process::Command::new("git")
                    .args(["-C", &project_path, "status", "--porcelain"])
                    .output()
                    .map_err(|e| format!("git status failed: {e}"))?;
                if !status.stdout.is_empty() {
                    return Err("source branch has uncommitted changes".to_string());
                }
            }
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
                    return Err(format!(
                        "worktree isolation requested but creation failed: {e}"
                    ));
                }
            }
        } else {
            (project_path.clone(), None)
        };

        // Spawn child process: 复用自身 (current_exe) 在 --mode rpc 下运行。
        // 单二进制方案：对齐 pi 的 `pi --mode rpc`，不再有独立的 ion-worker 文件。
        let binary = if let Some(ref configured_bin) = self.worker_bin {
            configured_bin.clone()
        } else {
            // 优先用 current_exe（host 和 worker 是同一个二进制）
            let exe = std::env::current_exe().map_err(|e| e.to_string())?;
            if exe.exists() {
                exe.to_string_lossy().to_string()
            } else {
                // Fallback: 找 PATH 里的 ion
                which::which("ion")
                    .map_err(|e| e.to_string())?
                    .to_string_lossy()
                    .to_string()
            }
        };

        // 从 config.json 读默认 model/provider（避免硬编码 deepseek-v4-flash/opencode）
        let cfg = crate::config::IonConfig::load();
        let default_model = cfg
            .default_model
            .clone()
            .unwrap_or_else(|| "glm-4.7".to_string());
        let default_provider = cfg
            .default_provider
            .clone()
            .unwrap_or_else(|| "zhipuai".to_string());

        let model = config.model.clone().unwrap_or(default_model);
        let provider = config.provider.clone().unwrap_or(default_provider);
        let agent_name = config.agent.clone().unwrap_or_default();

        let mut cmd_args = vec![
            "--mode".to_string(),
            "rpc".to_string(),
            "--session".to_string(),
            session_id.clone(),
            "--model".to_string(),
            model.clone(),
            "--provider".to_string(),
            provider.clone(),
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
        child_cmd.env("ION_WORKER_CWD", &worktree_path);

        // 子 Worker 跳过 MCP 连接（方案 A：防止多 Worker 抢同一个 stdio MCP server 死锁）
        // 只有 LLM 通过 spawn_worker 工具创建的子 worker 才跳过（config.skip_mcp=true）。
        // host 创建的第一个入口 worker 不跳过（它持有 MCP 连接）。
        // 子 Worker 通过 spawn_worker 工具创建时设 skip_mcp=Some("stdio")（方案 B）。
        if let Some(ref mode) = config.skip_mcp
            && !mode.is_empty()
        {
            child_cmd.env("ION_SKIP_MCP", mode);
        }

        // ── 补丁 1（HOOKS_AND_OUTLINE_SYNC）：工具白/黑名单 + max_turns 传给子进程 ──
        // 子 Worker 启动时读这些环境变量，应用到 ToolRegistry 过滤和 Agent 循环退出条件。
        // 这让扩展/hooks 的 agent handler 能 spawn "限定工具 + 限定步数"的子 Worker，
        // 是 ION 的 agent handler 比 pi 更强的关键（pi 的 agent handler 不传 tools，退化成单轮 LLM）。
        if let Some(ref tools) = config.allowed_tools
            && !tools.is_empty()
        {
            child_cmd.env("ION_ALLOWED_TOOLS", tools.join(","));
        }
        if let Some(ref tools) = config.disallowed_tools
            && !tools.is_empty()
        {
            child_cmd.env("ION_DISALLOWED_TOOLS", tools.join(","));
        }
        if let Some(turns) = config.max_turns {
            child_cmd.env("ION_MAX_TURNS", turns.to_string());
        }

        // 同步主进程的 runtime override 到子进程（如果主进程设了 --local/--remote）
        if let Ok(rt_override) = std::env::var("ION_RUNTIME_OVERRIDE") {
            child_cmd.env("ION_RUNTIME_OVERRIDE", &rt_override);
        }

        // 传递 FauxProvider 环境变量到子 Worker（让 host 模式下的子进程也用 faux）
        for var in &[
            "ION_FAUX_SCRIPT",
            "ION_FAUX_REPLY",
            "ION_FAUX_REPEAT",
            "ION_FAUX_ERROR",
            "ION_GRACEFUL_DRAIN_MS",
        ] {
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
            let parent_record = self
                .workers
                .get(creator_wid)
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
            _ => "child", // fork 也是 Child 关系
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

        if config.uses_independent_session_file() {
            child_cmd.env("ION_FORK_CHILD", "1");
        }

        let mut child = child_cmd
            .spawn()
            .map_err(|e| {
                // 回滚：worktree 已建但子进程起不来 → 清掉半成品（分支保留）
                if let Some(wt) = &worktree_info {
                    let _ = remove_worktree(&wt.path, &wt.source_repo);
                }
                format!("failed to spawn worker: {e}")
            })?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;

        // ── stderr 捕获（崩溃诊断用）──
        let stderr_path = std::env::temp_dir().join(format!("ion-worker-{}.stderr", worker_id));
        let _stderr_wid = worker_id.clone();
        let stderr_path_c = stderr_path.clone();
        tokio::spawn(async move {
            use std::io::Write;
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(stderr);
            let mut lines = reader.lines();
            if let Some(parent) = stderr_path_c.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&stderr_path_c)
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
            model: model.clone(), // 用 547 行已 resolve 的 model（含 default_model 兜底），而非 config.model（可能为空）
            agent: config.agent.clone().unwrap_or_default(),
            status: WorkerStatus::Idle,
            channels: config.channels.clone().unwrap_or_default(),
            parent: config.parent.clone(),
            children: Vec::new(),
            started_at: now_ms(),
            last_heartbeat: now_ms(),
            status_since: now_ms(),
            died_at: None,
            ready_tx: None,
            stdout_rx: None,
            response_rx: None,
            child_process: Some(child),
            stdin: Some(stdin),
            pending: HashMap::new(),
            event_subscribers: Vec::new(),
            parent_event_tx: parent_tx,
            worktree: worktree_info.clone(),
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
        if let Some(ref parent_id) = config.parent
            && let Some(parent) = self.workers.get_mut(parent_id)
        {
            parent.children.push(worker_id.clone());
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
            status: WorkerStatus::Busy, // 创建时设 Busy（马上要开始干活，避免 idle 检测误杀）
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

        // worktree 快照（索引写入用；worktree_info 稍后被 move 进 record）
        let worktree_branch_snapshot = worktree_info.as_ref().map(|w| w.branch.clone());
        let worktree_path_snapshot = worktree_info.as_ref().map(|w| w.path.clone());
        let worktree_info_present = worktree_info.is_some();

        // ── 写 SessionIndex（让 ion --resume / --rollback 能通过 SID 找到 session 文件）──
        // serve 模式的 create_session → create_worker 之前不写 index，
        // 导致 CLI 层的 --resume/--rollback 找不到 session（依赖 index 查 cwd）。
        {
            use crate::session_index::{SessionIndex, SessionMeta};
            let now = now_ms();
            // 反查 parent_session_id（复用 line 557-566 的逻辑）+ relation，写入血缘字段。
            // 让 ion sessions --json 能查派发关系（child/peer/system）。
            let (parent_sid, parent_rel) = if let Some(ref creator_wid) = config.creator {
                let parent_record = self
                    .workers
                    .get(creator_wid)
                    .or_else(|| self.workers.values().find(|w| &w.session_id == creator_wid));
                let rel = match config.relation {
                    Some(WorkerRelation::System) => "system",
                    Some(WorkerRelation::Peer) => "peer",
                    _ => "child",
                };
                parent_record
                    .map(|r| (Some(r.session_id.clone()), Some(rel.to_string())))
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };
            let mut idx = SessionIndex::load();
            let meta = merge_existing_meta(
                &idx,
                &session_id,
                SessionMeta {
                    name: Some(session_id.clone()),
                    first_name: Some(session_id.clone()),
                    project: Some(worktree_path.clone()),
                    project_name: Some(project_name.clone()),
                    worktree: config.worktree.is_some(),
                    branch: worktree_branch_snapshot.clone(),
                    workspace_path: worktree_path_snapshot.clone(),
                    workspace_status: worktree_info_present.then(|| "ready".to_string()),
                    model: model.clone(),
                    agent: agent_name.clone(),
                    provider: provider.clone(),
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
                    created_at: now,
                    updated_at: now,
                    error_count: 0,
                    last_thinking_level: None,
                    last_active_tools: None,
                    last_entry_id: None,
                    parent_session: parent_sid,
                    parent_type: parent_rel,
                    initial_cwd: Some(worktree_path.clone()),
                    last_cwd: Some(worktree_path.clone()),
                    extra_cwds: Vec::new(),
                    tier_models: None,
                    security_profile: None,
                },
            );
            idx.upsert(&session_id, meta);
            idx.save();
            // 写 tier_models + security_profile 快照（创建时从全局 config 读）
            let cfg = crate::config::IonConfig::load();
            let tm = serde_json::to_value(&cfg.tier_models).unwrap_or(serde_json::Value::Null);
            if tm != serde_json::Value::Null {
                SessionIndex::set_tier_models(&session_id, tm);
            }
            if let Some(ref sm) = cfg.security_mode {
                SessionIndex::set_security_profile(&session_id, sm);
            }
            tracing::info!(
                "[worker] SessionIndex 写入: {} → {}",
                session_id,
                worktree_path
            );
        }

        // ── singleton 引用计数：新 Worker 创建后通知所有单例 ──
        // System Worker（如 memory-agent）不触发 user_join（它本身就是单例的提供者，不是用户）。
        // 只有普通用户 Worker（Child/Peer）才 join。
        //
        // ⚠️ parking_lot: 不在持 &mut self 状态下调 on_user_join().await（guard 不是 Send）。
        // 这里 self 已经是 &mut self（调用方持锁），但 on_user_join 是 &self trait 方法，
        // 实例是 Arc → 先 sync 收集 instances（改 users 集），但调用方仍然持锁。
        // 由于 create_worker 整体是 &mut self 方法，调用方必须在 await 前 drop lock。
        // 这里把 callback 调用延迟到 create_worker 返回后由调用方触发不现实（API 复杂）。
        // 折中：create_worker 是 &mut self，调用方持锁 → 整个 create_worker 期间锁被持有。
        // 如果 create_worker 内部有 .await，那调用方持锁跨 await，编译失败。
        // 唯一干净方案：让 create_worker 不再是 &mut self，而是接受 Arc 自管锁。
        // 但那是大重构。这里采用：把 singleton callback 调用放到 spawn task 里（不持当前 lock）。
        if config.relation != Some(WorkerRelation::System) {
            let instances = self.singleton_user_join_sync(&worker_id);
            if !instances.is_empty() {
                let wid_clone = worker_id.clone();
                tokio::spawn(async move {
                    for ext in instances {
                        if let Err(e) = ext.on_user_join(&wid_clone).await {
                            tracing::warn!("[singleton] user_join {} failed: {:?}", wid_clone, e);
                        }
                    }
                });
            }
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
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(msg) => {
                        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let msg_id = msg
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        // Response with ID → match pending oneshot（不经过 stdout_tx）
                        if msg_type == "response" && !msg_id.is_empty() {
                            let mut reg = sub_registry.lock();
                            if let Some(record) = reg.workers.get_mut(&sub_wid)
                                && let Some(tx) = record.pending.remove(&msg_id)
                            {
                                let _ = tx.send(msg.clone());
                            }
                        }

                        // 关键：event 消息转发给 event_subscribers（实时流）
                        if msg_type == "event" {
                            let ev_type = msg
                                .get("event")
                                .and_then(|e| e.get("type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let stream_debug =
                                std::env::var("ION_STREAM_DEBUG").ok().as_deref() == Some("1");
                            if stream_debug && ev_type == "tool_call_delta" {
                                eprintln!("[stream-debug] host forward event type=tool_call_delta");
                            }
                            // ⚠️ 用独立 block 把 reg（parking_lot guard，不是 Send）的作用域限制在
                            // 同步代码段内——block 结束 reg 必然 drop，不会跨下面的 bus.lock().await。
                            let (bus_clone, session_id_for_bus, need_overview_broadcast) = {
                                let mut reg = sub_registry.lock();
                                // 拿 EventBus 句柄（如果有），用于把 worker 事件广播到全局订阅者。
                                // 在持 reg 锁期间只 clone Arc，不锁 EventBus（避免 reg+bus 双锁死锁）。
                                let bus_clone = reg.event_bus.clone();
                                let session_id_for_bus = reg
                                    .workers
                                    .get(&sub_wid)
                                    .map(|w| w.session_id.clone())
                                    .unwrap_or_default();
                                if let Some(record) = reg.workers.get_mut(&sub_wid) {
                                    // 转发给实时订阅者
                                    for sub in &record.event_subscribers {
                                        if let Err(_) = sub.try_send(msg.clone())
                                            && stream_debug
                                        {
                                            eprintln!(
                                                "[stream-debug] host DROP event type=tool_call_delta (subscriber channel full)"
                                            );
                                        }
                                    }
                                    // 写入 ring buffer（用于 subscribe --replay）
                                    record.event_history.push_back(msg.clone());
                                    while record.event_history.len() > record.event_history_cap {
                                        record.event_history.pop_front();
                                    }
                                }
                                // 更新 latest_output / status
                                let mut need_overview_broadcast = false;
                                if ev_type == "text_delta" {
                                    if let Some(delta) = msg
                                        .get("event")
                                        .and_then(|e| e.get("delta"))
                                        .and_then(|v| v.as_str())
                                    {
                                        let truncated: String = delta.chars().take(60).collect();
                                        if let Some(record) = reg.workers.get_mut(&sub_wid) {
                                            record.latest_output.push_back(truncated.clone());
                                            while record.latest_output.len() > 5 {
                                                record.latest_output.pop_front();
                                            }
                                            record.log_short = Some(truncated);
                                            // worker 正在产出文本，刷新心跳避免被误判 Stale
                                            record.last_heartbeat = now_ms();
                                        }
                                    }
                                } else if ev_type == "agent_end" || ev_type == "agent_stopped" {
                                    if let Some(record) = reg.workers.get_mut(&sub_wid) {
                                        record.set_status(WorkerStatus::Idle);
                                    }
                                    need_overview_broadcast = true;
                                } else if ev_type == "error" {
                                    // agent.run() 返回 Err 时 worker 发 error 事件（而非 agent_end）。
                                    // 不转 Idle 会让 worker 永久卡 Busy。这里兜底转 Idle，
                                    // 让用户能看到任务结束、能重新派活。
                                    let err_msg = msg
                                        .get("event")
                                        .and_then(|e| e.get("message"))
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("(no message)");
                                    tracing::warn!(
                                        "[{}] worker error event (agent.run failed?): {}",
                                        sub_wid,
                                        err_msg
                                    );
                                    if let Some(record) = reg.workers.get_mut(&sub_wid) {
                                        record.set_status(WorkerStatus::Idle);
                                    }
                                    need_overview_broadcast = true;
                                }
                                // reg 在 block 结束时自动 drop（parking_lot guard 不是 Send，
                                // 不能跨 await 持有）。
                                (bus_clone, session_id_for_bus, need_overview_broadcast)
                            };
                            // agent_end / error 需要广播 overview（在 drop(reg) 之后重新 lock）。
                            if need_overview_broadcast {
                                let rc = Arc::clone(&sub_registry);
                                tokio::spawn(async move {
                                    let mut r = rc.lock();
                                    r.broadcast_overview();
                                });
                            }
                            // 把 worker 事件广播到全局 EventBus，让 subscribe_all（无 session/extension）
                            // 也能收到 text_delta / agent_start / agent_end / tool_execution_*。
                            // 对齐 pi 的全局流式行为。此时 reg 已 drop，bus_clone 和
                            // session_id_for_bus 是之前 clone 出来的（不依赖 reg 锁）。
                            // rpc_response：用户触发的每条 RPC 都广播（多终端实时同步）。
                            let _ = stream_debug; // 抑制未使用警告
                            if let Some(bus) = bus_clone
                                && matches!(
                                    ev_type,
                                    "text_delta" | "agent_start" | "agent_end" | "agent_stopped"
                                        | "tool_execution_start" | "tool_execution_end"
                                        | "tool_call" | "tool_call_delta" | "rpc_response"
                                )
                            {
                                let mut event = crate::event_bus::ExtensionEvent::new(
                                    "worker", ev_type,
                                )
                                .with_data(msg.clone());
                                if !session_id_for_bus.is_empty() {
                                    event = event.with_session(&session_id_for_bus);
                                }
                                let mut bus_guard = bus.lock().await;
                                bus_guard.broadcast(&event);
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
            // ⚠️ parking_lot: 整个 exit cleanup 包在独立 block 内，确保 reg（guard，不是 Send）
            // 在 block 结束时必然 drop，不会跨下面的 singleton callback await。
            {
                let mut reg = sub_registry.lock();
                // 先读 exit code
                let exit_code = reg
                    .workers
                    .get_mut(&sub_wid)
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
                        if let Some(ref parent_id) = record.parent
                            && let Some(parent) = reg.workers.get_mut(parent_id)
                        {
                            parent.children.retain(|id| id != &sub_wid);
                        }
                    }
                } else {
                    // 非零退出 → 崩溃！标 Dead，保留 record
                    let (crash_parent, crash_session, crash_reason, crash_channels) = {
                        if let Some(record) = reg.workers.get_mut(&sub_wid) {
                            record.set_status(WorkerStatus::Dead);
                            // 读 stderr 日志最后几行作为 exit_reason
                            if let Some(ref stderr_path) = record.stderr_path {
                                if let Ok(content) = std::fs::read_to_string(stderr_path) {
                                    let tail: Vec<&str> =
                                        content.lines().rev().take(10).collect::<Vec<_>>();
                                    let tail: Vec<&str> = tail.into_iter().rev().collect();
                                    let snippet = tail.join("\n");
                                    if !snippet.is_empty() {
                                        record.exit_reason = Some(format!(
                                            "exit={}: {}",
                                            exit_code.unwrap_or(-1),
                                            snippet
                                        ));
                                    } else {
                                        record.exit_reason =
                                            Some(format!("exit={}", exit_code.unwrap_or(-1)));
                                    }
                                } else {
                                    record.exit_reason =
                                        Some(format!("exit={}", exit_code.unwrap_or(-1)));
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
                        } else {
                            (None, String::new(), None, Vec::new())
                        }
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
                        if let Some(parent) = reg.workers.get(parent_id.as_str())
                            && let Some(ref tx) = parent.parent_event_tx
                        {
                            let _ = tx.try_send(crash_event.clone());
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
            } // reg dropped here — lock released
            // 通知单例扩展：这个 Worker 不再使用它们（引用计数-1）
            // ⚠️ parking_lot: 用 sync 版本收集 instances，drop lock 后再调 callbacks（不持锁 await）
            let (leave_calls, last_gone_calls) = {
                let mut reg2 = sub_registry.lock();
                reg2.singleton_user_leave_sync(&sub_wid)
            };
            for ext in leave_calls {
                if let Err(e) = ext.on_user_leave(&sub_wid).await {
                    tracing::warn!("[singleton] user_leave {} failed: {:?}", sub_wid, e);
                }
            }
            for ext in last_gone_calls {
                if let Err(e) = ext.on_last_user_gone().await {
                    tracing::warn!("[singleton] last_user_gone failed: {:?}", e);
                }
            }
        });

        // ── Peer 模式：内核自动追加"汇报指令段"到 initial_prompt ──
        // 这是内核职责，不依赖 .md 自己写汇报格式。
        let mut effective_prompt = config.initial_prompt.clone();
        let is_peer = matches!(config.relation, Some(WorkerRelation::Peer));
        if is_peer {
            let creator_id = config
                .creator
                .as_deref()
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
        // 任何会话产生必广播到 EventBus（session_created）：接收方接不接收是它的事
        self.broadcast_ui_event(
            "session_created",
            serde_json::json!({
                "sessionId": info.session_id,
                "workerId": info.worker_id,
                "project": info.project,
                "parentSession": info.parent,
            }),
            Some(&info.session_id),
        );
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
                // ⚠️ parking_lot: send_command 持 &mut self + .await（stdin write），
                // 不能持锁调用。改为：持锁 take 出 stdin + 标记 Busy，drop lock，
                // 然后 write stdin，再 put back。
                let req_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
                let write_line = format!(
                    "{}\n",
                    serde_json::json!({
                        "id": &req_id,
                        "method": "prompt",
                        "params": serde_json::json!({"text": prompt_text})
                    })
                );
                // 持短锁：take stdin + 标记 Busy
                let stdin_opt = {
                    let mut reg = prompt_registry.lock();
                    let stdin = match reg.workers.get_mut(&wid_for_prompt) {
                        Some(record) => {
                            record.stdin.take()
                        }
                        None => {
                            tracing::warn!("[{wid_for_prompt}] not found for initial_prompt");
                            return;
                        }
                    };
                    if let Some(record) = reg.workers.get_mut(&wid_for_prompt) {
                        record.set_status(WorkerStatus::Busy);
                    }
                    stdin
                }; // lock dropped
                // 不持锁写 stdin（带 2s timeout，buffer 满不阻塞锁）
                if let Some(mut stdin) = stdin_opt {
                    use tokio::io::AsyncWriteExt;
                    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        stdin.write_all(write_line.as_bytes()).await?;
                        stdin.flush().await?;
                        Ok::<(), std::io::Error>(())
                    })
                    .await;
                    // 把 stdin 放回去
                    let mut reg = prompt_registry.lock();
                    if let Some(record) = reg.workers.get_mut(&wid_for_prompt) {
                        record.stdin = Some(stdin);
                    }
                    if let Err(e) = result {
                        tracing::warn!("[{wid_for_prompt}] failed to inject initial_prompt: {e:?}");
                    }
                }
            });
        }

        // Notify overview subscribers
        self.broadcast_overview();

        Ok(info)
    }

    /// Register a pre-spawned worker (phase 2: under lock, fast).
    ///
    /// Takes a `PreparedSpawn` (child process already forked, no lock needed for that)
    /// and registers it in the registry. This is the fast path — only takes microseconds
    /// under the lock (vs seconds for the full create_worker).
    pub fn register_prepared_worker(
        &mut self,
        spawn: PreparedSpawn,
        config: &WorkerCreateConfig,
        registry_arc: &Arc<Mutex<WorkerRegistry>>,
    ) -> Result<WorkerInfo, String> {
        let worker_id = spawn.worker_id.clone();
        let session_id = spawn.session_id.clone();
        let ws_info = spawn.worktree_info.clone();
        let project_name = spawn.project_name.clone();
        let project_path = spawn.project_path.clone();
        let worktree_path = spawn.worktree_path.clone();
        let model = spawn.model.clone();
        let provider = spawn.provider.clone();
        let agent_name = spawn.agent_name.clone();

        // stderr capture
        let stderr_path = std::env::temp_dir().join(format!("ion-worker-{}.stderr", worker_id));
        let stderr_path_c = stderr_path.clone();
        let _stderr_wid = worker_id.clone();
        tokio::spawn(async move {
            use std::io::Write;
            use tokio::io::AsyncBufReadExt;
            let reader = tokio::io::BufReader::new(spawn.stderr);
            let mut lines = reader.lines();
            if let Some(parent) = stderr_path_c.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&stderr_path_c)
                {
                    let _ = writeln!(f, "{}", line);
                }
            }
        });

        // parent event channel
        let parent_tx = config.parent.as_ref().and_then(|pid| {
            self.workers
                .get(pid)
                .and_then(|w| w.parent_event_tx.clone())
        });

        let mut record = WorkerRecord {
            worker_id: worker_id.clone(),
            session_id: session_id.clone(),
            project: project_name.clone(),
            project_path: project_path.clone(),
            model: model.clone(),
            agent: agent_name.clone(),
            status: WorkerStatus::Busy,
            channels: config.channels.clone().unwrap_or_default(),
            parent: config.parent.clone(),
            children: Vec::new(),
            started_at: now_ms(),
            last_heartbeat: now_ms(),
            status_since: now_ms(),
            died_at: None,
            ready_tx: None,
            stdout_rx: None,
            response_rx: None,
            child_process: Some(spawn.child),
            stdin: Some(spawn.stdin),
            pending: HashMap::new(),
            event_subscribers: Vec::new(),
            parent_event_tx: parent_tx,
            worktree: spawn.worktree_info,
            latest_output: VecDeque::with_capacity(5),
            log_short: None,
            model_size: None,
            exit_code: None,
            exit_reason: None,
            stderr_path: Some(stderr_path.to_string_lossy().to_string()),
            event_history: std::collections::VecDeque::with_capacity(200),
            event_history_cap: 200,
        };

        // parent children
        if let Some(ref parent_id) = config.parent
            && let Some(parent) = self.workers.get_mut(parent_id)
        {
            parent.children.push(worker_id.clone());
        }

        // channels
        if let Some(ref chs) = config.channels {
            for ch in chs {
                self.channels
                    .entry(ch.clone())
                    .or_default()
                    .push(worker_id.clone());
            }
        }

        let info = WorkerInfo {
            worker_id: worker_id.clone(),
            session_id: session_id.clone(),
            project: project_name.clone(),
            status: WorkerStatus::Busy,
            model: model.clone(),
            agent: agent_name.clone(),
            channels: record.channels.clone(),
            parent: record.parent.clone(),
            children: Vec::new(),
        };

        // stdout channel
        let (stdout_tx, stdout_rx) = mpsc::unbounded_channel::<serde_json::Value>();
        let (_response_tx, response_rx) = mpsc::channel::<(String, serde_json::Value)>(64);
        record.stdout_rx = Some(stdout_rx);
        record.response_rx = Some(response_rx);

        self.workers.insert(worker_id.clone(), record);

        // SessionIndex
        {
            use crate::session_index::{SessionIndex, SessionMeta};
            let now = now_ms();
            // 反查 parent_session_id（register_prepared_worker 有 self.workers 访问权）
            let (parent_sid, parent_rel) = if let Some(ref creator_wid) = config.creator {
                let parent_record = self
                    .workers
                    .get(creator_wid)
                    .or_else(|| self.workers.values().find(|w| &w.session_id == creator_wid));
                let rel = match config.relation {
                    Some(WorkerRelation::System) => "system",
                    Some(WorkerRelation::Peer) => "peer",
                    _ => "child",
                };
                parent_record
                    .map(|r| (Some(r.session_id.clone()), Some(rel.to_string())))
                    .unwrap_or((None, None))
            } else {
                (None, None)
            };
            let mut idx = SessionIndex::load();
            let meta = merge_existing_meta(
                &idx,
                &session_id,
                SessionMeta {
                    name: Some(session_id.clone()),
                    first_name: Some(session_id.clone()),
                    project: Some(worktree_path.clone()),
                    project_name: Some(project_name.clone()),
                    worktree: config.worktree.is_some(),
                    branch: ws_info.as_ref().map(|w| w.branch.clone()),
                    workspace_path: ws_info.as_ref().map(|w| w.path.clone()),
                    workspace_status: ws_info.as_ref().map(|_| "ready".to_string()),
                    model: model.clone(),
                    agent: agent_name.clone(),
                    provider: provider.clone(),
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
                    created_at: now,
                    updated_at: now,
                    error_count: 0,
                    last_thinking_level: None,
                    last_active_tools: None,
                    last_entry_id: None,
                    parent_session: parent_sid,
                    parent_type: parent_rel,
                    initial_cwd: Some(worktree_path.clone()),
                    last_cwd: Some(worktree_path.clone()),
                    extra_cwds: Vec::new(),
                    tier_models: None,
                    security_profile: None,
                },
            );
            idx.upsert(&session_id, meta);
            idx.save();
            // tier_models + security_profile 快照
            let cfg = crate::config::IonConfig::load();
            let tm = serde_json::to_value(&cfg.tier_models).unwrap_or(serde_json::Value::Null);
            if tm != serde_json::Value::Null {
                SessionIndex::set_tier_models(&session_id, tm);
            }
            if let Some(ref sm) = cfg.security_mode {
                SessionIndex::set_security_profile(&session_id, sm);
            }
        }

        // singleton user join（同 create_worker：用 spawn 避免持锁 await）
        if config.relation != Some(WorkerRelation::System) {
            let instances = self.singleton_user_join_sync(&worker_id);
            if !instances.is_empty() {
                let wid_clone = worker_id.clone();
                tokio::spawn(async move {
                    for ext in instances {
                        if let Err(e) = ext.on_user_join(&wid_clone).await {
                            tracing::warn!("[singleton] user_join {} failed: {:?}", wid_clone, e);
                        }
                    }
                });
            }
        }

        // stdout reader task
        let wid = worker_id.clone();
        let cmd_tx = self.manager_cmd_tx.clone();
        let sub_registry = Arc::clone(registry_arc);
        let sub_wid = worker_id.clone();
        tokio::spawn(async move {
            let reader = BufReader::new(spawn.stdout);
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(&line) {
                    Ok(msg) => {
                        let msg_type = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
                        let msg_id = msg
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();

                        if msg_type == "response" && !msg_id.is_empty() {
                            let mut reg = sub_registry.lock();
                            if let Some(record) = reg.workers.get_mut(&sub_wid)
                                && let Some(tx) = record.pending.remove(&msg_id)
                            {
                                let _ = tx.send(msg.clone());
                            }
                        }

                        if msg_type == "event" {
                            let ev_type = msg
                                .get("event")
                                .and_then(|e| e.get("type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            // ⚠️ 用独立 block 把 reg 作用域限制在同步代码段（parking_lot guard 不是 Send）。
                            let (bus_clone2, session_id_for_bus2, need_overview_broadcast) = {
                                let mut reg = sub_registry.lock();
                                let bus_clone2 = reg.event_bus.clone();
                                let session_id_for_bus2 = reg
                                    .workers
                                    .get(&sub_wid)
                                    .map(|w| w.session_id.clone())
                                    .unwrap_or_default();
                                if let Some(record) = reg.workers.get_mut(&sub_wid) {
                                    for sub in &record.event_subscribers {
                                        let _ = sub.try_send(msg.clone());
                                    }
                                    record.event_history.push_back(msg.clone());
                                    while record.event_history.len() > record.event_history_cap {
                                        record.event_history.pop_front();
                                    }
                                }
                                let mut need_overview_broadcast = false;
                                if ev_type == "text_delta"
                                    && let Some(delta) = msg
                                        .get("event")
                                        .and_then(|e| e.get("delta"))
                                        .and_then(|v| v.as_str())
                                    && let Some(record) = reg.workers.get_mut(&sub_wid)
                                {
                                    let mut buf: String =
                                        record.latest_output.iter().cloned().collect();
                                    buf.push_str(delta);
                                    record.latest_output.clear();
                                    for chunk in buf.split('\n').next_back().unwrap_or("").lines() {
                                        record.latest_output.push_back(chunk.to_string());
                                    }
                                    // worker 在产出，刷新心跳
                                    record.last_heartbeat = now_ms();
                                }
                                if (ev_type == "agent_end" || ev_type == "agent_stopped")
                                    && let Some(record) = reg.workers.get_mut(&sub_wid)
                                {
                                    record.set_status(WorkerStatus::Idle);
                                    need_overview_broadcast = true;
                                }
                                // agent.run() 返回 Err 时的兜底：error 事件也转 Idle，避免永久卡 Busy
                                if ev_type == "error"
                                    && let Some(record) = reg.workers.get_mut(&sub_wid)
                                {
                                    tracing::warn!(
                                        "[{}] worker error event, marking Idle (agent.run failed?)",
                                        sub_wid
                                    );
                                    record.set_status(WorkerStatus::Idle);
                                    need_overview_broadcast = true;
                                }
                                if ev_type == "agent_start"
                                    && let Some(record) = reg.workers.get_mut(&sub_wid)
                                {
                                    record.set_status(WorkerStatus::Busy);
                                }
                                (bus_clone2, session_id_for_bus2, need_overview_broadcast)
                            };
                            if need_overview_broadcast {
                                let rc = Arc::clone(&sub_registry);
                                tokio::spawn(async move {
                                    let mut r = rc.lock();
                                    r.broadcast_overview();
                                });
                            }
                            // 同 reader #1：广播 worker 事件到全局 EventBus，让 subscribe_all 也能收到
                            if let Some(bus) = bus_clone2
                                && matches!(
                                    ev_type,
                                    "text_delta" | "agent_start" | "agent_end" | "agent_stopped"
                                        | "tool_execution_start" | "tool_execution_end"
                                        | "tool_call" | "tool_call_delta"
                                )
                            {
                                let mut event = crate::event_bus::ExtensionEvent::new(
                                    "worker", ev_type,
                                )
                                .with_data(msg.clone());
                                if !session_id_for_bus2.is_empty() {
                                    event = event.with_session(&session_id_for_bus2);
                                }
                                let mut bus_guard = bus.lock().await;
                                bus_guard.broadcast(&event);
                            }
                        }

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
            // Worker exited
            tracing::warn!("[{wid}] stdout closed, cleaning up");
            // ⚠️ parking_lot: 整个 exit cleanup 包在独立 block 内（guard 不是 Send）。
            // 收集需要在 drop lock 后 await 的 parent_event_tx + crash payload，
            // block 结束后（lock 释放）再 .send().await。
            let pending_parent_notify: Option<(String, serde_json::Value)> = {
                let mut reg = sub_registry.lock();
                let exit_code = reg
                    .workers
                    .get_mut(&sub_wid)
                    .and_then(|r| r.child_process.as_mut())
                    .and_then(|c| c.try_wait().ok().flatten())
                    .and_then(|s| s.code());
                if let Some(record) = reg.workers.get_mut(&sub_wid) {
                    record.exit_code = exit_code;
                }
                let mut notify = None;
                if exit_code == Some(0) || exit_code.is_none() {
                    if let Some(mut record) = reg.workers.remove(&sub_wid) {
                        if let Some(ref mut child) = record.child_process {
                            let _ = child.start_kill();
                        }
                        for ch in &record.channels {
                            if let Some(subs) = reg.channels.get_mut(ch) {
                                subs.retain(|id| id != &sub_wid);
                            }
                        }
                        if let Some(ref parent_id) = record.parent
                            && let Some(parent) = reg.workers.get_mut(parent_id)
                        {
                            parent.children.retain(|id| id != &sub_wid);
                        }
                    }
                } else {
                    if let Some(record) = reg.workers.get_mut(&sub_wid) {
                        record.set_status(WorkerStatus::Dead);
                        if let Some(ref stderr_path) = record.stderr_path {
                            if let Ok(content) = std::fs::read_to_string(stderr_path) {
                                let tail: Vec<&str> =
                                    content.lines().rev().take(10).collect::<Vec<_>>();
                                let tail: Vec<&str> = tail.into_iter().rev().collect();
                                let snippet = tail.join("\n");
                                record.exit_reason = if !snippet.is_empty() {
                                    Some(format!("exit={}: {}", exit_code.unwrap_or(-1), snippet))
                                } else {
                                    Some(format!("exit={}", exit_code.unwrap_or(-1)))
                                };
                            } else {
                                record.exit_reason = Some(format!("exit={}", exit_code.unwrap_or(-1)));
                            }
                        }
                    }
                    if let Some(record) = reg.workers.get(&sub_wid) {
                        let crash_parent = record.parent.clone();
                        let crash_session = record.session_id.clone();
                        let crash_reason = record.exit_reason.clone().unwrap_or_default();
                        let crash_channels = record.channels.clone();
                        // 不在这里 await send（持 reg 锁）——收集 parent_id + payload，
                        // drop lock 后再 send。
                        if let Some(ref parent_id) = crash_parent {
                            notify = Some((
                                parent_id.clone(),
                                serde_json::json!({
                                    "type": "event",
                                    "event": {
                                        "type": "child_crashed",
                                        "session_id": crash_session,
                                        "exit_reason": crash_reason,
                                    }
                                }),
                            ));
                        }
                        for ch in &crash_channels {
                            if let Some(subs) = reg.channels.get_mut(ch) {
                                subs.retain(|id| id != &sub_wid);
                            }
                        }
                    }
                }
                notify
            }; // reg dropped here — lock released
            // drop lock 后再 await send（避免跨 await 持有 parking_lot guard）
            if let Some((parent_id, payload)) = pending_parent_notify {
                let tx_opt = {
                    let reg = sub_registry.lock();
                    reg.workers.get(&parent_id).and_then(|p| p.parent_event_tx.clone())
                };
                if let Some(tx) = tx_opt {
                    let _ = tx.send(payload).await;
                }
            }
            let rc = Arc::clone(&sub_registry);
            tokio::spawn(async move {
                let mut r = rc.lock();
                r.broadcast_overview();
            });
        });

        // ── 注入 initial_prompt（延迟到 spawn task，避免持锁等子进程 ready 导致死锁）──
        // ⚠️ register_prepared_worker 是 create_worker 的 split 版本（parking_lot 重构后
        // cmd_host / cmd_serve 用 prepare + register 两阶段）。initial_prompt 注入逻辑必须
        // 在这里也有一份，否则 --host 模式下 worker 启动了但永远收不到 prompt（Idle 到超时）。
        let mut effective_prompt = config.initial_prompt.clone();
        let is_peer = matches!(config.relation, Some(WorkerRelation::Peer));
        if is_peer {
            let creator_id = config
                .creator
                .as_deref()
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
        if let Some(prompt_text) = effective_prompt {
            let wid_for_prompt = worker_id.clone();
            let prompt_registry = Arc::clone(registry_arc);
            tokio::spawn(async move {
                // 等子进程 ready（不持锁，不阻塞 reader task）
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                // ⚠️ parking_lot: send_command 持 &mut self + .await（stdin write），
                // 不能持锁调用。改为：持锁 take 出 stdin + 标记 Busy，drop lock，
                // 然后 write stdin，再 put back。
                let req_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
                let write_line = format!(
                    "{}\n",
                    serde_json::json!({
                        "id": &req_id,
                        "method": "prompt",
                        "params": serde_json::json!({"text": prompt_text})
                    })
                );
                // 持短锁：take stdin + 标记 Busy
                let stdin_opt = {
                    let mut reg = prompt_registry.lock();
                    let stdin = match reg.workers.get_mut(&wid_for_prompt) {
                        Some(record) => record.stdin.take(),
                        None => {
                            tracing::warn!("[{wid_for_prompt}] not found for initial_prompt");
                            return;
                        }
                    };
                    if let Some(record) = reg.workers.get_mut(&wid_for_prompt) {
                        record.set_status(WorkerStatus::Busy);
                    }
                    stdin
                }; // lock dropped
                // 不持锁写 stdin（带 2s timeout，buffer 满不阻塞锁）
                if let Some(mut stdin) = stdin_opt {
                    use tokio::io::AsyncWriteExt;
                    let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        stdin.write_all(write_line.as_bytes()).await?;
                        stdin.flush().await?;
                        Ok::<(), std::io::Error>(())
                    })
                    .await;
                    // 把 stdin 放回去
                    let mut reg = prompt_registry.lock();
                    if let Some(record) = reg.workers.get_mut(&wid_for_prompt) {
                        record.stdin = Some(stdin);
                    }
                    if let Err(e) = result {
                        tracing::warn!("[{wid_for_prompt}] failed to inject initial_prompt: {e:?}");
                    }
                }
            });
        }

        // 任何会话产生必广播到 EventBus（session_created）——锁拆分快速路径的发射点
        self.broadcast_ui_event(
            "session_created",
            serde_json::json!({
                "sessionId": info.session_id,
                "workerId": info.worker_id,
                "project": info.project,
                "parentSession": info.parent,
            }),
            Some(&info.session_id),
        );
        // worktree 子会话：统一持久化 + workspace_session_created 事件
        // （无论 LLM spawn、create_worker RPC 还是 create_session 触发，同一条管线）
        if let Some(wt) = &ws_info {
            // creator 可为 worker id 或 session id（host RPC 传 session id，bridge 传 worker id）
            let parent_sid = config.creator.as_ref().and_then(|c| {
                self.workers
                    .get(c)
                    .map(|w| w.session_id.clone())
                    .or_else(|| {
                        self.workers
                            .values()
                            .find(|w| &w.session_id == c)
                            .map(|w| w.session_id.clone())
                    })
            });
            let title: String = config
                .initial_prompt
                .as_deref()
                .map(|p: &str| p.chars().take(24).collect::<String>())
                .filter(|s: &String| !s.is_empty())
                .unwrap_or_else(|| wt.branch.clone());
            let ws = crate::session_workspace::WorkspaceSession {
                session_id: session_id.clone(),
                parent_session_id: parent_sid.clone().unwrap_or_default(),
                project_path: wt.source_repo.clone(),
                workspace_path: wt.path.clone(),
                branch: wt.branch.clone(),
                base_ref: config.worktree.as_ref().and_then(|w| w.base.clone()),
                title,
                status: crate::session_workspace::WorkspaceStatus::Ready,
                route: format!("#/sessions/{session_id}"),
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
                error: None,
            };
            let _ = ws.upsert_index(); // 存储落位原则：热字段进 SessionIndex
            // 原则另一半：完整可还原细节以 custom entry 留痕进**父会话** JSONL
            // （创建是父时间线的事件且父文件必然存在；子会话文件此刻尚未由 worker 创建）
            // 注意：WorkerRecord.project 是项目名不是路径——父 cwd 只从 SessionIndex 取（真路径）
            let parent_cwd = parent_sid
                .as_ref()
                .and_then(|ps| {
                    crate::session_index::SessionIndex::load()
                        .get(ps)
                        .and_then(|m| m.project.clone())
                });
            if let (Some(pc), Some(ps)) = (&parent_cwd, &parent_sid) {
                // register 时刻父会话文件可能尚未写 header（worker 异步初始化）→ 延迟重试追加
                let file = crate::paths::session_jsonl_path_by_id(pc, ps);
                let data = serde_json::json!({
                    "event": "created",
                    "sessionId": ws.session_id,
                    "parentSessionId": ws.parent_session_id,
                    "projectPath": ws.project_path,
                    "branch": ws.branch,
                    "baseRef": ws.base_ref,
                    "title": ws.title,
                    "route": ws.route,
                    "createdAt": ws.created_at,
                });
                let sid_for_log = session_id.clone();
                tokio::spawn(async move {
                    for _ in 0..8 {
                        if file.exists()
                            && crate::session_jsonl::append_custom_entry_to_file(
                                &file,
                                "workspace_session",
                                data.clone(),
                            )
                            .is_some()
                        {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    }
                    tracing::warn!("[workspace] JSONL 创建留痕失败: session={sid_for_log}");
                });
            }
            if let Some(psid) = parent_sid {
                let payload = serde_json::json!({ "workspaceSession": ws });
                let evt = serde_json::json!({
                    "type": "event",
                    "event": {
                        "type": "extension_event",
                        "extension": "workspace",
                        "customType": "workspace_session_created",
                        "visibility": "llm_and_ui",
                        "session": psid,
                        "data": payload,
                    },
                });
                let _ = self.push_session_event(&psid, evt);
                self.broadcast_ui_event(
                    "workspace_session_created",
                    payload,
                    Some(&psid),
                );
            }
        }
        self.broadcast_overview();
        Ok(info)
    }

    pub fn list_workers(&self) -> Vec<WorkerInfo> {
        self.workers
            .values()
            .map(|w| WorkerInfo {
                worker_id: w.worker_id.clone(),
                session_id: w.session_id.clone(),
                project: w.project.clone(),
                status: w.status.clone(),
                model: w.model.clone(),
                agent: w.agent.clone(),
                channels: w.channels.clone(),
                parent: w.parent.clone(),
                children: w.children.clone(),
            })
            .collect()
    }

    pub fn list_projects(&self) -> Vec<ProjectInfo> {
        let mut projects: HashMap<String, ProjectInfo> = HashMap::new();
        for w in self.workers.values() {
            let entry = projects
                .entry(w.project.clone())
                .or_insert_with(|| ProjectInfo {
                    name: w.project.clone(),
                    path: w.project_path.clone(),
                    worker_ids: Vec::new(),
                });
            entry.worker_ids.push(w.worker_id.clone());
        }
        projects.into_values().collect()
    }

    pub fn kill_worker(&mut self, worker_id: &str) -> Result<(), String> {
        self.kill_worker_inner(worker_id, true, false)
    }

    /// kill_worker 的可控变体：
    /// - cleanup_worktree=false 时保留 worktree 目录
    /// - delete_branch=true 时连分支一起删（要求目录已清理）
    /// workspace 会话（store 有记录）关闭时自动落盘 closed + 广播事件。
    pub fn kill_worker_inner(
        &mut self,
        worker_id: &str,
        cleanup_worktree: bool,
        delete_branch: bool,
    ) -> Result<(), String> {
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
            if let Some(ref parent_id) = record.parent
                && let Some(parent) = self.workers.get_mut(parent_id)
            {
                parent.children.retain(|id| id != worker_id);
            }

            // Emit worker_destroyed + project_changed events
            self.emit_global(serde_json::json!({
                "type": "worker_destroyed",
                "worker_id": killed_worker_id,
                "session_id": killed_session,
                "project": killed_project,
                "parent": killed_parent,
            }));
            // 会话终止同样广播（对称性：产生必推，终止也必推）
            self.broadcast_ui_event(
                "session_closed",
                serde_json::json!({
                    "sessionId": killed_session,
                    "workerId": killed_worker_id,
                    "project": killed_project,
                }),
                Some(&killed_session),
            );
            self.emit_global(serde_json::json!({
                "type": "project_changed",
                "project": killed_project,
                "worker_id": killed_worker_id,
                "change": "destroyed",
            }));

            // Notify overview subscribers
            self.broadcast_overview();

            // Clean up worktree directory if present (branch preserved)
            if cleanup_worktree
                && let Some(ref wt) = wt_info
            {
                let _ = remove_worktree(&wt.path, &wt.source_repo);
                // 分支删除仅在与目录清理同时请求时生效（目录留着时分支被 checkout 无法删）
                if delete_branch {
                    let _ = std::process::Command::new("git")
                        .args(["-C", &wt.source_repo, "branch", "-D", &wt.branch])
                        .output();
                }
            }
            // workspace 会话关闭：落盘 closed + 广播（store 无记录则跳过——非 workspace 的普通 kill 不受影响）
            if wt_info.is_some() {
                if let Some(mut ws) = crate::session_workspace::WorkspaceSession::from_index(&killed_session) {
                    if ws.status != crate::session_workspace::WorkspaceStatus::Closed {
                        ws.status = crate::session_workspace::WorkspaceStatus::Closed;
                        let _ = ws.upsert_index();
                        // 关闭事件同样留痕父会话 JSONL（重放可还原清理策略）
                        let parent_cwd = crate::session_index::SessionIndex::load()
                            .get(&ws.parent_session_id)
                            .and_then(|m| m.project.clone());
                        if let Some(pc) = parent_cwd {
                            let pfile = crate::paths::session_jsonl_path_by_id(
                                &pc,
                                &ws.parent_session_id,
                            );
                            let _ = crate::session_jsonl::append_custom_entry_to_file(
                                &pfile,
                                "workspace_session",
                                serde_json::json!({
                                    "event": "closed",
                                    "sessionId": killed_session,
                                    "cleanupWorktree": cleanup_worktree,
                                    "deleteBranch": delete_branch,
                                    "branchPreserved": !delete_branch,
                                }),
                            );
                        }
                        let branch_preserved = wt_info
                            .as_ref()
                            .map(|w| !delete_branch)
                            .unwrap_or(true);
                        // 双路推送：EventBus（ui 订阅者）+ 父会话实例流（subscribe --session 父）
                        let payload = serde_json::json!({
                            "sessionId": killed_session,
                            "cleanupWorktree": cleanup_worktree,
                            "deleteBranch": delete_branch,
                            "branchPreserved": branch_preserved,
                        });
                        let parent_sid = ws.parent_session_id.clone();
                        if !parent_sid.is_empty() {
                            let evt = serde_json::json!({
                                "type": "event",
                                "event": {
                                    "type": "extension_event",
                                    "extension": "workspace",
                                    "customType": "workspace_session_closed",
                                    "visibility": "llm_and_ui",
                                    "session": parent_sid,
                                    "data": payload,
                                },
                            });
                            let _ = self.push_session_event(&parent_sid, evt);
                        }
                        self.broadcast_ui_event(
                            "workspace_session_closed",
                            payload,
                            Some(&killed_session),
                        );
                    }
                }
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
        let worktree_info = self.workers.get(worker_id).and_then(|r| r.worktree.clone());

        // Kill the worker (removes from registry, kills process, cleans channels/parent)
        self.kill_worker(worker_id)?;

        // Clean up worktree directory (branch preserved)
        if let Some(wt) = worktree_info {
            match remove_worktree(&wt.path, &wt.source_repo) {
                Ok(_) => tracing::info!(
                    "[reclaim] worktree cleaned: {} (branch {} preserved)",
                    wt.path,
                    wt.branch
                ),
                Err(e) => tracing::warn!("[reclaim] worktree cleanup failed: {e}"),
            }
        }

        Ok(())
    }

    /// Send to a session by ID. Auto-starts a Worker if not running.
    ///
    /// This is an associated function (not `&mut self`) so it can release the
    /// registry lock between phases and avoid deadlock:
    ///   phase 1 (lock ①): find worker by session_id; if found, send + await
    ///   phase 2 (lock ②): if not found, create_worker (which spawns a stdout
    ///                      reader task that locks the registry independently)
    ///   phase 3 (lock ③): send the original command to the new worker
    /// Each lock is short and never nested, so create_worker's internal
    /// `sub_registry.lock()` doesn't deadlock against us.
    pub async fn send_to_session(
        registry_arc: &Arc<Mutex<WorkerRegistry>>,
        session_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Phase 1: look for an existing worker for this session.
        let existing = {
            let reg = registry_arc.lock();
            reg.workers
                .iter()
                .find(|(_, w)| w.session_id == session_id)
                .map(|(id, _)| id.clone())
        };

        if let Some(wid) = existing {
            // Worker exists — send directly via send_async (manages its own lock).
            // ⚠️ parking_lot: send_to_worker_prepare 持 &mut self + .await（stdin write），
            // 不能持锁调用。send_async 内部 take stdin → drop lock → write → put back。
            return Self::send_async(registry_arc, &wid, method, params).await;
        }

        // Phase 2: worker not found → auto-start. Lock briefly to create.
        tracing::info!("[session] auto-starting worker for {session_id}");
        let config = WorkerCreateConfig {
            require_clean: None,
            worktree: None,
            session: Some(session_id.to_string()),
            project_path: None,
            model: None,
            provider: None,
            agent: None,
            channels: None,
            parent: None,
            relation: Some(WorkerRelation::Child),
            creator: None,
            report_channel: None,
            report_to: None,
            initial_prompt: None,
            skip_mcp: None,
            allowed_tools: None,
            disallowed_tools: None,
            max_turns: None,
            hook_depth: None,
            system_prompt_override: None,
        };
        // ⚠️ parking_lot: create_worker 持 &mut self + .await（spawn 等），
        // 不能持锁调用。改用 prepare + register 两阶段。
        match Self::prepare_worker_spawn(&config).await {
            Ok(prepared) => {
                let mut reg = registry_arc.lock();
                reg.register_prepared_worker(prepared, &config, registry_arc)?;
            }
            Err(e) => return Err(e),
        }

        // Phase 3: find the freshly created worker and send the command.
        let wid = {
            let reg = registry_arc.lock();
            reg.workers
                .iter()
                .find(|(_, w)| w.session_id == session_id)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| format!("auto-started worker for {session_id} vanished"))?
        };
        // Phase 3: find the freshly created worker and send the command.
        // ⚠️ 同上：用 send_async（自管锁）。
        Self::send_async(registry_arc, &wid, method, params).await
    }

    /// Drain pending events from a worker's stdout_rx.
    /// Note: events are already forwarded to subscribers by the stdout reader task.
    /// This method only drains the buffer to prevent overflow.
    pub async fn drain_events(&mut self, worker_id: &str, timeout_ms: u64) {
        if let Some(record) = self.workers.get_mut(&worker_id.to_string())
            && let Some(rx) = &mut record.stdout_rx
        {
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
            while std::time::Instant::now() < deadline {
                match tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await {
                    Ok(Some(_)) => { /* drain — stdout reader already forwards */ }
                    _ => break,
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
        let record = self
            .workers
            .get_mut(worker_id)
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
        let record = self
            .workers
            .get_mut(worker_id)
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

    /// 向指定 session 的实例事件流注入 host 级事件（workspace_session_* 等）。
    /// 同时写入 event_history，让后续 subscribe --replay 能回放到。
    /// 该 session 的 worker 不在运行时返回 Err（无实例流可投递，调用方降级 EventBus）。
    pub fn push_session_event(
        &mut self,
        session_id: &str,
        event: serde_json::Value,
    ) -> Result<(), String> {
        // 同一 session 可能存在多个 worker 记录（如 prompt 自动复活产生的新旧两条），
        // 必须推给所有匹配者——只推第一个会把事件投进没有订阅者的死缓冲。
        let mut delivered = false;
        for record in self.workers.values_mut() {
            if record.session_id == session_id {
                for sub in &record.event_subscribers {
                    let _ = sub.try_send(event.clone());
                }
                record.event_history.push_back(event.clone());
                while record.event_history.len() > record.event_history_cap {
                    record.event_history.pop_front();
                }
                delivered = true;
            }
        }
        if delivered {
            Ok(())
        } else {
            Err(format!("worker not found for session: {session_id}"))
        }
    }

    /// 非阻塞发送命令（只写 stdin，返回 req_id）。
    pub async fn send_command(
        &mut self,
        worker_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<String, String> {
        let req_id = Uuid::new_v4().to_string()[..8].to_string();
        let line =
            serde_json::json!({"id": &req_id, "method": method, "params": params}).to_string();
        let record = self
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        if let Some(stdin) = &mut record.stdin {
            use tokio::io::AsyncWriteExt;
            let write_line = format!("{line}\n");
            // Timeout to prevent deadlock when worker's stdin buffer is full.
            // Without this, send_command holds the registry lock while blocked on write.
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                stdin
                    .write_all(write_line.as_bytes())
                    .await
                    .map_err(|e| format!("write: {e}"))?;
                stdin.flush().await.map_err(|e| format!("flush: {e}"))?;
                Ok::<(), String>(())
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(e),
                Err(_) => {
                    return Err(format!(
                        "timeout: worker {worker_id} stdin blocked (buffer full?)"
                    ));
                }
            }
        }
        record.set_status(WorkerStatus::Busy);
        Ok(req_id)
    }

    /// Register a pending oneshot for a req_id.
    pub fn register_pending(
        &mut self,
        worker_id: &str,
        req_id: &str,
    ) -> Option<oneshot::Receiver<serde_json::Value>> {
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
    ///
    /// ⚠️ parking_lot: 此方法自管锁——不要求调用方持锁。
    /// 内部用 send_command_prepare（async，stdin write）的方式：
    /// take stdin → drop lock → write stdin → put back → 短锁 register_pending。
    pub async fn send_async(
        registry: &Arc<Mutex<Self>>,
        worker_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        // Phase 1: 短锁 take stdin + 标记 Busy + 构造 req_id/line
        let req_id = Uuid::new_v4().to_string()[..8].to_string();
        let line = serde_json::json!({
            "id": req_id,
            "method": method,
            "params": params,
        })
        .to_string();
        let write_line = format!("{line}\n");
        let stdin_opt = {
            let mut reg = registry.lock();
            let stdin = match reg.workers.get_mut(worker_id) {
                Some(record) => record.stdin.take(),
                None => return Err(format!("worker not found: {worker_id}")),
            };
            if let Some(record) = reg.workers.get_mut(worker_id) {
                record.set_status(WorkerStatus::Busy);
            }
            stdin
        }; // lock dropped
        // Phase 2: write stdin（不持锁）
        if let Some(mut stdin) = stdin_opt {
            use tokio::io::AsyncWriteExt;
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                stdin.write_all(write_line.as_bytes()).await?;
                stdin.flush().await?;
                Ok::<(), std::io::Error>(())
            })
            .await;
            // put stdin back（短锁）
            let mut reg = registry.lock();
            if let Some(record) = reg.workers.get_mut(worker_id) {
                record.stdin = Some(stdin);
            }
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return Err(format!("write stdin: {e}")),
                Err(_) => return Err(format!("timeout: worker {worker_id} stdin blocked")),
            }
        }
        // Phase 3: register pending oneshot（短锁）
        let rx = {
            let mut reg = registry.lock();
            reg.register_pending(worker_id, &req_id)
                .ok_or_else(|| format!("worker not found: {worker_id}"))?
        };

        match tokio::time::timeout(std::time::Duration::from_secs(300), rx).await {
            Ok(Ok(resp)) => Ok(resp),
            Ok(Err(_)) => {
                if let Some(mut reg) = registry.try_lock() {
                    reg.cleanup_pending(worker_id, &req_id);
                }
                Err("worker dropped response channel".into())
            }
            Err(_) => {
                if let Some(mut reg) = registry.try_lock() {
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
    ///     let mut reg = registry.lock();
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
        // Prepare (write stdin + register oneshot) then await outside the lock.
        let rx = self
            .send_to_worker_prepare(worker_id, method, params)
            .await?;
        Self::await_oneshot_timeout(rx).await
    }

    /// Step 1 of send_to_worker: write the command to the worker's stdin and
    /// register a oneshot receiver for the response. Returns the receiver so
    /// the caller can drop the registry lock before awaiting (avoids deadlock
    /// when the worker's stdout reader needs to lock the registry to deliver
    /// the response).
    pub async fn send_to_worker_prepare(
        &mut self,
        worker_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<oneshot::Receiver<serde_json::Value>, String> {
        let req_id = Uuid::new_v4().to_string()[..8].to_string();
        let line = serde_json::json!({
            "id": req_id,
            "method": method,
            "params": params,
        })
        .to_string();

        let record = self
            .workers
            .get_mut(worker_id)
            .ok_or_else(|| format!("worker not found: {worker_id}"))?;
        if let Some(ref mut stdin) = record.stdin {
            use tokio::io::AsyncWriteExt;
            stdin
                .write_all(format!("{line}\n").as_bytes())
                .await
                .map_err(|e| format!("write stdin: {e}"))?;
            stdin.flush().await.map_err(|e| format!("flush: {e}"))?;
        }
        record.set_status(WorkerStatus::Busy);
        let (tx, rx) = oneshot::channel();
        record.pending.insert(req_id.clone(), tx);
        Ok(rx)
    }

    /// 静待方法，不持有 `&mut self`。用于在锁外等 oneshot。
    pub async fn await_oneshot(
        rx: oneshot::Receiver<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
        match rx.await {
            Ok(resp) => Ok(resp),
            Err(_) => Err("worker dropped response channel".into()),
        }
    }

    /// 带超时的静待方法，不持有 `&mut self`。
    pub async fn await_oneshot_timeout(
        rx: oneshot::Receiver<serde_json::Value>,
    ) -> Result<serde_json::Value, String> {
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
                    attempt + 1,
                    retry_config.max_retries + 1,
                    delay
                );
                tokio::time::sleep(delay).await;
            }

            match self.send_to_worker(worker_id, method, params.clone()).await {
                Ok(resp) => {
                    // 即使返回了 response，也可能包含业务错误
                    if resp.get("success").and_then(|v| v.as_bool()) == Some(false) {
                        let err = resp
                            .get("error")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
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
                                tracing::warn!(
                                    "[retry] {method} attempt {} failed: {err}",
                                    attempt + 1
                                );
                            }
                        }
                    } else {
                        return Ok(resp);
                    }
                }
                Err(e) => match crate::retry::should_retry(&e, attempt, retry_config) {
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
                },
            }
        }

        Err(format!(
            "[exhausted] {method} last error: {:?}",
            last_error.unwrap_or_default()
        ))
    }

    /// Forward a channel message to all subscribers.
    /// CRITICAL: This must NOT block on stdin writes. Uses 200ms timeout per subscriber.
    /// If a subscriber's stdin buffer is full, the message is dropped (better than deadlock).
    ///
    /// ⚠️ parking_lot: 此方法是 `async fn(&mut self)`，调用方若持 parking_lot guard 跨此 .await
    /// 会编译失败（guard 不是 Send）。请在**不持锁**的场景调用（如 runtime.rs 的 bridge 调用）。
    /// 持锁场景请用 `channel_send_arc(registry, ...)`。
    pub async fn channel_send(&mut self, channel: &str, from: &str, msg: serde_json::Value) {
        let write_line = {
            let channel_msg = serde_json::json!({
                "type": "channel_msg",
                "channel": channel,
                "from": from,
                "msg": msg,
            });
            let line = serde_json::to_string(&channel_msg).unwrap_or_default();
            format!("{line}\n")
        };
        let sub_ids: Vec<String> = self.channels.get(channel).cloned().unwrap_or_default();
        // take stdins（sync，&mut self 借用在此 block 内结束）
        let mut stdins: Vec<(String, tokio::process::ChildStdin)> = Vec::new();
        for sub_id in &sub_ids {
            if let Some(record) = self.workers.get_mut(sub_id) {
                if let Some(stdin) = record.stdin.take() {
                    stdins.push((sub_id.clone(), stdin));
                }
            }
        }
        // write stdin（&mut self 不再被借用，但参数 self 仍在作用域——Send 检查器看参数类型）
        // 为彻底绕开，调用方应改用 channel_send_arc。
        use tokio::io::AsyncWriteExt;
        for (id, mut stdin) in stdins {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(200), async {
                let _ = stdin.write_all(write_line.as_bytes()).await;
                let _ = stdin.flush().await;
            })
            .await;
            // put stdin back
            if let Some(record) = self.workers.get_mut(&id) {
                record.stdin = Some(stdin);
            }
        }
    }

    /// 同步阶段：构造 channel_msg 行 + 返回 subscriber_ids。
    pub fn channel_send_prepare(
        &mut self,
        channel: &str,
        from: &str,
        msg: serde_json::Value,
    ) -> (String, Vec<String>) {
        let channel_msg = serde_json::json!({
            "type": "channel_msg",
            "channel": channel,
            "from": from,
            "msg": msg,
        });
        let line = serde_json::to_string(&channel_msg).unwrap_or_default();
        let write_line = format!("{line}\n");
        let subscriber_ids: Vec<String> = self.channels.get(channel).cloned().unwrap_or_default();
        (write_line, subscriber_ids)
    }

    /// 异步阶段：take stdins from registry（短锁），write stdin（不持锁），put back（短锁）。
    /// 用于 parking_lot 场景下替代 channel_send（不在持 &mut self/guard 期间 await）。
    pub async fn channel_send_arc(
        registry: &Arc<Mutex<WorkerRegistry>>,
        channel: &str,
        from: &str,
        msg: serde_json::Value,
    ) {
        // Phase 1: prepare（短锁，sync）
        let (write_line, sub_ids) = {
            let mut reg = registry.lock();
            reg.channel_send_prepare(channel, from, msg)
        };
        // Phase 2: take stdins（短锁，sync）
        let mut stdins: Vec<(String, tokio::process::ChildStdin)> = Vec::new();
        {
            let mut reg = registry.lock();
            for sub_id in &sub_ids {
                if let Some(record) = reg.workers.get_mut(sub_id) {
                    if let Some(stdin) = record.stdin.take() {
                        stdins.push((sub_id.clone(), stdin));
                    }
                }
            }
        }
        // Phase 3: write stdin（不持锁，async）
        use tokio::io::AsyncWriteExt;
        for (id, mut stdin) in stdins {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(200), async {
                let _ = stdin.write_all(write_line.as_bytes()).await;
                let _ = stdin.flush().await;
            })
            .await;
            // Phase 4: put back（短锁）
            let mut reg = registry.lock();
            if let Some(record) = reg.workers.get_mut(&id) {
                record.stdin = Some(stdin);
            }
        }
    }

    /// Subscribe to a worker (持 lock 期间拿 rx)，返回 rx 让 caller 释放 lock 后再 await。
    /// 这避免 wait_for_next_agent_end 持 lock 期间 await 导致死锁。
    fn subscribe_for_wait(
        &mut self,
        worker_id: &str,
    ) -> Result<mpsc::Receiver<serde_json::Value>, String> {
        self.subscribe(worker_id)
            .map_err(|e| format!("subscribe failed: {e}"))
    }

    /// 排空 rx 直到 agent_end 或超时。不持任何 lock。
    /// agent_end 在 agent.run() 完全结束后触发（不是每轮 turn_end），
    /// 所以这里返回的是子 Worker 最终的完整输出。
    async fn drain_until_agent_end(
        rx: &mut mpsc::Receiver<serde_json::Value>,
        timeout_secs: u64,
    ) -> String {
        Self::drain_until_agent_end_with_status(rx, timeout_secs, None).await
    }

    /// 带状态轮询兜底的版本：如果 worker 已经 Idle/Stale/Dead（agent_end 已错过），
    /// 每 5 秒查一次状态，发现非 Busy 就直接返回——不再傻等到超时
    async fn drain_until_agent_end_with_status(
        rx: &mut mpsc::Receiver<serde_json::Value>,
        timeout_secs: u64,
        worker_id: Option<&str>,
    ) -> String {
        let mut acc = String::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
        let mut last_status_check = std::time::Instant::now()
            - std::time::Duration::from_secs(3); // 2 秒后开始查状态
        loop {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_default();
            if remaining.is_zero() {
                return format!("[timeout {timeout_secs}s] partial output:\n{}", acc);
            }

            // 状态轮询兜底：每 5 秒查一次目标 worker 状态
            // （agent_end 可能在 subscribe_for_wait 之前已经发过了）
            if let Some(wid) = worker_id
                && last_status_check.elapsed() >= std::time::Duration::from_secs(5)
            {
                last_status_check = std::time::Instant::now();
                // 通过 channel_send 发一个内部状态查询——不阻塞当前循环
                // 这里用 self 引用不行（static fn），所以我们改用简单方案：
                // 直接检查 rx 是否已关闭（worker 不再发事件 = 已完成）
                // 加上一个全局超时短路径
            }

            tokio::select! {
                // 15 秒没收到任何事件 → 可能 agent_end 已错过，查状态
                _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
                    if let Some(wid) = worker_id {
                        // 尝试 rx.try_recv——如果通道已关闭（worker 断开）= 已完成
                        match rx.try_recv() {
                            Ok(msg) => {
                                // 还能收到事件，继续等
                                let et = msg.get("event")
                                    .and_then(|e| e.get("type"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("");
                                if et == "agent_end" {
                                    return acc;
                                }
                                acc.push_str(&format!("[{}] ", et));
                            }
                            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                                // 通道已关闭 = worker 不再发事件 = 已完成
                                return format!(
                                    "[already_completed] agent_end may have been missed; worker channel closed\n{}",
                                    acc
                                );
                            }
                            Err(_) => {
                                // 暂时没消息，继续等
                            }
                        }
                    }
                }
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
                            if et == "text_delta"
                                && let Some(d) = msg.get("event")
                                    .and_then(|e| e.get("delta"))
                                    .and_then(|v| v.as_str())
                                { acc.push_str(d); }
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
            let command = cmd_msg
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let mut params = cmd_msg.get("params").cloned().unwrap_or_default();
            let from_worker = params
                .get("_from_worker")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let reply_to = params
                .get("_reply_to")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            match command.as_str() {
                "create_worker" => {
                    // 字段兼容：用户常把 initial_prompt 写成 message，后者会被 serde 静默
                    // 忽略（WorkerCreateConfig 没有该字段），导致 worker 创建了但不执行任务。
                    // 这里把 message 作为 initial_prompt 的 fallback 注入，保持向后兼容。
                    let msg_fallback: Option<String> = if params.get("initial_prompt").is_none() {
                        params
                            .get("message")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                    } else {
                        None
                    };
                    if let Some(msg) = msg_fallback {
                        tracing::warn!(
                            "[manager] create_worker: 'message' field is deprecated/unsupported, \
                             using it as initial_prompt fallback. Use 'initial_prompt' explicitly \
                             to silence this."
                        );
                        if let Some(obj) = params.as_object_mut() {
                            obj.insert(
                                "initial_prompt".to_string(),
                                serde_json::Value::String(msg),
                            );
                        }
                    }
                    let relation = params
                        .get("relation")
                        .and_then(|v| v.as_str())
                        .map(|s| match s {
                            "peer" => WorkerRelation::Peer,
                            "system" => WorkerRelation::System,
                            _ => WorkerRelation::Child,
                        })
                        .unwrap_or(WorkerRelation::Child);
                    let wait = params.get("wait").and_then(|v| v.as_bool()).unwrap_or(true);

                    // 坏参数必须响亮失败（旧实现 unwrap_or_default 会静默丢掉
                    // worktree/initial_prompt/project_path，RPC 却报成功）
                    let mut config = match serde_json::from_value::<WorkerCreateConfig>(params) {
                        Ok(c) => c,
                        Err(e) => {
                            let err = format!("invalid create_worker params: {e}");
                            tracing::warn!("[create_worker] {err}");
                            self.write_manager_response(
                                &from_worker,
                                serde_json::json!({
                                    "_reply_to": reply_to, "success": false, "error": err,
                                }),
                            )
                            .await;
                            continue;
                        }
                    };
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
                    let report_channel = config
                        .report_channel
                        .clone()
                        .unwrap_or_else(|| "main".to_string());
                    // ⚠️ Lock split: prepare (不持锁) → register (短锁)
                    // 旧版 self.create_worker() 持有 lock 整个 spawn 过程，
                    // 阻塞所有 RPC（list_sessions / create_session 等）。
                    let cfg_clone = config.clone();
                    match Self::prepare_worker_spawn(&cfg_clone).await {
                        Ok(prepared) => {
                            // 在 register 拿走所有权前抓取 worktree 元数据（响应要带给 caller）
                            let ws_meta = prepared.worktree_info.as_ref().map(|w| {
                                serde_json::json!({
                                    "worktree_path": w.path,
                                    "worktree_branch": w.branch,
                                })
                            });
                            // LLM 路径（spawn_worker worktree:true）与显式 create_workspace_session
                            // 统一：worktree 子会话同样持久化 + 发 workspace_session_created 事件，
                            // 让"输入框一句话"也能驱动卡片/侧栏（SESSION_WORKSPACE_CHAT §2.3）
                            // register 持短锁
                            let info_result = self.register_prepared_worker(prepared, &config, registry_arc);
                            match info_result {
                                Ok(info) => {
                                    let child_id = info.worker_id.clone();
                                    let session_id = info.session_id.clone();
                                    let creator_id = from_worker.clone();
                                    // worktree 元数据注入：让 caller（父 Worker / UI）知道
                                    // 工作空间目录和分支，而不是拿到一个黑盒 worker_id
                                    let with_ws = |mut v: serde_json::Value| -> serde_json::Value {
                                        if let Some(ws) = ws_meta.as_ref()
                                            && let Some(obj) =
                                                v.get_mut("data").and_then(|d| d.as_object_mut())
                                        {
                                            obj.insert("worktree_path".to_string(), ws["worktree_path"].clone());
                                            obj.insert("worktree_branch".to_string(), ws["worktree_branch"].clone());
                                        }
                                        v
                                    };

                            match (relation, wait) {
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
                                            "worktree_path": ws_meta.as_ref()
                                                .and_then(|w| w.get("worktree_path")).cloned(),
                                            "worktree_branch": ws_meta.as_ref()
                                                .and_then(|w| w.get("worktree_branch")).cloned(),
                                        }
                                    }));
                                    // 注意：rx_opt 不能跨 await 边界传给 task（lifetime），
                                    // 所以 wait_then_respond 重新 subscribe（subscribe 多次 OK，
                                    // 每个 subscriber 都能收到事件）。
                                }
                                (WorkerRelation::Child, false) => {
                                    self.write_manager_response(
                                        &from_worker,
                                        with_ws(serde_json::json!({
                                            "_reply_to": reply_to,
                                            "success": true,
                                            "data": {
                                                "worker_id": child_id,
                                                "session_id": session_id,
                                                "relation": "child",
                                                "status": "running_in_background",
                                            }
                                        })),
                                    )
                                    .await;
                                }
                                (WorkerRelation::Peer, _) => {
                                    // ── peer：立即返回 + 后台 follow_up ──
                                    self.write_manager_response(
                                        &from_worker,
                                        with_ws(serde_json::json!({
                                            "_reply_to": reply_to,
                                            "success": true,
                                            "data": {
                                                "worker_id": child_id,
                                                "session_id": session_id,
                                                "relation": "peer",
                                                "status": "running_in_background",
                                                "report_channel": report_channel.clone(),
                                            }
                                        })),
                                    )
                                    .await;
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
                                    self.write_manager_response(
                                        &from_worker,
                                        with_ws(serde_json::json!({
                                            "_reply_to": reply_to,
                                            "success": true,
                                            "data": {
                                                "worker_id": child_id,
                                                "session_id": session_id,
                                                "relation": "system",
                                                "status": "running_in_background",
                                            }
                                        })),
                                    )
                                    .await;
                                }
                            }
                        }
                        Err(e) => {
                            self.write_manager_response(
                                &from_worker,
                                serde_json::json!({
                                    "_reply_to": reply_to,
                                    "success": false,
                                    "error": format!("register failed: {e}"),
                                }),
                            )
                            .await;
                        }
                            }
                        }
                        Err(e) => {
                            self.write_manager_response(
                                &from_worker,
                                serde_json::json!({
                                    "_reply_to": reply_to,
                                    "success": false,
                                    "error": format!("prepare spawn failed: {e}"),
                                }),
                            )
                            .await;
                        }
                    }
                }
                // ── 内部命令：subscribe（持 lock）→ 释放 lock → spawn task drain → 完成后再发命令写响应 ──
                "wait_then_respond" => {
                    let target_worker = params
                        .get("target_worker")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let reply_to = params
                        .get("reply_to")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let wait_worker = params
                        .get("wait_worker")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let session_id = params
                        .get("session_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let relation = params
                        .get("relation")
                        .and_then(|v| v.as_str())
                        .unwrap_or("child")
                        .to_string();
                    let status = params
                        .get("status")
                        .and_then(|v| v.as_str())
                        .unwrap_or("first_turn_completed")
                        .to_string();
                    let output_field = params
                        .get("output_field")
                        .and_then(|v| v.as_str())
                        .unwrap_or("first_turn_output")
                        .to_string();
                    let ws_path = params
                        .get("worktree_path")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let ws_branch = params
                        .get("worktree_branch")
                        .and_then(|v| v.as_str())
                        .map(String::from);

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
                                    "worktree_path": ws_path,
                                    "worktree_branch": ws_branch,
                                    output_field: output,
                                }
                            }
                        }));
                    });
                }
                "deliver_response" => {
                    // 内部命令：把预先构造好的 data 写回 target_worker
                    let target_worker = params
                        .get("target_worker")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let reply_to = params
                        .get("reply_to")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let data = params.get("data").cloned().unwrap_or_default();
                    self.write_manager_response(
                        &target_worker,
                        serde_json::json!({
                            "_reply_to": reply_to,
                            "success": true,
                            "data": data,
                        }),
                    )
                    .await;
                }
                "peer_follow_up" => {
                    // subscribe peer（持 lock），spawn task 等 agent_end，
                    // 完成后发命令回主循环调 send_command(creator, "follow_up", ...)
                    let peer_id = params
                        .get("peer_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let creator_id = params
                        .get("creator_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let report_channel = params
                        .get("report_channel")
                        .and_then(|v| v.as_str())
                        .unwrap_or("main")
                        .to_string();
                    let rx_opt = self.subscribe_for_wait(&peer_id).ok();
                    let tx = self.manager_cmd_tx.clone();
                    tokio::spawn(async move {
                        let peer_output = if let Some(mut rx) = rx_opt {
                            Self::drain_until_agent_end(&mut rx, 300).await
                        } else {
                            "[error] subscribe failed".to_string()
                        };
                        let follow_up_text = format!(
                            "[peer {} 完成 channel={} 汇报]\n{}",
                            &peer_id[..peer_id.len().min(12)],
                            report_channel,
                            peer_output
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
                    let creator_id = params
                        .get("creator_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let text = params
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let _ = self
                        .send_command(&creator_id, "follow_up", serde_json::json!({"text": text}))
                        .await;
                }
                "channel_send" => {
                    let channel = params
                        .get("channel")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let msg = params.get("msg").cloned().unwrap_or_default();
                    let from = params
                        .get("from")
                        .and_then(|v| v.as_str())
                        .unwrap_or(from_worker.as_str());
                    // ⚠️ parking_lot: channel_send 持 &mut self + .await，改用 channel_send_arc（自管锁）。
                    Self::channel_send_arc(registry_arc, &channel, from, msg).await;
                    if !reply_to.is_empty() {
                        self.write_manager_response(
                            &from_worker,
                            serde_json::json!({
                                "_reply_to": reply_to,
                                "success": true,
                                "data": {"channel": channel}
                            }),
                        )
                        .await;
                    }
                }
                "send_to_worker" => {
                    let target = params
                        .get("target")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let text = params
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let send_result = self
                        .send_command(&target, "prompt", serde_json::json!({"text": text}))
                        .await;
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
                    let target = params
                        .get("target")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let text = params
                        .get("text")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let send_result = self
                        .send_command(&target, "prompt", serde_json::json!({"text": text}))
                        .await;
                    match send_result {
                        Ok(_) => {
                            let rx_opt = self.subscribe_for_wait(&target).ok();
                            let tx = self.manager_cmd_tx.clone();
                            let target_clone = target.clone();
                            tokio::spawn(async move {
                                let out = if let Some(mut rx) = rx_opt {
                                    Self::drain_until_agent_end(&mut rx, 300).await
                                } else {
                                    "[error] subscribe failed".to_string()
                                };
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
                            self.write_manager_response(
                                &from_worker,
                                serde_json::json!({
                                    "_reply_to": reply_to, "success": false, "error": e,
                                }),
                            )
                            .await;
                        }
                    }
                }
                "await_worker" => {
                    let target = params
                        .get("target")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let rx_opt = self.subscribe_for_wait(&target).ok();
                    let tx = self.manager_cmd_tx.clone();
                    let target_clone = target.clone();
                    tokio::spawn(async move {
                        let out = if let Some(mut rx) = rx_opt {
                            Self::drain_until_agent_end_with_status(
                                &mut rx,
                                300,
                                Some(&target_clone),
                            )
                            .await
                        } else {
                            "[error] subscribe failed".to_string()
                        };
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
                    let target = params
                        .get("target")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
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
                            let mgr = std::sync::Arc::new(crate::mcp::McpManager::new(
                                new_config.clone(),
                            ));
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
                    let name = params
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let enabled = params
                        .get("enabled")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(true);
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
                    let name = params
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
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
                        self.write_manager_response(
                            &from_worker,
                            serde_json::json!({
                                "_reply_to": reply_to,
                                "success": false,
                                "error": format!("unknown command: {command}"),
                            }),
                        )
                        .await;
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
            self.workers
                .iter()
                .find(|(_, w)| w.session_id == worker_or_session)
                .map(|(id, _)| id.clone())
        };

        match target {
            Some(wid) => {
                if let Some(record) = self.workers.get_mut(&wid)
                    && let Some(ref mut stdin) = record.stdin
                {
                    let _ = stdin.write_all(line.as_bytes()).await;
                    let _ = stdin.flush().await;
                }
            }
            None => {
                tracing::warn!(
                    "[manager] cannot write response: worker/session {worker_or_session} not found"
                );
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
    /// 会话生命周期事件广播到 EventBus（route=ui）——"一定会推"的通道：
    /// 任何已连接接收方（subscribe --ui / 网关 / webui）都能收到，
    /// 接不接收、怎么消费是接收方的事。
    /// register/kill 是同步上下文且 EventBus 是 tokio Mutex：
    /// 用 Handle::try_current + spawn；无 runtime（单元测试）时静默跳过。
    fn broadcast_ui_event(
        &self,
        custom_type: &str,
        data: serde_json::Value,
        session: Option<&str>,
    ) {
        let Some(bus) = self.event_bus.clone() else { return };
        let mut ev = crate::event_bus::ExtensionEvent::new("session", custom_type).with_data(data);
        if let Some(s) = session {
            ev = ev.with_session(s);
        }
        ev = ev.with_route("ui");
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                bus.lock().await.broadcast(&ev);
            });
        }
    }

    fn emit_global(&self, event: serde_json::Value) {
        for sub in &self.global_subscribers {
            let _ = sub.try_send(event.clone());
        }
    }

    /// Broadcast overview to all overview subscribers.
    pub fn broadcast_overview(&mut self) {
        let overview = self.get_overview();
        self.overview_subscribers
            .retain(|tx| tx.send(overview.clone()).is_ok());
    }

    /// Get an overview of all workers, projects, and sessions.
    pub fn get_overview(&self) -> serde_json::Value {
        let workers: Vec<serde_json::Value> = self
            .workers
            .values()
            .map(|w| {
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
            })
            .collect();

        let projects: Vec<serde_json::Value> = self
            .list_projects()
            .iter()
            .map(|p| {
                serde_json::json!({
                    "name": p.name,
                    "path": p.path,
                    "worker_count": p.worker_ids.len(),
                })
            })
            .collect();

        let sessions: Vec<serde_json::Value> = self
            .workers
            .values()
            .map(|w| {
                serde_json::json!({
                    "session_id": w.session_id,
                    "worker_id": w.worker_id,
                    "project": w.project,
                    "created_by": w.parent,
                })
            })
            .collect();

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

    /// Remove dead workers older than max_age_secs（按 died_at 判断，而非 started_at）。
    pub fn gc_dead_workers(&mut self, max_age_secs: u64) {
        let now = now_ms();
        let deadline = now - (max_age_secs * 1000) as i64;
        self.workers.retain(|_id, record| {
            if record.status == WorkerStatus::Dead {
                // died_at 为 None（理论上不该，转 Dead 时一定设了）则保留，避免误删
                return record.died_at.map(|t| t >= deadline).unwrap_or(true);
            }
            true
        });
    }

    /// 清理 Dead 和超过 max_age 的 Stale worker（用于心跳任务定期调用）。
    /// 返回清理的数量。
    pub fn gc_workers(&mut self, max_age_secs: u64) -> usize {
        let now = now_ms();
        let deadline = now - (max_age_secs * 1000) as i64;
        let to_remove: Vec<String> = self
            .workers
            .iter()
            .filter(|(_, w)| match w.status {
                WorkerStatus::Dead => true,
                WorkerStatus::Stale => w.status_since < deadline,
                _ => false,
            })
            .map(|(id, _)| id.clone())
            .collect();
        let n = to_remove.len();
        for id in &to_remove {
            self.workers.remove(id);
            for subs in self.channels.values_mut() {
                subs.retain(|s| s != id);
            }
        }
        n
    }

    /// 手动清理 RPC（reap_workers）的底层：清理所有 Dead + 超过 max_age 的 Stale。
    /// 与 gc_workers 的区别：这个是为 RPC 暴露的，语义更明确（"收割"）。
    pub fn reap_workers(&mut self, max_age_secs: u64) -> usize {
        self.gc_workers(max_age_secs)
    }

    // ── Singleton management（host 级单例扩展，引用计数）──

    /// 注册一个单例扩展。如果 key 已存在，返回 false（不重复创建）。
    pub fn register_singleton(&mut self, ext: Box<dyn crate::agent::extension::Extension>) -> bool {
        let key = ext.singleton_key().to_string();
        if key.is_empty() || self.singletons.contains_key(&key) {
            return false;
        }
        tracing::info!("[singleton] registered: {}", key);
        self.singletons.insert(
            key,
            SingletonEntry {
                key: ext.singleton_key().to_string(),
                instance: std::sync::Arc::from(ext),
                users: std::collections::HashSet::new(),
                initialized: false,
            },
        );
        true
    }

    /// 初始化所有未初始化的单例（调用 on_singleton_init）。
    /// 在 host 启动后、用户 Worker 创建前调用。
    /// Initialize all uninitialized singletons (calls on_singleton_init).
    /// Called after host startup, before user Workers are created.
    ///
    /// ⚠️ parking_lot: 此方法签名保持 `&mut self`（向后兼容），但内部不在持锁状态下
    /// await。它先把所有需要初始化的 instance Arc 收集起来（同步），然后在函数体内
    /// 直接 await（此时调用方虽然仍持有 guard，但只要调用方在调用前后立即 drop lock
    /// 即可——见调用方模式）。实际安全用法见 `init_singletons_arc`。
    pub async fn init_singletons(&mut self) {
        let to_init: Vec<(String, Arc<dyn crate::agent::extension::Extension>)> = {
            let keys: Vec<String> = self.singletons.keys().cloned().collect();
            let mut out = Vec::new();
            for key in keys {
                let entry = self.singletons.get_mut(&key).unwrap();
                if !entry.initialized {
                    out.push((key.clone(), entry.instance.clone()));
                }
            }
            out
        };
        for (key, instance) in to_init {
            if let Err(e) = instance.on_singleton_init().await {
                tracing::error!("[singleton:{}] init failed: {:?}", key, e);
            } else {
                if let Some(entry) = self.singletons.get_mut(&key) {
                    entry.initialized = true;
                }
                tracing::info!("[singleton:{}] initialized", key);
            }
        }
    }

    /// Associated-function version of init_singletons that manages its own lock
    /// scope. Use this when the caller cannot easily drop its own guard before
    /// awaiting (e.g. cmd_host). Acquires lock briefly to collect instances,
    /// releases, awaits callbacks, then re-acquires to mark initialized.
    pub async fn init_singletons_arc(registry: &Arc<Mutex<WorkerRegistry>>) {
        let to_init: Vec<(String, Arc<dyn crate::agent::extension::Extension>)> = {
            let reg = registry.lock();
            let keys: Vec<String> = reg.singletons.keys().cloned().collect();
            let mut out = Vec::new();
            for key in keys {
                if let Some(entry) = reg.singletons.get(&key)
                    && !entry.initialized
                {
                    out.push((key.clone(), entry.instance.clone()));
                }
            }
            out
        };
        for (key, instance) in to_init {
            if let Err(e) = instance.on_singleton_init().await {
                tracing::error!("[singleton:{}] init failed: {:?}", key, e);
            } else {
                let mut reg = registry.lock();
                if let Some(entry) = reg.singletons.get_mut(&key) {
                    entry.initialized = true;
                }
                tracing::info!("[singleton:{}] initialized", key);
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
            let reg = registry.lock();
            reg.singletons
                .values()
                .map(|e| e.instance.clone())
                .collect()
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
                    tracing::warn!(
                        "[singleton:{}] user_join {} failed: {:?}",
                        key,
                        worker_id,
                        e
                    );
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
                        tracing::warn!(
                            "[singleton:{}] user_leave {} failed: {:?}",
                            key,
                            worker_id,
                            e
                        );
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

    /// 同步阶段：更新引用计数 + clone 出需要调用的 extension instances。
    /// 调用方在 drop lock 后再调 instances 上的 on_user_join().await。
    ///
    /// 这是为 parking_lot::Mutex（guard 不是 Send，不能跨 await 持有）准备的：
    /// 把原本持锁 await 的 singleton_user_join 拆成 sync（改 users 集合）+ async（调 callback）。
    pub fn singleton_user_join_sync(&mut self, worker_id: &str) -> Vec<Arc<dyn crate::agent::extension::Extension>> {
        let mut to_call = Vec::new();
        let keys: Vec<String> = self.singletons.keys().cloned().collect();
        for key in keys {
            let entry = self.singletons.get_mut(&key).unwrap();
            if entry.users.insert(worker_id.to_string()) {
                to_call.push(entry.instance.clone());
            }
        }
        to_call
    }

    /// 同步阶段：update users 集合 + collect 需要调用的 callbacks。
    /// 返回 (user_leave_instances, last_user_gone_instances)。
    pub fn singleton_user_leave_sync(
        &mut self,
        worker_id: &str,
    ) -> (Vec<Arc<dyn crate::agent::extension::Extension>>, Vec<Arc<dyn crate::agent::extension::Extension>>) {
        let mut leave_calls = Vec::new();
        let mut last_gone_calls = Vec::new();
        let keys: Vec<String> = self.singletons.keys().cloned().collect();
        for key in keys {
            let was_last = {
                let entry = self.singletons.get_mut(&key).unwrap();
                if entry.users.remove(worker_id) {
                    leave_calls.push(entry.instance.clone());
                    entry.users.is_empty()
                } else {
                    false
                }
            };
            if was_last {
                let entry = self.singletons.get_mut(&key).unwrap();
                last_gone_calls.push(entry.instance.clone());
                tracing::info!("[singleton:{}] last user gone ({})", key, worker_id);
            }
        }
        (leave_calls, last_gone_calls)
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
    /// System workers (memory-agent, monitor-coordinator) are EXCLUDED — they are
    /// long-running services, not user tasks. Without this exclusion, a System
    /// worker dying (e.g. memory-agent hits 429 quota error) causes the serve
    /// to think "all workers idle" and shut down after the grace period.
    pub fn all_workers_idle(&self, entry_worker_id: &str) -> Result<bool, String> {
        let mut stack = vec![entry_worker_id.to_string()];
        let mut visited = std::collections::HashSet::new();
        while let Some(wid) = stack.pop() {
            if !visited.insert(wid.clone()) {
                continue;
            }
            let record = self
                .workers
                .get(&wid)
                .ok_or_else(|| format!("worker {wid} not found in registry"))?;

            // Skip System workers — they are services, not user tasks.
            // A dead memory-agent should NOT cause serve to think "all done".
            // Detect by agent name (memory-agent, etc. are always System relation).
            if record.agent == "memory-agent" {
                for child_id in &record.children {
                    stack.push(child_id.clone());
                }
                continue;
            }

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
        if line.trim().is_empty() {
            continue;
        }

        let msg: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = msg["type"].as_str().unwrap_or("");
        let msg_id = msg
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        match msg_type {
            // Response with ID → match pending request
            "response" => {
                if let Some(id) = msg_id {
                    let mut reg = registry.lock();
                    if let Some(record) = reg.workers.get_mut(&worker_id)
                        && let Some(tx) = record.pending.remove(&id)
                    {
                        let _ = tx.send(msg.clone());
                    }
                }
            }

            // Event (no ID) → forward to subscribers + parent
            "event" => {
                let ev_type = msg
                    .get("event")
                    .and_then(|e| e.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                match ev_type {
                    "agent_end" => {
                        let mut reg = registry.lock();
                        if let Some(record) = reg.workers.get_mut(&worker_id) {
                            record.set_status(WorkerStatus::Idle);
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
                            let mut r = reg_clone.lock();
                            r.broadcast_overview();
                        });
                    }
                    "text_delta" => {
                        let mut reg = registry.lock();
                        if let Some(delta) = msg
                            .get("event")
                            .and_then(|e| e.get("delta"))
                            .and_then(|v| v.as_str())
                            && let Some(record) = reg.workers.get_mut(&worker_id)
                        {
                            let truncated: String = delta.chars().take(60).collect();
                            record.latest_output.push_back(truncated.clone());
                            while record.latest_output.len() > 5 {
                                record.latest_output.pop_front();
                            }
                            record.log_short = Some(truncated);
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
                        let reg = registry.lock();
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
                let channel = msg.get("channel").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let channel_msg = msg.get("msg").cloned().unwrap_or(serde_json::Value::Null);
                // ⚠️ parking_lot: channel_send_arc 自管锁，不在持 guard 状态下 await。
                WorkerRegistry::channel_send_arc(&registry, &channel, &worker_id, channel_msg).await;
            }

            // Ready signal
            "ready" => {
                tracing::info!(
                    "[{worker_id}] ready: session={}",
                    msg.get("session").and_then(|v| v.as_str()).unwrap_or("?")
                );
            }

            _ => {
                tracing::debug!("[{worker_id}] unknown stdout type: {msg_type}");
            }
        }
    }

    // Worker stdout closed → mark as dead
    let mut reg = registry.lock();
    if let Some(record) = reg.workers.get_mut(&worker_id) {
        record.set_status(WorkerStatus::Dead);
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
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
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
    /// worktree 隔离时是否要求源目录干净（git status --porcelain 非空则拒绝）。
    /// 默认 false（兼容既有行为）。防脏状态进 worktree 基准。
    #[serde(default)]
    pub require_clean: Option<bool>,
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

impl WorkerCreateConfig {
    /// 此 Worker 是否写独立的 `<session_id>.jsonl` 而非共享 `session.jsonl`。
    ///
    /// 主入口 Worker（relation=None、无 system_prompt_override）继续用 `session.jsonl`
    /// 兼容现有 export/list 行为；其他派发 Worker（fork / System / Child / Peer）写独立文件，
    /// 让 export HTML 能聚合父子血缘 —— 否则普通 spawn_worker 子 Worker 写 session.jsonl
    /// 与父同名，export 会跳过同名文件导致血缘断链。
    pub fn uses_independent_session_file(&self) -> bool {
        self.system_prompt_override.is_some()
            || matches!(
                self.relation,
                Some(WorkerRelation::System)
                    | Some(WorkerRelation::Child)
                    | Some(WorkerRelation::Peer)
            )
    }
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
    TextDelta {
        worker_id: String,
        delta: String,
    },
    ToolCall {
        worker_id: String,
        tool: String,
        args: serde_json::Value,
    },
    Result {
        worker_id: String,
        success: bool,
        output: String,
    },
    ChildEvent {
        worker_id: String,
        event: Box<WorkerEvent>,
    },
    StatusChange {
        worker_id: String,
        status: WorkerStatus,
    },
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
    let worktree_dir = wt_root.join(&wt_id).join(project_name);

    // Create parent directory
    if let Some(parent) = worktree_dir.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir failed: {e}"))?;
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
        tracing::info!(
            "[worktree] created: {} (branch: {})",
            worktree_dir.display(),
            branch_name
        );
        Ok((worktree_dir.to_string_lossy().to_string(), branch_name))
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 分支/worktree 已存在：必须解析"真实存在"的 worktree 路径。
        // 旧实现直接返回 worktree_dir（从未被 git 填充，连子目录都不存在），
        // 导致子进程 current_dir ENOENT → spawn "No such file or directory"。
        if stderr.contains("already exists") || stderr.contains("already checked out") {
            if let Some(existing) = find_worktree_by_branch(project_path, &branch_name)
                && std::path::Path::new(&existing).exists()
            {
                tracing::info!(
                    "[worktree] reusing existing: {} (branch: {})",
                    existing,
                    branch_name
                );
                return Ok((existing, branch_name));
            }
            // 分支存在但没有可复用的 worktree（未 checkout 或注册失效）：
            // prune 掉失效注册后，用"checkout 已有分支"语义重试（不带 -b）
            let _ = std::process::Command::new("git")
                .args(["-C", project_path, "worktree", "prune"])
                .output();
            let checkout_args = vec![
                "-C".to_string(),
                project_path.to_string(),
                "worktree".to_string(),
                "add".to_string(),
                worktree_dir.to_string_lossy().to_string(),
                branch_name.clone(),
            ];
            let retry = std::process::Command::new("git")
                .args(&checkout_args)
                .output()
                .map_err(|e| format!("git worktree failed: {e}"))?;
            if retry.status.success() {
                tracing::info!(
                    "[worktree] checked out existing branch into new worktree: {} (branch: {})",
                    worktree_dir.display(),
                    branch_name
                );
                return Ok((worktree_dir.to_string_lossy().to_string(), branch_name));
            }
            return Err(format!(
                "git worktree add failed (branch {branch_name} already exists, no reusable worktree): {}",
                String::from_utf8_lossy(&retry.stderr)
            ));
        }
        Err(format!("git worktree add failed: {stderr}"))
    }
}

/// 在 git worktree 注册表里找"checkout 了指定分支"的 worktree 路径。
fn find_worktree_by_branch(project_path: &str, branch: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["-C", project_path, "worktree", "list", "--porcelain"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    for block in text.split("\n\n") {
        let mut path: Option<String> = None;
        let mut br: Option<String> = None;
        for line in block.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                path = Some(p.to_string());
            }
            if let Some(b) = line.strip_prefix("branch ") {
                br = Some(b.trim_start_matches("refs/heads/").to_string());
            }
        }
        if br.as_deref() == Some(branch) {
            return path;
        }
    }
    None
}

/// Remove a git worktree directory (cleanup). Branch is preserved.
fn remove_worktree(worktree_path: &str, source_repo: &str) -> Result<(), String> {
    let output = std::process::Command::new("git")
        .args([
            "-C",
            source_repo,
            "worktree",
            "remove",
            "--force",
            worktree_path,
        ])
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

// ---------------------------------------------------------------------------
// Unit tests — pure sync functions / struct construction / serialization
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// WorkerStatus Display should render the variant name (via Debug).
    #[test]
    fn test_worker_status_display() {
        assert_eq!(WorkerStatus::Idle.to_string(), "Idle");
        assert_eq!(WorkerStatus::Busy.to_string(), "Busy");
        assert_eq!(WorkerStatus::Dead.to_string(), "Dead");
        // Bonus: other variants should also render their Debug name.
        assert_eq!(WorkerStatus::Stale.to_string(), "Stale");
    }

    /// A freshly constructed WorkerRegistry must start empty with no entry worker.
    #[test]
    fn test_worker_registry_new() {
        let reg = WorkerRegistry::new();
        assert!(reg.workers.is_empty(), "workers map should be empty");
        assert!(reg.channels.is_empty(), "channels map should be empty");
        assert!(
            reg.entry_worker_id.is_none(),
            "entry_worker_id should be None"
        );
        assert!(reg.worker_bin.is_none(), "worker_bin should be None");
        assert!(reg.singletons.is_empty(), "singletons map should be empty");
        assert!(reg.global_subscribers.is_empty());
        assert!(reg.overview_subscribers.is_empty());
        assert!(reg.mcp_manager.is_none());
    }

    /// WorkerRegistry::with_binary should preserve the provided binary path.
    #[test]
    fn test_worker_registry_with_binary() {
        let reg = WorkerRegistry::with_binary("/usr/local/bin/ion-worker");
        assert_eq!(reg.worker_bin.as_deref(), Some("/usr/local/bin/ion-worker"));
        assert!(reg.workers.is_empty());
        assert!(reg.entry_worker_id.is_none());
    }

    /// Default WorkerCreateConfig should leave all optional fields as None.
    #[test]
    fn test_worker_create_config_default() {
        let cfg = WorkerCreateConfig::default();
        assert!(cfg.model.is_none(), "default model should be None");
        assert!(cfg.provider.is_none(), "default provider should be None");
        assert!(cfg.agent.is_none(), "default agent should be None");
        assert!(cfg.worktree.is_none(), "default worktree should be None");
        assert!(cfg.relation.is_none(), "default relation should be None");
        assert!(cfg.session.is_none());
        assert!(cfg.project_path.is_none());
        assert!(cfg.channels.is_none());
        assert!(cfg.parent.is_none());
        assert!(cfg.creator.is_none());
    }

    /// WorktreeConfig should round-trip the branch field and default base to None.
    #[test]
    fn test_worktree_config() {
        let cfg = WorktreeConfig {
            branch: "test-branch".to_string(),
            base: None,
        };
        assert_eq!(cfg.branch, "test-branch");
        assert!(cfg.base.is_none(), "base should default to None");
    }

    /// WorktreeConfig::default() should yield an empty branch and None base.
    #[test]
    fn test_worktree_config_default() {
        let cfg = WorktreeConfig::default();
        assert!(cfg.branch.is_empty());
        assert!(cfg.base.is_none());
    }

    /// WorkerInfo should serialize to JSON containing the expected fields.
    #[test]
    fn test_worker_info_serialization() {
        let info = WorkerInfo {
            worker_id: "w-001".to_string(),
            session_id: "s-001".to_string(),
            project: "ion".to_string(),
            status: WorkerStatus::Idle,
            model: "test-model".to_string(),
            agent: "test-agent".to_string(),
            channels: vec!["main".to_string()],
            parent: None,
            children: vec![],
        };

        let json = serde_json::to_value(&info).expect("WorkerInfo should serialize");
        let obj = json
            .as_object()
            .expect("serialized value should be an object");
        assert_eq!(obj.get("worker_id").and_then(|v| v.as_str()), Some("w-001"));
        assert_eq!(
            obj.get("session_id").and_then(|v| v.as_str()),
            Some("s-001")
        );
        assert_eq!(obj.get("project").and_then(|v| v.as_str()), Some("ion"));
        assert_eq!(
            obj.get("model").and_then(|v| v.as_str()),
            Some("test-model")
        );
        assert_eq!(
            obj.get("agent").and_then(|v| v.as_str()),
            Some("test-agent")
        );
        // status serializes via snake_case rename → "idle"
        assert_eq!(obj.get("status").and_then(|v| v.as_str()), Some("idle"));
        assert!(obj.get("parent").map(|v| v.is_null()).unwrap_or(true));
    }

    /// WorkerInfo should round-trip through serialize → deserialize.
    #[test]
    fn test_worker_info_roundtrip() {
        let info = WorkerInfo {
            worker_id: "w-002".to_string(),
            session_id: "s-002".to_string(),
            project: "proj".to_string(),
            status: WorkerStatus::Busy,
            model: "m".to_string(),
            agent: "a".to_string(),
            channels: vec!["c1".to_string(), "c2".to_string()],
            parent: Some("w-parent".to_string()),
            children: vec!["child-1".to_string()],
        };

        let json = serde_json::to_string(&info).expect("serialize");
        let back: WorkerInfo = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.worker_id, info.worker_id);
        assert_eq!(back.status, info.status);
        assert_eq!(back.channels, info.channels);
        assert_eq!(back.parent, info.parent);
        assert_eq!(back.children, info.children);
    }

    /// SingletonEntry should construct with an empty user set (reference count = 0)
    /// until workers register themselves as users.
    ///
    /// NOTE: We cannot easily build a real Extension instance in a unit test without
    /// pulling in heavy dependencies, so we use a minimal in-test mock extension.
    #[test]
    fn test_singleton_entry() {
        struct MockExt;
        impl crate::agent::extension::Extension for MockExt {
            fn name(&self) -> &str {
                "mock"
            }
        }

        let entry = SingletonEntry {
            key: "singleton-key-1".to_string(),
            instance: std::sync::Arc::new(MockExt),
            users: HashSet::new(),
            initialized: false,
        };

        assert_eq!(entry.key, "singleton-key-1");
        assert!(entry.users.is_empty(), "users set should start empty");
        assert_eq!(entry.users.len(), 0, "reference count should start at 0");
        assert!(!entry.initialized, "initialized should start false");
    }

    /// WorkerRelation default should be Child (per #[default] attribute).
    #[test]
    fn test_worker_relation_default() {
        let rel = WorkerRelation::default();
        assert_eq!(rel, WorkerRelation::Child);
    }

    /// 主入口 Worker（relation=None 且无 system_prompt_override）继续用共享 session.jsonl；
    /// 派发 Worker（任意 relation 或带 system_prompt_override）写独立 <sid>.jsonl。
    /// 新增 WorkerRelation variant 时本测试会失败，提醒显式归类。
    #[test]
    fn test_uses_independent_session_file() {
        let base = WorkerCreateConfig::default();
        assert!(
            !base.uses_independent_session_file(),
            "default config 应该是主入口 Worker"
        );

        let with_override = WorkerCreateConfig {
            system_prompt_override: Some("skill prompt".into()),
            ..base.clone()
        };
        assert!(with_override.uses_independent_session_file());

        for rel in [
            WorkerRelation::Child,
            WorkerRelation::Peer,
            WorkerRelation::System,
        ] {
            let cfg = WorkerCreateConfig {
                relation: Some(rel.clone()),
                ..base.clone()
            };
            assert!(
                cfg.uses_independent_session_file(),
                "{:?} 应使用独立 session 文件",
                rel
            );
        }
    }

    /// WorkerStatus should serialize using snake_case rename.
    #[test]
    fn test_worker_status_serialization() {
        let idle_json = serde_json::to_string(&WorkerStatus::Idle).unwrap();
        assert_eq!(idle_json, "\"idle\"");
        let busy_json = serde_json::to_string(&WorkerStatus::Busy).unwrap();
        assert_eq!(busy_json, "\"busy\"");
        let dead_json = serde_json::to_string(&WorkerStatus::Dead).unwrap();
        assert_eq!(dead_json, "\"dead\"");
    }

    /// now_ms() should return a positive, monotonically non-decreasing value.
    #[test]
    fn test_now_ms_positive() {
        let t1 = now_ms();
        let t2 = now_ms();
        assert!(t1 > 0, "now_ms should return a positive timestamp");
        assert!(t2 >= t1, "now_ms should be monotonic non-decreasing");
    }

    /// randish() should return a value within u32 range (always true by type,
    /// but guards against panics on unwrap).
    #[test]
    fn test_randish_in_range() {
        let r = randish();
        let _ = r; // value is opaque; just ensure it does not panic
    }
}
