use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::Deserialize;

use crate::protocol::{ContentBlock, Message, Prompt, Role};
use crate::provider::{
    tool_specs_to_deepseek, Capability, Provider, ProviderError, ProviderEvent, StopReason, Usage,
};

pub struct DeepSeekProvider {
    api_key: String,
    base_url: String,
    model: String,
    reasoning_effort: String,
    capability: Capability,
    client: reqwest::Client,
}

impl DeepSeekProvider {
    #[must_use]
    pub fn new(
        api_key: String,
        base_url: String,
        model: String,
        reasoning_effort: String,
        capability: Capability,
    ) -> Self {
        Self {
            api_key,
            base_url,
            model,
            reasoning_effort,
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
        let messages = prompt_to_deepseek_messages(prompt);
        // NOTE: DeepSeek thinking mode silently ignores temperature / top_p / presence_penalty /
        // frequency_penalty. Do NOT add them — they look effective but do nothing, which is the
        // worst kind of footgun. If sampling control is ever needed, disable thinking first.
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": streaming,
            "thinking": { "type": "enabled" },
            "reasoning_effort": self.reasoning_effort,
        });
        if streaming {
            body["stream_options"] = serde_json::json!({ "include_usage": true });
        }
        if let Some(max) = max_output_override {
            body["max_tokens"] = serde_json::json!(max);
        }
        if !prompt.tools.is_empty() {
            body["tools"] = serde_json::Value::Array(tool_specs_to_deepseek(&prompt.tools));
        }
        body
    }
}

#[async_trait]
impl Provider for DeepSeekProvider {
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

