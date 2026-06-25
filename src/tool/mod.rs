pub mod approval;
pub mod bash;
pub mod output;
pub mod registry;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

pub use approval::{ApprovalGate, ApprovalMode, ApprovalOutcome};
pub use output::ToolOutput;
pub use registry::ToolRegistry;

use crate::protocol::{ToolApprovalAdvice, ToolSpec};
use crate::sink::EventSink;

/// Per-call execution context provided by the framework.
#[derive(Clone)]
pub struct ExecutionContext {
    pub session_id: String,
    pub call_id: String,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub max_output_bytes: usize,
    pub cancel: CancellationToken,
    pub sink: Arc<dyn EventSink>,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;

    fn spec(&self) -> ToolSpec;

    /// Risk advice for this specific input. Default: `Default` decision (defer to gate).
    fn approval_advice(&self, _input: &serde_json::Value) -> ToolApprovalAdvice {
        ToolApprovalAdvice::default_for(self.spec().risk)
    }

    /// Execute the tool. Framework handles approval + timeout + cancel.
    async fn run(&self, input: serde_json::Value, ctx: &ExecutionContext) -> ToolOutput;
}
