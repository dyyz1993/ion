use thiserror::Error;

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Stream error: {0}")]
    Stream(String),

    #[error("Tool call failed: {0}")]
    Tool(String),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Extension error: {0}")]
    Extension(String),

    #[error("Compact error: {0}")]
    Compact(String),

    #[error("Agent aborted")]
    Aborted,

    /// Soft interrupt：工具被 immediate steer 打断，agent 不退出（可恢复）
    #[error("Agent interrupted (steer)")]
    Interrupted,

    #[error("Agent paused")]
    Paused,

    #[error("Max retries exceeded: {0}")]
    MaxRetries(String),

    #[error("Rate limited, retry after {0}ms")]
    RateLimited(u64),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Reqwest(#[from] reqwest::Error),
}

pub type AgentResult<T> = Result<T, AgentError>;

impl From<ion_provider::ProviderError> for AgentError {
    fn from(e: ion_provider::ProviderError) -> Self {
        match e {
            ion_provider::ProviderError::Provider(msg) => AgentError::Provider(msg),
            ion_provider::ProviderError::Stream(msg) => AgentError::Stream(msg),
            ion_provider::ProviderError::HttpError { status, body } => {
                AgentError::Provider(format!("HTTP {status}: {body}"))
            }
            ion_provider::ProviderError::MissingApiKey(p) => {
                AgentError::Provider(format!("missing API key for {p}"))
            }
            ion_provider::ProviderError::ProviderNotFound(a) => {
                AgentError::Provider(format!("provider not found: {a}"))
            }
            ion_provider::ProviderError::ModelNotFound(m) => {
                AgentError::Provider(format!("model not found: {m}"))
            }
            ion_provider::ProviderError::Json(e) => AgentError::Json(e),
            ion_provider::ProviderError::Reqwest(e) => AgentError::Reqwest(e),
        }
    }
}
