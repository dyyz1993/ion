use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

/// A unique identifier for a task.
///
/// # Why `Uuid` and not a `&str`?
///
/// `TaskId` crosses **async boundaries** (sent through mpsc channels, stored in
/// `HashMap`, returned in replies).  A `&'a str` would tie it to a borrower
/// whose lifetime must outlive all those users — impossible to prove statically
/// across `tokio::spawn` boundaries.  The compiler would force a `'static`
/// bound, which means we'd need `&'static str` (impractical) or an
/// `Arc<str>`/`String`.  We choose `Uuid` (Copy + Send + 'static) so `TaskId`
/// flows freely through every channel without any lifetime parameters.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct TaskId(Uuid);

impl TaskId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Create a TaskId from an existing UUID (for CLI parsing).
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TaskId({})", self.0)
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

/// A unique identifier for a worker instance in the pool.
#[derive(Clone, Copy, Eq, PartialEq, Hash, Debug)]
pub struct WorkerId(u64);

impl WorkerId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

impl fmt::Display for WorkerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wkr_{}", self.0)
    }
}

/// A unique identifier for a session.
#[derive(Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
pub struct SessionId(String);

impl SessionId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

// ---- helpers for tests ----
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_id_is_unique() {
        let a = TaskId::new();
        let b = TaskId::new();
        assert_ne!(a, b);
    }

    #[test]
    fn task_id_is_copy() {
        let a = TaskId::new();
        let b = a; // copy
        assert_eq!(a, b); // both still valid
    }

    #[test]
    fn task_id_roundtrips_string() {
        let a = TaskId::new();
        let s = a.to_string();
        // just verify it's a plausible UUID format
        assert_eq!(s.len(), 36);
        assert_eq!(s.chars().filter(|&c| c == '-').count(), 4);
    }
}
