use crate::error::{IonError, IonResult};
use crate::ids::TaskId;
use crate::types::{Task, TaskConfig, TaskResult, TaskSnapshot, TaskStatus, timestamp_now};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------------
// Queue commands
// ---------------------------------------------------------------------------

pub enum QueueCmd {
    Enqueue {
        task: Task,
        reply: oneshot::Sender<TaskId>,
    },
    Dequeue {
        reply: oneshot::Sender<Option<Task>>,
    },
    Complete {
        task_id: TaskId,
        result: TaskResult,
        reply: oneshot::Sender<IonResult<TaskStatus>>,
    },
    Fail {
        task_id: TaskId,
        error: String,
        reply: oneshot::Sender<IonResult<TaskStatus>>,
    },
    Cancel {
        task_id: TaskId,
        reply: oneshot::Sender<bool>,
    },
    Snapshot {
        task_id: TaskId,
        reply: oneshot::Sender<Option<TaskSnapshot>>,
    },
    List {
        reply: oneshot::Sender<Vec<TaskSnapshot>>,
    },
    Stats {
        reply: oneshot::Sender<QueueStats>,
    },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Default)]
pub struct QueueStats {
    pub queued: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub cancelled: usize,
}

// ---------------------------------------------------------------------------
// QueueHandle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct QueueHandle {
    tx: mpsc::Sender<QueueCmd>,
}

impl QueueHandle {
    pub fn new(tx: mpsc::Sender<QueueCmd>) -> Self {
        Self { tx }
    }

    pub async fn enqueue(&self, task: Task) -> IonResult<TaskId> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(QueueCmd::Enqueue { task, reply })
            .await
            .map_err(|_| IonError::Queue("queue closed".into()))?;
        rx.await
            .map_err(|_| IonError::Queue("reply cancelled".into()))
    }

    pub async fn dequeue(&self) -> IonResult<Option<Task>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(QueueCmd::Dequeue { reply })
            .await
            .map_err(|_| IonError::Queue("queue closed".into()))?;
        rx.await
            .map_err(|_| IonError::Queue("reply cancelled".into()))
    }

    pub async fn complete(&self, task_id: TaskId, result: TaskResult) -> IonResult<TaskStatus> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(QueueCmd::Complete {
                task_id,
                result,
                reply,
            })
            .await
            .map_err(|_| IonError::Queue("queue closed".into()))?;
        rx.await
            .map_err(|_| IonError::Queue("reply cancelled".into()))?
    }

    pub async fn fail(&self, task_id: TaskId, error: String) -> IonResult<TaskStatus> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(QueueCmd::Fail {
                task_id,
                error,
                reply,
            })
            .await
            .map_err(|_| IonError::Queue("queue closed".into()))?;
        rx.await
            .map_err(|_| IonError::Queue("reply cancelled".into()))?
    }

    pub async fn cancel(&self, task_id: TaskId) -> IonResult<bool> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(QueueCmd::Cancel { task_id, reply })
            .await
            .map_err(|_| IonError::Queue("queue closed".into()))?;
        rx.await
            .map_err(|_| IonError::Queue("reply cancelled".into()))
    }

    pub async fn snapshot(&self, task_id: TaskId) -> IonResult<Option<TaskSnapshot>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(QueueCmd::Snapshot { task_id, reply })
            .await
            .map_err(|_| IonError::Queue("queue closed".into()))?;
        rx.await
            .map_err(|_| IonError::Queue("reply cancelled".into()))
    }

    pub async fn list(&self) -> IonResult<Vec<TaskSnapshot>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(QueueCmd::List { reply })
            .await
            .map_err(|_| IonError::Queue("queue closed".into()))?;
        rx.await
            .map_err(|_| IonError::Queue("reply cancelled".into()))
    }

    pub async fn stats(&self) -> IonResult<QueueStats> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(QueueCmd::Stats { reply })
            .await
            .map_err(|_| IonError::Queue("queue closed".into()))?;
        rx.await
            .map_err(|_| IonError::Queue("reply cancelled".into()))
    }
}

// ---------------------------------------------------------------------------
// TaskQueue actor
// ---------------------------------------------------------------------------

pub struct TaskQueue {
    handle: QueueHandle,
    join_handle: tokio::task::JoinHandle<()>,
}

impl TaskQueue {
    pub fn new(config: TaskConfig) -> Self {
        let (tx, rx) = mpsc::channel(256);
        let handle = QueueHandle::new(tx);

        let join_handle = tokio::spawn(async move {
            let mut actor = QueueActor::new(config, rx);
            actor.run().await;
        });

        Self {
            handle,
            join_handle,
        }
    }

