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
    /// Channel for workers to send manager commands (create_worker, channel_send, etc.)
    pub manager_cmd_tx: mpsc::UnboundedSender<serde_json::Value>,
    pub manager_cmd_rx: mpsc::UnboundedReceiver<serde_json::Value>,
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
            manager_cmd_tx,
            manager_cmd_rx,
        }
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
            manager_cmd_tx,
            manager_cmd_rx,
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

        let mut child = tokio::process::Command::new(&binary)
            .args(&cmd_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(&worktree_path)
            .spawn()
            .map_err(|e| format!("failed to spawn worker: {e}"))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;

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
		                            let reg = sub_registry.lock().await;
		                            if let Some(record) = reg.workers.get(&sub_wid) {
		                                for sub in &record.event_subscribers {
		                                    let _ = sub.try_send(msg.clone());
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
		                            } else if ev_type == "agent_end" {
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
		            if let Some(mut record) = reg.workers.remove(&sub_wid) {
		                // Kill child process if still alive
		                if let Some(ref mut child) = record.child_process {
		                    let _ = child.start_kill();
		                }
		                // Remove from channels
		                for ch in &record.channels {
		                    if let Some(subs) = reg.channels.get_mut(ch) {
		                        subs.retain(|id| id != &sub_wid);
		                    }
		                }
		                // Remove from parent's children
		                if let Some(ref parent_id) = record.parent {
		                    if let Some(parent) = reg.workers.get_mut(parent_id) {
		                        parent.children.retain(|id| id != &sub_wid);
		                    }
		                }
		                reg.broadcast_overview();
		            }
		        });

	        // Wait for ready signal
	        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

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

	        // ── 注入 initial_prompt（如果有）──
	        // 通过 prompt RPC 把初始任务（peer 模式含汇报指令段）发给子进程。
	        // 注意：这是 fire-and-forget，不等 agent_end —— agent_end 等待逻辑在
	        // process_pending_commands 里按 relation=Child 单独处理。
	        if let Some(prompt_text) = effective_prompt {
	            let wid_for_prompt = worker_id.clone();
	            // 直接调用 send_command（同 &mut self 上下文）
	            if let Err(e) = self.send_command(&wid_for_prompt, "prompt", serde_json::json!({"text": prompt_text})).await {
            let _rx_ignore = ();
	                tracing::warn!("[{wid_for_prompt}] failed to inject initial_prompt: {e}");
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
        let (tx, rx) = mpsc::channel(256);
        record.event_subscribers.push(tx);
        Ok(rx)
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
                            _ => WorkerRelation::Child,
                        })
                        .unwrap_or(WorkerRelation::Child);
                    let wait = params.get("wait").and_then(|v| v.as_bool()).unwrap_or(true);

                    let config: WorkerCreateConfig = serde_json::from_value(params).unwrap_or_default();
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
            "total_workers": workers.len(),
            "total_projects": projects.len(),
            "total_stale": self.workers.values().filter(|w| w.status == WorkerStatus::Stale).count(),
            "sessions": sessions,
        })
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
                WorkerStatus::Idle => {}
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
#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkerRelation {
    #[default]
    Child,
    Peer,
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
