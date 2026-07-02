use crate::ids::TaskId;
use crate::types::TaskResult;
use serde::Serialize;
use std::fmt;

// ---------------------------------------------------------------------------
// Event — all events emitted by the orchestration system
// ---------------------------------------------------------------------------

/// Every event is an owned, `'static`, `Clone` type so it flows freely through
/// `broadcast` channels without borrowing worries.
#[derive(Clone, Debug, Serialize)]
pub enum Event {
    /// A new task was submitted.
    TaskSubmitted {
        task_id: TaskId,
        description: String,
    },
    /// A task was picked up by a worker and moved to Running.
    TaskStarted { task_id: TaskId, worker_id: String },
    /// Streaming delta from a running task.
    TaskOutput { task_id: TaskId, delta: String },
    /// A task completed successfully.
    TaskCompleted { task_id: TaskId, result: TaskResult },
    /// A task failed (with or without retries remaining).
    TaskFailed {
        task_id: TaskId,
        error: String,
        will_retry: bool,
    },
    /// A task was cancelled.
    TaskCancelled { task_id: TaskId },
    /// A worker went offline unexpectedly.
    WorkerLost {
        worker_id: String,
        task_id: Option<TaskId>,
    },
    /// The manager is shutting down.
    ManagerShutdown,
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TaskSubmitted {
                task_id,
                description,
            } => {
                write!(f, "[submit] {task_id} {description}")
            }
            Self::TaskStarted { task_id, worker_id } => {
                write!(f, "[start]  {task_id} → wkr {worker_id}")
            }
            Self::TaskOutput { task_id, delta } => {
                write!(f, "[output] {task_id} {delta}")
            }
            Self::TaskCompleted { task_id, result } => {
                write!(f, "[done]   {task_id} success={}", result.success)
            }
            Self::TaskFailed {
                task_id,
                error,
                will_retry,
            } => {
                write!(f, "[fail]   {task_id} {error} retry={will_retry}")
            }
            Self::TaskCancelled { task_id } => {
                write!(f, "[cancel] {task_id}")
            }
            Self::WorkerLost { worker_id, task_id } => {
                if let Some(tid) = task_id {
                    write!(f, "[lost]   wkr {worker_id} on task {tid}")
                } else {
                    write!(f, "[lost]   wkr {worker_id}")
                }
            }
            Self::ManagerShutdown => write!(f, "[shutdown] manager stopping"),
        }
    }
}

// ---------------------------------------------------------------------------
// EventBus — a thin wrapper around tokio::broadcast
// ---------------------------------------------------------------------------

use tokio::sync::broadcast;

/// An event bus built on `tokio::sync::broadcast`.
///
/// # Ownership
/// `EventBus` wraps a `broadcast::Sender<Event>`.  The `Sender` is `Clone` and
/// `'static`, so the bus can be shared freely across tasks.  The only life‑time
/// parameter in the system comes from the receiver, which is fine because the
/// receiver is used in one spot and drops when done.
#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    /// Publish an event.  Returns how many receivers saw it, or an error if
    /// there are no receivers (no lagged failure).
    pub fn publish(&self, event: Event) -> usize {
        // If all receivers lagged, that's okay — we can miss some.
        let _ = self.tx.send(event);
        self.tx.receiver_count()
    }

    /// Subscribe — returns a receiver.  Late receivers will get events sent
    /// after this call.  Old events are dropped (lagged → error, caller
    /// decides what to do).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// Number of active subscribers.
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Get the inner sender (for advanced use, e.g. passing directly to
    /// another component that wants to publish).
    pub fn sender(&self) -> broadcast::Sender<Event> {
        self.tx.clone()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_bus_publish_subscribe() {
        let bus = EventBus::new(16);
        let mut rx = bus.subscribe();

        bus.publish(Event::ManagerShutdown);
        bus.publish(Event::ManagerShutdown);

        // We should be able to receive at least one
        let first = rx.try_recv();
        assert!(first.is_ok(), "should receive first event");
    }

    #[test]
    fn event_display_format() {
        let id = crate::ids::TaskId::new();
        let e = Event::TaskSubmitted {
            task_id: id,
            description: "test".into(),
        };
        let s = e.to_string();
        assert!(s.contains("[submit]"));
        assert!(s.contains(&id.to_string()));
    }
}
