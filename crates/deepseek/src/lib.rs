use std::collections::BTreeMap;

use async_trait::async_trait;
use flash_core::{ContentBlock, Message, Role};
use flash_provider::{
    send_event, ChatProvider, ChatRequest, ProviderError, ProviderEvent, StopReason, ToolCall,
    ToolSpec, Usage,
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
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        let body = build_request_body(&request)?;
        let response = tokio::select! {
            () = request.cancellation.cancelled() => {
                return Err(ProviderError::Cancelled("DeepSeek request cancelled".to_string()));
            }
            result = self.post_chat(body) => result?,
        };
        let mut decoder = SseDecoder::default();
        let mut stream = response.bytes_stream();
        loop {
            let chunk = tokio::select! {
                () = request.cancellation.cancelled() => {
                    return Err(ProviderError::Cancelled("DeepSeek stream cancelled".to_string()));
                }
                chunk = stream.next() => chunk,
            };
            let Some(chunk) = chunk else {
                break;
            };
            let chunk = chunk.map_err(map_reqwest_error)?;
            let mut decoded = Vec::new();
            decoder.push_bytes(&chunk, &mut |event| decoded.push(event))?;
            for event in decoded {
                send_event(&events, event).await?;
            }
        }
        let mut decoded = Vec::new();
        decoder.finish(&mut |event| decoded.push(event))?;
        for event in decoded {
            send_event(&events, event).await?;
        }
        Ok(())
    }
}

pub fn parse_sse(input: &str) -> Result<Vec<ProviderEvent>, DeepSeekParseError> {
    parse_sse_chunks(&[input.as_bytes()])
}

pub fn parse_sse_chunks(chunks: &[&[u8]]) -> Result<Vec<ProviderEvent>, DeepSeekParseError> {
    let mut events = Vec::new();
    let mut decoder = SseDecoder::default();
    for chunk in chunks {
        decoder.push_bytes(chunk, &mut |event| events.push(event))?;
    }
    decoder.finish(&mut |event| events.push(event))?;
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
    Utf8(String),
    Json(String),
    ToolArguments(String),
    InvalidNumber(String),
    MissingFinishReason,
    PendingToolCalls,
    UnknownFinishReason(String),
}

impl std::fmt::Display for DeepSeekParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Utf8(message) => write!(formatter, "invalid UTF-8 in DeepSeek SSE: {message}"),
            Self::Json(message) => write!(formatter, "invalid DeepSeek SSE JSON: {message}"),
            Self::ToolArguments(message) => {
                write!(formatter, "invalid DeepSeek tool arguments JSON: {message}")
            }
            Self::InvalidNumber(value) => write!(formatter, "invalid number `{value}`"),
            Self::MissingFinishReason => {
                write!(
                    formatter,
                    "DeepSeek SSE ended without an explicit finish reason"
                )
            }
            Self::PendingToolCalls => {
                write!(formatter, "DeepSeek SSE ended with incomplete tool calls")
            }
            Self::UnknownFinishReason(reason) => {
                write!(formatter, "unknown DeepSeek finish reason `{reason}`")
            }
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
struct SseDecoder {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    parser: SseParser,
}

impl SseDecoder {
    fn push_bytes(
        &mut self,
        bytes: &[u8],
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), DeepSeekParseError> {
        self.buffer.extend_from_slice(bytes);
        while let Some(line_end) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line = self.buffer.drain(..=line_end).collect::<Vec<_>>();
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.push_line(&line, on_event)?;
        }
        Ok(())
    }

    fn finish(
        &mut self,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), DeepSeekParseError> {
        if !self.buffer.is_empty() {
            let mut line = std::mem::take(&mut self.buffer);
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            self.push_line(&line, on_event)?;
        }
        self.flush_data(on_event)?;
        self.parser.finish()
    }

    fn push_line(
        &mut self,
        bytes: &[u8],
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), DeepSeekParseError> {
        let line = std::str::from_utf8(bytes)
            .map_err(|error| DeepSeekParseError::Utf8(error.to_string()))?;
        if line.is_empty() {
            return self.flush_data(on_event);
        }
        if line.starts_with(':') {
            return Ok(());
        }
        let Some(value) = line.strip_prefix("data:") else {
            return Ok(());
        };
        self.data_lines
            .push(value.strip_prefix(' ').unwrap_or(value).to_string());

        let data = self.data_lines.join("\n");
        if data == "[DONE]" || serde_json::from_str::<serde_json::Value>(&data).is_ok() {
            self.flush_data(on_event)?;
        }
        Ok(())
    }

    fn flush_data(
        &mut self,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), DeepSeekParseError> {
        if self.data_lines.is_empty() {
            return Ok(());
        }
        let data = std::mem::take(&mut self.data_lines).join("\n");
        self.parser.push_data(&data, on_event)
    }
}

#[derive(Debug, Default)]
struct SseParser {
    tool_calls: BTreeMap<u64, ToolCallBuilder>,
    done_emitted: bool,
}

