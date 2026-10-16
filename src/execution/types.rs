use serde::{Deserialize, Serialize};

pub const TERMINAL_RUN_STATES: [&str; 3] = ["succeeded", "failed", "cancelled"];

#[derive(Clone, Debug)]
pub struct ToolCheckpoint {
    pub task_id: String,
    pub logical_key: String,
    pub name: String,
    pub call_id: String,
    pub arguments: serde_json::Value,
    pub policy: crate::agent::tool::RecoveryPolicy,
}

#[derive(Clone, Debug)]
pub struct ChildRunPlan {
    pub parent_task_id: String,
    pub spawn_key: String,
    pub session_id: String,
    pub input: serde_json::Value,
}

#[derive(Clone, Debug)]
pub struct ApprovalCheckpoint {
    pub approval_id: String,
    pub task_id: String,
    pub action_digest: String,
    pub generation: i64,
    pub policy_version: String,
}

#[derive(Clone, Debug)]
pub struct AttemptLease {
    pub task_id: String,
    pub attempt_id: String,
    pub attempt_no: i64,
    pub host_epoch: i64,
    pub generation: i64,
    pub fence: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutionCursor {
    pub store_id: String,
    pub run_id: String,
    pub seq: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ResumeActions {
    #[serde(default)]
    pub resume_allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub logical_key: String,
    pub kind: String,
    pub state: String,
    pub version: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunSnapshot {
    pub run_id: String,
    pub session_id: String,
    pub branch_id: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    pub version: i64,
    pub generation: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_plan_id: Option<String>,
    pub cursor: ExecutionCursor,
    #[serde(default)]
    pub tasks: Vec<TaskSummary>,
    pub actions: ResumeActions,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunListEntry {
    pub run_id: String,
    pub session_id: String,
    pub state: String,
    pub version: i64,
    pub updated_ms: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResumeReceipt {
    pub status: String,
    pub operation_id: String,
    pub accepted_version: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventPage {
    pub cursor: ExecutionCursor,
    pub events: Vec<serde_json::Value>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ToolOutcome {
    pub task_id: String,
    pub success: bool,
    pub output: String,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ReconciliationSummary {
    pub host_epoch: i64,
    pub runs_scanned: usize,
    pub changed_attempts: usize,
    pub auto_resumed: usize,
}
