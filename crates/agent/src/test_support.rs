//! Shared test fixtures (providers, tools, helpers) for the flash-agent crate.
//! Compiled only under `cfg(test)`; never part of the production runtime.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use flash_core::{
    ContentBlock, Message, Role, Tool, ToolContext, ToolError, ToolExitStatus, ToolOutput,
    ToolRegistry, ToolRisk,
};
use flash_provider::{
    send_event, ChatProvider, ChatRequest, ProviderError, ProviderEvent, StopReason, ToolCall,
};
use serde_json::{json, Value};

pub(crate) struct RecordingProvider {
    pub(crate) requests: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait(?Send)]
impl ChatProvider for RecordingProvider {
    async fn chat(
        &mut self,
        request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        self.requests.lock().unwrap().push(request);
        send_event(&events, ProviderEvent::TextDelta("continued".to_string())).await?;
        send_event(&events, ProviderEvent::Done(StopReason::EndTurn)).await?;
        Ok(())
    }
}

pub(crate) struct UnknownToolProvider;

pub(crate) struct DeepSeekSseProvider {
    pub(crate) turn: u32,
}

#[async_trait(?Send)]
impl ChatProvider for DeepSeekSseProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        sender: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        self.turn += 1;
        let events = if self.turn == 1 {
            flash_deepseek::parse_sse(concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_read\",\"function\":{\"name\":\"Read\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
                "data: [DONE]\n"
            ))
            .map_err(|error| ProviderError::Unrecoverable(error.to_string()))?
        } else {
            vec![
                ProviderEvent::TextDelta("done".to_string()),
                ProviderEvent::Done(StopReason::EndTurn),
            ]
        };
        for event in events {
            send_event(&sender, event).await?;
        }
        Ok(())
    }
}

#[async_trait(?Send)]
impl ChatProvider for UnknownToolProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(
            &events,
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_missing".to_string(),
                name: "missing".to_string(),
                input: serde_json::json!({}),
            }),
        )
        .await?;
        send_event(&events, ProviderEvent::Done(StopReason::ToolUse)).await?;
        Ok(())
    }
}

pub(crate) struct PartialProvider;

#[async_trait(?Send)]
impl ChatProvider for PartialProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(&events, ProviderEvent::TextDelta("half".to_string())).await?;
        Ok(())
    }
}

pub(crate) struct LoopProvider;

#[async_trait(?Send)]
impl ChatProvider for LoopProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(
            &events,
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_read".to_string(),
                name: "fake".to_string(),
                input: serde_json::json!({}),
            }),
        )
        .await?;
        send_event(&events, ProviderEvent::Done(StopReason::ToolUse)).await?;
        Ok(())
    }
}

pub(crate) struct MaxTokensProvider;

#[async_trait(?Send)]
impl ChatProvider for MaxTokensProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(&events, ProviderEvent::TextDelta("truncated".to_string())).await?;
        send_event(&events, ProviderEvent::Done(StopReason::MaxTokens)).await?;
        Ok(())
    }
}

pub(crate) struct InvalidCompletionProvider {
    pub(crate) reason: StopReason,
    pub(crate) include_tool_call: bool,
}

#[async_trait(?Send)]
impl ChatProvider for InvalidCompletionProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        if self.include_tool_call {
            send_event(
                &events,
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_invalid".to_string(),
                    name: "Read".to_string(),
                    input: serde_json::json!({"path": "README.md"}),
                }),
            )
            .await?;
        }
        send_event(&events, ProviderEvent::Done(self.reason.clone())).await?;
        Ok(())
    }
}

pub(crate) struct RetryProvider {
    pub(crate) calls: u32,
}

pub(crate) struct PartialRetryProvider {
    pub(crate) calls: Arc<AtomicU32>,
}

pub(crate) struct AlwaysErrorProvider {
    pub(crate) error: ProviderError,
    pub(crate) calls: Arc<AtomicU32>,
}

pub(crate) struct RetryAfterProvider {
    pub(crate) calls: Arc<AtomicU32>,
    pub(crate) retry_after: Duration,
}

