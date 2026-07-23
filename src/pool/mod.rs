pub mod slot;

use crate::error::{IonError, IonResult};
use crate::ids::WorkerId;
use crate::pool::slot::WorkerSlot;
use crate::types::{PoolOptions, PoolStats, timestamp_now};
use crate::worker::{Worker, WorkerCmd, WorkerHandle, WorkerStatus};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Pool commands (actor protocol)
// ---------------------------------------------------------------------------

pub enum PoolCmd {
    Acquire {
        reply: oneshot::Sender<IonResult<WorkerHandle>>,
    },
    Release {
        worker_id: WorkerId,
    },
    Stats {
        reply: oneshot::Sender<PoolStats>,
    },
    ScaleTo {
        n: usize,
        reply: oneshot::Sender<usize>,
    },
    Shutdown,
}

// ---------------------------------------------------------------------------
// PoolHandle — send commands to the pool actor
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct PoolHandle {
    tx: mpsc::Sender<PoolCmd>,
}

impl PoolHandle {
    pub fn new(tx: mpsc::Sender<PoolCmd>) -> Self {
        Self { tx }
    }

    pub async fn acquire(&self) -> IonResult<WorkerHandle> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(PoolCmd::Acquire { reply })
            .await
            .map_err(|_| IonError::Pool("pool closed".into()))?;
        rx.await
            .map_err(|_| IonError::Pool("reply cancelled".into()))?
    }

    pub async fn release(&self, worker_id: WorkerId) {
        let _ = self.tx.send(PoolCmd::Release { worker_id }).await;
    }

    pub async fn stats(&self) -> IonResult<PoolStats> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(PoolCmd::Stats { reply })
            .await
            .map_err(|_| IonError::Pool("pool closed".into()))?;
        rx.await
            .map_err(|_| IonError::Pool("reply cancelled".into()))
    }

    pub async fn scale_to(&self, n: usize) -> IonResult<usize> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(PoolCmd::ScaleTo { n, reply })
            .await
            .map_err(|_| IonError::Pool("pool closed".into()))?;
        rx.await
            .map_err(|_| IonError::Pool("reply cancelled".into()))
    }

    pub async fn shutdown(&self) {
        let _ = self.tx.send(PoolCmd::Shutdown).await;
    }
}

// ---------------------------------------------------------------------------
// WorkerPool actor
// ---------------------------------------------------------------------------

type WorkerFactory = Arc<dyn Fn(WorkerId) -> Box<dyn Worker + Send> + Send + Sync>;

/// A pool of worker actors.
///
/// # Ownership
/// The pool is an **actor** — it runs a `tokio::spawn`ed task that owns
/// `Vec<WorkerSlot>` and `VecDeque<WorkerId>`.  External code communicates
/// via `PoolCmd` over a `mpsc` channel.  This means:
///
/// - The pool's inner data has **no lifetime parameters** — it lives for as
///   long as the task runs.
/// - Calling code holds a `PoolHandle` (a `mpsc::Sender<PoolCmd>`) which is
///   `Clone + Send + 'static`.
/// - Each worker runs in its own task, and the pool communicates with it via
///   a `WorkerHandle` (another mpsc sender).
///
/// This pattern avoids any shared `&mut` or `Mutex<Vec<WorkerSlot>>` — the
/// actor serialises all access internally.
pub struct WorkerPool {
    handle: PoolHandle,
    join_handle: JoinHandle<()>,
    shutdown_token: CancellationToken,
}

impl WorkerPool {
    /// Create a new `WorkerPool` and spawn its actor task.
    pub fn new<F>(options: PoolOptions, factory: F) -> Self
    where
        F: Fn(WorkerId) -> Box<dyn Worker + Send> + Send + Sync + 'static,
    {
        let (tx, rx) = mpsc::channel(256);
        let shutdown_token = CancellationToken::new();
        let token = shutdown_token.child_token();

        let join_handle = tokio::spawn(async move {
            let mut pool = PoolActor {
                slots: Vec::new(),
                idle_queue: std::collections::VecDeque::new(),
                next_worker_id: 1,
                options,
                worker_factory: Arc::new(factory),
                rx,
                reaper_interval: Duration::from_secs(30),
                shutdown: token,
            };
            pool.run().await;
        });

        Self {
            handle: PoolHandle::new(tx),
            join_handle,
            shutdown_token,
        }
    }

    pub fn handle(&self) -> &PoolHandle {
        &self.handle
    }

    /// Gracefully shutdown the pool.
    pub async fn shutdown(self) {
        self.handle.shutdown().await;
        self.shutdown_token.cancel();
        let _ = self.join_handle.await;
    }
}

// ---------------------------------------------------------------------------
// PoolActor — internal state machine
// ---------------------------------------------------------------------------

