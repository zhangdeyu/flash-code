pub mod openai;

use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::error::Result;
use crate::protocol::{Prompt, ToolSpec};

/// Events produced by a streaming model call.
#[derive(Debug, Clone)]
pub enum ModelEvent {
    /// A text token from the assistant.
    Token(String),
    /// The model requests a tool call.
    ToolUse {
        call_id: String,
        name: String,
        input: serde_json::Value,
    },
    /// The model has finished generating.
    Done,
}

/// Trait abstracting an LLM provider.
///
/// Two methods:
/// - `stream`: streaming chat completion with tool support (main path)
/// - `complete_once`: single non-streaming completion (used only for history compaction)
#[async_trait]
pub trait Provider: Send + Sync {
    async fn stream(&self, prompt: &Prompt) -> BoxStream<'_, ModelEvent>;
    async fn complete_once(&self, prompt: &Prompt) -> Result<String>;
}

/// Build the tools array in OpenAI format from our `ToolSpec` list.
pub fn tool_specs_to_openai(specs: &[ToolSpec]) -> Vec<serde_json::Value> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_specs_to_openai_format() {
        let specs = vec![ToolSpec {
            name: "bash".into(),
            description: "Run a command".into(),
            input_schema: serde_json::json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }];
        let result = tool_specs_to_openai(&specs);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["type"], "function");
        assert_eq!(result[0]["function"]["name"], "bash");
    }
}