    pub fn handle(&self) -> &QueueHandle {
        &self.handle
    }

    pub async fn shutdown(self) {
        let _ = self.handle.tx.send(QueueCmd::Shutdown).await;
        let _ = self.join_handle.await;
    }
}

// ---------------------------------------------------------------------------
// QueueActor — internal state
// ---------------------------------------------------------------------------

struct QueueActor {
    config: TaskConfig,
    pending: VecDeque<Task>,
    tasks: HashMap<TaskId, TrackedTask>,
    rx: mpsc::Receiver<QueueCmd>,
}

struct TrackedTask {
    status: TaskStatus,
    result: Option<TaskResult>,
    error: Option<String>,
    retry_count: u32,
    created_at: f64,
    started_at: Option<f64>,
    completed_at: Option<f64>,
}

impl QueueActor {
    fn new(config: TaskConfig, rx: mpsc::Receiver<QueueCmd>) -> Self {
        Self {
            config,
            pending: VecDeque::new(),
            tasks: HashMap::new(),
            rx,
        }
    }

    async fn run(&mut self) {
        while let Some(cmd) = self.rx.recv().await {
            match cmd {
                QueueCmd::Enqueue { task, reply } => {
                    let id = task.id;
                    self.tasks.insert(
                        id,
                        TrackedTask {
                            status: TaskStatus::Queued,
                            result: None,
                            error: None,
                            retry_count: 0,
                            created_at: task.created_at,
                            started_at: None,
                            completed_at: None,
                        },
                    );
                    self.pending.push_back(task);
                    let _ = reply.send(id);
                }
                QueueCmd::Dequeue { reply } => {
                    let task = self.pending.pop_front();
                    if let Some(ref t) = task
                        && let Some(tracked) = self.tasks.get_mut(&t.id)
                    {
                        tracked.status = TaskStatus::Running;
                        tracked.started_at = Some(timestamp_now());
                    }
                    let _ = reply.send(task);
                }
                QueueCmd::Complete {
                    task_id,
                    result,
                    reply,
                } => {
                    let res = self.complete(task_id, result);
                    let _ = reply.send(res);
                }
                QueueCmd::Fail {
                    task_id,
                    error,
                    reply,
                } => {
                    let res = self.fail(task_id, error);
                    let _ = reply.send(res);
                }
                QueueCmd::Cancel { task_id, reply } => {
                    let cancelled = self.cancel(task_id);
                    let _ = reply.send(cancelled);
                }
                QueueCmd::Snapshot { task_id, reply } => {
                    let snap = self.snapshot(task_id);
                    let _ = reply.send(snap);
                }
                QueueCmd::List { reply } => {
                    let list: Vec<TaskSnapshot> = self
                        .tasks
                        .iter()
                        .map(|(id, t)| t.to_snapshot(*id))
                        .collect();
                    let _ = reply.send(list);
                }
                QueueCmd::Stats { reply } => {
                    let mut stats = QueueStats {
                        queued: 0,
                        running: 0,
                        completed: 0,
                        failed: 0,
                        cancelled: 0,
                    };
                    for t in self.tasks.values() {
                        match t.status {
                            TaskStatus::Queued => stats.queued += 1,
                            TaskStatus::Running => stats.running += 1,
                            TaskStatus::Completed => stats.completed += 1,
                            TaskStatus::Failed => stats.failed += 1,
                            TaskStatus::Cancelled => stats.cancelled += 1,
                        }
                    }
                    let _ = reply.send(stats);
                }
                QueueCmd::Shutdown => break,
            }
        }
    }

    fn complete(&mut self, task_id: TaskId, result: TaskResult) -> IonResult<TaskStatus> {
        if let Some(tracked) = self.tasks.get_mut(&task_id) {
            tracked.status = TaskStatus::Completed;
            tracked.result = Some(result);
            tracked.completed_at = Some(timestamp_now());
            Ok(TaskStatus::Completed)
        } else {
            Err(IonError::TaskNotFound(task_id.to_string()))
        }
    }

