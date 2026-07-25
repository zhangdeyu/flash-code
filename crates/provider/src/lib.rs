use flash_core::Message;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub supports_reasoning: bool,
    pub supports_tool_calls: bool,
    pub requires_reasoning_for_tool_turns: bool,
    pub supports_json_mode: bool,
    pub supports_prompt_cache_metrics: bool,
    pub max_context_tokens: u32,
    pub max_output_tokens: u32,
}

pub trait Provider {
    fn name(&self) -> &str;

    fn capabilities(&self) -> &ModelCapabilities;
}

pub trait ChatProvider {
    /// Send a chat request and emit events via `on_event` as they arrive.
    /// The callback is called for each `ProviderEvent` in streaming order.
    fn chat(
        &mut self,
        request: ChatRequest,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), ProviderError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    /// Short description of what the tool does, used in the model's function-calling prompt.
    pub description: String,
    /// JSON Schema string describing the tool's input parameters.
    /// Format: `{"type":"object","properties":{...},"required":[...]}`
    pub parameters: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallComplete(ToolCall),
    Usage(Usage),
    Done(StopReason),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub input: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    Authentication(String),
    Billing(String),
    RateLimited(String),
    Server(String),
    InvalidRequest(String),
    Unrecoverable(String),
}

impl ProviderError {
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited(_) | Self::Server(_))
    }
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Authentication(message)
            | Self::Billing(message)
            | Self::RateLimited(message)
            | Self::Server(message)
            | Self::InvalidRequest(message)
            | Self::Unrecoverable(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeProvider {
        capabilities: ModelCapabilities,
    }

    impl Provider for FakeProvider {
        fn name(&self) -> &str {
            "fake"
        }

        fn capabilities(&self) -> &ModelCapabilities {
            &self.capabilities
        }
    }

    struct EchoProvider;

    impl ChatProvider for EchoProvider {
        fn chat(
            &mut self,
            _request: ChatRequest,
            on_event: &mut dyn FnMut(ProviderEvent),
        ) -> Result<(), ProviderError> {
            on_event(ProviderEvent::TextDelta("hello".to_string()));
            on_event(ProviderEvent::Done(StopReason::EndTurn));
            Ok(())
        }
    }

    #[test]
    fn provider_should_expose_capabilities() {
        let provider = FakeProvider {
            capabilities: ModelCapabilities {
                supports_reasoning: true,
                supports_tool_calls: true,
                requires_reasoning_for_tool_turns: true,
                supports_json_mode: false,
                supports_prompt_cache_metrics: true,
                max_context_tokens: 64_000,
                max_output_tokens: 8_000,
            },
        };

        assert!(provider.capabilities().supports_tool_calls);
    }

    #[test]
    fn chat_provider_should_emit_events_via_callback() {
        let mut provider = EchoProvider;
        let mut events = Vec::new();
        provider
            .chat(
                ChatRequest {
                    messages: Vec::new(),
                    tools: Vec::new(),
                    model: "test".to_string(),
                },
                &mut |event| events.push(event),
            )
            .unwrap();
        assert_eq!(events.len(), 2);
    }
}
