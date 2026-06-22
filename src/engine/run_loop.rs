use tokio::sync::mpsc;

use crate::engine::approval::{ask_approval, ApprovalOutcome};
use crate::engine::compact::maybe_compact;
use crate::engine::execute::execute_all_parallel;
use crate::engine::stream::stream_model;
use crate::error::{Error, Result};
use crate::protocol::{Event, Message, Prompt};
use crate::provider::Provider;
use crate::session::Session;
use crate::tool::Tool;

/// Approval mode for tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalMode {
    /// Auto-approve all tool calls.
    Yolo,
    /// Require user approval for each tool call (default).
    Default,
}

const MAX_TURNS: usize = 50;

/// The main agent loop. Implements the state machine described in init.md §3.
///
/// Three cancellation scenarios are handled:
/// 1. During streaming → history not polluted, just return
/// 2. While awaiting approval → backfill tool_results to keep history consistent
/// 3. During tool execution → tool naturally returns Cancelled outcome
pub async fn run_loop(
    session: &mut Session,
    mode: ApprovalMode,
    provider: &dyn Provider,
    tools: &[Box<dyn Tool>],
    approval_rx: &mut mpsc::Receiver<bool>,
) -> Result<()> {
    let sink = session.sink.clone();
    let mut turns = 0_usize;

    loop {
        if session.cancel.is_cancelled() {
            sink.emit(Event::Cancelled {
                session_id: session.session_id.clone(),
                reason: "before model call".into(),
            })
            .await;
            return Ok(());
        }
        if turns >= MAX_TURNS {
            sink.emit(Event::Error {
                session_id: session.session_id.clone(),
                message: "max turns exceeded".into(),
            })
            .await;
            return Err(Error::MaxTurnsExceeded);
        }
        turns += 1;

        // Compress history if needed (only `messages`, not system/tools)
        session.history = maybe_compact(
            session.history.clone(),
            provider,
            &session.session_id,
            sink.clone(),
        )
        .await?;

        let prompt = Prompt {
            system: session.system.clone(),
            tools: tools.iter().map(|t| t.spec()).collect(),
            messages: session.history.clone(),
        };

        // Cancel scenario 1: stream_model handles cancel internally
        let (assistant_msg, tool_calls) = match stream_model(
            &prompt,
            provider,
            &session.cancel,
            &session.session_id,
            sink.clone(),
        )
        .await
        {
            Ok(result) => result,
            Err(Error::Cancelled) => {
                sink.emit(Event::Cancelled {
                    session_id: session.session_id.clone(),
                    reason: "model streaming".into(),
                })
                .await;
                return Ok(()); // history is clean, can resume
            }
            Err(e) => return Err(e),
        };
        session.history.push(assistant_msg);

        if tool_calls.is_empty() {
            return Ok(()); // Done — assistant produced final answer
        }

        let outcome = match mode {
            ApprovalMode::Yolo => ApprovalOutcome::Approved,
            ApprovalMode::Default => {
                ask_approval(
                    &tool_calls,
                    &session.cancel,
                    &session.session_id,
                    approval_rx,
                    sink.clone(),
                )
                .await
            }
        };

        match outcome {
            ApprovalOutcome::Approved => {
                let results = execute_all_parallel(
                    &tool_calls,
                    tools,
                    &session.cancel,
                    &session.session_id,
                    sink.clone(),
                )
                .await;
                session.history.extend(results);
            }
            ApprovalOutcome::Rejected => {
                for call in &tool_calls {
                    sink.emit(Event::ApprovalRejected {
                        session_id: session.session_id.clone(),
                        call_id: call.call_id.clone(),
                    })
                    .await;
                }
                session.history.extend(tool_calls.iter().map(|c| {
                    Message::tool_result(
                        c.call_id.clone(),
                        "user rejected this tool call".to_owned(),
                        true,
                    )
                }));
                continue; // Let model see rejection and decide next step
            }
            ApprovalOutcome::Cancelled => {
                // Cancel scenario 2: backfill tool_results to keep history valid
                for call in &tool_calls {
                    sink.emit(Event::ToolCancelled {
                        session_id: session.session_id.clone(),
                        call_id: call.call_id.clone(),
                    })
                    .await;
                }
                session.history.extend(tool_calls.iter().map(|c| {
                    Message::tool_result(
                        c.call_id.clone(),
                        "cancelled by user before execution".to_owned(),
                        true,
                    )
                }));
                sink.emit(Event::Cancelled {
                    session_id: session.session_id.clone(),
                    reason: "awaiting approval".into(),
                })
                .await;
                return Ok(());
            }
        }
    }
}
// Main agent loop
