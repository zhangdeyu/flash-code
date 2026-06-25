use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::sink::EventSink;
use crate::tool::ApprovalMode;

#[derive(Clone)]
pub struct RunContext {
    pub session_id: String,
    pub approval_mode: ApprovalMode,
    pub cwd: PathBuf,
    pub tool_timeout: Duration,
    pub tool_max_output_bytes: usize,
    pub cancel: CancellationToken,
    pub sink: Arc<dyn EventSink>,
}