    fn fail(&mut self, task_id: TaskId, error: String) -> IonResult<TaskStatus> {
        // Also remove from pending queue if still there
        self.pending.retain(|t| t.id != task_id);

        let now = timestamp_now();
        if let Some(tracked) = self.tasks.get_mut(&task_id) {
            tracked.retry_count += 1;
            tracked.error = Some(error.clone());
            tracked.completed_at = Some(now);

            if tracked.retry_count < self.config.max_retries {
                // Re-enqueue with incremented retry count
                let mut retry_task = Task::new(crate::types::TaskPayload::Prompt(error));
                retry_task.id = task_id;
                retry_task.retry_count = tracked.retry_count;
                retry_task.max_retries = self.config.max_retries;
                retry_task.created_at = tracked.created_at;
                self.pending.push_back(retry_task);
                tracked.status = TaskStatus::Queued;
                tracked.started_at = None;
                Ok(TaskStatus::Queued)
            } else {
                tracked.status = TaskStatus::Failed;
                Ok(TaskStatus::Failed)
            }
        } else {
            Err(IonError::TaskNotFound(task_id.to_string()))
        }
    }

    fn cancel(&mut self, task_id: TaskId) -> bool {
        self.pending.retain(|t| t.id != task_id);
        if let Some(tracked) = self.tasks.get_mut(&task_id)
            && !tracked.status.is_terminal()
        {
            tracked.status = TaskStatus::Cancelled;
            tracked.completed_at = Some(timestamp_now());
            return true;
        }
        false
    }

    fn snapshot(&self, task_id: TaskId) -> Option<TaskSnapshot> {
        self.tasks.get(&task_id).map(|t| t.to_snapshot(task_id))
    }
}

impl TrackedTask {
    fn to_snapshot(&self, id: TaskId) -> TaskSnapshot {
        TaskSnapshot {
            id,
            status: self.status.clone(),
            created_at: self.created_at,
            started_at: self.started_at,
            completed_at: self.completed_at,
            retry_count: self.retry_count,
            error: self.error.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TaskPayload;

    #[tokio::test]
    async fn enqueue_dequeue() {
        let queue = TaskQueue::new(TaskConfig::default());
        let task = Task::new(TaskPayload::Prompt("hello".into()));
        let id = queue.handle().enqueue(task).await.unwrap();
        let dequeued = queue.handle().dequeue().await.unwrap().unwrap();
        assert_eq!(dequeued.id, id);
        queue.shutdown().await;
    }

    #[tokio::test]
    async fn enqueue_complete() {
        let queue = TaskQueue::new(TaskConfig::default());
        let task = Task::new(TaskPayload::Prompt("hello".into()));
        let id = queue.handle().enqueue(task).await.unwrap();
        queue.handle().dequeue().await.unwrap();
        let status = queue
            .handle()
            .complete(id, TaskResult::ok("done"))
            .await
            .unwrap();
        assert_eq!(status, TaskStatus::Completed);
        queue.shutdown().await;
    }

    #[tokio::test]
    async fn fail_triggers_retry() {
        let queue = TaskQueue::new(TaskConfig { max_retries: 2 });
        let task = Task::new(TaskPayload::Prompt("hello".into()));
        let id = queue.handle().enqueue(task).await.unwrap();
        queue.handle().dequeue().await.unwrap();
        let status = queue.handle().fail(id, "oops".into()).await.unwrap();
        // Should be re-queued
        assert_eq!(status, TaskStatus::Queued);
        let snap = queue.handle().snapshot(id).await.unwrap().unwrap();
        assert_eq!(snap.retry_count, 1);
        queue.shutdown().await;
    }

    #[tokio::test]
    async fn fail_exhausts_retries() {
        let queue = TaskQueue::new(TaskConfig { max_retries: 1 });
        let task = Task::new(TaskPayload::Prompt("hello".into()));
        let id = queue.handle().enqueue(task).await.unwrap();
        queue.handle().dequeue().await.unwrap();
        let _ = queue.handle().fail(id, "oops".into()).await;
        // Re-queued once, now try again
        queue.handle().dequeue().await.unwrap();
        let status = queue.handle().fail(id, "oops again".into()).await.unwrap();
        assert_eq!(status, TaskStatus::Failed);
        queue.shutdown().await;
    }

    #[tokio::test]
    async fn cancel_pending() {
        let queue = TaskQueue::new(TaskConfig::default());
        let task = Task::new(TaskPayload::Prompt("hello".into()));
        let id = queue.handle().enqueue(task).await.unwrap();
        let cancelled = queue.handle().cancel(id).await.unwrap();
        assert!(cancelled);
        let snap = queue.handle().snapshot(id).await.unwrap().unwrap();
        assert_eq!(snap.status, TaskStatus::Cancelled);
        queue.shutdown().await;
    }
}
