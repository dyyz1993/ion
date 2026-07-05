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
}

/// Index of all sessions, stored in sessions.index.json
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct SessionIndex {
    pub sessions: HashMap<String, SessionMeta>,
}

impl SessionIndex {
    pub fn path() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".into());
        PathBuf::from(home).join(".ion").join("agent").join("sessions.index.json")
    }

    pub fn load() -> Self {
        let path = Self::path();
        if !path.exists() {
            return Self { sessions: HashMap::new() };
        }
        match std::fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self { sessions: HashMap::new() },
        }
    }

    pub fn save(&self) {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(content) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, &content);
        }
    }

    pub fn get(&self, id: &str) -> Option<&SessionMeta> {
        self.sessions.get(id)
    }

    pub fn upsert(&mut self, id: &str, meta: SessionMeta) {
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
            std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string())
        });
        let project_name = project_path.as_ref().and_then(|p| {
            std::path::Path::new(p).file_name().map(|n| n.to_string_lossy().to_string())
        });

        let branch = std::process::Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout).ok().map(|s| s.trim().to_string())
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
            name: name.map(|s| s.to_string()).or(existing.as_ref().and_then(|e| e.name.clone())),
            first_name: existing.as_ref().and_then(|e| e.first_name.clone()).or(name.map(|s| s.to_string())),
            project: project_path,
            project_name,
            worktree: is_worktree,
            branch,
            model: model.to_string(),
            agent: agent.to_string(),
            provider: provider.to_string(),
            token_input: existing.as_ref().map_or(0, |e| e.token_input) + token_input,
            token_output: existing.as_ref().map_or(0, |e| e.token_output) + token_output,
            token_cache_read: existing.as_ref().map_or(0, |e| e.token_cache_read) + token_cache,
            token_cache_write: 0,
            compress_count: existing.as_ref().map_or(0, |e| e.compress_count),
            message_count: existing.as_ref().map_or(0, |e| e.message_count) + message_count,
            turn_count: existing.as_ref().map_or(0, |e| e.turn_count) + turn_count,
            created_at: existing.as_ref().map_or(now, |e| e.created_at),
            updated_at: now,
            error_count: existing.as_ref().map_or(0, |e| e.error_count),
            last_thinking_level: existing.as_ref().and_then(|e| e.last_thinking_level.clone()),
            last_active_tools: existing.as_ref().and_then(|e| e.last_active_tools.clone()),
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
            id, model, provider, agent, name, None,
            token_input, token_output, 0,
            message_count, turn_count,
        );
        let mut index = Self::load();
        index.upsert(id, meta);
        index.save();
    }

    /// Patch specific fields on an existing session meta without rebuilding the whole entry.
    /// Used by append_* RPCs to keep the index in sync (e.g. thinking level, active tools, name).
    /// If the session isn't yet in the index, creates a minimal entry first.
    pub fn patch_meta<F>(id: &str, patch_fn: F)
    where
        F: FnOnce(&mut SessionMeta),
    {
        let mut index = Self::load();
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
                    model: String::new(),
                    agent: "default".to_string(),
                    provider: String::new(),
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
                },
            );
        }

        if let Some(meta) = index.sessions.get_mut(id) {
            patch_fn(meta);
            meta.updated_at = now;
            index.save();
        }
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
}
