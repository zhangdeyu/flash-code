use std::collections::HashMap;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::protocol::{Message, Prompt, Role};
use crate::provider::{tool_specs_to_openai, ModelEvent, Provider};

/// OpenAI-compatible provider (also works for Azure OpenAI, OpenRouter, and any
/// other API that follows OpenAI's chat completions schema).
pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    model: String,
    client: reqwest::Client,
}

impl OpenAiProvider {
    #[must_use]
    pub fn new(api_key: String, base_url: String, model: String) -> Self {
        Self {
            api_key,
            base_url,
            model,
            client: reqwest::Client::new(),
        }
    }

    fn build_request_body(&self, prompt: &Prompt, streaming: bool) -> serde_json::Value {
        let messages = prompt_to_openai_messages(prompt);
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": streaming,
        });
        if !prompt.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tool_specs_to_openai(&prompt.tools));
        }
        body
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn stream(&self, prompt: &Prompt) -> BoxStream<'_, ModelEvent> {
        use reqwest_eventsource::EventSource;

        let body = self.build_request_body(prompt, true);
        let request = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body);

        let es = match EventSource::new(request) {
            Ok(es) => es,
            Err(e) => {
                let err = ModelEvent::Token(format!("[provider error] {e}"));
                return Box::pin(stream::iter(vec![err, ModelEvent::Done]));
            }
        };

        Box::pin(sse_to_model_events(es))
    }

    async fn complete_once(&self, prompt: &Prompt) -> Result<String> {
        let body = self.build_request_body(prompt, false);
        let response = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(Error::Provider(format!("status {status}: {text}")));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| Error::Provider(e.to_string()))?;

        Ok(body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }
}

// ---------- Format conversion ----------

/// Convert our `Prompt` into the OpenAI messages array format.
pub fn prompt_to_openai_messages(prompt: &Prompt) -> Vec<serde_json::Value> {
    let mut messages = Vec::with_capacity(prompt.system.len() + prompt.messages.len());
    for sys in &prompt.system {
        messages.push(serde_json::json!({"role": "system", "content": sys.content}));
    }
    for msg in &prompt.messages {
        messages.push(message_to_openai(msg));
    }
    messages
}

fn message_to_openai(msg: &Message) -> serde_json::Value {
    match msg.role {
        Role::System => serde_json::json!({"role": "system", "content": msg.content}),
        Role::User => serde_json::json!({"role": "user", "content": msg.content}),
        Role::Assistant => serde_json::json!({"role": "assistant", "content": msg.content}),
        Role::Tool => serde_json::json!({
            "role": "tool",
            "tool_call_id": msg.tool_call_id.as_deref().unwrap_or(""),
            "content": msg.content,
        }),
    }
}

// ---------- SSE chunk parsing ----------

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChoiceDelta>,
}

#[derive(Deserialize)]
struct ChoiceDelta {
    #[serde(default)]
    delta: Delta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct Delta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDelta>>,
}

#[derive(Deserialize, Default)]
struct ToolCallDelta {
    #[serde(default)]
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDelta>,
}

#[derive(Deserialize, Default)]
struct FunctionDelta {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Default)]
struct ToolCallAccum {
    id: String,
    name: String,
    arguments: String,
}

/// State carried through `stream::unfold` to convert SSE messages to `ModelEvent`s.
struct StreamState {
    es: reqwest_eventsource::EventSource,
    tool_calls: HashMap<u32, ToolCallAccum>,
    pending: std::collections::VecDeque<ModelEvent>,
    done: bool,
}