struct PoolActor {
    slots: Vec<WorkerSlot>,
    idle_queue: std::collections::VecDeque<WorkerId>,
    next_worker_id: u64,
    options: PoolOptions,
    worker_factory: WorkerFactory,
    rx: mpsc::Receiver<PoolCmd>,
    reaper_interval: Duration,
    shutdown: CancellationToken,
}

impl PoolActor {
    async fn run(&mut self) {
        let mut reaper_interval = tokio::time::interval(self.reaper_interval);
        reaper_interval.tick().await; // skip first immediate tick

        // Scale to min_workers at startup
        self.scale_up_to(self.options.min_workers).await;

        loop {
            tokio::select! {
                Some(cmd) = self.rx.recv() => {
                    match cmd {
                        PoolCmd::Acquire { reply } => {
                            let result = self.acquire().await;
                            let _ = reply.send(result);
                        }
                        PoolCmd::Release { worker_id } => {
                            self.release(worker_id).await;
                        }
                        PoolCmd::Stats { reply } => {
                            let _ = reply.send(self.compute_stats());
                        }
                        PoolCmd::ScaleTo { n, reply } => {
                            let actual = self.scale_to(n).await;
                            let _ = reply.send(actual);
                        }
                        PoolCmd::Shutdown => {
                            self.shutdown_all().await;
                            break;
                        }
                    }
                }
                _ = reaper_interval.tick() => {
                    self.reap_idle().await;
                }
                _ = self.shutdown.cancelled() => {
                    self.shutdown_all().await;
                    break;
                }
            }
        }
    }

    // ---- acquire ----

    async fn acquire(&mut self) -> IonResult<WorkerHandle> {
        // Try idle queue first
        while let Some(wid) = self.idle_queue.pop_front() {
            if let Some(slot) = self.slots.iter_mut().find(|s| s.worker_id == wid)
                && slot.status == WorkerStatus::Idle
            {
                slot.status = WorkerStatus::Claimed;
                slot.last_active = timestamp_now();
                return Ok(slot.handle.clone());
            }
        }

        // Scale up if under max
        if self.slots.len() < self.options.max_workers {
            let slot = self.spawn_worker().await;
            let wid = slot.worker_id;
            let handle = slot.handle.clone();
            self.slots.push(slot);
            if let Some(s) = self.slots.iter_mut().find(|s| s.worker_id == wid) {
                s.status = WorkerStatus::Claimed;
            }
            return Ok(handle);
        }

        // All workers busy, wait for one (simple: spin with small backoff)
        // In a production system, this would use a wait queue. For our study
        // we return an error and let the caller decide.
        Err(IonError::WorkerUnavailable("all workers busy".into()))
    }

    // ---- release ----

    async fn release(&mut self, worker_id: WorkerId) {
        if let Some(slot) = self.slots.iter_mut().find(|s| s.worker_id == worker_id) {
            slot.status = WorkerStatus::Idle;
            slot.last_active = timestamp_now();
            slot.task_id = None;
            self.idle_queue.push_back(worker_id);
            tracing::debug!("released worker {}", worker_id);
        }
    }

    // ---- spawn ----

    async fn spawn_worker(&mut self) -> WorkerSlot {
        let wid = WorkerId::new(self.next_worker_id);
        self.next_worker_id += 1;

        let mut worker = (self.worker_factory)(wid);
        let (tx, mut rx) = mpsc::channel(32);

        let handle = WorkerHandle::new(wid, tx);

        // Spawn the worker's command loop
        let worker_id = wid;
        tokio::spawn(async move {
            // Connect
            if let Err(e) = worker.connect().await {
                tracing::error!("worker {worker_id} connect failed: {e}");
                return;
            }

            loop {
                tokio::select! {
                    Some(cmd) = rx.recv() => {
                        match cmd {
                            WorkerCmd::Prompt { task_id: _, text, reply } => {
                                let result = worker.prompt(text).await;
                                let _ = reply.send(result);
                            }
                            WorkerCmd::Steer { msg, reply } => {
                                let result = worker.steer(msg).await;
                                let _ = reply.send(result);
                            }
                            WorkerCmd::State { reply } => {
                                let result = worker.state().await;
                                let _ = reply.send(result);
                            }
                            WorkerCmd::Dispose { reply } => {
                                let result = worker.dispose().await;
                                let _ = reply.send(result);
                                break;
                            }
                            WorkerCmd::Shutdown => {
                                let _ = worker.dispose().await;
                                break;
                            }
                        }
                    }
                    else => break, // channel closed
                }
            }

            tracing::debug!("worker {worker_id} task ended");
        });

        WorkerSlot::new(wid, handle)
    }

    // ---- stats ----

