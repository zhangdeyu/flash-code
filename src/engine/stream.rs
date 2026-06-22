use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::protocol::{Event, Message, Prompt, ToolCall};
use crate::provider::{ModelEvent, Provider};
use crate::sink::EventSink;

/// Stream model output, accumulating text and tool calls.
///
/// Cancel point 1: if cancelled during streaming, history is NOT polluted
/// (the assistant message has not yet been pushed).
pub async fn stream_model(
    prompt: &Prompt,
    provider: &dyn Provider,
    cancel: &CancellationToken,
    session_id: &str,
    sink: Arc<dyn EventSink>,
) -> Result<(Message, Vec<ToolCall>), Error> {
    sink.emit(Event::AssistantMessageStart {
        session_id: session_id.into(),
    })
    .await;

    let mut stream = provider.stream(prompt).await;
    let mut text = String::new();
    let mut tool_calls = Vec::new();

    loop {
        tokio::select! {
            next = stream.next() => {
                match next {
                    Some(ModelEvent::Token(t)) => {
                        text.push_str(&t);
                        sink.emit(Event::AssistantToken {
                            session_id: session_id.into(),
                            text: t,
                        }).await;
                    }
                    Some(ModelEvent::ToolUse { call_id, name, input }) => {
                        tool_calls.push(ToolCall { call_id, name, input });
                    }
                    Some(ModelEvent::Done) | None => break,
                }
            }
            _ = cancel.cancelled() => {
                return Err(Error::Cancelled);
            }
        }
    }

    sink.emit(Event::AssistantMessageEnd {
        session_id: session_id.into(),
    })
    .await;

    Ok((Message::assistant(text), tool_calls))
}
// stream_model: cancel point 1
