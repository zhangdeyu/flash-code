use std::sync::Arc;

use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::protocol::{ContentBlock, Event, Prompt};
use crate::provider::{Provider, ProviderError, ProviderEvent, StopReason};
use crate::sink::EventSink;

#[derive(Debug)]
pub enum StreamError {
    Cancelled,
    Provider(ProviderError),
}

pub async fn stream_model(
    prompt: &Prompt,
    provider: &dyn Provider,
    cancel: &CancellationToken,
    session_id: &str,
    sink: Arc<dyn EventSink>,
) -> Result<(Vec<ContentBlock>, StopReason), StreamError> {
    sink.emit(Event::AssistantMessageStart {
        session_id: session_id.to_owned(),
    })
    .await;

    // Outer call may be cancelled before we even start.
    let stream_res = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            sink.emit(Event::AssistantMessageEnd { session_id: session_id.to_owned() }).await;
            return Err(StreamError::Cancelled);
        }
        s = provider.stream(prompt) => s,
    };
    let mut stream = match stream_res {
        Ok(s) => s,
        Err(e) => {
            sink.emit(Event::AssistantMessageEnd {
                session_id: session_id.to_owned(),
            })
            .await;
            return Err(StreamError::Provider(e));
        }
    };

    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut text_buf = String::new();
    let mut reasoning_buf: Option<(String, Option<String>)> = None;
    let mut stop_reason = StopReason::Other;

    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                sink.emit(Event::AssistantMessageEnd { session_id: session_id.to_owned() }).await;
                return Err(StreamError::Cancelled);
            }
            n = stream.next() => n,
        };
        let event = match next {
            None => break,
            Some(Err(e)) => {
                sink.emit(Event::AssistantMessageEnd {
                    session_id: session_id.to_owned(),
                })
                .await;
                return Err(StreamError::Provider(e));
            }
            Some(Ok(ev)) => ev,
        };

        match event {
            ProviderEvent::TextDelta(t) => {
                text_buf.push_str(&t);
                sink.emit(Event::AssistantToken {
                    session_id: session_id.to_owned(),
                    text: t,
                })
                .await;
            }
            ProviderEvent::ReasoningDelta { text, signature } => {
                let entry = reasoning_buf.get_or_insert((String::new(), None));
                entry.0.push_str(&text);
                if signature.is_some() {
                    entry.1 = signature;
                }
            }
            ProviderEvent::ToolUseDelta { .. } => {}
            ProviderEvent::ToolUseComplete {
                call_id,
                name,
                input,
            } => {
                flush_text_and_reasoning(&mut blocks, &mut text_buf, &mut reasoning_buf);
                blocks.push(ContentBlock::ToolUse {
                    call_id,
                    name,
                    input,
                });
            }
            ProviderEvent::Done { stop_reason: sr } => {
                stop_reason = sr;
                break;
            }
        }
    }
    flush_text_and_reasoning(&mut blocks, &mut text_buf, &mut reasoning_buf);

    sink.emit(Event::AssistantMessageEnd {
        session_id: session_id.to_owned(),
    })
    .await;
    Ok((blocks, stop_reason))
}

fn flush_text_and_reasoning(
    blocks: &mut Vec<ContentBlock>,
    text_buf: &mut String,
    reasoning_buf: &mut Option<(String, Option<String>)>,
) {
    if !text_buf.is_empty() {
        blocks.push(ContentBlock::Text {
            text: std::mem::take(text_buf),
        });
    }
    if let Some((text, signature)) = reasoning_buf.take() {
        if !text.is_empty() {
            blocks.push(ContentBlock::Reasoning { text, signature });
        }
    }
}