impl SseParser {
    fn push_data(
        &mut self,
        data: &str,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), DeepSeekParseError> {
        if data == "[DONE]" {
            return self.finish();
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
                        self.flush_tool_calls(on_event)?;
                        self.emit_done_once(StopReason::ToolUse, on_event);
                    }
                    "length" => self.emit_done_once(StopReason::MaxTokens, on_event),
                    "stop" => self.emit_done_once(StopReason::EndTurn, on_event),
                    _ => return Err(DeepSeekParseError::UnknownFinishReason(reason)),
                }
            }
        }
        Ok(())
    }

    fn finish(&self) -> Result<(), DeepSeekParseError> {
        if !self.tool_calls.is_empty() {
            return Err(DeepSeekParseError::PendingToolCalls);
        }
        if !self.done_emitted {
            return Err(DeepSeekParseError::MissingFinishReason);
        }
        Ok(())
    }

    fn flush_tool_calls(
        &mut self,
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), DeepSeekParseError> {
        for (_, builder) in std::mem::take(&mut self.tool_calls) {
            if let Some(call) = builder.build()? {
                on_event(ProviderEvent::ToolCallComplete(call));
            }
        }
        Ok(())
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
    fn build(self) -> Result<Option<ToolCall>, DeepSeekParseError> {
        if self.name.is_empty() {
            return Ok(None);
        }
        let input = serde_json::from_str(&self.input)
            .map_err(|error| DeepSeekParseError::ToolArguments(error.to_string()))?;
        Ok(Some(ToolCall {
            call_id: if self.call_id.is_empty() {
                "call_deepseek".to_string()
            } else {
                self.call_id
            },
            name: self.name,
            input,
        }))
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
    reasoning_content: Option<String>,
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
    let mut reasoning = Vec::new();
    let mut tool_call_id = None;
    let mut tool_calls = Vec::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text } => content.push(text.clone()),
            ContentBlock::Reasoning { text } => reasoning.push(text.clone()),
            ContentBlock::ToolUse {
                call_id,
                name,
                input,
            } => tool_calls.push(DeepSeekAssistantToolCall {
                id: call_id.clone(),
                kind: "function",
                function: DeepSeekAssistantToolFunction {
                    name: name.clone(),
                    arguments: input.to_string(),
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
        reasoning_content: (!reasoning.is_empty()).then(|| reasoning.join("\n")),
        tool_call_id,
        tool_calls,
    }
}

fn convert_tool(tool: &ToolSpec) -> Result<DeepSeekTool, ProviderError> {
    Ok(DeepSeekTool {
        kind: "function",
        function: DeepSeekToolFunction {
            name: tool.name.clone(),
            description: tool.description.clone(),
            parameters: tool.parameters.clone(),
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
                parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }],
            cancellation: flash_core::CancellationToken::new(),
        };

        let body = serde_json::to_value(build_request_body(&request).unwrap()).unwrap();

        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn request_body_should_advertise_exactly_seven_canonical_builtin_tools() {
        let registry = flash_tools::builtin_registry().unwrap();
        let request = ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: Vec::new(),
            tools: registry
                .descriptors()
                .map(|descriptor| ToolSpec {
                    name: descriptor.name,
                    description: descriptor.description,
                    parameters: descriptor.parameters,
                })
                .collect(),
            cancellation: flash_core::CancellationToken::new(),
        };

        let body = serde_json::to_value(build_request_body(&request).unwrap()).unwrap();
        let names = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["function"]["name"].as_str().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec!["Bash", "Edit", "Glob", "Grep", "ListFiles", "Read", "Write"]
        );
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
                    input: serde_json::json!({"path": "src/lib.rs"}),
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
    fn parse_sse_chunks_should_preserve_utf8_split_inside_code_point() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"中文🙂\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n",
            "data: [DONE]\n"
        );
        let split = sse
            .as_bytes()
            .iter()
            .position(|byte| *byte >= 0x80)
            .unwrap()
            + 1;

        let events =
            parse_sse_chunks(&[&sse.as_bytes()[..split], &sse.as_bytes()[split..]]).unwrap();

        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("中文🙂".to_string()),
                ProviderEvent::Done(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn parse_sse_chunks_should_support_multiline_data_crlf_comments_and_no_final_newline() {
        let sse = concat!(
            ": heartbeat\r\n",
            "\r\n",
            "data: {\"choices\":[\r\n",
            "data: {\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}\r\n",
            "data: ]}\r\n",
            "\r\n",
            "data: [DONE]"
        );

        let events = parse_sse_chunks(&[&sse.as_bytes()[..37], &sse.as_bytes()[37..]]).unwrap();

        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("hello".to_string()),
                ProviderEvent::Done(StopReason::EndTurn),
            ]
        );
    }

    #[test]
    fn parse_sse_should_reject_done_with_pending_tool_call() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}]}}]}\n",
            "data: [DONE]\n"
        );

        let error = parse_sse(sse).unwrap_err();

        assert_eq!(error, DeepSeekParseError::PendingToolCalls);
    }

    #[test]
    fn parse_sse_should_reject_done_without_finish_reason() {
        let error = parse_sse("data: [DONE]\n").unwrap_err();

        assert_eq!(error, DeepSeekParseError::MissingFinishReason);
    }

    #[test]
    fn parse_sse_should_reject_unknown_finish_reason() {
        let sse = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"mystery\"}]}\n";

        let error = parse_sse(sse).unwrap_err();

        assert_eq!(
            error,
            DeepSeekParseError::UnknownFinishReason("mystery".to_string())
        );
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
