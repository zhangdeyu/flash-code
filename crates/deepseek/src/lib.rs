use std::collections::BTreeMap;

use async_trait::async_trait;
use flash_core::{ContentBlock, Message, Role};
use flash_provider::{
    ChatProvider, ChatRequest, ProviderError, ProviderEvent, StopReason, ToolCall, ToolSpec, Usage,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct DeepSeekProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl DeepSeekProvider {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    pub fn from_env(base_url: impl Into<String>, api_key_env: &str) -> Result<Self, ProviderError> {
        let api_key = std::env::var(api_key_env).map_err(|_| {
            ProviderError::Authentication(format!("missing DeepSeek API key: set {api_key_env}"))
        })?;
        Ok(Self::new(base_url, api_key))
    }

    async fn post_chat(
        &self,
        body: DeepSeekChatRequest,
    ) -> Result<reqwest::Response, ProviderError> {
        let response = self
            .client
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(map_reqwest_error)?;
        let status = response.status();
        if !status.is_success() {
            let code = status.as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(map_error(code, &body));
        }
        Ok(response)
    }
}

#[async_trait(?Send)]
impl ChatProvider for DeepSeekProvider {
    async fn chat(
        &mut self,
        request: ChatRequest,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), ProviderError> {
        let response = self.post_chat(build_request_body(&request)?).await?;
        let mut buffer = String::new();
        let mut parser = SseParser::default();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(map_reqwest_error)?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(line_end) = buffer.find('\n') {
                let line = buffer[..line_end].trim_end_matches('\r').to_string();
                buffer = buffer[line_end + 1..].to_string();
                parser.push_line(&line, on_event)?;
            }
        }
        if !buffer.is_empty() {
            parser.push_line(buffer.trim_end_matches('\r'), on_event)?;
        }
        Ok(())
    }
}

pub fn parse_sse(input: &str) -> Result<Vec<ProviderEvent>, DeepSeekParseError> {
    let mut events = Vec::new();
    let mut parser = SseParser::default();
    for raw_line in input.lines() {
        parser.push_line(raw_line.trim_end_matches('\r'), &mut |event| {
            events.push(event)
        })?;
    }
    Ok(events)
}

pub fn map_error(status: u16, body: &str) -> ProviderError {
    let message = if body.is_empty() {
        format!("DeepSeek request failed with HTTP {status}")
    } else {
        format!("DeepSeek request failed with HTTP {status}: {body}")
    };
    match status {
        400 | 422 => ProviderError::InvalidRequest(message),
        401 => ProviderError::Authentication(message),
        402 => ProviderError::Billing(message),
        429 => ProviderError::RateLimited(message),
        500..=599 => ProviderError::Server(message),
        _ => ProviderError::Unrecoverable(message),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepSeekParseError {
    Json(String),
    InvalidNumber(String),
}

impl std::fmt::Display for DeepSeekParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Json(message) => write!(formatter, "invalid DeepSeek SSE JSON: {message}"),
            Self::InvalidNumber(value) => write!(formatter, "invalid number `{value}`"),
        }
    }
}

impl std::error::Error for DeepSeekParseError {}

impl From<DeepSeekParseError> for ProviderError {
    fn from(error: DeepSeekParseError) -> Self {
        ProviderError::Unrecoverable(error.to_string())
    }
}

#[derive(Debug, Default)]
struct SseParser {
    tool_calls: BTreeMap<u64, ToolCallBuilder>,
    done_emitted: bool,
}

