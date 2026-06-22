use serde::{Deserialize, Serialize};

/// Message role in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// A single message in the conversation history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Only `Some` when `role == Tool`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Only meaningful when `role == Tool`; marks the tool_result as failure/rejection/cancellation.
    #[serde(default)]
    pub is_error: bool,
}

impl Message {
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            is_error: false,
        }
    }

    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            is_error: false,
        }
    }

    #[must_use]
    pub fn tool_result(call_id: String, content: impl Into<String>, is_error: bool) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(call_id),
            is_error,
        }
    }
}

/// The prompt sent to a provider: system messages, tool specs, and conversation history.
#[derive(Debug, Clone)]
pub struct Prompt {
    /// System messages. Sent in full, never compressed.
    pub system: Vec<Message>,
    /// Tool specifications. Sent in full, never compressed.
    pub tools: Vec<super::tool::ToolSpec>,
    /// Conversation messages. The only part that may be compressed.
    pub messages: Vec<Message>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_sets_correct_fields() {
        let msg = Message::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.content, "hello");
        assert!(msg.tool_call_id.is_none());
        assert!(!msg.is_error);
    }

    #[test]
    fn assistant_message_sets_correct_fields() {
        let msg = Message::assistant("world");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.content, "world");
        assert!(msg.tool_call_id.is_none());
        assert!(!msg.is_error);
    }

    #[test]
    fn tool_result_success() {
        let msg = Message::tool_result("c1".into(), "ok", false);
        assert_eq!(msg.role, Role::Tool);
        assert_eq!(msg.tool_call_id.as_deref(), Some("c1"));
        assert!(!msg.is_error);
    }

    #[test]
    fn tool_result_error() {
        let msg = Message::tool_result("c2".into(), "failed", true);
        assert_eq!(msg.role, Role::Tool);
        assert!(msg.is_error);
    }

    #[test]
    fn user_message_serializes_without_tool_fields() {
        let msg = Message::user("hi");
        let json = serde_json::to_value(&msg).expect("serialize");
        assert!(json.get("tool_call_id").is_none());
    }
}
// Role, Message, Prompt
