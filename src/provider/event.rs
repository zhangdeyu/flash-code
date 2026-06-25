use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Other,
}

#[derive(Debug, Clone)]
pub enum ProviderEvent {
    TextDelta(String),

    ReasoningDelta {
        text: String,
        signature: Option<String>,
    },

    ToolUseDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        args_delta: Option<String>,
    },

    ToolUseComplete {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },

    Done {
        stop_reason: StopReason,
    },
}
