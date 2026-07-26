use async_trait::async_trait;
use flash_core::{CancellationToken, Message};
use serde_json::Value;

#[async_trait(?Send)]
pub trait ChatProvider {
    /// Send a chat request and emit events through the bounded channel in streaming order.
    async fn chat(
        &mut self,
        request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError>;
}

pub async fn send_event(
    events: &tokio::sync::mpsc::Sender<ProviderEvent>,
    event: ProviderEvent,
) -> Result<(), ProviderError> {
    events
        .send(event)
        .await
        .map_err(|_| ProviderError::Cancelled("provider event consumer closed".to_string()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub model: String,
    pub cancellation: CancellationToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    /// Short description of what the tool does, used in the model's function-calling prompt.
    pub description: String,
    /// JSON Schema object describing the tool's input parameters.
    pub parameters: Value,
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
    pub input: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    StopSequence,
    Refusal,
    Cancelled,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderError {
    Authentication(String),
    Billing(String),
    RateLimited(String),
    Server(String),
    InvalidRequest(String),
    Cancelled(String),
    Unrecoverable(String),
}

impl ProviderError {
    pub const fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited(_) | Self::Server(_))
    }

    pub const fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled(_))
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
            | Self::Cancelled(message)
            | Self::Unrecoverable(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoProvider;

    #[async_trait(?Send)]
    impl ChatProvider for EchoProvider {
        async fn chat(
            &mut self,
            _request: ChatRequest,
            events: tokio::sync::mpsc::Sender<ProviderEvent>,
        ) -> Result<(), ProviderError> {
            send_event(&events, ProviderEvent::TextDelta("hello".to_string())).await?;
            send_event(&events, ProviderEvent::Done(StopReason::EndTurn)).await?;
            Ok(())
        }
    }

    #[tokio::test]
    async fn chat_provider_should_emit_events_via_channel() {
        let mut provider = EchoProvider;
        let (sender, mut receiver) = tokio::sync::mpsc::channel(4);
        provider
            .chat(
                ChatRequest {
                    messages: Vec::new(),
                    tools: Vec::new(),
                    model: "test".to_string(),
                    cancellation: CancellationToken::new(),
                },
                sender,
            )
            .await
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        assert_eq!(events.len(), 2);
    }
}
