use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use crate::protocol::{Event, Message, ToolCall};
use crate::sink::EventSink;
use crate::tool::{Tool, ToolOutcome};

/// Cancel point 3: execute all tool calls in parallel.
///
/// Each future holds its own `Arc<dyn EventSink>` clone (reference count only).
/// Events from different calls may interleave — this is fine because Replay
/// pairs events by `call_id`, not by adjacency.
///
/// Same-call_id ordering (ToolStart → ToolEnd) is naturally guaranteed because
/// they are emitted sequentially within the same future.
pub async fn execute_all_parallel(
    calls: &[ToolCall],
    tools: &[Box<dyn Tool>],
    cancel: &CancellationToken,
    session_id: &str,
    sink: Arc<dyn EventSink>,
) -> Vec<Message> {
    // Build the futures. Each closure captures references that outlive `join_all`,
    // since we await it inside this function before returning.
    let futs = calls.iter().map(|call| {
        let tool: &dyn Tool = tools
            .iter()
            .find(|t| t.name() == call.name)
            .map(|b| b.as_ref())
            .expect("tool not found in registry");
        run_one(
            tool,
            call.clone(),
            cancel.clone(),
            session_id.to_owned(),
            sink.clone(),
        )
    });

    futures::future::join_all(futs).await
}

/// Execute a single tool call, emitting Start/End/Error/Cancelled events and
/// returning the tool_result message to append to history.
async fn run_one(
    tool: &dyn Tool,
    call: ToolCall,
    cancel: CancellationToken,
    session_id: String,
    sink: Arc<dyn EventSink>,
) -> Message {
    sink.emit(Event::ToolStart {
        session_id: session_id.clone(),
        call_id: call.call_id.clone(),
        tool: call.name.clone(),
        input: call.input.clone(),
    })
    .await;

    let started = Instant::now();
    let outcome = tool.run(call.input.clone(), cancel).await;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    match outcome {
        ToolOutcome::Success(output) => {
            sink.emit(Event::ToolEnd {
                session_id,
                call_id: call.call_id.clone(),
                output: output.clone(),
                duration_ms,
            })
            .await;
            Message::tool_result(call.call_id, output.to_string(), false)
        }
        ToolOutcome::Failure(err) => {
            sink.emit(Event::ToolError {
                session_id,
                call_id: call.call_id.clone(),
                error: err.clone(),
            })
            .await;
            Message::tool_result(call.call_id, err, true)
        }
        ToolOutcome::Cancelled => {
            sink.emit(Event::ToolCancelled {
                session_id,
                call_id: call.call_id.clone(),
            })
            .await;
            Message::tool_result(call.call_id, "cancelled during execution".to_owned(), true)
        }
    }
}