/// Drive the SSE event source and yield `ModelEvent`s. Handles:
/// - text deltas → `ModelEvent::Token`
/// - tool_call deltas accumulated by index, finalized on finish_reason
/// - `[DONE]` marker → end of stream, yields `ModelEvent::Done` once
fn sse_to_model_events(
    es: reqwest_eventsource::EventSource,
) -> impl futures::Stream<Item = ModelEvent> {
    let state = StreamState {
        es,
        tool_calls: HashMap::new(),
        pending: std::collections::VecDeque::new(),
        done: false,
    };

    futures::stream::unfold(state, |mut state| async move {
        // Drain any queued events first
        if let Some(ev) = state.pending.pop_front() {
            return Some((ev, state));
        }

        if state.done {
            return None;
        }

        loop {
            match state.es.next().await {
                Some(Ok(reqwest_eventsource::Event::Open)) => continue,
                Some(Ok(reqwest_eventsource::Event::Message(msg))) => {
                    if msg.data == "[DONE]" {
                        state.done = true;
                        // Flush any remaining tool calls (no finish_reason came)
                        flush_tool_calls(&mut state.tool_calls, &mut state.pending);
                        state.pending.push_back(ModelEvent::Done);
                        if let Some(ev) = state.pending.pop_front() {
                            return Some((ev, state));
                        }
                        return None;
                    }
                    let chunk: ChatChunk = match serde_json::from_str(&msg.data) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    for choice in chunk.choices {
                        if let Some(text) = choice.delta.content {
                            if !text.is_empty() {
                                state.pending.push_back(ModelEvent::Token(text));
                            }
                        }
                        if let Some(deltas) = choice.delta.tool_calls {
                            for d in deltas {
                                let accum = state.tool_calls.entry(d.index).or_default();
                                if let Some(id) = d.id {
                                    accum.id = id;
                                }
                                if let Some(func) = d.function {
                                    if let Some(name) = func.name {
                                        accum.name.push_str(&name);
                                    }
                                    if let Some(args) = func.arguments {
                                        accum.arguments.push_str(&args);
                                    }
                                }
                            }
                        }
                        if choice.finish_reason.is_some() {
                            flush_tool_calls(&mut state.tool_calls, &mut state.pending);
                        }
                    }
                    if let Some(ev) = state.pending.pop_front() {
                        return Some((ev, state));
                    }
                }
                Some(Err(_)) | None => {
                    state.done = true;
                    flush_tool_calls(&mut state.tool_calls, &mut state.pending);
                    state.pending.push_back(ModelEvent::Done);
                    if let Some(ev) = state.pending.pop_front() {
                        return Some((ev, state));
                    }
                    return None;
                }
            }
        }
    })
}

fn flush_tool_calls(
    tool_calls: &mut HashMap<u32, ToolCallAccum>,
    pending: &mut std::collections::VecDeque<ModelEvent>,
) {
    let mut indices: Vec<u32> = tool_calls.keys().copied().collect();
    indices.sort_unstable();
    for idx in indices {
        if let Some(accum) = tool_calls.remove(&idx) {
            let input = serde_json::from_str(&accum.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));
            pending.push_back(ModelEvent::ToolUse {
                call_id: accum.id,
                name: accum.name,
                input,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_to_openai_format() {
        let msg = Message::user("hello");
        let json = message_to_openai(&msg);
        assert_eq!(json["role"], "user");
        assert_eq!(json["content"], "hello");
    }

    #[test]
    fn tool_message_includes_call_id() {
        let msg = Message::tool_result("c1".into(), "result", false);
        let json = message_to_openai(&msg);
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "c1");
        assert_eq!(json["content"], "result");
    }

    #[test]
    fn prompt_with_system_and_messages() {
        let prompt = Prompt {
            system: vec![Message {
                role: Role::System,
                content: "you are helpful".into(),
                tool_call_id: None,
                is_error: false,
            }],
            tools: vec![],
            messages: vec![Message::user("hi"), Message::assistant("hello")],
        };
        let messages = prompt_to_openai_messages(&prompt);
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
    }

    #[test]
    fn build_request_body_includes_tools_when_present() {
        let provider = OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
        );
        let prompt = Prompt {
            system: vec![],
            tools: vec![crate::protocol::ToolSpec {
                name: "bash".into(),
                description: "run".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
            messages: vec![Message::user("hi")],
        };
        let body = provider.build_request_body(&prompt, true);
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], true);
        assert!(body["tools"].is_array());
    }

    #[test]
    fn build_request_body_omits_tools_when_empty() {
        let provider = OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1".into(),
            "gpt-4o".into(),
        );
        let prompt = Prompt {
            system: vec![],
            tools: vec![],
            messages: vec![Message::user("hi")],
        };
        let body = provider.build_request_body(&prompt, false);
        assert!(body.get("tools").is_none());
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn endpoint_strips_trailing_slash() {
        let provider = OpenAiProvider::new(
            "key".into(),
            "https://api.openai.com/v1/".into(),
            "gpt-4o".into(),
        );
        assert_eq!(provider.endpoint(), "https://api.openai.com/v1/chat/completions");
    }

    #[test]
    fn flush_tool_calls_orders_by_index() {
        let mut tool_calls: HashMap<u32, ToolCallAccum> = HashMap::new();
        tool_calls.insert(
            1,
            ToolCallAccum {
                id: "c2".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"pwd\"}".into(),
            },
        );
        tool_calls.insert(
            0,
            ToolCallAccum {
                id: "c1".into(),
                name: "bash".into(),
                arguments: "{\"command\":\"ls\"}".into(),
            },
        );
        let mut pending = std::collections::VecDeque::new();
        flush_tool_calls(&mut tool_calls, &mut pending);
        assert_eq!(pending.len(), 2);
        assert!(matches!(&pending[0], ModelEvent::ToolUse { call_id, .. } if call_id == "c1"));
        assert!(matches!(&pending[1], ModelEvent::ToolUse { call_id, .. } if call_id == "c2"));
    }
}
// OpenAI-compatible provider
