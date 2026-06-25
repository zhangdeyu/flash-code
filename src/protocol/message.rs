use serde::{Deserialize, Serialize};

use super::content_block::ContentBlock;
use super::tool::ToolSpec;

pub type MessageId = String;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
    /// Compaction summary placeholder. Provider adapters degrade to user + prefix.
    Summary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: MessageId,
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

fn new_id() -> MessageId {
    uuid::Uuid::new_v4().to_string()
}

impl Message {
    /// System prompt message. Content must be Text only.
    #[must_use]
    pub fn system_text(text: impl Into<String>) -> Self {
        Self {
            id: new_id(),
            role: Role::System,
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// User message. Content allows Text + Image.
    #[must_use]
    pub fn user(blocks: Vec<ContentBlock>) -> Self {
        for b in &blocks {
            assert!(
                matches!(
                    b,
                    ContentBlock::Text { .. } | ContentBlock::Image { .. }
                ),
                "user message only allows Text/Image blocks",
            );
        }
        Self {
            id: new_id(),
            role: Role::User,
            content: blocks,
        }
    }

    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        Self::user(vec![ContentBlock::Text { text: text.into() }])
    }

    /// Assistant message. Content allows Text + ToolUse + Reasoning.
    #[must_use]
    pub fn assistant(blocks: Vec<ContentBlock>) -> Self {
        for b in &blocks {
            assert!(
                matches!(
                    b,
                    ContentBlock::Text { .. }
                        | ContentBlock::ToolUse { .. }
                        | ContentBlock::Reasoning { .. }
                ),
                "assistant message only allows Text/ToolUse/Reasoning blocks",
            );
        }
        Self {
            id: new_id(),
            role: Role::Assistant,
            content: blocks,
        }
    }

    /// Tool result message. All blocks must be ToolResult.
    #[must_use]
    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        assert!(
            !results.is_empty(),
            "tool_results message must contain at least one ToolResult"
        );
        for b in &results {
            assert!(
                matches!(b, ContentBlock::ToolResult { .. }),
                "tool_results message only allows ToolResult blocks",
            );
        }
        Self {
            id: new_id(),
            role: Role::Tool,
            content: results,
        }
    }

    /// Compaction summary message. Crate-internal only — never enters raw history.
    pub(crate) fn summary(text: String) -> Self {
        Self {
            id: new_id(),
            role: Role::Summary,
            content: vec![ContentBlock::Text { text }],
        }
    }

    /// First text content (best-effort). Useful for adapters that flatten content.
    #[must_use]
    pub fn first_text(&self) -> Option<&str> {
        self.content.iter().find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
    }

    /// All ToolUse call_ids in this message (Assistant only).
    #[must_use]
    pub fn tool_use_call_ids(&self) -> Vec<String> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect()
    }

    /// All ToolResult call_ids in this message (Tool only).
    #[must_use]
    pub fn tool_result_call_ids(&self) -> Vec<String> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { call_id, .. } => Some(call_id.clone()),
                _ => None,
            })
            .collect()
    }
}

/// The prompt sent to a provider.
#[derive(Debug, Clone)]
pub struct Prompt {
    pub system: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub messages: Vec<Message>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_constructs() {
        let m = Message::user_text("hi");
        assert_eq!(m.role, Role::User);
        assert_eq!(m.first_text(), Some("hi"));
    }

    #[test]
    #[should_panic]
    fn assistant_rejects_tool_result() {
        let bad = ContentBlock::tool_result("c1", vec![ContentBlock::text("x")], false).unwrap();
        let _ = Message::assistant(vec![bad]);
    }

    #[test]
    fn tool_results_extracts_call_ids() {
        let r = ContentBlock::tool_result("c1", vec![ContentBlock::text("ok")], false).unwrap();
        let m = Message::tool_results(vec![r]);
        assert_eq!(m.tool_result_call_ids(), vec!["c1".to_owned()]);
    }
}
