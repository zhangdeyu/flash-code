use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

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
    options: DeepSeekOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeepSeekOptions {
    pub connect_timeout: Duration,
    pub first_byte_timeout: Duration,
    pub stream_idle_timeout: Duration,
    pub max_error_body_bytes: usize,
    pub max_sse_frame_bytes: usize,
    pub max_tool_arguments_bytes: usize,
}

impl Default for DeepSeekOptions {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            first_byte_timeout: Duration::from_secs(30),
            stream_idle_timeout: Duration::from_secs(30),
            max_error_body_bytes: 64 * 1024,
            max_sse_frame_bytes: 1024 * 1024,
            max_tool_arguments_bytes: 1024 * 1024,
        }
    }
}

impl DeepSeekProvider {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self::with_options(base_url, api_key, DeepSeekOptions::default())
            .expect("default DeepSeek HTTP client options must be valid")
    }

    pub fn with_options(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        options: DeepSeekOptions,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .connect_timeout(options.connect_timeout)
            .build()
            .map_err(|error| {
                ProviderError::InvalidRequest(format!(
                    "failed to build DeepSeek HTTP client: {error}"
                ))
            })?;
        Ok(Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            options,
        })
    }

    pub fn from_env(base_url: impl Into<String>, api_key_env: &str) -> Result<Self, ProviderError> {
        let api_key = std::env::var(api_key_env).map_err(|_| {
            ProviderError::Authentication(format!("missing DeepSeek API key: set {api_key_env}"))
        })?;
        Ok(Self::new(base_url, api_key))
    }

    pub fn from_env_with_options(
        base_url: impl Into<String>,
        api_key_env: &str,
        options: DeepSeekOptions,
    ) -> Result<Self, ProviderError> {
        let api_key = std::env::var(api_key_env).map_err(|_| {
            ProviderError::Authentication(format!("missing DeepSeek API key: set {api_key_env}"))
        })?;
        Self::with_options(base_url, api_key, options)
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
            let retry_after = parse_retry_after(response.headers());
            let body = read_limited_error_body(
                response,
                self.options.max_error_body_bytes,
                self.options.stream_idle_timeout,
            )
            .await?;
            return Err(map_error_with_retry_after(code, &body, retry_after));
        }
        Ok(response)
    }
}

#[async_trait]
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
            result = tokio::time::timeout(self.options.first_byte_timeout, self.post_chat(body)) => {
                result.map_err(|_| ProviderError::Timeout(
                    "DeepSeek timed out waiting for response headers".to_string()
                ))??
            },
        };
        let mut decoder = SseDecoder::new(
            self.options.max_sse_frame_bytes,
            self.options.max_tool_arguments_bytes,
        );
        let mut stream = response.bytes_stream();
        let mut first_chunk = true;
        loop {
            let timeout = if first_chunk {
                self.options.first_byte_timeout
            } else {
                self.options.stream_idle_timeout
            };
            let chunk = tokio::select! {
                () = request.cancellation.cancelled() => {
                    return Err(ProviderError::Cancelled("DeepSeek stream cancelled".to_string()));
                }
                chunk = tokio::time::timeout(timeout, stream.next()) => {
                    chunk.map_err(|_| {
                        let phase = if first_chunk { "first byte" } else { "stream data" };
                        ProviderError::Timeout(format!("DeepSeek timed out waiting for {phase}"))
                    })?
                },
            };
            let Some(chunk) = chunk else {
                break;
            };
            first_chunk = false;
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
    map_error_with_retry_after(status, body, None)
}

