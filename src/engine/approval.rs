use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::protocol::{Event, ToolCall};
use crate::sink::EventSink;

/// Result of the approval process (three possible outcomes).
pub enum ApprovalOutcome {
    Approved,
    Rejected,
    Cancelled,
}

/// Cancel point 2: ask user for approval of pending tool calls.
///
/// Emits `ApprovalRequired` for each pending call, then waits for either
/// a response on `approval_rx` or cancellation.
pub async fn ask_approval(
    pending: &[ToolCall],
    cancel: &CancellationToken,
    session_id: &str,
    approval_rx: &mut mpsc::Receiver<bool>,
    sink: Arc<dyn EventSink>,
) -> ApprovalOutcome {
    for call in pending {
        sink.emit(Event::ApprovalRequired {
            session_id: session_id.into(),
            call_id: call.call_id.clone(),
            command: call.input["command"]
                .as_str()
                .unwrap_or_default()
                .to_owned(),
        })
        .await;
    }

    tokio::select! {
        response = approval_rx.recv() => {
            match response {
                Some(true) => {
                    for call in pending {
                        sink.emit(Event::ApprovalGranted {
                            session_id: session_id.into(),
                            call_id: call.call_id.clone(),
                        }).await;
                    }
                    ApprovalOutcome::Approved
                }
                Some(false) | None => ApprovalOutcome::Rejected,
            }
        }
        _ = cancel.cancelled() => ApprovalOutcome::Cancelled,
    }
}