impl SseParser {
    fn push_line(
        &mut self,
        raw_line: &str,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), DeepSeekParseError> {
        let line = raw_line.trim();
        if line.is_empty() || !line.starts_with("data:") {
            return Ok(());
        }
        let data = line.trim_start_matches("data:").trim();
        if data == "[DONE]" {
            self.emit_done_once(StopReason::EndTurn, on_event);
            return Ok(());
        }
        let chunk: StreamChunk = serde_json::from_str(data)
            .map_err(|error| DeepSeekParseError::Json(error.to_string()))?;
        if let Some(usage) = chunk.usage {
            on_event(ProviderEvent::Usage(Usage {
                input_tokens: usage.prompt_tokens.unwrap_or_default(),
                output_tokens: usage.completion_tokens.unwrap_or_default(),
            }));
        }
        for choice in chunk.choices {
            if let Some(reasoning) = choice.delta.reasoning_content {
                if !reasoning.is_empty() {
                    on_event(ProviderEvent::ReasoningDelta(reasoning));
                }
            }
            if let Some(content) = choice.delta.content {
                if !content.is_empty() {
                    on_event(ProviderEvent::TextDelta(content));
                }
            }
            for tool_call in choice.delta.tool_calls {
                let index = tool_call.index.unwrap_or(0);
                let builder = self.tool_calls.entry(index).or_default();
                if let Some(id) = tool_call.id {
                    builder.call_id = id;
                }
                if let Some(function) = tool_call.function {
                    if let Some(name) = function.name {
                        builder.name = name;
                    }
                    if let Some(arguments) = function.arguments {
                        builder.input.push_str(&arguments);
                    }
                }
            }
            if let Some(reason) = choice.finish_reason {
                match reason.as_str() {
                    "tool_calls" => {
                        self.flush_tool_calls(on_event);
                        self.emit_done_once(StopReason::ToolUse, on_event);
                    }
                    "length" => self.emit_done_once(StopReason::MaxTokens, on_event),
                    "stop" => self.emit_done_once(StopReason::EndTurn, on_event),
                    _ => {}
                }
            }
        }
        Ok(())
    }

    fn flush_tool_calls(&mut self, on_event: &mut dyn FnMut(ProviderEvent)) {
        for (_, builder) in std::mem::take(&mut self.tool_calls) {
            if let Some(call) = builder.build() {
                on_event(ProviderEvent::ToolCallComplete(call));
            }
        }
    }

    fn emit_done_once(&mut self, reason: StopReason, on_event: &mut dyn FnMut(ProviderEvent)) {
        if !self.done_emitted {
            self.done_emitted = true;
            on_event(ProviderEvent::Done(reason));
        }
    }
}

#[derive(Debug, Default)]
struct ToolCallBuilder {
    call_id: String,
    name: String,
    input: String,
}

impl ToolCallBuilder {
    fn build(self) -> Option<ToolCall> {
        if self.name.is_empty() {
            return None;
        }
        Some(ToolCall {
            call_id: if self.call_id.is_empty() {
                "call_deepseek".to_string()
            } else {
                self.call_id
            },
            name: self.name,
            input: self.input,
        })
    }
}

#[derive(Debug, Deserialize)]
struct StreamChunk {
    #[serde(default)]
    choices: Vec<ChoiceChunk>,
    usage: Option<UsageChunk>,
}

#[derive(Debug, Deserialize)]
struct ChoiceChunk {
    #[serde(default)]
    delta: DeltaChunk,
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct DeltaChunk {
    reasoning_content: Option<String>,
    content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ToolCallChunk>,
}

#[derive(Debug, Deserialize)]
struct ToolCallChunk {
    index: Option<u64>,
    id: Option<String>,
    function: Option<ToolCallFunctionChunk>,
}

#[derive(Debug, Deserialize)]
struct ToolCallFunctionChunk {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UsageChunk {
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
}

#[derive(Debug, Serialize)]
struct DeepSeekChatRequest {
    model: String,
    messages: Vec<DeepSeekMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<DeepSeekTool>,
    stream: bool,
}

#[derive(Debug, Serialize)]
struct DeepSeekMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<DeepSeekAssistantToolCall>,
}

#[derive(Debug, Serialize)]
struct DeepSeekAssistantToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: DeepSeekAssistantToolFunction,
}

#[derive(Debug, Serialize)]
struct DeepSeekAssistantToolFunction {
    name: String,
    arguments: String,
}

#[derive(Debug, Serialize)]
struct DeepSeekTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: DeepSeekToolFunction,
}

#[derive(Debug, Serialize)]
struct DeepSeekToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

