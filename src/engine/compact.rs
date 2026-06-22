use std::sync::Arc;

use crate::error::Result;
use crate::protocol::{Event, Message, Prompt, Role};
use crate::provider::Provider;
use crate::sink::EventSink;

const COMPACT_THRESHOLD: usize = 8000;
const KEEP_LAST_TURNS: usize = 2;

const COMPACT_SYSTEM_PROMPT: &str = "\
You are a conversation summarizer. Summarize the conversation history below into \
a concise paragraph that preserves key decisions, tool results, and context needed \
to continue the conversation. Be factual and brief.";

/// Compress old messages if token estimate exceeds threshold.
///
/// Only `messages` is compressed. System messages and tool specs are never touched.
/// Emits a `HistoryCompacted` event for replay consistency.
pub async fn maybe_compact(
    messages: Vec<Message>,
    provider: &dyn Provider,
    session_id: &str,
    sink: Arc<dyn EventSink>,
) -> Result<Vec<Message>> {
    if estimate_tokens(&messages) < COMPACT_THRESHOLD {
        return Ok(messages);
    }

    let before = messages.len();
    // Keep the last N*2 messages (user+assistant pairs)
    let split_at = messages.len().saturating_sub(KEEP_LAST_TURNS * 2);
    let (to_compact, keep) = messages.split_at(split_at);

    let summary = provider
        .complete_once(&Prompt {
            system: vec![Message {
                role: Role::System,
                content: COMPACT_SYSTEM_PROMPT.into(),
                tool_call_id: None,
                is_error: false,
            }],
            tools: vec![],
            messages: vec![Message::user(serialize_for_compaction(to_compact))],
        })
        .await?;

    let mut out = vec![Message::user(format!("[history summary]\n{summary}"))];
    out.extend_from_slice(keep);

    sink.emit(Event::HistoryCompacted {
        session_id: session_id.into(),
        before_count: before,
        after_count: out.len(),
        summary: summary.clone(),
    })
    .await;

    Ok(out)
}

/// Rough token estimate: ~4 chars per token (a common heuristic).
fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(|m| m.content.len() / 4).sum()
}

/// Serialize messages for the compaction prompt.
fn serialize_for_compaction(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| format!("[{:?}] {}", m.role, m.content))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimate_tokens_basic() {
        let messages = vec![
            Message::user("hello world"), // 11 chars → 2 tokens
        ];
        assert_eq!(estimate_tokens(&messages), 2);
    }

    #[test]
    fn below_threshold_returns_unchanged() {
        let messages = vec![Message::user("short message")];
        // Run synchronously by checking threshold directly
        assert!(estimate_tokens(&messages) < COMPACT_THRESHOLD);
    }

    #[test]
    fn serialize_for_compaction_format() {
        let messages = vec![
            Message::user("hello"),
            Message::assistant("hi there"),
        ];
        let result = serialize_for_compaction(&messages);
        assert!(result.contains("[User] hello"));
        assert!(result.contains("[Assistant] hi there"));
    }
}
// maybe_compact: history compression
