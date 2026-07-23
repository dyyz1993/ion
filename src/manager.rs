use crate::error::{IonError, IonResult};
use crate::event::{Event, EventBus};
use crate::ids::TaskId;
use crate::pool::{PoolHandle, WorkerPool};
use crate::queue::{QueueHandle, QueueStats, TaskQueue};
use crate::session::{InMemorySessionStore, SessionStore};
use crate::types::{
    PoolOptions, PoolStats, Task, TaskConfig, TaskPayload, TaskResult, TaskSnapshot,
};
use crate::worker::{Worker, WorkerHandle};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Manager commands
// ---------------------------------------------------------------------------

pub enum ManagerCmd {
    Submit {
        task: Task,
        reply: oneshot::Sender<IonResult<TaskId>>,
    },
    Status {
        task_id: TaskId,
        reply: oneshot::Sender<Option<TaskSnapshot>>,
    },
    Cancel {
        task_id: TaskId,
        reply: oneshot::Sender<IonResult<bool>>,
    },
    Subscribe {
        reply: oneshot::Sender<tokio::sync::broadcast::Receiver<Event>>,
    },
    List {
        reply: oneshot::Sender<Vec<TaskSnapshot>>,
    },
    PoolStats {
        reply: oneshot::Sender<PoolStats>,
    },
    QueueStats {
        reply: oneshot::Sender<QueueStats>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// ManagerHandle
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct ManagerHandle {
    tx: mpsc::Sender<ManagerCmd>,
}

impl ManagerHandle {
    pub fn new(tx: mpsc::Sender<ManagerCmd>) -> Self {
        Self { tx }
    }

    /// Submit a task. The task gets a unique `TaskId` assigned.
    pub async fn submit(&self, payload: impl Into<TaskPayload>) -> IonResult<TaskId> {
        let payload = payload.into();
        let task = Task::new(payload);
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::Submit { task, reply })
            .await
            .map_err(|_| IonError::Shutdown)?;
        rx.await.map_err(|_| IonError::Shutdown)?
    }

    pub async fn status(&self, task_id: TaskId) -> IonResult<Option<TaskSnapshot>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::Status { task_id, reply })
            .await
            .map_err(|_| IonError::Shutdown)?;
        Ok(rx.await.unwrap_or(None))
    }

    pub async fn cancel(&self, task_id: TaskId) -> IonResult<bool> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::Cancel { task_id, reply })
            .await
            .map_err(|_| IonError::Shutdown)?;
        rx.await.map_err(|_| IonError::Shutdown)?
    }

    pub async fn subscribe(&self) -> IonResult<tokio::sync::broadcast::Receiver<Event>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::Subscribe { reply })
            .await
            .map_err(|_| IonError::Shutdown)?;
        rx.await.map_err(|_| IonError::Shutdown)
    }

    pub async fn list(&self) -> IonResult<Vec<TaskSnapshot>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::List { reply })
            .await
            .map_err(|_| IonError::Shutdown)?;
        Ok(rx.await.unwrap_or_default())
    }

    pub async fn pool_stats(&self) -> IonResult<PoolStats> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::PoolStats { reply })
            .await
            .map_err(|_| IonError::Shutdown)?;
        Ok(rx.await.unwrap_or(PoolStats {
            total_workers: 0,
            idle_workers: 0,
            claimed_workers: 0,
            running_workers: 0,
            dead_workers: 0,
        }))
    }

    pub async fn queue_stats(&self) -> IonResult<QueueStats> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(ManagerCmd::QueueStats { reply })
            .await
            .map_err(|_| IonError::Shutdown)?;
        Ok(rx.await.unwrap_or(QueueStats {
            queued: 0,
            running: 0,
            completed: 0,
            failed: 0,
            cancelled: 0,
        }))
    }

    pub async fn shutdown(&self) {
        let _ = self.tx.send(ManagerCmd::Shutdown).await;
    }
}

// ---------------------------------------------------------------------------
// AgentManager
// ---------------------------------------------------------------------------

pub struct AgentManager {
    pub handle: ManagerHandle,
    pub events: EventBus,
    join_handle: JoinHandle<()>,
}

impl AgentManager {
    /// Create a new `AgentManager`.
    ///
    /// The `factory` closure is called to create each new worker
    /// (e.g. `|_| Box::new(StubWorker::new("test"))`).
    pub fn new<F>(pool_options: PoolOptions, task_config: TaskConfig, factory: F) -> Self
    where
        F: Fn(crate::ids::WorkerId) -> Box<dyn Worker + Send> + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel(256);
        let handle = ManagerHandle::new(tx);
        let events = EventBus::new(256);

