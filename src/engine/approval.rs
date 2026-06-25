use std::sync::Arc;

use futures::future::BoxFuture;

use crate::engine::run_context::RunContext;
use crate::protocol::ToolCall;

/// User approval callback. Per-tool. Returns `true` to approve, `false` to reject.
pub type ApprovalCallback =
    Arc<dyn Fn(&ToolCall, &RunContext) -> BoxFuture<'static, bool> + Send + Sync>;

#[must_use]
pub fn yolo_approval() -> ApprovalCallback {
    Arc::new(|_, _| Box::pin(async { true }))
}

#[must_use]
pub fn const_approval(answer: bool) -> ApprovalCallback {
    Arc::new(move |_, _| Box::pin(async move { answer }))
}
