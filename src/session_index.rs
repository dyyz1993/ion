use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Per-session metadata stored in the index.
/// Allows O(1) access to session stats without parsing the full session file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Human-readable session name (last set via --name)
    pub name: Option<String>,
    /// First name ever set
    pub first_name: Option<String>,
    /// Project path (cwd when session was created)
    pub project: Option<String>,
    /// Project directory name
    pub project_name: Option<String>,
    /// Whether this session uses a worktree
    pub worktree: bool,
    /// Git branch at time of last update
    pub branch: Option<String>,
    /// Model ID used
    pub model: String,
    /// Agent name
    pub agent: String,
    /// Provider name
    pub provider: String,
    /// Total input tokens
    pub token_input: u64,
    /// Total output tokens
    pub token_output: u64,
    /// Cache read tokens
    pub token_cache_read: u64,
    /// Cache write tokens
    pub token_cache_write: u64,
    /// 用户提问次数（每轮 turn +1）
    #[serde(default)]
    pub user_prompt_count: u32,
    /// LLM 循环次数（= Assistant message 数 = LLM API 调用次数）
    #[serde(default)]
    pub llm_request_count: u32,
    /// 总耗时（毫秒，Agent 单次 LLM 调用耗时累加）
    #[serde(default)]
    pub total_duration_ms: u64,
    /// Number of context compressions
    pub compress_count: u32,
    /// Total messages in session
    pub message_count: u32,
    /// Turn count
    pub turn_count: u32,
    /// Creation timestamp (Unix ms)
    pub created_at: i64,
    /// Last update timestamp (Unix ms)
    pub updated_at: i64,
    /// Error count
    pub error_count: u32,
    /// Last thinking level set (e.g. "off"/"low"/"medium"/"high")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_thinking_level: Option<String>,
    /// Last active tool names (from append_active_tools_change)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_active_tools: Option<Vec<String>>,
    /// 最后一条 entry 的 id（增量拉取锚点）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_entry_id: Option<String>,
    /// 父会话 id（fork 来源，null = 根会话）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// 父会话关系类型（fork）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_type: Option<String>,
    /// 首次启动时的工作路径（CI 启动路径 / 首次选中路径，创建时记录，不变）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_cwd: Option<String>,
    /// 最后切换到的工作路径（switch_cwd / cd 时更新）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cwd: Option<String>,
    /// 追加的额外工作目录（read/write 操作过的 cwd 外路径，数组）
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_cwds: Vec<String>,
    /// tier_models 快照（创建时从全局 config 读，让历史 session 能还原当时用的 fast/pro/max）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier_models: Option<serde_json::Value>,
    /// 权限模式（permissive/standard/strict/autopilot/readonly，创建时记录）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_profile: Option<String>,
    /// 工作空间目录（worktree 子会话的绝对路径，创建时随索引顺便写入；恢复/清理用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    /// 工作空间生命周期（ready/closed/failed；关闭时更新）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_status: Option<String>,
}

impl SessionIndex {
    /// 工作空间字段部分更新（branch/workspace_path/workspace_status）。
    /// 遵循存储落位原则：工作空间绑定随生命周期顺便更新进索引，不建 sidecar 文件。
    pub fn update_workspace(
        &mut self,
        session_id: &str,
        branch: Option<&str>,
        workspace_path: Option<&str>,
        workspace_status: Option<&str>,
    ) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        if let Some(meta) = self.sessions.get_mut(session_id) {
            if let Some(b) = branch {
                meta.branch = Some(b.to_string());
            }
            if let Some(p) = workspace_path {
                meta.workspace_path = Some(p.to_string());
            }
            if let Some(s) = workspace_status {
                meta.workspace_status = Some(s.to_string());
            }
            meta.updated_at = now;
        }
    }
}