        let pool = WorkerPool::new(pool_options, factory);
        let queue = TaskQueue::new(task_config);
        let store = Arc::new(InMemorySessionStore::new());

        let pool_handle = pool.handle().clone();
        let queue_handle = queue.handle().clone();
        let event_bus = events.clone();

        let join_handle = tokio::spawn(async move {
            let mut mgr = ManagerActor {
                pool: pool_handle,
                queue: queue_handle,
                _store: store,
                events: event_bus,
                rx,
                dispatch_interval: Duration::from_millis(100),
                // keep pool and queue alive until manager exits
                _pool: pool,
                _queue: queue,
            };
            mgr.run().await;
        });

        Self {
            handle,
            events,
            join_handle,
        }
    }

    pub async fn join(self) {
        let _ = self.join_handle.await;
    }
}

// ---------------------------------------------------------------------------
// ManagerActor
// ---------------------------------------------------------------------------

struct ManagerActor {
    pool: PoolHandle,
    queue: QueueHandle,
    _store: Arc<dyn SessionStore>,
    events: EventBus,
    rx: mpsc::Receiver<ManagerCmd>,
    dispatch_interval: Duration,
    _pool: WorkerPool,
    _queue: TaskQueue,
}

impl ManagerActor {
    async fn run(&mut self) {
        let mut dispatch_timer = tokio::time::interval(self.dispatch_interval);
        dispatch_timer.tick().await; // skip immediate

        loop {
            tokio::select! {
                Some(cmd) = self.rx.recv() => {
                    match cmd {
                        ManagerCmd::Submit { task, reply } => {
                            let id = task.id;
                            let result = self.queue.enqueue(task).await;
                            match result {
                                Ok(task_id) => {
                                    self.events.publish(Event::TaskSubmitted {
                                        task_id: id,
                                        description: "submitted".into(),
                                    });
                                    let _ = reply.send(Ok(task_id));
                                }
                                Err(e) => {
                                    let _ = reply.send(Err(e));
                                }
                            }
                        }
                        ManagerCmd::Status { task_id, reply } => {
                            let snap = self.queue.snapshot(task_id).await.unwrap_or(None);
                            let _ = reply.send(snap);
                        }
                        ManagerCmd::Cancel { task_id, reply } => {
                            let result = self.queue.cancel(task_id).await;
                            if let Ok(true) = result {
                                self.events.publish(Event::TaskCancelled { task_id });
                            }
                            let _ = reply.send(result);
                        }
                        ManagerCmd::Subscribe { reply } => {
                            let rx = self.events.subscribe();
                            let _ = reply.send(rx);
                        }
                        ManagerCmd::List { reply } => {
                            let list = self.queue.list().await.unwrap_or_default();
                            let _ = reply.send(list);
                        }
                        ManagerCmd::PoolStats { reply } => {
                            let stats = self.pool.stats().await.unwrap_or_default();
                            let _ = reply.send(stats);
                        }
                        ManagerCmd::QueueStats { reply } => {
                            let stats = self.queue.stats().await.unwrap_or(QueueStats {
                                queued: 0, running: 0, completed: 0, failed: 0, cancelled: 0,
                            });
                            let _ = reply.send(stats);
                        }
                        ManagerCmd::Shutdown => {
                            self.events.publish(Event::ManagerShutdown);
                            break;
                        }
                    }
                }
                _ = dispatch_timer.tick() => {
                    self.dispatch().await;
                }
            }
        }
    }

    async fn dispatch(&mut self) {
        // Try to pop a task from the queue
        let task = match self.queue.dequeue().await {
            Ok(Some(t)) => t,
            Ok(None) => return,
            Err(_) => return,
        };

        let task_id = task.id;
        let payload = task.payload.clone();

        // Acquire a worker
        let worker = match self.pool.acquire().await {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("dispatch: no worker available for {task_id}: {e}");
                // Put task back
                let _ = self.queue.enqueue(task).await;
                return;
            }
        };

        let worker_id = worker.worker_id;
        let event_bus = self.events.clone();
        let queue_handle = self.queue.clone();
        let pool_handle = self.pool.clone();

        self.events.publish(Event::TaskStarted {
            task_id,
            worker_id: worker_id.to_string(),
        });

        tracing::info!("task {task_id} → worker {worker_id}");

