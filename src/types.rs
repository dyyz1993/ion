use crate::ids::{SessionId, TaskId};
use serde::{Deserialize, Serialize};
use std::time::Duration;

// ---------------------------------------------------------------------------
// Pool configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct PoolOptions {
    /// Minimum workers alive (idle). Default 0.
    pub min_workers: usize,
    /// Maximum concurrent workers. Default 10.
    pub max_workers: usize,
    /// How long an idle worker lives before being reaped. Default 5 min.
    pub idle_timeout: Duration,
    /// Hard timeout for a single task. Default 1 h.
    pub worker_timeout: Duration,
}

impl Default for PoolOptions {
    fn default() -> Self {
        Self {
            min_workers: 0,
            max_workers: 10,
            idle_timeout: Duration::from_secs(300),
            worker_timeout: Duration::from_secs(3600),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TaskConfig {
    pub max_retries: u32,
}

impl Default for TaskConfig {
    fn default() -> Self {
        Self { max_retries: 3 }
    }
}

// ---------------------------------------------------------------------------
// Stats (returned to clients — all owned, 'static)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Default)]
pub struct PoolStats {
    pub total_workers: usize,
    pub idle_workers: usize,
    pub claimed_workers: usize,
    pub running_workers: usize,
    pub dead_workers: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub status: TaskStatus,
    pub created_at: f64,
    pub started_at: Option<f64>,
    pub completed_at: Option<f64>,
    pub retry_count: u32,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    /// Returns `true` if this is a terminal status.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Task {
    pub id: TaskId,
    pub payload: TaskPayload,
    pub session_id: Option<SessionId>,
    pub retry_count: u32,
    pub max_retries: u32,
    pub created_at: f64,
    pub started_at: Option<f64>,
    pub completed_at: Option<f64>,
}

impl Task {
    pub fn new(payload: TaskPayload) -> Self {
        let now = timestamp_now();
        Self {
            id: TaskId::new(),
            payload,
            session_id: None,
            retry_count: 0,
            max_retries: TaskConfig::default().max_retries,
            created_at: now,
            started_at: None,
            completed_at: None,
        }
    }

    pub fn with_session(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskPayload {
    Prompt(String),
    Steer(String),
    Delegate(String),
    Fork(String),
}

impl TaskPayload {
    pub fn description(&self) -> &str {
        match self {
            Self::Prompt(_) => "prompt",
            Self::Steer(_) => "steer",
            Self::Delegate(_) => "delegate",
            Self::Fork(_) => "fork",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskResult {
    pub success: bool,
    pub output: String,
    pub tokens_used: Option<u64>,
}

impl TaskResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            success: true,
            output: output.into(),
            tokens_used: None,
        }
    }

    pub fn err(output: impl Into<String>) -> Self {
        Self {
            success: false,
            output: output.into(),
            tokens_used: None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct SessionState {
    pub message_count: u64,
    pub turn_index: u64,
    pub summary: Option<String>,
}

// ---------------------------------------------------------------------------
// Helper
// ---------------------------------------------------------------------------

/// A monotonic-ish timestamp (sub-seconds since epoch).
pub fn timestamp_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_transitions_are_detected() {
        assert!(!TaskStatus::Queued.is_terminal());
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn task_gets_unique_id() {
        let t1 = Task::new(TaskPayload::Prompt("hi".into()));
        let t2 = Task::new(TaskPayload::Prompt("bye".into()));
        assert_ne!(t1.id, t2.id);
    }

    #[test]
    fn task_payload_description() {
        assert_eq!(TaskPayload::Prompt("x".into()).description(), "prompt");
        assert_eq!(TaskPayload::Steer("x".into()).description(), "steer");
    }
}
