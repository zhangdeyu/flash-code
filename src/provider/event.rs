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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    /// DeepSeek thinking mode reports reasoning tokens under
    /// `completion_tokens_details.reasoning_tokens`. Absent for non-thinking responses.
    pub reasoning_tokens: Option<u64>,
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

    /// Token usage reported by the provider. With `stream_options.include_usage = true`,
    /// DeepSeek emits this in the final chunk before `[DONE]`.
    Usage(Usage),

    Done {
        stop_reason: StopReason,
    },
}