pub(crate) struct CancellableProvider;

#[async_trait(?Send)]
impl ChatProvider for CancellableProvider {
    async fn chat(
        &mut self,
        request: ChatRequest,
        _events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        request.cancellation.cancelled().await;
        Err(ProviderError::Cancelled(
            "provider stream cancelled".to_string(),
        ))
    }
}

#[async_trait(?Send)]
impl ChatProvider for PartialRetryProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        send_event(&events, ProviderEvent::TextDelta("partial".to_string())).await?;
        Err(ProviderError::Server {
            message: "stream failed".to_string(),
            retry_after: None,
        })
    }
}

#[async_trait(?Send)]
impl ChatProvider for AlwaysErrorProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        _events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(self.error.clone())
    }
}

#[async_trait(?Send)]
impl ChatProvider for RetryAfterProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        if call == 0 {
            return Err(ProviderError::RateLimited {
                message: "rate limited".to_string(),
                retry_after: Some(self.retry_after),
            });
        }
        send_event(&events, ProviderEvent::TextDelta("ok".to_string())).await?;
        send_event(&events, ProviderEvent::Done(StopReason::EndTurn)).await?;
        Ok(())
    }
}

pub(crate) struct MultipleToolProvider;

#[async_trait(?Send)]
impl ChatProvider for MultipleToolProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        for call_id in ["call_a", "call_b"] {
            send_event(
                &events,
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: call_id.to_string(),
                    name: "execute".to_string(),
                    input: json!({}),
                }),
            )
            .await?;
        }
        send_event(&events, ProviderEvent::Done(StopReason::ToolUse)).await?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl ChatProvider for RetryProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        self.calls += 1;
        if self.calls == 1 {
            return Err(ProviderError::RateLimited {
                message: "rate limited".to_string(),
                retry_after: None,
            });
        }
        send_event(&events, ProviderEvent::TextDelta("ok".to_string())).await?;
        send_event(&events, ProviderEvent::Done(StopReason::EndTurn)).await?;
        Ok(())
    }
}

pub(crate) struct LargeToolProvider;

pub(crate) struct HugeDeltaProvider;

#[async_trait(?Send)]
impl ChatProvider for HugeDeltaProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(&events, ProviderEvent::TextDelta("x".repeat(1024))).await?;
        send_event(&events, ProviderEvent::Done(StopReason::EndTurn)).await?;
        Ok(())
    }
}

#[async_trait(?Send)]
impl ChatProvider for LargeToolProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(
            &events,
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_large".to_string(),
                name: "large".to_string(),
                input: serde_json::json!({}),
            }),
        )
        .await?;
        send_event(&events, ProviderEvent::Done(StopReason::ToolUse)).await?;
        Ok(())
    }
}

pub(crate) struct ErrorToolProvider;

#[async_trait(?Send)]
impl ChatProvider for ErrorToolProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(
            &events,
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_error".to_string(),
                name: "error".to_string(),
                input: serde_json::json!({}),
            }),
        )
        .await?;
        send_event(&events, ProviderEvent::Done(StopReason::ToolUse)).await?;
        Ok(())
    }
}

pub(crate) struct ExecuteToolProvider;

#[async_trait(?Send)]
impl ChatProvider for ExecuteToolProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(
            &events,
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_execute".to_string(),
                name: "execute".to_string(),
                input: serde_json::json!({}),
            }),
        )
        .await?;
        send_event(&events, ProviderEvent::Done(StopReason::ToolUse)).await?;
        Ok(())
    }
}

pub(crate) struct CancelToolProvider;

#[async_trait(?Send)]
impl ChatProvider for CancelToolProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(
            &events,
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_cancel".to_string(),
                name: "cancel".to_string(),
                input: serde_json::json!({}),
            }),
        )
        .await?;
        send_event(&events, ProviderEvent::Done(StopReason::ToolUse)).await?;
        Ok(())
    }
}

pub(crate) struct DestructiveToolProvider;

