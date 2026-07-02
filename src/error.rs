use thiserror::Error;

#[derive(Error, Debug)]
pub enum IonError {
    #[error("Worker not available: {0}")]
    WorkerUnavailable(String),

    #[error("Worker error: {0}")]
    Worker(String),

    #[error("Pool error: {0}")]
    Pool(String),

    #[error("Queue error: {0}")]
    Queue(String),

    #[error("Task not found: {0}")]
    TaskNotFound(String),

    #[error("Session error: {0}")]
    Session(String),

    #[error("RPC error: {0}")]
    Rpc(String),

    #[error("Timeout")]
    Timeout,

    #[error("Cancelled")]
    Cancelled,

    #[error("Shutdown")]
    Shutdown,

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
}

pub type IonResult<T> = Result<T, IonError>;
