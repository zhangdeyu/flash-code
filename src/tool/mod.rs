pub mod bash;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::protocol::ToolSpec;

/// Outcome of executing a tool.
#[derive(Debug, Clone)]
pub enum ToolOutcome {
    Success(serde_json::Value),
    Failure(String),
    Cancelled,
}

/// Trait for tools that the model can invoke.
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn spec(&self) -> ToolSpec;
    async fn run(&self, input: serde_json::Value, cancel: CancellationToken) -> ToolOutcome;
}