        let message = &body["choices"][0]["message"];
        let content = message["content"].as_str().unwrap_or_default();
        if !content.is_empty() {
            return Ok(content.to_owned());
        }
        // Thinking mode may leave content empty when answer fits entirely in reasoning_content
        // (compaction prompts are short and tool-free, so this fallback keeps summaries non-empty).
        Ok(message["reasoning_content"]
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

fn flatten_reasoning(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Reasoning { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn message_role_blocks_to_deepseek(msg: &Message) -> String {
    flatten_text(&msg.content)
}

fn assistant_to_deepseek(msg: &Message) -> serde_json::Value {
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
            _ => {}
        }
    }
    let mut obj = serde_json::json!({"role": "assistant", "content": text});
    // DeepSeek thinking-mode contract:
    // - tool_call 轮次:必须回传 reasoning_content,否则 400
    // - 非 tool_call 轮次:服务端忽略 reasoning_content
    // 因此只在有 tool_calls 时回传,既满足硬约束又避免浪费上下文 token。
    let has_tool_calls = !tool_calls.is_empty();
    if has_tool_calls {
        let reasoning = flatten_reasoning(&msg.content);
        obj["reasoning_content"] = serde_json::Value::String(reasoning);
        obj["tool_calls"] = serde_json::Value::Array(tool_calls);
    }
    obj
}

/// Each tool_result block becomes a separate `role: tool` message.
fn tool_message_to_deepseek_many(msg: &Message) -> Vec<serde_json::Value> {
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
pub fn prompt_to_deepseek_messages(prompt: &Prompt) -> Vec<serde_json::Value> {
    let mut messages: Vec<serde_json::Value> = Vec::new();
    for sys in &prompt.system {
        messages.push(serde_json::json!({
            "role": "system",
            "content": message_role_blocks_to_deepseek(sys),
        }));
    }
    for msg in &prompt.messages {
        match msg.role {
            Role::System => messages.push(serde_json::json!({
                "role": "system",
                "content": message_role_blocks_to_deepseek(msg),
            })),
            Role::User => messages.push(serde_json::json!({
                "role": "user",
                "content": message_role_blocks_to_deepseek(msg),
            })),
            Role::Assistant => messages.push(assistant_to_deepseek(msg)),
            Role::Tool => messages.extend(tool_message_to_deepseek_many(msg)),
            Role::Summary => messages.push(serde_json::json!({
                "role": "user",
                "content": format!(
                    "<COMPACTION_SUMMARY>\n{}",
                    message_role_blocks_to_deepseek(msg)
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
    #[serde(default)]
    usage: Option<UsagePayload>,
}

#[derive(Deserialize, Default)]
struct UsagePayload {
    #[serde(default)]
    prompt_tokens: u64,
    #[serde(default)]
    completion_tokens: u64,
    #[serde(default)]
    total_tokens: u64,
    #[serde(default)]
    completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Deserialize, Default)]
struct CompletionTokensDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

impl From<UsagePayload> for Usage {
    fn from(p: UsagePayload) -> Self {
        Self {
            prompt_tokens: p.prompt_tokens,
            completion_tokens: p.completion_tokens,
            total_tokens: p.total_tokens,
            reasoning_tokens: p
                .completion_tokens_details
                .and_then(|d| d.reasoning_tokens),
        }
    }
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
                    if let Some(u) = chunk.usage {
                        state
                            .pending
                            .push_back(Ok(ProviderEvent::Usage(u.into())));
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
        let out = tool_message_to_deepseek_many(&msg);
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
        let v = assistant_to_deepseek(&msg);
        assert_eq!(v["role"], "assistant");
        assert!(v["tool_calls"].is_array());
        assert_eq!(v["tool_calls"][0]["id"], "c1");
    }

    #[test]
    fn assistant_with_tool_call_but_no_reasoning_still_includes_field() {
        // Hard contract: DeepSeek returns 400 if a tool_call assistant turn omits
        // reasoning_content on replay. Even if upstream streamed no reasoning, we must
        // send the key explicitly.
        let msg = Message::assistant(vec![ContentBlock::ToolUse {
            call_id: "c1".into(),
            name: "bash".into(),
            input: serde_json::json!({"command": "ls"}),
        }]);
        let v = assistant_to_deepseek(&msg);
        assert!(
            v.get("reasoning_content").is_some(),
            "tool_call assistant must include reasoning_content key"
        );
        assert!(v["tool_calls"].is_array());
    }

    #[test]
    fn assistant_without_tool_call_drops_reasoning() {
        // DeepSeek ignores reasoning_content on non-tool turns. Don't waste context tokens.
        let msg = Message::assistant(vec![
            ContentBlock::Reasoning {
                text: "internal thought".into(),
                signature: None,
            },
            ContentBlock::text("final answer"),
        ]);
        let v = assistant_to_deepseek(&msg);
        assert!(
            v.get("reasoning_content").is_none(),
            "non-tool assistant must NOT include reasoning_content"
        );
        assert_eq!(v["content"], "final answer");
    }

    #[test]
    fn assistant_with_reasoning_replays_reasoning_content() {
        let msg = Message::assistant(vec![
            ContentBlock::Reasoning {
                text: "thinking step".into(),
                signature: None,
            },
            ContentBlock::text("final answer"),
            ContentBlock::ToolUse {
                call_id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ]);
        let v = assistant_to_deepseek(&msg);
        assert_eq!(v["reasoning_content"], "thinking step");
        assert_eq!(v["content"], "final answer");
        assert!(v["tool_calls"].is_array());
    }

    #[test]
    fn summary_role_degrades_to_user_with_prefix() {
        let prompt = Prompt {
            system: vec![],
            tools: vec![],
            messages: vec![Message::summary("prior summary text".into())],
        };
        let v = prompt_to_deepseek_messages(&prompt);
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
    fn usage_payload_parses_with_reasoning_tokens() {
        let body = r#"{
            "choices": [],
            "usage": {
                "prompt_tokens": 42,
                "completion_tokens": 17,
                "total_tokens": 59,
                "completion_tokens_details": { "reasoning_tokens": 11 }
            }
        }"#;
        let chunk: ChatChunk = serde_json::from_str(body).expect("parse");
        let usage: Usage = chunk.usage.expect("usage present").into();
        assert_eq!(usage.prompt_tokens, 42);
        assert_eq!(usage.completion_tokens, 17);
        assert_eq!(usage.total_tokens, 59);
        assert_eq!(usage.reasoning_tokens, Some(11));
    }

    #[test]
    fn usage_payload_parses_without_reasoning_details() {
        let body = r#"{
            "choices": [],
            "usage": {
                "prompt_tokens": 5,
                "completion_tokens": 3,
                "total_tokens": 8
            }
        }"#;
        let chunk: ChatChunk = serde_json::from_str(body).expect("parse");
        let usage: Usage = chunk.usage.expect("usage present").into();
        assert_eq!(usage.reasoning_tokens, None);
    }

    #[test]
    fn finish_reason_mapping() {
        assert_eq!(map_finish_reason("stop"), StopReason::EndTurn);
        assert_eq!(map_finish_reason("tool_calls"), StopReason::ToolUse);
        assert_eq!(map_finish_reason("length"), StopReason::MaxTokens);
        assert_eq!(map_finish_reason("other"), StopReason::Other);
    }
}