fn map_error_with_retry_after(
    status: u16,
    body: &str,
    retry_after: Option<Duration>,
) -> ProviderError {
    let message = if body.is_empty() {
        format!("DeepSeek request failed with HTTP {status}")
    } else {
        format!("DeepSeek request failed with HTTP {status}: {body}")
    };
    match status {
        400 | 422 => ProviderError::InvalidRequest(message),
        401 => ProviderError::Authentication(message),
        402 => ProviderError::Billing(message),
        429 => ProviderError::RateLimited {
            message,
            retry_after,
        },
        500..=599 => ProviderError::Server {
            message,
            retry_after,
        },
        _ => ProviderError::Unrecoverable(message),
    }
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = httpdate::parse_http_date(value).ok()?;
    retry_at.duration_since(SystemTime::now()).ok()
}

async fn read_limited_error_body(
    response: reqwest::Response,
    max_bytes: usize,
    idle_timeout: Duration,
) -> Result<String, ProviderError> {
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    loop {
        let chunk = tokio::time::timeout(idle_timeout, stream.next())
            .await
            .map_err(|_| {
                ProviderError::Timeout(
                    "DeepSeek timed out while reading HTTP error response".to_string(),
                )
            })?;
        let Some(chunk) = chunk else {
            break;
        };
        let chunk = chunk.map_err(map_reqwest_error)?;
        if body.len().saturating_add(chunk.len()) > max_bytes {
            return Err(ProviderError::ResponseTooLarge(format!(
                "DeepSeek HTTP error body exceeded {max_bytes} bytes"
            )));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
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
    FrameTooLarge(usize),
    ToolArgumentsTooLarge(usize),
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
            Self::FrameTooLarge(limit) => {
                write!(formatter, "DeepSeek SSE frame exceeded {limit} bytes")
            }
            Self::ToolArgumentsTooLarge(limit) => {
                write!(formatter, "DeepSeek tool arguments exceeded {limit} bytes")
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

#[derive(Debug)]
struct SseDecoder {
    buffer: Vec<u8>,
    data_lines: Vec<String>,
    data_bytes: usize,
    max_frame_bytes: usize,
    parser: SseParser,
}

impl Default for SseDecoder {
    fn default() -> Self {
        let options = DeepSeekOptions::default();
        Self::new(
            options.max_sse_frame_bytes,
            options.max_tool_arguments_bytes,
        )
    }
}

impl SseDecoder {
    fn new(max_frame_bytes: usize, max_tool_arguments_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            data_lines: Vec::new(),
            data_bytes: 0,
            max_frame_bytes,
            parser: SseParser::new(max_tool_arguments_bytes),
        }
    }

    fn push_bytes(
        &mut self,
        bytes: &[u8],
        on_event: &mut dyn FnMut(ProviderEvent),
    ) -> Result<(), DeepSeekParseError> {
        for byte in bytes {
            if *byte == b'\n' {
                let mut line = std::mem::take(&mut self.buffer);
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                self.push_line(&line, on_event)?;
            } else {
                if self.buffer.len() >= self.max_frame_bytes {
                    return Err(DeepSeekParseError::FrameTooLarge(self.max_frame_bytes));
                }
                self.buffer.push(*byte);
            }
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
        let value = value.strip_prefix(' ').unwrap_or(value);
        let separator = usize::from(!self.data_lines.is_empty());
        if self
            .data_bytes
            .saturating_add(separator)
            .saturating_add(value.len())
            > self.max_frame_bytes
        {
            return Err(DeepSeekParseError::FrameTooLarge(self.max_frame_bytes));
        }
        self.data_bytes += separator + value.len();
        self.data_lines.push(value.to_string());

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
        self.data_bytes = 0;
        let data = std::mem::take(&mut self.data_lines).join("\n");
        self.parser.push_data(&data, on_event)
    }
}

#[derive(Debug)]
struct SseParser {
    tool_calls: BTreeMap<u64, ToolCallBuilder>,
    done_emitted: bool,
    max_tool_arguments_bytes: usize,
    tool_arguments_bytes: usize,
}

impl SseParser {
    fn new(max_tool_arguments_bytes: usize) -> Self {
        Self {
            tool_calls: BTreeMap::new(),
            done_emitted: false,
            max_tool_arguments_bytes,
            tool_arguments_bytes: 0,
        }
    }

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
                        if self.tool_arguments_bytes.saturating_add(arguments.len())
                            > self.max_tool_arguments_bytes
                        {
                            return Err(DeepSeekParseError::ToolArgumentsTooLarge(
                                self.max_tool_arguments_bytes,
                            ));
                        }
                        self.tool_arguments_bytes += arguments.len();
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
        self.tool_arguments_bytes = 0;
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
    if error.is_timeout() {
        ProviderError::Timeout(format!("DeepSeek request timed out: {error}"))
    } else if error.is_connect() {
        ProviderError::Server {
            message: format!("DeepSeek connection failed: {error}"),
            retry_after: None,
        }
    } else {
        ProviderError::Unrecoverable(format!("DeepSeek request failed: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flash_core::{ContentBlock, Message, Role};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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
        assert_eq!(error.retry_after(), None);
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

    #[tokio::test]
    async fn local_http_stream_should_emit_real_reqwest_events() {
        let (base_url, server) = serve_http(vec![
            (
                Duration::ZERO,
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            (
                Duration::ZERO,
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n".to_vec(),
            ),
            (
                Duration::from_millis(10),
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\ndata: [DONE]\n"
                    .to_vec(),
            ),
        ])
        .await;
        let mut provider = DeepSeekProvider::with_options(
            base_url,
            "test-key",
            test_options(Duration::from_secs(1)),
        )
        .unwrap();

        let events = chat_events(&mut provider).await.unwrap();
        server.await.unwrap();

        assert_eq!(
            events,
            vec![
                ProviderEvent::TextDelta("hello".to_string()),
                ProviderEvent::Done(StopReason::EndTurn),
            ]
        );
    }

    #[tokio::test]
    async fn first_byte_timeout_should_fail_within_bound() {
        let (base_url, server) = serve_http(vec![(
            Duration::from_millis(150),
            b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec(),
        )])
        .await;
        let mut provider = DeepSeekProvider::with_options(
            base_url,
            "test-key",
            test_options(Duration::from_millis(30)),
        )
        .unwrap();
        let started = tokio::time::Instant::now();

        let error = chat_events(&mut provider).await.unwrap_err();
        let elapsed = started.elapsed();
        server.await.unwrap();

        assert!(matches!(error, ProviderError::Timeout(_)), "{error}");
        assert!(elapsed < Duration::from_millis(140));
    }

    #[tokio::test]
    async fn connect_timeout_should_bound_unreachable_endpoint() {
        let mut options = test_options(Duration::from_millis(30));
        options.first_byte_timeout = Duration::from_millis(200);
        let mut provider =
            DeepSeekProvider::with_options("http://192.0.2.1:81", "test-key", options).unwrap();
        provider.client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_millis(30))
            .build()
            .unwrap();
        let started = tokio::time::Instant::now();

        let error = tokio::time::timeout(Duration::from_millis(500), chat_events(&mut provider))
            .await
            .expect("connect attempt exceeded outer safety bound")
            .unwrap_err();

        assert!(
            matches!(
                error,
                ProviderError::Timeout(_) | ProviderError::Server { .. }
            ),
            "{error}"
        );
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn idle_timeout_should_fail_after_publishing_first_delta() {
        let (base_url, server) = serve_http(vec![
            (
                Duration::ZERO,
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n"
                    .to_vec(),
            ),
            (
                Duration::ZERO,
                b"data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n".to_vec(),
            ),
            (
                Duration::from_millis(150),
                b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n".to_vec(),
            ),
        ])
        .await;
        let mut options = test_options(Duration::from_secs(1));
        options.stream_idle_timeout = Duration::from_millis(30);
        let mut provider = DeepSeekProvider::with_options(base_url, "test-key", options).unwrap();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(8);

        let error = provider
            .chat(test_chat_request(), sender)
            .await
            .unwrap_err();
        let first = receiver.recv().await.unwrap();
        server.await.unwrap();

        assert_eq!(first, ProviderEvent::TextDelta("partial".to_string()));
        assert!(matches!(error, ProviderError::Timeout(_)), "{error}");
    }

    #[tokio::test]
    async fn retry_after_should_be_parsed_from_http_response() {
        let (base_url, server) = serve_http(vec![(
            Duration::ZERO,
            b"HTTP/1.1 429 Too Many Requests\r\nRetry-After: 2\r\nContent-Length: 4\r\nConnection: close\r\n\r\nslow"
                .to_vec(),
        )])
        .await;
        let mut provider = DeepSeekProvider::with_options(
            base_url,
            "test-key",
            test_options(Duration::from_secs(1)),
        )
        .unwrap();

        let error = chat_events(&mut provider).await.unwrap_err();
        server.await.unwrap();

        assert!(matches!(error, ProviderError::RateLimited { .. }));
        assert_eq!(error.retry_after(), Some(Duration::from_secs(2)));
    }

    #[tokio::test]
    async fn oversized_http_error_body_should_be_rejected() {
        let (base_url, server) = serve_http(vec![(
            Duration::ZERO,
            b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 16\r\nConnection: close\r\n\r\n0123456789abcdef"
                .to_vec(),
        )])
        .await;
        let mut options = test_options(Duration::from_secs(1));
        options.max_error_body_bytes = 8;
        let mut provider = DeepSeekProvider::with_options(base_url, "test-key", options).unwrap();

        let error = chat_events(&mut provider).await.unwrap_err();
        server.await.unwrap();

        assert!(matches!(error, ProviderError::ResponseTooLarge(_)));
    }

    #[test]
    fn decoder_should_bound_sse_frames_and_tool_arguments() {
        let mut frame_decoder = SseDecoder::new(16, 128);
        let frame_error = frame_decoder
            .push_bytes(b"data: 01234567890123456", &mut |_| {})
            .unwrap_err();

        let mut tool_decoder = SseDecoder::new(1024, 4);
        let tool_error = tool_decoder
            .push_bytes(
                b"data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"Read\",\"arguments\":\"123\"}},{\"index\":1,\"function\":{\"name\":\"Read\",\"arguments\":\"456\"}}]}}]}\n",
                &mut |_| {},
            )
            .unwrap_err();

        assert_eq!(frame_error, DeepSeekParseError::FrameTooLarge(16));
        assert_eq!(tool_error, DeepSeekParseError::ToolArgumentsTooLarge(4));
    }

    async fn chat_events(
        provider: &mut DeepSeekProvider,
    ) -> Result<Vec<ProviderEvent>, ProviderError> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        provider.chat(test_chat_request(), sender).await?;
        let mut events = Vec::new();
        while let Some(event) = receiver.recv().await {
            events.push(event);
        }
        Ok(events)
    }

    fn test_chat_request() -> ChatRequest {
        ChatRequest {
            model: "deepseek-chat".to_string(),
            messages: Vec::new(),
            tools: Vec::new(),
            cancellation: flash_core::CancellationToken::new(),
        }
    }

    fn test_options(timeout: Duration) -> DeepSeekOptions {
        DeepSeekOptions {
            connect_timeout: timeout,
            first_byte_timeout: timeout,
            stream_idle_timeout: timeout,
            max_error_body_bytes: 1024,
            max_sse_frame_bytes: 1024,
            max_tool_arguments_bytes: 1024,
        }
    }

    async fn serve_http(parts: Vec<(Duration, Vec<u8>)>) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            let _read = socket.read(&mut request).await.unwrap();
            for (delay, bytes) in parts {
                tokio::time::sleep(delay).await;
                if socket.write_all(&bytes).await.is_err() {
                    break;
                }
                let _result = socket.flush().await;
            }
        });
        (format!("http://{address}"), server)
    }
}
