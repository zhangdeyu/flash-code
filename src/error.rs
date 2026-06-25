use std::path::PathBuf;

use crate::protocol::HistoryError;
use crate::provider::ProviderError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("operation cancelled")]
    Cancelled,

    #[error(transparent)]
    Provider(#[from] ProviderError),

    #[error(transparent)]
    History(#[from] HistoryError),

    #[error("max turns exceeded")]
    MaxTurnsExceeded,

    #[error("context overflow (after fallback compaction)")]
    ContextOverflow,

    #[error("config error: {0}")]
    Config(String),

    #[error("config file not found: {}", .0.display())]
    ConfigNotFound(PathBuf),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[error(transparent)]
    Toml(#[from] toml::de::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
