use std::sync::Arc;
use std::time::Instant;

use crate::protocol::{ContentBlock, Event, ToolCall};
use crate::sink::EventSink;
use crate::tool::{ExecutionContext, Tool, ToolOutput};

fn first_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn blocks_to_json(blocks: &[ContentBlock]) -> serde_json::Value {
    serde_json::to_value(blocks).unwrap_or(serde_json::Value::Null)
}

pub async fn invoke_tool(
    tool: &dyn Tool,
    call: ToolCall,
    ctx: &ExecutionContext,
    sink: Arc<dyn EventSink>,
    session_id: String,
) -> ContentBlock {
    sink.emit(Event::ToolStart {
        session_id: session_id.clone(),
        call_id: call.call_id.clone(),
        tool: call.name.clone(),
        input: call.input.clone(),
    })
    .await;

    let started = Instant::now();

    let outcome: ToolOutput = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => ToolOutput::failure("cancelled"),
        out = tokio::time::timeout(ctx.timeout, tool.run(call.input.clone(), ctx)) => match out {
            Ok(o) => o,
            Err(_) => ToolOutput::failure(format!("timeout after {:?}", ctx.timeout)),
        },
    };

    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    if outcome.is_error {
        sink.emit(Event::ToolError {
            session_id,
            call_id: call.call_id.clone(),
            error: first_text(&outcome.content),
        })
        .await;
    } else {
        sink.emit(Event::ToolEnd {
            session_id,
            call_id: call.call_id.clone(),
            output: blocks_to_json(&outcome.content),
            duration_ms,
        })
        .await;
    }

    ContentBlock::ToolResult {
        call_id: call.call_id,
        content: outcome.content,
        is_error: outcome.is_error,
    }
}