/// Index of all sessions, stored in sessions.index.json
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionIndex {
    pub sessions: HashMap<String, SessionMeta>,
    /// 墓碑：被 session_remove 显式删除的 sid。patch 类更新（patch_meta/
    /// increment_turn_stats）看到墓碑直接跳过——否则 worker 退出前的收尾
    /// 统计会把刚删的条目重建出来（2026-08-29 实测复活的根因之一）。
    /// 显式 create（update）会清掉墓碑。
    #[serde(default, skip_serializing_if = "std::collections::HashSet::is_empty")]
    pub removed_sessions: std::collections::HashSet<String>,
}

impl SessionIndex {
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        PathBuf::from(home)
            .join(".ion")
            .join("agent")
            .join("sessions.index.json")
    }

    fn lock_path() -> PathBuf {
        Self::path().with_extension("json.lock")
    }

    /// 跨进程写事务：持排它 flock 期间 load→mutate→save。
    /// 索引的写者分布在 host 与各 worker 进程（patch_meta/on_turn_end 等），
    /// 各自独立 load→modify→save 会 last-write-wins 覆盖彼此——事务化后
    /// 串行执行，读到的必然是最新状态。锁文件创建失败时退化为无锁路径保功能。
    pub fn write_txn<F, R>(f: F) -> R
    where
        F: FnOnce(&mut Self) -> R,
    {
        use fs2::FileExt;
        let lp = Self::lock_path();
        if let Some(parent) = lp.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::File::create(&lp) {
            Ok(lock) => {
                let _ = lock.lock_exclusive(); // 阻塞至拿到；进程退出自动释放
                let mut idx = Self::load();
                let out = f(&mut idx);
                idx.save();
                let _ = lock.unlock();
                let _ = lock.sync_all();
                out
            }
            Err(_) => {
                let mut idx = Self::load();
                let out = f(&mut idx);
                idx.save();
                out
            }
        }
    }

    pub fn load() -> Self {
        let path = Self::path();
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(self) {
            // 原子写：先写 .tmp 再 rename，防多 worker 进程并发写导致 last-write-wins 丢更新。
            // rename 在同一文件系统内是原子的（POSIX 保证），.tmp 和目标在同目录确保同 FS。
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &content).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub fn get(&self, id: &str) -> Option<&SessionMeta> {
        self.sessions.get(id)
    }

    /// Number of tracked sessions (used by GC to detect whether removal happened).
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// True if no sessions are tracked.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Remove a session entry. Used by session GC after a session file is deleted.
    /// Returns true if the entry existed and was removed.
    /// ⚠️ 只改内存不落盘——持久化删除走 [`Self::remove_persist`]（带墓碑防复活）。
    pub fn remove(&mut self, id: &str) -> bool {
        self.sessions.remove(id).is_some()
    }

    /// 持久化删除（写事务 + 墓碑）。session_remove / GC 用这个——
    /// 此前 host 直接 `load().remove(&sid)` 是空操作（没 save），
    /// 且 patch_meta 的"不在索引则重建"会把条目救活。
    pub fn remove_persist(id: &str) -> bool {
        let mut existed = false;
        Self::write_txn(|idx| {
            existed = idx.sessions.remove(id).is_some();
            idx.removed_sessions.insert(id.to_string());
        });
        existed
    }

    /// 该会话是否被显式删除（墓碑）。patch 类更新据此跳过防复活。
    pub fn is_removed(&self, id: &str) -> bool {
        self.removed_sessions.contains(id)
    }

    /// 查直接子会话（反向索引，O(n) 单次内存扫描，不持久化）。
    /// 血缘只一层——要看整棵血缘树前端递归调用。
    pub fn get_children(&self, parent_id: &str) -> Vec<&SessionMeta> {
        self.sessions
            .values()
            .filter(|m| m.parent_session.as_deref() == Some(parent_id))
            .collect()
    }

    /// 该会话是否有子会话
    pub fn has_children(&self, id: &str) -> bool {
        self.sessions
            .values()
            .any(|m| m.parent_session.as_deref() == Some(id))
    }

    /// 该会话的子会话数量
    pub fn child_count(&self, id: &str) -> usize {
        self.sessions
            .values()
            .filter(|m| m.parent_session.as_deref() == Some(id))
            .count()
    }

    /// upsert 是整个 SessionMeta 替换。为防任何"重建 meta"路径清掉历史
    /// （AGENTS.md 存储落位原则），在唯一汇聚点做兜底保护：
    /// - 有意义的旧标题（≠ 裸 sid）不被 sid 覆盖
    /// - 计数取 max、created_at 取 min——单调量只增不减
    /// 调用方仍应尽量部分更新（update_workspace / patch_meta / increment_*）。
    pub fn upsert(&mut self, id: &str, mut meta: SessionMeta) {
        if let Some(old) = self.sessions.get(id) {
            if old.name.as_deref().is_some_and(|n| n != id)
                && meta.name.as_deref() == Some(id)
            {
                meta.name = old.name.clone();
                if meta.first_name.is_none() {
                    meta.first_name = old.first_name.clone();
                }
            }
            meta.token_input = meta.token_input.max(old.token_input);
            meta.token_output = meta.token_output.max(old.token_output);
            meta.token_cache_read = meta.token_cache_read.max(old.token_cache_read);
            meta.token_cache_write = meta.token_cache_write.max(old.token_cache_write);
            meta.user_prompt_count = meta.user_prompt_count.max(old.user_prompt_count);
            meta.llm_request_count = meta.llm_request_count.max(old.llm_request_count);
            meta.total_duration_ms = meta.total_duration_ms.max(old.total_duration_ms);
            meta.compress_count = meta.compress_count.max(old.compress_count);
            meta.message_count = meta.message_count.max(old.message_count);
            meta.turn_count = meta.turn_count.max(old.turn_count);
            meta.error_count = meta.error_count.max(old.error_count);
            if old.created_at > 0 {
                meta.created_at = meta.created_at.min(old.created_at);
            }
        }
        self.sessions.insert(id.to_string(), meta);
    }

    /// Build a SessionMeta from current context.
    pub fn build(
        id: &str,
        model: &str,
        provider: &str,
        agent: &str,
        name: Option<&str>,
        project: Option<&str>,
        token_input: u64,
        token_output: u64,
        token_cache: u64,
        message_count: u32,
        turn_count: u32,
    ) -> SessionMeta {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        let existing = Self::load().get(id).cloned();

        let project_path = project.map(|p| p.to_string()).or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        });
        let project_name = project_path.as_ref().and_then(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
        });

        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            });

        let is_worktree = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .output()
            .ok()
            .map(|o| o.status.success())
            .unwrap_or(false);

        SessionMeta {
            name: name
                .map(|s| s.to_string())
                .or(existing.as_ref().and_then(|e| e.name.clone())),
            first_name: existing
                .as_ref()
                .and_then(|e| e.first_name.clone())
                .or(name.map(|s| s.to_string())),
            project: project_path,
            project_name,
            worktree: is_worktree,
            branch,
            workspace_path: existing.as_ref().and_then(|e| e.workspace_path.clone()),
            workspace_status: existing.as_ref().and_then(|e| e.workspace_status.clone()),
            model: model.to_string(),
            agent: agent.to_string(),
            provider: provider.to_string(),
            token_input: existing.as_ref().map_or(0, |e| e.token_input) + token_input,
            token_output: existing.as_ref().map_or(0, |e| e.token_output) + token_output,
            token_cache_read: existing.as_ref().map_or(0, |e| e.token_cache_read) + token_cache,
            token_cache_write: 0,
            user_prompt_count: existing.as_ref().map_or(0, |e| e.user_prompt_count),
            llm_request_count: existing.as_ref().map_or(0, |e| e.llm_request_count),
            total_duration_ms: existing.as_ref().map_or(0, |e| e.total_duration_ms),
            compress_count: existing.as_ref().map_or(0, |e| e.compress_count),
            message_count: existing.as_ref().map_or(0, |e| e.message_count) + message_count,
            turn_count: existing.as_ref().map_or(0, |e| e.turn_count) + turn_count,
            created_at: existing.as_ref().map_or(now, |e| e.created_at),
            updated_at: now,
            error_count: existing.as_ref().map_or(0, |e| e.error_count),
            last_thinking_level: existing
                .as_ref()
                .and_then(|e| e.last_thinking_level.clone()),
            last_active_tools: existing.as_ref().and_then(|e| e.last_active_tools.clone()),
            last_entry_id: existing.as_ref().and_then(|e| e.last_entry_id.clone()),
            parent_session: existing.as_ref().and_then(|e| e.parent_session.clone()),
            parent_type: existing.as_ref().and_then(|e| e.parent_type.clone()),
            initial_cwd: existing.as_ref().and_then(|e| e.initial_cwd.clone()),
            last_cwd: existing.as_ref().and_then(|e| e.last_cwd.clone()),
            extra_cwds: existing
                .as_ref()
                .map_or(Vec::new(), |e| e.extra_cwds.clone()),
            tier_models: existing.as_ref().and_then(|e| e.tier_models.clone()),
            security_profile: existing.as_ref().and_then(|e| e.security_profile.clone()),
        }
    }

    /// Update the index with new session data (called after each agent run).
    pub fn update(
        id: &str,
        model: &str,
        provider: &str,
        agent: &str,
        name: Option<&str>,
        token_input: u64,
        token_output: u64,
        message_count: u32,
        turn_count: u32,
    ) {
        let meta = Self::build(
            id,
            model,
            provider,
            agent,
            name,
            None,
            token_input,
            token_output,
            0,
            message_count,
            turn_count,
        );
        Self::write_txn(|index| {
            // 显式 create 清墓碑——同名 sid 重新创建是合法生命周期
            index.removed_sessions.remove(id);
            index.upsert(id, meta.clone());
        });
    }

    /// Synchronize fields that describe the current persisted message tree.
    ///
    /// Unlike [`Self::increment_turn_stats`], these values are snapshots rather
    /// than counters. Keeping the two paths separate prevents each turn from
    /// adding the complete historical token/message totals again.
    pub fn sync_message_tree(
        id: &str,
        model: &str,
        provider: &str,
        agent: &str,
        message_count: u32,
    ) {
        Self::patch_meta(id, |meta| {
            // 只填空值——不覆盖 set_model 已写入的值。
            // SessionIndexExtension 在构造时捕获 model（默认值），on_turn_end
            // 每次调用都会跑这里，如果无条件覆盖会把用户 set_model 切的模型
            // 冲回默认值（实测切 GLM-4.7 → agent 跑完变回 5.2）
            if meta.model.is_empty() {
                meta.model = model.to_string();
            }
            if meta.provider.is_empty() {
                meta.provider = provider.to_string();
            }
            meta.agent = agent.to_string();
            meta.message_count = message_count;
        });
    }

    /// Patch specific fields on an existing session meta without rebuilding the whole entry.
    /// Used by append_* RPCs to keep the index in sync (e.g. thinking level, active tools, name).
    /// If the session isn't yet in the index, creates a minimal entry first.
    pub fn patch_meta<F>(id: &str, patch_fn: F)
    where
        F: FnOnce(&mut SessionMeta),
    {
        // 墓碑守卫：被显式删除的会话不重建（worker 退出前的收尾统计、
        // 迟到的 turn patch 都会走到这里）
        if Self::load().is_removed(id) {
            return;
        }
        Self::write_txn(|index| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;

            // 若 session 不在 index，先建一个最小条目（worker 通过 manager 跑时
            // 没有 ion CLI 的 update 调用路径，所以这里要兜底）
            if !index.sessions.contains_key(id) {
                let cwd = std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default();
                let project_name = std::path::Path::new(&cwd)
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                index.sessions.insert(
                    id.to_string(),
                    SessionMeta {
                        name: None,
                        first_name: None,
                        project: Some(cwd),
                        project_name: Some(project_name),
                        worktree: false,
                        branch: None,
                        workspace_path: None,
                        workspace_status: None,
                        model: String::new(),
                        agent: "default".to_string(),
                        provider: String::new(),
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

            if let Some(meta) = index.sessions.get_mut(id) {
                patch_fn(meta);
                meta.updated_at = now;
            }
        });
    }

    /// Convenience: update session name (from append_session_name RPC).
    pub fn set_name(id: &str, name: &str) {
        Self::patch_meta(id, |m| {
            if m.first_name.is_none() {
                m.first_name = Some(name.to_string());
            }
            m.name = Some(name.to_string());
        });
    }

    /// Convenience: update thinking level (from append_thinking_level_change RPC).
    pub fn set_thinking_level(id: &str, level: &str) {
        Self::patch_meta(id, |m| {
            m.last_thinking_level = Some(level.to_string());
        });
    }

    /// Convenience: update active tools (from append_active_tools_change RPC).
    pub fn set_active_tools(id: &str, tools: Vec<String>) {
        Self::patch_meta(id, |m| {
            m.last_active_tools = Some(tools);
        });
    }

    /// Convenience: update model + provider (from append_model_change RPC).
    pub fn set_model(id: &str, provider: &str, model_id: &str) {
        Self::patch_meta(id, |m| {
            m.provider = provider.to_string();
            m.model = model_id.to_string();
        });
    }

    /// Convenience: update agent name (from append_agent_change RPC).
    pub fn set_agent(id: &str, agent: &str) {
        Self::patch_meta(id, |m| {
            m.agent = agent.to_string();
        });
    }

    /// turn 结束时一次性增量更新多项统计（合并到一个 patch_meta 调用，减少写盘次数）。
    /// - user_prompts: 本轮用户提问数（通常 1）
    /// - llm_calls: 本轮 LLM 调用次数（StreamEvent::Done 计数）
    /// - duration_ms: 本轮耗时
    /// - tok_in/tok_out: 本轮 token 消耗
    /// - is_error: 本轮是否以 Error 结束（true 则 error_count +1）
    pub fn increment_turn_stats(
        id: &str,
        user_prompts: u32,
        llm_calls: u32,
        duration_ms: u64,
        tok_in: u64,
        tok_out: u64,
        is_error: bool,
    ) {
        Self::patch_meta(id, |m| {
            m.user_prompt_count = m.user_prompt_count.saturating_add(user_prompts);
            m.llm_request_count = m.llm_request_count.saturating_add(llm_calls);
            m.total_duration_ms = m.total_duration_ms.saturating_add(duration_ms);
            m.token_input = m.token_input.saturating_add(tok_in);
            m.token_output = m.token_output.saturating_add(tok_out);
            // turn_count 与真实用户输入一一对应；一次用户回合可能包含多次 LLM 调用。
            m.turn_count = m.turn_count.saturating_add(user_prompts);
            if is_error {
                m.error_count = m.error_count.saturating_add(1);
            }
        });
    }

    /// 压缩触发时 +1（compaction 成功后调用）。
    pub fn increment_compress_count(id: &str) {
        Self::patch_meta(id, |m| {
            m.compress_count = m.compress_count.saturating_add(1);
        });
    }

    /// 设置血缘：父会话 id + 关系类型（child/peer/system/fork）。
    /// 在 worker 创建时（upsert）调用，让 ion sessions --json 能查派发关系。
    pub fn set_parent(id: &str, parent_session: &str, relation: &str) {
        Self::patch_meta(id, |m| {
            m.parent_session = Some(parent_session.to_string());
            m.parent_type = Some(relation.to_string());
        });
    }

    /// 设置首次启动工作路径（CI 启动路径 / 首次选中，创建时调一次，后续不变）。
    pub fn set_initial_cwd(id: &str, cwd: &str) {
        Self::patch_meta(id, |m| {
            if m.initial_cwd.is_none() {
                m.initial_cwd = Some(cwd.to_string());
            }
        });
    }

    /// 更新最后工作路径（switch_cwd / cd 时调）。
    pub fn set_last_cwd(id: &str, cwd: &str) {
        Self::patch_meta(id, |m| {
            m.last_cwd = Some(cwd.to_string());
        });
    }

    /// 追加额外工作目录（read/write 操作过的 cwd 外路径，去重）。
    pub fn add_extra_cwd(id: &str, cwd: &str) {
        Self::patch_meta(id, |m| {
            if !m.extra_cwds.iter().any(|c| c == cwd) {
                m.extra_cwds.push(cwd.to_string());
            }
        });
    }

    /// 写入 tier_models 快照（创建时从全局 config 读，让历史 session 还原当时的 fast/pro/max）。
    pub fn set_tier_models(id: &str, tier_models: serde_json::Value) {
        Self::patch_meta(id, |m| {
            m.tier_models = Some(tier_models);
        });
    }

    /// 写入权限模式（permissive/standard/strict/autopilot/readonly，创建时记录）。
    pub fn set_security_profile(id: &str, profile: &str) {
        Self::patch_meta(id, |m| {
            m.security_profile = Some(profile.to_string());
        });
    }

    /// Count how many sessions in the index match the given project key.
    /// Loads the index from disk and counts entries where `project` == `project_key`.
    pub fn count_sessions_by_project(&self, project_key: &str) -> Result<i64, String> {
        let count = self
            .sessions
            .values()
            .filter(|m| m.project.as_deref() == Some(project_key))
            .count();
        Ok(count as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_meta(parent: Option<&str>) -> SessionMeta {
        SessionMeta {
            name: None,
            first_name: None,
            project: None,
            project_name: None,
            worktree: false,
            workspace_path: None,
            workspace_status: None,
            branch: None,
            model: "test".into(),
            agent: "default".into(),
            provider: "test".into(),
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
            parent_session: parent.map(|s| s.to_string()),
            parent_type: parent.map(|_| "fork".to_string()),
            initial_cwd: None,
            last_cwd: None,
            extra_cwds: Vec::new(),
            tier_models: None,
            security_profile: None,
        }
    }

    #[test]
    fn test_get_children() {
        let mut idx = SessionIndex::default();
        idx.sessions.insert("root".into(), make_meta(None));
        idx.sessions
            .insert("child1".into(), make_meta(Some("root")));
        idx.sessions
            .insert("child2".into(), make_meta(Some("root")));
        idx.sessions
            .insert("other".into(), make_meta(Some("different")));

        let children = idx.get_children("root");
        assert_eq!(children.len(), 2);
    }

    #[test]
    fn test_has_children() {
        let mut idx = SessionIndex::default();
        idx.sessions.insert("root".into(), make_meta(None));
        assert!(!idx.has_children("root"));

        idx.sessions.insert("child".into(), make_meta(Some("root")));
        assert!(idx.has_children("root"));
    }

    #[test]
    fn test_child_count() {
        let mut idx = SessionIndex::default();
        idx.sessions.insert("root".into(), make_meta(None));
        idx.sessions.insert("c1".into(), make_meta(Some("root")));
        idx.sessions.insert("c2".into(), make_meta(Some("root")));
        idx.sessions.insert("c3".into(), make_meta(Some("root")));
        assert_eq!(idx.child_count("root"), 3);
        assert_eq!(idx.child_count("nonexistent"), 0);
    }

    #[test]
    fn test_root_session_has_no_parent() {
        let idx = SessionIndex::default();
        let mut sessions = idx.sessions.clone();
        sessions.insert("root".into(), make_meta(None));
        let idx2 = SessionIndex { sessions, removed_sessions: Default::default() };
        let root = idx2.get("root").unwrap();
        assert!(root.parent_session.is_none());
    }

    #[test]
    fn test_forked_session_has_parent() {
        let mut sessions = std::collections::HashMap::new();
        sessions.insert("fork".into(), make_meta(Some("parent_sess")));
        let idx = SessionIndex { sessions, removed_sessions: Default::default() };
        let fork = idx.get("fork").unwrap();
        assert_eq!(fork.parent_session.as_deref(), Some("parent_sess"));
        assert_eq!(fork.parent_type.as_deref(), Some("fork"));
    }

    #[test]
    fn test_count_sessions_by_project() {
        use std::io::Write;

        // Create a temp index file with known data
        let tmp_dir = std::env::temp_dir();
        let index_path = tmp_dir.join("test_sessions_index.json");

        let test_data = serde_json::json!({
            "sessions": {
                "sess1": {
                    "name": null,
                    "first_name": null,
                    "project": "my-project",
                    "project_name": "my-project",
                    "worktree": false,
                    "branch": null,
                    "model": "gpt4",
                    "agent": "default",
                    "provider": "openai",
                    "token_input": 0,
                    "token_output": 0,
                    "token_cache_read": 0,
                    "token_cache_write": 0,
                    "compress_count": 0,
                    "message_count": 0,
                    "turn_count": 0,
                    "created_at": 0,
                    "updated_at": 0,
                    "error_count": 0,
                    "last_thinking_level": null,
                    "last_active_tools": null,
                    "last_entry_id": null,
                    "parent_session": null,
                    "parent_type": null
                },
                "sess2": {
                    "name": null,
                    "first_name": null,
                    "project": "my-project",
                    "project_name": "my-project",
                    "worktree": false,
                    "branch": null,
                    "model": "gpt4",
                    "agent": "default",
                    "provider": "openai",
                    "token_input": 0,
                    "token_output": 0,
                    "token_cache_read": 0,
                    "token_cache_write": 0,
                    "compress_count": 0,
                    "message_count": 0,
                    "turn_count": 0,
                    "created_at": 0,
                    "updated_at": 0,
                    "error_count": 0,
                    "last_thinking_level": null,
                    "last_active_tools": null,
                    "last_entry_id": null,
                    "parent_session": null,
                    "parent_type": null
                },
                "sess3": {
                    "name": null,
                    "first_name": null,
                    "project": "other-project",
                    "project_name": "other-project",
                    "worktree": false,
                    "branch": null,
                    "model": "gpt4",
                    "agent": "default",
                    "provider": "openai",
                    "token_input": 0,
                    "token_output": 0,
                    "token_cache_read": 0,
                    "token_cache_write": 0,
                    "compress_count": 0,
                    "message_count": 0,
                    "turn_count": 0,
                    "created_at": 0,
                    "updated_at": 0,
                    "error_count": 0,
                    "last_thinking_level": null,
                    "last_active_tools": null,
                    "last_entry_id": null,
                    "parent_session": null,
                    "parent_type": null
                }
            }
        });

        // Backup the real index path and override HOME/temp
        let _original_home = std::env::var("HOME").ok();

        // Write temp index and override the path used by SessionIndex::path()
        {
            let mut file = std::fs::File::create(&index_path).unwrap();
            file.write_all(serde_json::to_string_pretty(&test_data).unwrap().as_bytes())
                .unwrap();
        }

        // Temporarily set HOME to the temp dir so SessionIndex::path() resolves to our test file
        // We can't easily override path(), so let's just use the index directly.
        let _idx = SessionIndex::default();
        // Parse the test data into a SessionIndex and count
        let parsed: SessionIndex = serde_json::from_value(test_data).unwrap();
        let count = parsed.count_sessions_by_project("my-project").unwrap();
        assert_eq!(count, 2, "Expected 2 sessions for 'my-project'");

        let count_other = parsed.count_sessions_by_project("other-project").unwrap();
        assert_eq!(count_other, 1, "Expected 1 session for 'other-project'");

        let count_none = parsed.count_sessions_by_project("nonexistent").unwrap();
        assert_eq!(count_none, 0, "Expected 0 sessions for 'nonexistent'");

        // Cleanup temp file
        let _ = std::fs::remove_file(&index_path);
    }
}
