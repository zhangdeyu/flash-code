use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Reasoning {
        text: String,
    },
    ToolUse {
        call_id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        call_id: String,
        status: ToolResultStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub created_at: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultStatus {
    Success,
    Error,
    Rejected,
    Cancelled,
}

impl ToolResultStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Error => "error",
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
}

impl Outcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl SessionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted {
        session_id: String,
    },
    UserMessageAppended {
        message_id: String,
    },
    ModelRequestStarted {
        request_id: String,
        attempt: u32,
        model: String,
    },
    ReasoningDelta {
        request_id: String,
        attempt: u32,
        text: String,
    },
    AssistantDelta {
        request_id: String,
        attempt: u32,
        text: String,
    },
    ModelAttemptFailed {
        request_id: String,
        attempt: u32,
        retryable: bool,
        message: String,
    },
    ModelAttemptCommitted {
        request_id: String,
        attempt: u32,
    },
    AssistantMessageCompleted {
        message_id: String,
    },
    ToolCallRequested {
        call_id: String,
        name: String,
    },
    ApprovalRequired {
        call_id: String,
    },
    ApprovalResolved {
        call_id: String,
        approved: bool,
    },
    ToolStarted {
        call_id: String,
        name: String,
    },
    ToolOutputDelta {
        call_id: String,
        stream: String,
        text: String,
    },
    ToolFinished {
        call_id: String,
        status: ToolResultStatus,
    },
    UsageRecorded {
        request_id: String,
        attempt: u32,
        input_tokens: u64,
        output_tokens: u64,
    },
    Error {
        message: String,
    },
    SessionFinished {
        outcome: Outcome,
    },
}

impl Event {
    pub const fn event_type(&self) -> &'static str {
        match self {
            Self::SessionStarted { .. } => "session_started",
            Self::UserMessageAppended { .. } => "user_message_appended",
            Self::ModelRequestStarted { .. } => "model_request_started",
            Self::ReasoningDelta { .. } => "reasoning_delta",
            Self::AssistantDelta { .. } => "assistant_delta",
            Self::ModelAttemptFailed { .. } => "model_attempt_failed",
            Self::ModelAttemptCommitted { .. } => "model_attempt_committed",
            Self::AssistantMessageCompleted { .. } => "assistant_message_completed",
            Self::ToolCallRequested { .. } => "tool_call_requested",
            Self::ApprovalRequired { .. } => "approval_required",
            Self::ApprovalResolved { .. } => "approval_resolved",
            Self::ToolStarted { .. } => "tool_started",
            Self::ToolOutputDelta { .. } => "tool_output_delta",
            Self::ToolFinished { .. } => "tool_finished",
            Self::UsageRecorded { .. } => "usage_recorded",
            Self::Error { .. } => "error",
            Self::SessionFinished { .. } => "session_finished",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_should_round_trip_through_serde_json() {
        let message = Message {
            id: "msg_1".to_string(),
            role: Role::Assistant,
            created_at: "123".to_string(),
            content: vec![ContentBlock::ToolUse {
                call_id: "call_1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({"path": "src/lib.rs"}),
            }],
        };

        let encoded = serde_json::to_string(&message).unwrap();
        let decoded: Message = serde_json::from_str(&encoded).unwrap();

        assert_eq!(decoded, message);
    }

    #[test]
    fn event_should_use_snake_case_tagged_schema() {
        let event = Event::ToolFinished {
            call_id: "call_1".to_string(),
            status: ToolResultStatus::Success,
        };

        let encoded = serde_json::to_string(&event).unwrap();

        assert_eq!(
            encoded,
            r#"{"type":"tool_finished","call_id":"call_1","status":"success"}"#
        );
    }
}