    fn compute_stats(&self) -> PoolStats {
        let mut idle = 0;
        let mut claimed = 0;
        let mut running = 0;
        let mut dead = 0;

        for slot in &self.slots {
            match slot.status {
                WorkerStatus::Idle => idle += 1,
                WorkerStatus::Claimed | WorkerStatus::Connecting => claimed += 1,
                WorkerStatus::Running => running += 1,
                WorkerStatus::Completed | WorkerStatus::Dead => dead += 1,
            }
        }

        PoolStats {
            total_workers: self.slots.len(),
            idle_workers: idle,
            claimed_workers: claimed,
            running_workers: running,
            dead_workers: dead,
        }
    }

    // ---- scale ----

    async fn scale_to(&mut self, n: usize) -> usize {
        let n = n.min(self.options.max_workers);
        if n > self.slots.len() {
            self.scale_up_to(n).await;
        } else if n < self.slots.len() {
            self.scale_down_to(n).await;
        }
        self.slots.len()
    }

    async fn scale_up_to(&mut self, n: usize) {
        while self.slots.len() < n {
            let slot = self.spawn_worker().await;
            let wid = slot.worker_id;
            self.idle_queue.push_back(wid);
            self.slots.push(slot);
        }
    }

    async fn scale_down_to(&mut self, n: usize) {
        // Remove dead ones first, then idle, oldest first
        self.slots.retain(|s| s.status != WorkerStatus::Dead);
        while self.slots.len() > n {
            if let Some(pos) = self
                .slots
                .iter()
                .position(|s| s.status == WorkerStatus::Idle)
            {
                let slot = self.slots.remove(pos);
                self.idle_queue.retain(|&id| id != slot.worker_id);
                tracing::debug!("scaled down worker {}", slot.worker_id);
            } else {
                break; // can't scale down busy workers
            }
        }
    }

    // ---- reaper ----

    async fn reap_idle(&mut self) {
        let timeout = self.options.idle_timeout;
        let to_remove: Vec<WorkerId> = self
            .slots
            .iter()
            .filter(|s: &&WorkerSlot| s.is_idle_expired(timeout))
            .map(|s| s.worker_id)
            .collect();

        // Only reap if above min_workers
        let keep = self.options.min_workers;
        for wid in &to_remove {
            let idle_count = self
                .slots
                .iter()
                .filter(|s| s.status == WorkerStatus::Idle)
                .count();
            if idle_count <= keep {
                break;
            }
            if let Some(pos) = self.slots.iter().position(|s| s.worker_id == *wid) {
                tracing::debug!("reaping idle worker {}", wid);
                self.slots.remove(pos);
                self.idle_queue.retain(|&id| id != *wid);
            }
        }
    }

    // ---- shutdown ----

