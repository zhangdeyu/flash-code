use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::Deserialize;

use crate::protocol::{ContentBlock, Message, Prompt, Role};
use crate::provider::{
    tool_specs_to_openai, Capability, Provider, ProviderError, ProviderEvent, StopReason,
};

pub struct OpenAiProvider {
    api_key: String,
    base_url: String,
    model: String,
    capability: Capability,
    client: reqwest::Client,
}

impl OpenAiProvider {
    #[must_use]
    pub fn new(
        api_key: String,
        base_url: String,
        model: String,
        capability: Capability,
    ) -> Self {
        Self {
            api_key,
            base_url,
            model,
            capability,
            client: reqwest::Client::new(),
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn build_request_body(
        &self,
        prompt: &Prompt,
        streaming: bool,
        max_output_override: Option<usize>,
    ) -> serde_json::Value {
        let messages = prompt_to_openai_messages(prompt);
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": streaming,
        });
        if let Some(max) = max_output_override {
            body["max_tokens"] = serde_json::json!(max);
        }
        if !prompt.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tool_specs_to_openai(&prompt.tools));
        }
        body
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn capability(&self) -> &Capability {
        &self.capability
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    async fn stream(
        &self,
        prompt: &Prompt,
    ) -> Result<BoxStream<'_, Result<ProviderEvent, ProviderError>>, ProviderError> {
        use reqwest_eventsource::EventSource;

        let body = self.build_request_body(prompt, true, None);
        let request = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body);

        let es = EventSource::new(request)
            .map_err(|e| ProviderError::Transient(format!("event source: {e}")))?;

        Ok(Box::pin(sse_to_events(es)))
    }

    async fn complete_once(&self, prompt: &Prompt) -> Result<String, ProviderError> {
        // Force tools = [] per spec §11.5.5.
        let stripped = Prompt {
            system: prompt.system.clone(),
            tools: vec![],
            messages: prompt.messages.clone(),
        };
        let body = self.build_request_body(&stripped, false, None);
        let response = self
            .client
            .post(self.endpoint())
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ProviderError::Transient(e.to_string()))?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(map_http_error(status, &text));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| ProviderError::Transient(e.to_string()))?;

        Ok(body["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned())
    }
}

// ---------- Format conversion ----------

fn flatten_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_role_blocks_to_openai(msg: &Message) -> String {
    flatten_text(&msg.content)
}

fn assistant_to_openai(msg: &Message) -> serde_json::Value {
    let mut text = String::new();
    let mut tool_calls: Vec<serde_json::Value> = Vec::new();
    for b in &msg.content {
        match b {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::ToolUse {
                call_id,
                name,
                input,
            } => {
                tool_calls.push(serde_json::json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": serde_json::to_string(input).unwrap_or_else(|_| "{}".into()),
                    }
                }));
            }
            // Reasoning: input side dropped per §11.5.2
            ContentBlock::Reasoning { .. } => {}
            _ => {}
        }
    }
    let mut obj = serde_json::json!({"role": "assistant", "content": text});
    if !tool_calls.is_empty() {
        obj["tool_calls"] = serde_json::Value::Array(tool_calls);
    }
    obj
}

/// Each tool_result block becomes a separate `role: tool` message.
fn tool_message_to_openai_many(msg: &Message) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for b in &msg.content {
        if let ContentBlock::ToolResult {
            call_id, content, ..
        } = b
        {
            let text = flatten_text(content);
            out.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": text,
            }));
        }
    }
    out
}

#[must_use]
pub fn prompt_to_openai_messages(prompt: &Prompt) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for sys in &prompt.system {
        messages.push(serde_json::json!({
            "role": "system",
            "content": message_role_blocks_to_openai(sys),
        }));
    }
    for msg in &prompt.messages {
        match msg.role {
            Role::System => messages.push(serde_json::json!({
                "role": "system",
                "content": message_role_blocks_to_openai(msg),
            })),
            Role::User => messages.push(serde_json::json!({
                "role": "user",
                "content": message_role_blocks_to_openai(msg),
            })),
            Role::Assistant => messages.push(assistant_to_openai(msg)),
            Role::Tool => messages.extend(tool_message_to_openai_many(msg)),
            Role::Summary => messages.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "<COMPACTION_SUMMARY>\n{}",
                    message_role_blocks_to_openai(msg)
                ),
            })),
        }
    }
    messages
}