        // Run the task in a separate task so we don't block dispatch.
        // Cloned handles (mpsc Senders) are cheap and 'static.
        tokio::spawn(async move {
            let result = run_task(&worker, task_id, &payload).await;

            match result {
                Ok(task_result) => {
                    event_bus.publish(Event::TaskCompleted {
                        task_id,
                        result: task_result.clone(),
                    });
                    let _ = queue_handle.complete(task_id, task_result).await;
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    let will_retry = match queue_handle.fail(task_id, err_msg.clone()).await {
                        Ok(ts) => ts == crate::types::TaskStatus::Queued,
                        _ => false,
                    };
                    event_bus.publish(Event::TaskFailed {
                        task_id,
                        error: err_msg,
                        will_retry,
                    });
                }
            }

            // Return worker to pool
            pool_handle.release(worker_id).await;
            tracing::info!("task {task_id} done, worker {worker_id} released");
        });
    }
}

async fn run_task(
    worker: &WorkerHandle,
    task_id: TaskId,
    payload: &TaskPayload,
) -> IonResult<TaskResult> {
    match payload {
        TaskPayload::Prompt(text) => worker.prompt(task_id, text.clone()).await,
        TaskPayload::Steer(msg) => {
            worker.steer(msg.clone()).await?;
            Ok(TaskResult::ok("steered"))
        }
        TaskPayload::Delegate(text) => worker.prompt(task_id, text.clone()).await,
        TaskPayload::Fork(text) => worker.prompt(task_id, text.clone()).await,
    }
}

/// Test helper: build a `WorkerHandle` backed by a real worker task so that
/// `prompt`/`steer` RPCs are answered without spinning up the full manager.
#[cfg(test)]
fn test_worker_handle() -> (WorkerHandle, tokio::task::JoinHandle<()>) {
    use crate::worker::{stub::StubWorker, WorkerCmd};
    let (tx, mut rx) = mpsc::channel(16);
    let worker_id = crate::ids::WorkerId::new(0u64);
    let handle = WorkerHandle::new(worker_id, tx);

    let join = tokio::spawn(async move {
        let mut worker = StubWorker::new("test-handle");
        // drive the worker command loop
        while let Some(cmd) = rx.recv().await {
            match cmd {
                WorkerCmd::Prompt { text, reply, .. } => {
                    let res = worker.prompt(text).await;
                    let _ = reply.send(res);
                }
                WorkerCmd::Steer { msg, reply } => {
                    let res = worker.steer(msg).await;
                    let _ = reply.send(res);
                }
                WorkerCmd::State { reply } => {
                    let res = worker.state().await;
                    let _ = reply.send(res);
                }
                WorkerCmd::Dispose { reply } => {
                    let res = worker.dispose().await;
                    let _ = reply.send(res);
                }
                WorkerCmd::Shutdown => break,
            }
        }
    });

    (handle, join)
}

// ---------------------------------------------------------------------------
// Unit tests for pure functions
// ---------------------------------------------------------------------------

#[cfg(test)]
mod pure_tests {
    use super::*;

    // --- TaskResult helpers -------------------------------------------------

    #[test]
    fn task_result_ok_marks_success() {
        let r = TaskResult::ok("done");
        assert!(r.success);
        assert_eq!(r.output, "done");
        assert!(r.tokens_used.is_none());
    }

    #[test]
    fn task_result_err_marks_failure() {
        let r = TaskResult::err("boom");
        assert!(!r.success);
        assert_eq!(r.output, "boom");
        assert!(r.tokens_used.is_none());
    }

    // --- TaskPayload descriptions ------------------------------------------

    #[test]
    fn payload_descriptions() {
        assert_eq!(TaskPayload::Prompt("x".into()).description(), "prompt");
        assert_eq!(TaskPayload::Steer("x".into()).description(), "steer");
        assert_eq!(TaskPayload::Delegate("x".into()).description(), "delegate");
        assert_eq!(TaskPayload::Fork("x".into()).description(), "fork");
    }

    // --- Task construction --------------------------------------------------

    #[test]
    fn new_task_has_defaults() {
        let task = Task::new(TaskPayload::Prompt("hello".into()));
        // retry_count starts at zero
        assert_eq!(task.retry_count, 0);
        // default max_retries comes from TaskConfig::default()
        assert_eq!(task.max_retries, TaskConfig::default().max_retries);
        // no session assigned yet
        assert!(task.session_id.is_none());
        // timestamps unset
        assert!(task.started_at.is_none());
        assert!(task.completed_at.is_none());
    }

    #[test]
    fn new_task_generates_unique_ids() {
        let a = Task::new(TaskPayload::Prompt("a".into()));
        let b = Task::new(TaskPayload::Prompt("b".into()));
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn task_with_max_retries_builder() {
        let task = Task::new(TaskPayload::Steer("s".into())).with_max_retries(7);
        assert_eq!(task.max_retries, 7);
    }

    #[test]
    fn task_with_session_builder() {
        let sid = crate::ids::SessionId::new("sess-1");
        let task = Task::new(TaskPayload::Prompt("p".into())).with_session(sid);
        assert!(task.session_id.is_some());
    }

    // --- run_task via StubWorker (pure-ish: no manager needed) -------------

    #[tokio::test]
    async fn run_task_prompt_returns_worker_result() {
        let (worker, join) = test_worker_handle();
        let task_id = TaskId::new();
        let payload = TaskPayload::Prompt("echo me".into());
        let result = run_task(&worker, task_id, &payload).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("echo me"));
        let _ = worker.shutdown().await;
        let _ = join.await;
    }

