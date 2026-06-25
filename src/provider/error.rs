use std::time::Duration;

#[derive(Debug, Clone, thiserror::Error)]
pub enum ProviderError {
    #[error("context window exceeded: {0}")]
    ContextOverflow(String),

    #[error("rate limited: {message}")]
    RateLimited {
        retry_after: Option<Duration>,
        message: String,
    },

    #[error("auth failed: {0}")]
    Auth(String),

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("transient: {0}")]
    Transient(String),
}

impl ProviderError {
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Transient(_))
    }
}
