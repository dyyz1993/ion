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
    /// 最后一条 entry 的 id（增量拉取锚点）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_entry_id: Option<String>,
    /// 父会话 id（fork 来源，null = 根会话）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    /// 父会话关系类型（fork）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_type: Option<String>,
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
            last_entry_id: existing.as_ref().and_then(|e| e.last_entry_id.clone()),
            parent_session: existing.as_ref().and_then(|e| e.parent_session.clone()),
            parent_type: existing.as_ref().and_then(|e| e.parent_type.clone()),
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
                    last_entry_id: None,
                    parent_session: None,
                    parent_type: None,
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
            branch: None,
            model: "test".into(),
            agent: "default".into(),
            provider: "test".into(),
            token_input: 0,
            token_output: 0,
            token_cache_read: 0,
            token_cache_write: 0,
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
        }
    }

    #[test]
    fn test_get_children() {
        let mut idx = SessionIndex::default();
        idx.sessions.insert("root".into(), make_meta(None));
        idx.sessions.insert("child1".into(), make_meta(Some("root")));
        idx.sessions.insert("child2".into(), make_meta(Some("root")));
        idx.sessions.insert("other".into(), make_meta(Some("different")));

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
        let idx2 = SessionIndex { sessions };
        let root = idx2.get("root").unwrap();
        assert!(root.parent_session.is_none());
    }

    #[test]
    fn test_forked_session_has_parent() {
        let mut sessions = std::collections::HashMap::new();
        sessions.insert("fork".into(), make_meta(Some("parent_sess")));
        let idx = SessionIndex { sessions };
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
