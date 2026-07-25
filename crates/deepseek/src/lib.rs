use flash_provider::{ProviderError, ProviderEvent, StopReason, ToolCall, Usage};

pub fn parse_sse(input: &str) -> Result<Vec<ProviderEvent>, DeepSeekParseError> {
    let mut events = Vec::new();
    for raw_line in input.lines() {
        let line = raw_line.trim();
        if line.is_empty() || !line.starts_with("data:") {
            continue;
        }
        let data = line.trim_start_matches("data:").trim();
        if data == "[DONE]" {
            events.push(ProviderEvent::Done(StopReason::EndTurn));
            continue;
        }
        if let Some(text) = find_json_string(data, "reasoning_content") {
            events.push(ProviderEvent::ReasoningDelta(text));
        }
        if let Some(text) = find_json_string(data, "content") {
            if !text.is_empty() {
                events.push(ProviderEvent::TextDelta(text));
            }
        }
        if let Some(name) = find_json_string(data, "tool_name") {
            let call_id = find_json_string(data, "tool_call_id")
                .unwrap_or_else(|| "call_deepseek".to_string());
            let input = find_json_string(data, "tool_input").unwrap_or_default();
            events.push(ProviderEvent::ToolCallComplete(ToolCall {
                call_id,
                name,
                input,
            }));
        }
        if let Some(input_tokens) = find_json_number(data, "prompt_tokens") {
            let output_tokens = find_json_number(data, "completion_tokens").unwrap_or_default();
            events.push(ProviderEvent::Usage(Usage {
                input_tokens: input_tokens
                    .parse()
                    .map_err(|_| DeepSeekParseError::InvalidNumber(input_tokens.clone()))?,
                output_tokens: output_tokens.parse().unwrap_or_default(),
            }));
        }
        if data.contains("\"finish_reason\":\"tool_calls\"") {
            events.push(ProviderEvent::Done(StopReason::ToolUse));
        }
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
        500 | 503 => ProviderError::Server(message),
        _ => ProviderError::Unrecoverable(message),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeepSeekParseError {
    InvalidNumber(String),
}

impl std::fmt::Display for DeepSeekParseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidNumber(value) => write!(formatter, "invalid number `{value}`"),
        }
    }
}

impl std::error::Error for DeepSeekParseError {}

fn find_json_string(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    let end = rest.find('"')?;
    Some(rest[..end].replace("\\\"", "\""))
}

fn find_json_number(content: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let start = content.find(&needle)? + needle.len();
    let rest = &content[start..];
    let end = rest
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sse_should_emit_reasoning_text_tool_usage_and_done() {
        let sse = concat!(
            "data: {\"reasoning_content\":\"think\"}\n",
            "data: {\"content\":\"hello\"}\n",
            "data: {\"tool_call_id\":\"call_1\",\"tool_name\":\"search\",\"tool_input\":\".\"}\n",
            "data: {\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3}}\n",
            "data: [DONE]\n"
        );

        let events = parse_sse(sse).unwrap();

        assert_eq!(events.len(), 5);
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
}