// ---------- Error mapping ----------

fn map_http_error(status: reqwest::StatusCode, body: &str) -> ProviderError {
    let lower = body.to_ascii_lowercase();
    if status == reqwest::StatusCode::BAD_REQUEST {
        if lower.contains("context_length_exceeded")
            || lower.contains("maximum context length")
            || lower.contains("context length")
            || lower.contains("too long")
        {
            return ProviderError::ContextOverflow(body.to_owned());
        }
        return ProviderError::InvalidRequest(body.to_owned());
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return ProviderError::Auth(body.to_owned());
    }
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        return ProviderError::RateLimited {
            retry_after: None,
            message: body.to_owned(),
        };
    }
    ProviderError::Transient(format!("status {status}: {body}"))
}

fn map_finish_reason(s: &str) -> StopReason {
    match s {
        "stop" => StopReason::EndTurn,
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::Other,
    }
}

// ---------- SSE stream ----------

#[derive(Deserialize)]
struct ChatChunk {
    #[serde(default)]
    choices: Vec<ChoiceDelta>,
    #[serde(default)]
    error: Option<ErrorPayload>,
}

#[derive(Deserialize)]
struct ErrorPayload {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    message: Option<String>,
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
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
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

struct StreamState {
    es: reqwest_eventsource::EventSource,
    tool_calls: HashMap<u32, ToolCallAccum>,
    pending: VecDeque<Result<ProviderEvent, ProviderError>>,
    done: bool,
    final_stop: Option<StopReason>,
}

fn flush_tool_calls(
    tool_calls: &mut HashMap<u32, ToolCallAccum>,
    pending: &mut VecDeque<Result<ProviderEvent, ProviderError>>,
) {
    let mut indices: Vec<u32> = tool_calls.keys().copied().collect();
    indices.sort_unstable();
    for idx in indices {
        if let Some(accum) = tool_calls.remove(&idx) {
            let input = serde_json::from_str(&accum.arguments)
                .unwrap_or_else(|_| serde_json::json!({}));
            pending.push_back(Ok(ProviderEvent::ToolUseComplete {
                call_id: accum.id,
                name: accum.name,
                input,
            }));
        }
    }
}

fn sse_to_events(
    es: reqwest_eventsource::EventSource,
) -> impl futures::Stream<Item = Result<ProviderEvent, ProviderError>> {
    let state = StreamState {
        es,
        tool_calls: HashMap::new(),
        pending: VecDeque::new(),
        done: false,
        final_stop: None,
    };

    futures::stream::unfold(state, |mut state| async move {
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
                        flush_tool_calls(&mut state.tool_calls, &mut state.pending);
                        let stop = state.final_stop.unwrap_or(StopReason::Other);
                        state.pending.push_back(Ok(ProviderEvent::Done { stop_reason: stop }));
                        return state.pending.pop_front().map(|ev| (ev, state));
                    }
                    let chunk: ChatChunk = match serde_json::from_str(&msg.data) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };
                    if let Some(err) = chunk.error {
                        let m = err.message.unwrap_or_default();
                        let lower = m.to_ascii_lowercase();
                        let mapped = if err.code.as_deref() == Some("context_length_exceeded")
                            || lower.contains("maximum context length")
                            || lower.contains("context length")
                        {
                            ProviderError::ContextOverflow(m)
                        } else {
                            ProviderError::Transient(m)
                        };
                        state.done = true;
                        return Some((Err(mapped), state));
                    }
                    for choice in chunk.choices {
                        if let Some(text) = choice.delta.content {
                            if !text.is_empty() {
                                state.pending.push_back(Ok(ProviderEvent::TextDelta(text)));
                            }
                        }
                        let r = choice
                            .delta
                            .reasoning
                            .or(choice.delta.reasoning_content);
                        if let Some(text) = r {
                            if !text.is_empty() {
                                state.pending.push_back(Ok(ProviderEvent::ReasoningDelta {
                                    text,
                                    signature: None,
                                }));
                            }
                        }
                        if let Some(deltas) = choice.delta.tool_calls {
                            for d in deltas {
                                let idx = d.index;
                                let id_clone = d.id.clone();
                                let name_clone = d
                                    .function
                                    .as_ref()
                                    .and_then(|f| f.name.clone());
                                let args_clone = d
                                    .function
                                    .as_ref()
                                    .and_then(|f| f.arguments.clone());

                                let accum = state.tool_calls.entry(idx).or_default();
                                if let Some(id) = d.id {
                                    if !id.is_empty() {
                                        accum.id = id;
                                    }
                                }
                                if let Some(func) = d.function {
                                    if let Some(name) = func.name {
                                        accum.name.push_str(&name);
                                    }
                                    if let Some(args) = func.arguments {
                                        accum.arguments.push_str(&args);
                                    }
                                }
                                state.pending.push_back(Ok(ProviderEvent::ToolUseDelta {
                                    index: idx,
                                    id: id_clone,
                                    name: name_clone,
                                    args_delta: args_clone,
                                }));
                            }
                        }
                        if let Some(fr) = choice.finish_reason {
                            state.final_stop = Some(map_finish_reason(&fr));
                            flush_tool_calls(&mut state.tool_calls, &mut state.pending);
                        }
                    }
                    if let Some(ev) = state.pending.pop_front() {
                        return Some((ev, state));
                    }
                }
                Some(Err(reqwest_eventsource::Error::StreamEnded)) | None => {
                    state.done = true;
                    flush_tool_calls(&mut state.tool_calls, &mut state.pending);
                    let stop = state.final_stop.unwrap_or(StopReason::Other);
                    state.pending.push_back(Ok(ProviderEvent::Done { stop_reason: stop }));
                    return state.pending.pop_front().map(|ev| (ev, state));
                }
                Some(Err(e)) => {
                    state.done = true;
                    return Some((
                        Err(ProviderError::Transient(format!("stream interrupted: {e}"))),
                        state,
                    ));
                }
            }
        }
    })
}

