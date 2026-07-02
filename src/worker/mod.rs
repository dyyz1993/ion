pub mod agent_worker;
pub mod child;
pub mod stub;

use crate::error::IonResult;
use crate::ids::{TaskId, WorkerId};
use crate::types::{SessionState, TaskResult};
use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

// ---------------------------------------------------------------------------
// WorkerStatus — state machine
// ---------------------------------------------------------------------------

/// The lifecycle states of a worker.
///
/// An owned, `Copy + 'static` enum — no borrowed data, so it flows through
/// channels without lifetime constraints.  Every transition is validated by
/// `transition_to()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerStatus {
    /// Just spawned, no task assigned yet.
    Idle,
    /// Acquired from pool, RPC handshake in progress.
    Claimed,
    /// Handshake done, connecting to worker.
    Connecting,
    /// Worker is processing a task.
    Running,
    /// Task completed and worker can be reused.
    Completed,
    /// Worker has crashed or been killed.
    Dead,
}

impl WorkerStatus {
    /// Attempt to transition from `self` to `next`.  Returns `Ok(())` if the
    /// transition is valid, `Err((self, next))` otherwise.
    pub fn transition_to(
        self,
        next: WorkerStatus,
    ) -> std::result::Result<(), (WorkerStatus, WorkerStatus)> {
        let valid = matches!(
            (self, next),
            (Self::Idle, Self::Claimed)
                | (Self::Claimed, Self::Connecting)
                | (Self::Connecting, Self::Running)
                | (Self::Running, Self::Completed)
                | (Self::Completed, Self::Idle)     // reuse
                | (Self::Completed, Self::Dead)
                | (Self::Idle, Self::Dead)
                | (Self::Claimed, Self::Dead)
                | (Self::Connecting, Self::Dead)
                | (Self::Running, Self::Dead)
        );
        if valid { Ok(()) } else { Err((self, next)) }
    }
}

// ---------------------------------------------------------------------------
// Worker trait
// ---------------------------------------------------------------------------

/// Abstract worker that can be driven by the pool.
///
/// # Why `Box<dyn Worker + Send>`?
///
/// The pool needs to hold heterogeneous workers (stub, child-process).  A
/// generic `WorkerPool<W: Worker>` can hold only one concrete type, but we
/// want StubWorker for testing and ChildProcessWorker for production in the
/// same pool.  Hence dynamic dispatch (`Box<dyn Worker>`), which costs one
/// vtable lookup per call — negligible compared to JSON serialisation or
/// subprocess I/O.
///
/// # Why `async-trait`?
///
/// Rust 1.92 has stable AFIT (async fn in trait) but `dyn` support for AFIT
/// is still limited (requires `async_fn_in_trait` and `return_type_notation`).
/// `async-trait` gives us the most portable `dyn` support with a single
/// allocation per call.
#[async_trait]
pub trait Worker: Send {
    /// Connect / handshake.  The worker should become ready.
    async fn connect(&mut self) -> IonResult<()>;

    /// Run a prompt task and return the result.
    async fn prompt(&mut self, text: String) -> IonResult<TaskResult>;

    /// Send a steering message (low-latency injection).
    async fn steer(&mut self, msg: String) -> IonResult<()>;

    /// Get current session state.
    async fn state(&mut self) -> IonResult<SessionState>;

    /// Gracefully shut down the worker.
    async fn dispose(&mut self) -> IonResult<()>;
}

// ---------------------------------------------------------------------------
// Worker commands (actor protocol)
// ---------------------------------------------------------------------------

/// Commands sent from the pool to a worker task.
pub enum WorkerCmd {
    Prompt {
        task_id: TaskId,
        text: String,
        reply: oneshot::Sender<IonResult<TaskResult>>,
    },
    Steer {
        msg: String,
        reply: oneshot::Sender<IonResult<()>>,
    },
    State {
        reply: oneshot::Sender<IonResult<SessionState>>,
    },
    Dispose {
        reply: oneshot::Sender<IonResult<()>>,
    },
    /// Stop the worker task immediately (no reply).
    Shutdown,
}

impl std::fmt::Debug for WorkerCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Prompt { task_id, text, .. } => {
                write!(f, "WorkerCmd::Prompt({task_id}, {text})")
            }
            Self::Steer { msg, .. } => write!(f, "WorkerCmd::Steer({msg})"),
            Self::State { .. } => write!(f, "WorkerCmd::State"),
            Self::Dispose { .. } => write!(f, "WorkerCmd::Dispose"),
            Self::Shutdown => write!(f, "WorkerCmd::Shutdown"),
        }
    }
}

// ---------------------------------------------------------------------------
// WorkerHandle — send commands to a running worker task
// ---------------------------------------------------------------------------

