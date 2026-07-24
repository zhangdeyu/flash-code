#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    },
    ToolResult {
        call_id: String,
        status: ToolResultStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: String,
    pub role: Role,
    pub created_at: String,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Running,
    Completed,
    Failed,
    Interrupted,
}

impl SessionStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    SessionStarted {
        session_id: String,
    },
    UserMessageAppended {
        message_id: String,
    },
    ModelRequestStarted {
        request_id: String,
        model: String,
    },
    ReasoningDelta {
        text: String,
    },
    AssistantDelta {
        text: String,
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

pub fn escape_json(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => escaped.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => escaped.push(ch),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_json_should_escape_quotes_and_newlines() {
        assert_eq!(escape_json("a\"b\nc"), "a\\\"b\\nc");
    }
}
