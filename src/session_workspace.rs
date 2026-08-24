//! Workspace Session — 会话工作空间的元数据模型与持久化。
//!
//! 对应设计文档 docs/design/SESSION_WORKSPACE_CHAT.md §2.1：
//! 用户在 Session A 里创建一个绑定独立 Git worktree 的子 Session B，
//! 元数据（workspace 路径/分支/状态/父会话）必须持久化，重启后可恢复。
//!
//! 存储位置：`~/.ion/agent/workspaces.json`（跟随 ION_AGENT_DIR）。
//! 状态机：creating → ready → running → idle → closed，任一步可 → failed。
//! running/idle 是运行态（由 worker Busy/Idle 实时推断，不落盘），
//! 落盘的生命周期状态是 creating / ready / closed / failed。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 工作空间会话状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WorkspaceStatus {
    Creating,
    Ready,
    Running,
    Idle,
    Closed,
    Failed,
}

impl WorkspaceStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkspaceStatus::Creating => "creating",
            WorkspaceStatus::Ready => "ready",
            WorkspaceStatus::Running => "running",
            WorkspaceStatus::Idle => "idle",
            WorkspaceStatus::Closed => "closed",
            WorkspaceStatus::Failed => "failed",
        }
    }
}

/// 工作空间会话元数据（字段命名对齐设计文档 §2.1 的 camelCase JSON）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceSession {
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "parentSessionId")]
    pub parent_session_id: String,
    /// 主仓库路径（worktree 从这里切出）。
    #[serde(rename = "projectPath")]
    pub project_path: String,
    /// worktree 目录绝对路径。
    #[serde(rename = "workspacePath")]
    pub workspace_path: String,
    /// worktree 分支名。
    pub branch: String,
    /// 切出基准（None = HEAD）。
    #[serde(rename = "baseRef")]
    pub base_ref: Option<String>,
    /// 展示标题。
    pub title: String,
    /// 生命周期状态（creating/ready/closed/failed；running/idle 运行态实时推断）。
    pub status: WorkspaceStatus,
    /// UI 路由（"#/sessions/<sid>"）。
    pub route: String,
    /// 创建时间（毫秒时间戳）。
    #[serde(rename = "createdAt")]
    pub created_at: u64,
    /// failed 时的错误信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 存储落位（遵循 AGENTS.md 存储落位原则）：**不建 sidecar 文件**。
/// 热字段（branch/workspace_path/workspace_status）随生命周期顺便更新进 SessionIndex；
/// 完整可还原细节需要时以 custom entry 进会话 JSONL。本模块只提供索引之上的薄封装。
impl WorkspaceSession {
    /// 写入/更新索引中的工作空间字段（创建路径调用，status=ready）。
    pub fn upsert_index(&self) {
        let mut idx = crate::session_index::SessionIndex::load();
        idx.update_workspace(
            &self.session_id,
            Some(&self.branch),
            Some(&self.workspace_path),
            Some(self.status.as_str()),
        );
        idx.save();
    }

    /// 从索引恢复（快照/刷新恢复路径调用）。索引无记录（非工作空间会话）返回 None。
    pub fn from_index(session_id: &str) -> Option<WorkspaceSession> {
        let idx = crate::session_index::SessionIndex::load();
        let meta = idx.get(session_id)?;
        if meta.workspace_path.is_none() && !meta.worktree {
            return None;
        }
        Some(WorkspaceSession {
            session_id: session_id.to_string(),
            parent_session_id: meta.parent_session.clone().unwrap_or_default(),
            project_path: meta.project.clone().unwrap_or_default(),
            workspace_path: meta.workspace_path.clone().unwrap_or_default(),
            branch: meta.branch.clone().unwrap_or_default(),
            base_ref: None,
            title: meta.name.clone().unwrap_or_else(|| meta.project_name.clone().unwrap_or_default()),
            status: meta
                .workspace_status
                .as_deref()
                .and_then(|s| match s {
                    "creating" => Some(WorkspaceStatus::Creating),
                    "ready" => Some(WorkspaceStatus::Ready),
                    "running" => Some(WorkspaceStatus::Running),
                    "idle" => Some(WorkspaceStatus::Idle),
                    "closed" => Some(WorkspaceStatus::Closed),
                    "failed" => Some(WorkspaceStatus::Failed),
                    _ => None,
                })
                .unwrap_or(WorkspaceStatus::Ready),
            route: format!("#/sessions/{session_id}"),
            created_at: meta.created_at.max(0) as u64,
            error: None,
        })
    }

    /// 更新状态到索引（关闭/失败路径调用）。
    pub fn set_status_index(session_id: &str, status: WorkspaceStatus) {
        let mut idx = crate::session_index::SessionIndex::load();
        idx.update_workspace(session_id, None, None, Some(status.as_str()));
        idx.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camel_case_json_matches_design_doc() {
        let ws = WorkspaceSession {
            session_id: "sess_b2".to_string(),
            parent_session_id: "sess_parent".to_string(),
            project_path: "/tmp/repo".to_string(),
            workspace_path: "/tmp/wt/repo".to_string(),
            branch: "feat/demo".to_string(),
            base_ref: Some("main".to_string()),
            title: "演示".to_string(),
            status: WorkspaceStatus::Ready,
            route: "#/sessions/sess_b2".to_string(),
            created_at: 1787000000000u64,
            error: None,
        };
        let json = serde_json::to_value(&ws).unwrap();
        assert_eq!(json["sessionId"], "sess_b2");
        assert_eq!(json["parentSessionId"], "sess_parent");
        assert_eq!(json["workspacePath"], "/tmp/wt/repo");
        assert_eq!(json["baseRef"], "main");
        assert_eq!(json["createdAt"], 1787000000000u64);
        assert_eq!(json["status"], "ready");
    }

    #[test]
    fn status_str_roundtrip() {
        for s in ["creating", "ready", "running", "idle", "closed", "failed"] {
            let st = match s {
                "creating" => WorkspaceStatus::Creating,
                "ready" => WorkspaceStatus::Ready,
                "running" => WorkspaceStatus::Running,
                "idle" => WorkspaceStatus::Idle,
                "closed" => WorkspaceStatus::Closed,
                _ => WorkspaceStatus::Failed,
            };
            assert_eq!(st.as_str(), s);
        }
    }
}
