use serde::{Deserialize, Serialize};

use super::content_block::{ContentBlock, ImageSource};
use super::message::{Message, Role};

pub const MICRO_PLACEHOLDER: &str = "[Old tool result content cleared]";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MicroCompactPolicy {
    pub stale_after_turns: usize,
    pub keep_recent: usize,
    pub size_threshold_bytes: usize,
}

impl Default for MicroCompactPolicy {
    fn default() -> Self {
        Self {
            stale_after_turns: 4,
            keep_recent: 5,
            size_threshold_bytes: 4_000,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct MicroCompactResult {
    pub redacted_ids: Vec<String>,
    pub bytes_saved: usize,
}

struct ToolResultRef {
    msg_idx: usize,
    block_idx: usize,
    call_id: String,
    size: usize,
}

fn content_size_bytes(blocks: &[ContentBlock]) -> usize {
    blocks
        .iter()
        .map(|b| match b {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::Image { source } => match source {
                ImageSource::Base64 { data, .. } => data.len(),
                ImageSource::Url { url } => url.len(),
            },
            _ => 0,
        })
        .sum()
}

/// Apply MicroCompact in-place to a projected message slice.
/// Returns the list of redacted call_ids and total bytes saved.
pub fn apply_micro_compact(
    messages: &mut [Message],
    policy: &MicroCompactPolicy,
) -> MicroCompactResult {
    let mut tool_results: Vec<ToolResultRef> = Vec::new();
    for (mi, msg) in messages.iter().enumerate() {
        if msg.role != Role::Tool {
            continue;
        }
        for (bi, block) in msg.content.iter().enumerate() {
            if let ContentBlock::ToolResult {
                call_id, content, ..
            } = block
            {
                let size = content_size_bytes(content);
                tool_results.push(ToolResultRef {
                    msg_idx: mi,
                    block_idx: bi,
                    call_id: call_id.clone(),
                    size,
                });
            }
        }
    }

    if tool_results.is_empty() {
        return MicroCompactResult::default();
    }

    let total = tool_results.len();
    let mut redacted_ids = Vec::new();
    let mut bytes_saved = 0_usize;

    for (rank, tr) in tool_results.iter().enumerate() {
        let distance = total - 1 - rank;
        let in_keep_recent = distance < policy.keep_recent;
        if in_keep_recent {
            continue;
        }
        if distance < policy.stale_after_turns {
            continue;
        }
        if tr.size < policy.size_threshold_bytes {
            continue;
        }

        if let ContentBlock::ToolResult { content, .. } =
            &mut messages[tr.msg_idx].content[tr.block_idx]
        {
            *content = vec![ContentBlock::Text {
                text: MICRO_PLACEHOLDER.to_owned(),
            }];
        }
        redacted_ids.push(tr.call_id.clone());
        bytes_saved += tr.size;
    }

    MicroCompactResult {
        redacted_ids,
        bytes_saved,
    }
}
