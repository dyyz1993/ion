use crate::ids::WorkerId;
use crate::types::timestamp_now;
use crate::worker::{WorkerHandle, WorkerStatus};
use std::time::Duration;

/// A slot in the worker pool — metadata for one worker.
#[derive(Debug)]
pub struct WorkerSlot {
    pub worker_id: WorkerId,
    pub handle: WorkerHandle,
    pub status: WorkerStatus,
    pub created_at: f64,
    pub last_active: f64,
    pub task_id: Option<String>,
}

impl WorkerSlot {
    pub fn new(worker_id: WorkerId, handle: WorkerHandle) -> Self {
        let now = timestamp_now();
        Self {
            worker_id,
            handle,
            status: WorkerStatus::Idle,
            created_at: now,
            last_active: now,
            task_id: None,
        }
    }

    /// Returns `true` if the worker has been idle for longer than `timeout`.
    pub fn is_idle_expired(&self, timeout: Duration) -> bool {
        self.status == WorkerStatus::Idle
            && timestamp_now() - self.last_active > timeout.as_secs_f64()
    }
}
