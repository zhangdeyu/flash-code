pub mod capability;
pub mod deepseek;
pub mod error;
pub mod event;

use async_trait::async_trait;
use futures::stream::BoxStream;

pub use capability::Capability;
pub use error::ProviderError;
pub use event::{ProviderEvent, StopReason, Usage};

use crate::protocol::{Prompt, ToolSpec};

/// LLM provider trait.
///
/// V1's only production implementation is `DeepSeekProvider`. The trait exists
/// for testing (MockProvider) and is intentionally minimal.
#[async_trait]
pub trait Provider: Send + Sync {
    fn capability(&self) -> &Capability;

    fn model_id(&self) -> &str;

    /// Streaming inference. The first error (connection/auth) is the outer Result.
    /// Mid-stream errors are stream elements.
    async fn stream(
        &self,
        prompt: &Prompt,
    ) -> Result<BoxStream<'_, Result<ProviderEvent, ProviderError>>, ProviderError>;

    /// Non-streaming one-shot. Used by compaction (tools forced to []).
    async fn complete_once(&self, prompt: &Prompt) -> Result<String, ProviderError>;
}

/// Build the OpenAI-compatible tools array from `ToolSpec` list. DeepSeek's
/// function-calling shape is identical to OpenAI's, so the same encoding works.
#[must_use]
pub fn tool_specs_to_deepseek(specs: &[ToolSpec]) -> Vec<serde_json::Value> {
    specs
        .iter()
        .map(|s| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": s.name,
                    "description": s.description,
                    "parameters": s.input_schema,
                }
            })
        })
        .collect()
}