// allow unused — kept for potential future read of retry-after
#[allow(dead_code)]
fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    value
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ContentBlock, Message, Prompt};

    #[test]
    fn tool_message_expands_per_result() {
        let r1 =
            ContentBlock::tool_result("c1", vec![ContentBlock::text("a")], false).unwrap();
        let r2 =
            ContentBlock::tool_result("c2", vec![ContentBlock::text("b")], true).unwrap();
        let msg = Message::tool_results(vec![r1, r2]);
        let out = tool_message_to_openai_many(&msg);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["tool_call_id"], "c1");
        assert_eq!(out[1]["tool_call_id"], "c2");
    }

    #[test]
    fn assistant_with_tool_use() {
        let msg = Message::assistant(vec![
            ContentBlock::text("hello"),
            ContentBlock::ToolUse {
                call_id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ]);
        let v = assistant_to_openai(&msg);
        assert_eq!(v["role"], "assistant");
        assert!(v["tool_calls"].is_array());
        assert_eq!(v["tool_calls"][0]["id"], "c1");
    }

    #[test]
    fn summary_role_degrades_to_user_with_prefix() {
        let prompt = Prompt {
            system: vec![],
            tools: vec![],
            messages: vec![Message::summary("prior summary text".into())],
        };
        let v = prompt_to_openai_messages(&prompt);
        assert_eq!(v[0]["role"], "user");
        assert!(v[0]["content"]
            .as_str()
            .unwrap()
            .starts_with("<COMPACTION_SUMMARY>"));
    }

    #[test]
    fn error_mapping_context_overflow() {
        let body = r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#;
        let e = map_http_error(reqwest::StatusCode::BAD_REQUEST, body);
        assert!(matches!(e, ProviderError::ContextOverflow(_)));
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason("stop"), StopReason::EndTurn);
        assert_eq!(map_finish_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_finish_reason("length"), StopReason::MaxTokens);
        assert_eq!(map_finish_reason("other"), StopReason::Other);
    }
}