#[async_trait(?Send)]
impl ChatProvider for DestructiveToolProvider {
    async fn chat(
        &mut self,
        _request: ChatRequest,
        events: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        send_event(
            &events,
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_destructive".to_string(),
                name: "destructive".to_string(),
                input: serde_json::json!({}),
            }),
        )
        .await?;
        send_event(&events, ProviderEvent::Done(StopReason::ToolUse)).await?;
        Ok(())
    }
}

pub(crate) struct FakeTool;

impl Tool for FakeTool {
    fn name(&self) -> &str {
        "fake"
    }

    fn description(&self) -> &str {
        "fake tool"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success("ok"))
    }
}

pub(crate) struct LargeTool;

impl Tool for LargeTool {
    fn name(&self) -> &str {
        "large"
    }

    fn description(&self) -> &str {
        "large tool"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success("abcdef"))
    }
}

pub(crate) struct DualOutputTool;

impl Tool for DualOutputTool {
    fn name(&self) -> &str {
        "large"
    }

    fn description(&self) -> &str {
        "two large streams"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            stdout: "abcd".to_string(),
            stderr: "efgh".to_string(),
            status: ToolExitStatus::Success,
            exit_code: Some(0),
            signal: None,
            duration_ms: 0,
            timed_out: false,
            truncated: false,
            artifact: None,
        })
    }
}

pub(crate) struct ErrorTool;

impl Tool for ErrorTool {
    fn name(&self) -> &str {
        "error"
    }

    fn description(&self) -> &str {
        "error tool"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
        Err(ToolError::new("tool failed"))
    }
}

pub(crate) struct ExecuteTool;

impl Tool for ExecuteTool {
    fn name(&self) -> &str {
        "execute"
    }

    fn description(&self) -> &str {
        "execute tool"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Execute)
    }

    fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success("should not run"))
    }
}

pub(crate) struct ApprovingApproval;

impl crate::hooks::ApprovalController for ApprovingApproval {
    fn approve(&mut self, _request: &crate::hooks::ApprovalRequest) -> bool {
        true
    }
}

pub(crate) struct CancelTool;

impl Tool for CancelTool {
    fn name(&self) -> &str {
        "cancel"
    }

    fn description(&self) -> &str {
        "cancel tool"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput {
            stdout: String::new(),
            stderr: "cancelled".to_string(),
            status: ToolExitStatus::Cancelled,
            exit_code: None,
            signal: None,
            duration_ms: 0,
            timed_out: false,
            truncated: false,
            artifact: None,
        })
    }
}

pub(crate) struct DestructiveTool;

impl Tool for DestructiveTool {
    fn name(&self) -> &str {
        "destructive"
    }

    fn description(&self) -> &str {
        "destructive tool"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Destructive)
    }

    fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success("destructive ran"))
    }
}

pub(crate) fn flash_tools_for_tests() -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(FakeTool)).unwrap();
    registry.register(Box::new(SearchFakeTool)).unwrap();
    registry
}

pub(crate) struct SearchFakeTool;

impl Tool for SearchFakeTool {
    fn name(&self) -> &str {
        "search"
    }

    fn description(&self) -> &str {
        "search fake tool"
    }

    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }

    fn risk(&self, _input: &Value) -> Result<ToolRisk, ToolError> {
        Ok(ToolRisk::Read)
    }

    fn call(&self, _input: Value, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::success("src/lib.rs"))
    }
}

pub(crate) fn temp_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("flash_agent_{name}_{nanos}"))
}

pub(crate) fn prepared_workspace(name: &str) -> PathBuf {
    let root = temp_dir(name);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "").unwrap();
    root
}

pub(crate) fn only_session_path(root: &Path) -> PathBuf {
    fs::read_dir(root.join(".flash/sessions"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path()
}

pub(crate) fn test_message(text: &str) -> Message {
    Message {
        id: text.to_string(),
        role: Role::User,
        created_at: "0".to_string(),
        content: vec![ContentBlock::Text {
            text: text.to_string(),
        }],
    }
}