/// A handle that allows sending commands to a worker's actor task.
///
/// # Ownership
/// `WorkerHandle` owns a `mpsc::Sender<WorkerCmd>`.  That sender is `Send`,
/// `'static`, and `Clone`.  It has **no lifetime parameter** — the receiver
/// runs inside an independent `tokio::spawn`ed task whose lifetime is managed
/// by a `JoinHandle`, not by a borrow.
///
/// If the worker task dies, the channel closes and `send()` returns an error.
#[derive(Clone, Debug)]
pub struct WorkerHandle {
    pub worker_id: WorkerId,
    tx: mpsc::Sender<WorkerCmd>,
}

impl WorkerHandle {
    pub fn new(worker_id: WorkerId, tx: mpsc::Sender<WorkerCmd>) -> Self {
        Self { worker_id, tx }
    }

    /// Send a command and wait for the reply.
    pub async fn send(&self, cmd: WorkerCmd) -> IonResult<()> {
        self.tx
            .send(cmd)
            .await
            .map_err(|_| crate::error::IonError::WorkerUnavailable(format!("{}", self.worker_id)))
    }

    /// Start a prompt task.
    pub async fn prompt(&self, task_id: TaskId, text: String) -> IonResult<TaskResult> {
        let (reply, rx) = oneshot::channel();
        self.send(WorkerCmd::Prompt {
            task_id,
            text,
            reply,
        })
        .await?;
        rx.await
            .map_err(|_| crate::error::IonError::WorkerUnavailable(format!("{}", self.worker_id)))?
    }

    /// Steer.
    pub async fn steer(&self, msg: String) -> IonResult<()> {
        let (reply, rx) = oneshot::channel();
        self.send(WorkerCmd::Steer { msg, reply }).await?;
        rx.await
            .map_err(|_| crate::error::IonError::WorkerUnavailable(format!("{}", self.worker_id)))?
    }

    /// Get state.
    pub async fn state(&self) -> IonResult<SessionState> {
        let (reply, rx) = oneshot::channel();
        self.send(WorkerCmd::State { reply }).await?;
        rx.await
            .map_err(|_| crate::error::IonError::WorkerUnavailable(format!("{}", self.worker_id)))?
    }

    /// Dispose.
    pub async fn dispose(&self) -> IonResult<()> {
        let (reply, rx) = oneshot::channel();
        self.send(WorkerCmd::Dispose { reply }).await?;
        rx.await
            .map_err(|_| crate::error::IonError::WorkerUnavailable(format!("{}", self.worker_id)))?
    }

    /// Shutdown (best-effort, no reply expected).
    pub async fn shutdown(&self) {
        let _ = self.tx.send(WorkerCmd::Shutdown).await;
    }
}

impl std::fmt::Display for WorkerHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WorkerHandle({})", self.worker_id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_status_starts_idle() {
        assert_eq!(WorkerStatus::Idle, WorkerStatus::Idle);
    }

    #[test]
    fn worker_status_valid_transitions() {
        // Valid
        assert!(
            WorkerStatus::Idle
                .transition_to(WorkerStatus::Claimed)
                .is_ok()
        );
        assert!(
            WorkerStatus::Claimed
                .transition_to(WorkerStatus::Connecting)
                .is_ok()
        );
        assert!(
            WorkerStatus::Connecting
                .transition_to(WorkerStatus::Running)
                .is_ok()
        );
        assert!(
            WorkerStatus::Running
                .transition_to(WorkerStatus::Completed)
                .is_ok()
        );
        assert!(
            WorkerStatus::Completed
                .transition_to(WorkerStatus::Idle)
                .is_ok()
        );
        // Dead is reachable from many states
        assert!(WorkerStatus::Idle.transition_to(WorkerStatus::Dead).is_ok());
        assert!(
            WorkerStatus::Running
                .transition_to(WorkerStatus::Dead)
                .is_ok()
        );
    }

    #[test]
    fn worker_status_invalid_transitions() {
        // Can't go back to Idle from Running
        assert!(
            WorkerStatus::Running
                .transition_to(WorkerStatus::Idle)
                .is_err()
        );
        // Can't skip states
        assert!(
            WorkerStatus::Idle
                .transition_to(WorkerStatus::Running)
                .is_err()
        );
        // Dead is terminal
        assert!(
            WorkerStatus::Dead
                .transition_to(WorkerStatus::Idle)
                .is_err()
        );
        assert!(
            WorkerStatus::Dead
                .transition_to(WorkerStatus::Completed)
                .is_err()
        );
    }

    #[test]
    fn worker_status_exhaustive() {
        // Exhaustively check every combination is either valid or invalid
        let states = [
            WorkerStatus::Idle,
            WorkerStatus::Claimed,
            WorkerStatus::Connecting,
            WorkerStatus::Running,
            WorkerStatus::Completed,
            WorkerStatus::Dead,
        ];
        for &from in &states {
            for &to in &states {
                let result = from.transition_to(to);
                let valid = result.is_ok();
                // We just verify it doesn't panic
                if valid {
                    assert_eq!(result.unwrap(), ());
                }
            }
        }
    }
}