    #[tokio::test]
    async fn run_task_steer_returns_ok_steered() {
        let (worker, join) = test_worker_handle();
        let task_id = TaskId::new();
        let payload = TaskPayload::Steer("turn left".into());
        let result = run_task(&worker, task_id, &payload).await.unwrap();
        assert!(result.success);
        assert_eq!(result.output, "steered");
        let _ = worker.shutdown().await;
        let _ = join.await;
    }

    #[tokio::test]
    async fn run_task_delegate_routes_like_prompt() {
        let (worker, join) = test_worker_handle();
        let task_id = TaskId::new();
        let payload = TaskPayload::Delegate("do work".into());
        let result = run_task(&worker, task_id, &payload).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("do work"));
        let _ = worker.shutdown().await;
        let _ = join.await;
    }

    #[tokio::test]
    async fn run_task_fork_routes_like_prompt() {
        let (worker, join) = test_worker_handle();
        let task_id = TaskId::new();
        let payload = TaskPayload::Fork("branch".into());
        let result = run_task(&worker, task_id, &payload).await.unwrap();
        assert!(result.success);
        assert!(result.output.contains("branch"));
        let _ = worker.shutdown().await;
        let _ = join.await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::stub::StubWorker;

    fn stub_factory() -> impl Fn(crate::ids::WorkerId) -> Box<dyn Worker + Send> {
        |_id| Box::new(StubWorker::new("mgr-test").with_delay(5))
    }

    #[tokio::test]
    async fn submit_and_complete() {
        let mgr = AgentManager::new(
            PoolOptions {
                min_workers: 1,
                max_workers: 2,
                ..Default::default()
            },
            TaskConfig::default(),
            stub_factory(),
        );

        let id = mgr
            .handle
            .submit(TaskPayload::Prompt("hello".into()))
            .await
            .unwrap();
        tracing::info!("submitted task {id}");

        // Wait briefly for it to complete
        tokio::time::sleep(Duration::from_millis(200)).await;

        let snap = mgr.handle.status(id).await.unwrap().unwrap();
        tracing::info!("snapshot: {snap:?}");
        assert!(
            snap.status.is_terminal(),
            "expected terminal, got {:?}",
            snap.status
        );

        mgr.handle.shutdown().await;
    }

    #[tokio::test]
    async fn submit_multiple_tasks() {
        let mgr = AgentManager::new(
            PoolOptions {
                min_workers: 0,
                max_workers: 4,
                ..Default::default()
            },
            TaskConfig::default(),
            stub_factory(),
        );

        // Submit 5 tasks sequentially (avoids needing `futures` crate for join_all)
        let mut task_ids = Vec::new();
        for i in 0..5 {
            let id = mgr
                .handle
                .submit(TaskPayload::Prompt(format!("task {i}")))
                .await
                .unwrap();
            task_ids.push(id);
        }

        // Poll all tasks until completion (with timeout)
        tokio::time::timeout(Duration::from_secs(5), async {
            for id in &task_ids {
                loop {
                    let snap = mgr.handle.status(*id).await.unwrap().unwrap();
                    if snap.status.is_terminal() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        })
        .await
        .expect("tasks did not complete within timeout");

        mgr.handle.shutdown().await;
    }

    #[tokio::test]
    async fn events_are_emitted() {
        let mgr = AgentManager::new(
            PoolOptions {
                min_workers: 1,
                max_workers: 2,
                ..Default::default()
            },
            TaskConfig::default(),
            stub_factory(),
        );

        let mut rx = mgr.handle.subscribe().await.unwrap();

        let id = mgr
            .handle
            .submit(TaskPayload::Prompt("event test".into()))
            .await
            .unwrap();

        // Collect events until we see completion
        loop {
            tokio::select! {
                event = rx.recv() => {
                    match event {
                        Ok(Event::TaskCompleted { task_id, .. }) if task_id == id => break,
                        Ok(Event::TaskFailed { task_id, .. }) if task_id == id => break,
                        _ => continue,
                    }
                }
                _ = tokio::time::sleep(Duration::from_secs(2)) => {
                    panic!("timeout waiting for completion event");
                }
            }
        }

        mgr.handle.shutdown().await;
    }
}
