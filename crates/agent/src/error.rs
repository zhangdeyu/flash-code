use flash_core::storage::StorageError;
use flash_core::{Outcome, ToolError};
use flash_provider::ProviderError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRun {
    pub session_id: String,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub enum AgentError {
    Storage(StorageError),
    Provider(ProviderError),
    Tool(ToolError),
    History(String),
    Finalization { primary: String, finalize: String },
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "{error}"),
            Self::Provider(error) => write!(formatter, "{error}"),
            Self::Tool(error) => write!(formatter, "tool error: {}", error.message),
            Self::History(message) => write!(formatter, "history error: {message}"),
            Self::Finalization { primary, finalize } => {
                write!(
                    formatter,
                    "{primary}; session finalization also failed: {finalize}"
                )
            }
        }
    }
}

impl std::error::Error for AgentError {}

impl From<StorageError> for AgentError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ProviderError> for AgentError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}