fn build_request_body(request: &ChatRequest) -> Result<DeepSeekChatRequest, ProviderError> {
    Ok(DeepSeekChatRequest {
        model: request.model.clone(),
        messages: request.messages.iter().map(convert_message).collect(),
        tools: request
            .tools
            .iter()
            .map(convert_tool)
            .collect::<Result<Vec<_>, _>>()?,
        stream: true,
    })
}

fn convert_message(message: &Message) -> DeepSeekMessage {
    let role = match message.role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    };
    let mut content = Vec::new();
    let mut tool_call_id = None;
    let mut tool_calls = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => {
                content.push(text.clone());
            }
            ContentBlock::ToolUse {
                call_id,
                name,
                input,
            } => tool_calls.push(DeepSeekAssistantToolCall {
                id: call_id.clone(),
                kind: "function",
                function: DeepSeekAssistantToolFunction {
                    name: name.clone(),
                    arguments: input.clone(),
                },
            }),
            ContentBlock::ToolResult { call_id, status } => {
                tool_call_id = Some(call_id.clone());
                content.push(format!("tool result status: {}", status.as_str()));
            }
        }
    }
    DeepSeekMessage {
        role,
        content: content.join("\n"),
        tool_call_id,
        tool_calls,
    }
}

fn convert_tool(tool: &ToolSpec) -> Result<DeepSeekTool, ProviderError> {
    let parameters = serde_json::from_str(&tool.parameters).map_err(|error| {
        ProviderError::InvalidRequest(format!(
            "invalid JSON schema for tool `{}`: {error}",
            tool.name
        ))
    })?;
    Ok(DeepSeekTool {
        kind: "function",
        function: DeepSeekToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters,
        },
    })
}

fn map_reqwest_error(error: reqwest::Error) -> ProviderError {
    if error.is_timeout() || error.is_connect() {
        ProviderError::Server(format!("DeepSeek request failed: {error}"))
    } else {
        ProviderError::Unrecoverable(format!("DeepSeek request failed: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flash_core::{ContentBlock, Message, Role};

    #[test]
    fn request_body_should_use_typed_streaming_tool_payload() {
        let request = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: vec![Message {
                id: "msg_1".to_string(),
                role: Role::User,
                created_at: "0".to_string(),
                content: vec![ContentBlock::Text {
                    text: "hello".to_string(),
                }],
            }],
            tools: vec![ToolSpec {
                name: "Read".to_string(),
                description: "read a file".to_string(),
                parameters: r#"{"type":"object","properties":{"path":{"type":"string"}}}"#
                    .to_string(),
            }],
        };

        let body = serde_json::to_value(build_request_body(&request).unwrap()).unwrap();

        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn parse_sse_should_emit_reasoning_text_tool_usage_and_done() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"src/lib.rs\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n",
            "data: [DONE]\n"
        );

        let events = parse_sse(sse).unwrap();

        assert_eq!(
            events,
            vec![
                ProviderEvent::ReasoningDelta("think".to_string()),
                ProviderEvent::TextDelta("hello".to_string()),
                ProviderEvent::Usage(Usage {
                    input_tokens: 7,
                    output_tokens: 3,
                }),
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_1".to_string(),
                    name: "Read".to_string(),
                    input: "{\"path\":\"src/lib.rs\"}".to_string(),
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ]
        );
    }

    #[test]
    fn parse_sse_should_emit_max_tokens_done() {
        let sse = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n";

        let events = parse_sse(sse).unwrap();

        assert_eq!(events, vec![ProviderEvent::Done(StopReason::MaxTokens)]);
    }

    #[test]
    fn map_error_should_mark_429_as_retryable() {
        let error = map_error(429, "rate limited");

        assert!(error.is_retryable());
    }

    #[test]
    fn map_error_should_not_retry_authentication_errors() {
        let error = map_error(401, "bad key");

        assert!(!error.is_retryable());
    }

    #[test]
    fn from_env_should_fail_without_api_key_before_network_access() {
        let missing_env = "FLASH_DEEPSEEK_TEST_MISSING_API_KEY";
        std::env::remove_var(missing_env);

        let error =
            DeepSeekProvider::from_env("https://api.deepseek.com", missing_env).unwrap_err();

        assert!(error.to_string().contains(missing_env));
    }
}