    async fn shutdown_all(&mut self) {
        tracing::info!("shutting down {} workers", self.slots.len());
        for slot in &self.slots {
            slot.handle.shutdown().await;
        }
        self.slots.clear();
        self.idle_queue.clear();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::stub::StubWorker;
    // (no external test crates needed)

    fn stub_factory() -> impl Fn(WorkerId) -> Box<dyn Worker + Send> {
        move |_id| Box::new(StubWorker::new("pool-test"))
    }

    #[tokio::test]
    async fn pool_spins_up_min_workers() {
        let pool = WorkerPool::new(
            PoolOptions {
                min_workers: 2,
                max_workers: 10,
                idle_timeout: Duration::from_secs(3600),
                worker_timeout: Duration::from_secs(3600),
            },
            stub_factory(),
        );
        let stats = pool.handle().stats().await.unwrap();
        assert_eq!(stats.total_workers, 2);
        assert_eq!(stats.idle_workers, 2);
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn pool_acquire_release() {
        let pool = WorkerPool::new(PoolOptions::default(), stub_factory());
        let handle = pool.handle().acquire().await.unwrap();
        let stats = pool.handle().stats().await.unwrap();
        assert_eq!(stats.claimed_workers, 1);
        pool.handle().release(handle.worker_id).await;
        let stats = pool.handle().stats().await.unwrap();
        assert_eq!(stats.idle_workers, 1);
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn pool_acquire_and_use() {
        let pool = WorkerPool::new(PoolOptions::default(), stub_factory());
        let handle = pool.handle().acquire().await.unwrap();
        let result = handle
            .prompt(crate::ids::TaskId::new(), "hello".into())
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.output.contains("echo"));
        pool.handle().release(handle.worker_id).await;
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn pool_respects_max_workers() {
        let pool = WorkerPool::new(
            PoolOptions {
                min_workers: 0,
                max_workers: 3,
                ..Default::default()
            },
            stub_factory(),
        );
        let h1 = pool.handle().acquire().await.unwrap();
        let h2 = pool.handle().acquire().await.unwrap();
        let h3 = pool.handle().acquire().await.unwrap();
        // 4th should fail
        let h4 = pool.handle().acquire().await;
        assert!(h4.is_err());

        // Release one, then acquire should work
        pool.handle().release(h3.worker_id).await;
        let h4 = pool.handle().acquire().await;
        assert!(h4.is_ok());

        drop(h1);
        drop(h2);
        drop(h4);
        pool.shutdown().await;
    }

    #[tokio::test]
    async fn pool_scale_to() {
        let pool = WorkerPool::new(PoolOptions::default(), stub_factory());
        let n = pool.handle().scale_to(5).await.unwrap();
        assert_eq!(n, 5);
        let stats = pool.handle().stats().await.unwrap();
        assert_eq!(stats.total_workers, 5);
        pool.shutdown().await;
    }

    // ---- pure (non-async) unit tests ----

    #[test]
    fn pool_options_default_values() {
        let opts = PoolOptions::default();
        assert_eq!(opts.min_workers, 0);
        assert_eq!(opts.max_workers, 10);
        assert_eq!(opts.idle_timeout, Duration::from_secs(300));
        assert_eq!(opts.worker_timeout, Duration::from_secs(3600));
    }

    #[test]
    fn pool_options_custom_values() {
        let opts = PoolOptions {
            min_workers: 3,
            max_workers: 42,
            idle_timeout: Duration::from_secs(10),
            worker_timeout: Duration::from_secs(99),
        };
        assert_eq!(opts.min_workers, 3);
        assert_eq!(opts.max_workers, 42);
        assert_eq!(opts.idle_timeout, Duration::from_secs(10));
        assert_eq!(opts.worker_timeout, Duration::from_secs(99));
    }

    #[test]
    fn pool_options_clone_preserves_fields() {
        let original = PoolOptions {
            min_workers: 2,
            max_workers: 8,
            ..Default::default()
        };
        let cloned = original.clone();
        assert_eq!(original.min_workers, cloned.min_workers);
        assert_eq!(original.max_workers, cloned.max_workers);
        assert_eq!(original.idle_timeout, cloned.idle_timeout);
    }

    #[test]
    fn pool_stats_default_is_zeroed() {
        let stats = PoolStats::default();
        assert_eq!(stats.total_workers, 0);
        assert_eq!(stats.idle_workers, 0);
        assert_eq!(stats.claimed_workers, 0);
        assert_eq!(stats.running_workers, 0);
        assert_eq!(stats.dead_workers, 0);
    }

    #[test]
    fn pool_handle_clone_shares_channel() {
        let (tx, mut rx) = mpsc::channel::<PoolCmd>(8);
        let h1 = PoolHandle::new(tx);
        let h2 = h1.clone();

        // Both handles share the same sender, so a send from one is
        // observable by the receiver.
        tokio_test_block_on(async {
            h1.release(WorkerId::new(1)).await;
            let cmd = rx.recv().await;
            assert!(matches!(
                cmd,
                Some(PoolCmd::Release { worker_id }) if worker_id == WorkerId::new(1)
            ));
            drop(h2);
        });
    }

    #[test]
    fn pool_cmd_release_carries_worker_id() {
        let cmd = PoolCmd::Release {
            worker_id: WorkerId::new(7),
        };
        match cmd {
            PoolCmd::Release { worker_id } => assert_eq!(worker_id, WorkerId::new(7)),
            _ => panic!("expected Release variant"),
        }
    }

    #[test]
    fn pool_cmd_scale_to_carries_target() {
        let cmd = PoolCmd::ScaleTo {
            n: 4,
            reply: oneshot::channel().0,
        };
        match cmd {
            PoolCmd::ScaleTo { n, .. } => assert_eq!(n, 4),
            _ => panic!("expected ScaleTo variant"),
        }
    }

    #[test]
    fn worker_status_variants_are_distinct() {
        // Sanity check: the statuses the pool relies on are distinct
        // single-value enum variants.
        let idle = WorkerStatus::Idle;
        let claimed = WorkerStatus::Claimed;
        let running = WorkerStatus::Running;
        let dead = WorkerStatus::Dead;

        // Format each to a string to compare distinctness without PartialEq.
        let tags = [
            format!("{idle:?}"),
            format!("{claimed:?}"),
            format!("{running:?}"),
            format!("{dead:?}"),
        ];
        let unique: std::collections::HashSet<&str> =
            tags.iter().map(|s| s.as_str()).collect();
        assert_eq!(unique.len(), tags.len());
    }
}

/// Tiny helper so we can drive a small async snippet from a synchronous test
/// without pulling in extra dev-dependencies.
#[cfg(test)]
fn tokio_test_block_on<F: std::future::Future>(future: F) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build test runtime")
        .block_on(future);
}
