use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::protocol::{
    estimate_message_tokens, Compaction, CompactionTrigger, ContentBlock, Event, History,
    Message, MessageId, Prompt, Role,
};
use crate::provider::{Capability, Provider};
use crate::sink::EventSink;

const DEFAULT_SUMMARY_PROMPT: &str = "You are a conversation summarization assistant. Your job is to compress the older
portion of an ongoing conversation between a user and a coding agent into a concise
summary, while preserving everything needed for the agent to continue the work.

Input format:
- A series of messages tagged with role (user / assistant / tool).
- May begin with a <previous-summary> block: this is a summary from an earlier
  compaction. Treat it as already-condensed context to merge into your new summary,
  not as new content to summarize again.

Output a single paragraph (or short bulleted list) that captures:
1. User's explicit goals and constraints
2. Key decisions made and rationale
3. Files / commands / data referenced
4. Errors encountered and how they were resolved
5. Outstanding TODOs or open questions

Be factual and dense. Do not editorialize. Do not add information not present in
the input. Do not include role tags or formatting from the input.";

#[async_trait]
pub trait CompactionPolicy: Send + Sync {
    fn should_compact(&self, history: &History, capability: &Capability) -> bool;

    fn select_tail(&self, history: &History) -> Option<MessageId>;

    fn select_tail_overflow(&self, history: &History) -> Option<MessageId>;

    fn summary_prompt(&self) -> &str {
        DEFAULT_SUMMARY_PROMPT
    }
}

pub struct DefaultPolicy {
    pub keep_last_turns: usize,
    pub max_tail_tokens: usize,
    pub reserved_tokens: usize,
    pub overflow_keep_last_turns: usize,
}

impl Default for DefaultPolicy {
    fn default() -> Self {
        Self {
            keep_last_turns: 2,
            max_tail_tokens: 20_000,
            reserved_tokens: 20_000,
            overflow_keep_last_turns: 1,
        }
    }
}

fn estimate_last_turn_tokens(history: &History) -> usize {
    let n = history.raw_messages().len();
    if n == 0 {
        return 2_000;
    }
    let start = n.saturating_sub(3);
    history.raw_messages()[start..]
        .iter()
        .map(estimate_message_tokens)
        .sum()
}

fn select_tail_with_limit(
    history: &History,
    target_turns: usize,
    max_tokens: usize,
) -> Option<MessageId> {
    let messages = history.raw_messages();
    if messages.len() < 3 {
        return None;
    }

    let user_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == Role::User)
        .map(|(i, _)| i)
        .collect();

    if user_indices.len() <= target_turns {
        return None;
    }

    let mut candidate_idx = user_indices[user_indices.len() - target_turns];

    loop {
        let tail_tokens: usize = messages[candidate_idx..]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        if tail_tokens <= max_tokens {
            break;
        }
        match user_indices.iter().find(|&&i| i > candidate_idx) {
            Some(&next) => candidate_idx = next,
            None => return None,
        }
    }

    if candidate_idx >= messages.len() - 1 {
        return None;
    }

    Some(messages[candidate_idx].id.clone())
}

#[async_trait]
impl CompactionPolicy for DefaultPolicy {
    fn should_compact(&self, history: &History, capability: &Capability) -> bool {
        let current = history.estimate_tokens();
        let next_turn_growth = estimate_last_turn_tokens(history);
        let usable = capability.max_context.saturating_sub(self.reserved_tokens);
        current + next_turn_growth > usable
    }

    fn select_tail(&self, history: &History) -> Option<MessageId> {
        select_tail_with_limit(history, self.keep_last_turns, self.max_tail_tokens)
    }

    fn select_tail_overflow(&self, history: &History) -> Option<MessageId> {
        select_tail_with_limit(
            history,
            self.overflow_keep_last_turns,
            self.max_tail_tokens / 2,
        )
    }
}

fn render_blocks_as_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();
    for b in blocks {
        match b {
            ContentBlock::Text { text } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            ContentBlock::ToolUse { name, input, .. } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!(
                    "[tool_use {name}] {}",
                    serde_json::to_string(input).unwrap_or_default()
                ));
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                let inner = render_blocks_as_text(content);
                out.push_str(&format!(
                    "[tool_result{}] {inner}",
                    if *is_error { " error" } else { "" }
                ));
            }
            _ => {}
        }
    }
    out
}

fn serialize_for_compaction(messages: &[Message]) -> String {
    messages
        .iter()
        .map(|m| {
            let content = render_blocks_as_text(&m.content);
            if m.role == Role::Summary {
                format!("<previous-summary>\n{content}\n</previous-summary>")
            } else {
                let role = format!("{:?}", m.role).to_lowercase();
                format!("[{role}] {content}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

async fn run_summary(
    history: &History,
    tail_id: &MessageId,
    policy: &dyn CompactionPolicy,
    provider: &dyn Provider,
) -> Result<String> {
    let to_compact = history.messages_before(tail_id);
    let body = serialize_for_compaction(&to_compact);
    let prompt = Prompt {
        system: vec![Message::system_text(policy.summary_prompt())],
        tools: vec![],
        messages: vec![Message::user_text(body)],
    };
    let s = provider.complete_once(&prompt).await?;
    Ok(s)
}

pub async fn maybe_compact(
    history: &mut History,
    policy: &dyn CompactionPolicy,
    provider: &dyn Provider,
    capability: &Capability,
    sink: Arc<dyn EventSink>,
    session_id: &str,
) -> Result<bool> {
    if !policy.should_compact(history, capability) {
        return Ok(false);
    }
    let Some(tail_id) = policy.select_tail(history) else {
        return Ok(false);
    };

    let summary = run_summary(history, &tail_id, policy, provider).await?;
    let c = Compaction::new(summary, tail_id, CompactionTrigger::Auto);
    let before = history.raw_messages().len();
    history.record_compaction(c.clone())?;
    let tail_count = history.to_prompt_messages().len().saturating_sub(1);

    sink.emit(Event::HistoryCompacted {
        session_id: session_id.to_owned(),
        compaction: c,
        before_count: before,
        tail_count,
    })
    .await;

    Ok(true)
}

pub async fn overflow_compact(
    history: &mut History,
    policy: &dyn CompactionPolicy,
    provider: &dyn Provider,
    sink: Arc<dyn EventSink>,
    session_id: &str,
) -> Result<()> {
    let Some(tail_id) = policy.select_tail_overflow(history) else {
        return Err(crate::error::Error::ContextOverflow);
    };

    let summary = run_summary(history, &tail_id, policy, provider)
        .await
        .map_err(|_| crate::error::Error::ContextOverflow)?;

    let c = Compaction::new(summary, tail_id, CompactionTrigger::Overflow);
    let before = history.raw_messages().len();
    history.record_compaction(c.clone())?;
    let tail_count = history.to_prompt_messages().len().saturating_sub(1);

    sink.emit(Event::HistoryCompacted {
        session_id: session_id.to_owned(),
        compaction: c,
        before_count: before,
        tail_count,
    })
    .await;
    Ok(())
}
