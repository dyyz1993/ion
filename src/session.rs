use crate::error::IonResult;
use crate::ids::SessionId;
use crate::types::SessionState;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// ---------------------------------------------------------------------------
// SessionStore trait
// ---------------------------------------------------------------------------

/// Abstracts session persistence.  In-memory by default, could be backed by
/// Redis / SQLite / etc.
///
/// # Ownership
/// Each method takes `&self` and returns owned `'static` data, so the trait
/// is object-safe and can be wrapped in `Arc<dyn SessionStore>` or passed
/// through channels.
#[async_trait::async_trait]
pub trait SessionStore: Send + Sync {
    /// Insert or update a session.
    async fn set(&self, id: SessionId, session: SessionState) -> IonResult<()>;
    /// Look up a session by id.
    async fn get(&self, id: &SessionId) -> IonResult<Option<SessionState>>;
    /// List all session IDs.
    async fn list(&self) -> IonResult<Vec<SessionId>>;
    /// Delete a session.
    async fn delete(&self, id: &SessionId) -> IonResult<()>;
}

// ---------------------------------------------------------------------------
// InMemorySessionStore
// ---------------------------------------------------------------------------

/// A simple in-memory session store backed by `Arc<RwLock<HashMap>>`.
///
/// This is our default — no external dependencies, good for study.  The
/// `Arc<RwLock<…>>` allows concurrent reads from multiple tasks.  For a
/// production system you'd swap in a Redis-backed implementation.
#[derive(Clone, Default)]
pub struct InMemorySessionStore {
    inner: Arc<RwLock<HashMap<SessionId, SessionState>>>,
}

impl InMemorySessionStore {
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait::async_trait]
impl SessionStore for InMemorySessionStore {
    async fn set(&self, id: SessionId, session: SessionState) -> IonResult<()> {
        self.inner.write().await.insert(id, session);
        Ok(())
    }

    async fn get(&self, id: &SessionId) -> IonResult<Option<SessionState>> {
        Ok(self.inner.read().await.get(id).cloned())
    }

    async fn list(&self) -> IonResult<Vec<SessionId>> {
        Ok(self.inner.read().await.keys().cloned().collect())
    }

    async fn delete(&self, id: &SessionId) -> IonResult<()> {
        self.inner.write().await.remove(id);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_get_delete() {
        let store = InMemorySessionStore::new();
        let id = SessionId::new("test-session");
        let state = SessionState {
            message_count: 10,
            turn_index: 3,
            summary: Some("test".into()),
        };

        store.set(id.clone(), state.clone()).await.unwrap();
        let got = store.get(&id).await.unwrap().unwrap();
        assert_eq!(got.message_count, 10);
        assert_eq!(got.summary, Some("test".into()));

        store.delete(&id).await.unwrap();
        assert!(store.get(&id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_sessions() {
        let store = InMemorySessionStore::new();
        store
            .set(SessionId::new("a"), SessionState::default())
            .await
            .unwrap();
        store
            .set(SessionId::new("b"), SessionState::default())
            .await
            .unwrap();
        let list = store.list().await.unwrap();
        assert_eq!(list.len(), 2);
    }
}
