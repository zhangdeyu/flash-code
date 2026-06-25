use serde::{Deserialize, Deserializer, Serialize};

use super::compaction::Compaction;
use super::message::Message;

/// All events emitted during a session.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    SessionStarted {
        session_id: String,
    },

    AssistantMessageStart {
        session_id: String,
    },
    AssistantToken {
        session_id: String,
        text: String,
    },
    AssistantMessageEnd {
        session_id: String,
    },

    ToolStart {
        session_id: String,
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    ToolEnd {
        session_id: String,
        call_id: String,
        output: serde_json::Value,
        duration_ms: u64,
    },
    ToolError {
        session_id: String,
        call_id: String,
        error: String,
    },
    ToolCancelled {
        session_id: String,
        call_id: String,
    },

    ApprovalRequired {
        session_id: String,
        call_id: String,
        command: String,
    },
    ApprovalGranted {
        session_id: String,
        call_id: String,
    },
    ApprovalRejected {
        session_id: String,
        call_id: String,
    },

    /// One message appended to the raw history.
    MessageAppended {
        session_id: String,
        message: Message,
    },

    /// History compacted: replaces older messages with a summary anchored at tail_start_id.
    HistoryCompacted {
        session_id: String,
        compaction: Compaction,
        before_count: usize,
        tail_count: usize,
    },

    /// Old large tool_results redacted at projection time.
    MicroCompacted {
        session_id: String,
        redacted_ids: Vec<String>,
        bytes_saved: usize,
    },

    Cancelled {
        session_id: String,
        reason: String,
    },
    Error {
        session_id: String,
        message: String,
    },

    /// Catch-all for unknown event types. Preserves the raw JSON payload.
    #[serde(skip_serializing)]
    Unknown(serde_json::Value),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum KnownEvent {
    SessionStarted { session_id: String },

    AssistantMessageStart { session_id: String },
    AssistantToken { session_id: String, text: String },
    AssistantMessageEnd { session_id: String },

    ToolStart {
        session_id: String,
        call_id: String,
        tool: String,
        input: serde_json::Value,
    },
    ToolEnd {
        session_id: String,
        call_id: String,
        output: serde_json::Value,
        duration_ms: u64,
    },
    ToolError {
        session_id: String,
        call_id: String,
        error: String,
    },
    ToolCancelled {
        session_id: String,
        call_id: String,
    },

    ApprovalRequired {
        session_id: String,
        call_id: String,
        command: String,
    },
    ApprovalGranted { session_id: String, call_id: String },
    ApprovalRejected { session_id: String, call_id: String },

    MessageAppended {
        session_id: String,
        message: Message,
    },
    HistoryCompacted {
        session_id: String,
        compaction: Compaction,
        before_count: usize,
        tail_count: usize,
    },
    MicroCompacted {
        session_id: String,
        redacted_ids: Vec<String>,
        bytes_saved: usize,
    },

    Cancelled { session_id: String, reason: String },
    Error { session_id: String, message: String },
}

impl From<KnownEvent> for Event {
    fn from(k: KnownEvent) -> Self {
        match k {
            KnownEvent::SessionStarted { session_id } => Self::SessionStarted { session_id },
            KnownEvent::AssistantMessageStart { session_id } => {
                Self::AssistantMessageStart { session_id }
            }
            KnownEvent::AssistantToken { session_id, text } => {
                Self::AssistantToken { session_id, text }
            }
            KnownEvent::AssistantMessageEnd { session_id } => {
                Self::AssistantMessageEnd { session_id }
            }
            KnownEvent::ToolStart {
                session_id,
                call_id,
                tool,
                input,
            } => Self::ToolStart {
                session_id,
                call_id,
                tool,
                input,
            },
            KnownEvent::ToolEnd {
                session_id,
                call_id,
                output,
                duration_ms,
            } => Self::ToolEnd {
                session_id,
                call_id,
                output,
                duration_ms,
            },
            KnownEvent::ToolError {
                session_id,
                call_id,
                error,
            } => Self::ToolError {
                session_id,
                call_id,
                error,
            },
            KnownEvent::ToolCancelled {
                session_id,
                call_id,
            } => Self::ToolCancelled {
                session_id,
                call_id,
            },
            KnownEvent::ApprovalRequired {
                session_id,
                call_id,
                command,
            } => Self::ApprovalRequired {
                session_id,
                call_id,
                command,
            },
            KnownEvent::ApprovalGranted {
                session_id,
                call_id,
            } => Self::ApprovalGranted {
                session_id,
                call_id,
            },
            KnownEvent::ApprovalRejected {
                session_id,
                call_id,
            } => Self::ApprovalRejected {
                session_id,
                call_id,
            },
            KnownEvent::MessageAppended {
                session_id,
                message,
            } => Self::MessageAppended {
                session_id,
                message,
            },
            KnownEvent::HistoryCompacted {
                session_id,
                compaction,
                before_count,
                tail_count,
            } => Self::HistoryCompacted {
                session_id,
                compaction,
                before_count,
                tail_count,
            },
            KnownEvent::MicroCompacted {
                session_id,
                redacted_ids,
                bytes_saved,
            } => Self::MicroCompacted {
                session_id,
                redacted_ids,
                bytes_saved,
            },
            KnownEvent::Cancelled {
                session_id,
                reason,
            } => Self::Cancelled {
                session_id,
                reason,
            },
            KnownEvent::Error {
                session_id,
                message,
            } => Self::Error {
                session_id,
                message,
            },
        }
    }
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match serde_json::from_value::<KnownEvent>(value.clone()) {
            Ok(known) => Ok(known.into()),
            Err(_) => Ok(Self::Unknown(value)),
        }
    }
}
