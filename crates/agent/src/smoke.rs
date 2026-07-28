use async_trait::async_trait;
use flash_core::{ContentBlock, Role};
use flash_provider::{
    send_event, ChatProvider, ChatRequest, ProviderError, ProviderEvent, StopReason, ToolCall,
    Usage,
};

/// A deterministic, in-process [`ChatProvider`] used for smoke tests and the
/// `--provider smoke` CLI entry point. It is gated behind the `smoke` feature
/// (and `cfg(test)`) so it does not compile into the default production runtime.
pub struct SmokeProvider {
    turn: u32,
}

impl SmokeProvider {
    pub const fn new() -> Self {
        Self { turn: 0 }
    }
}

impl Default for SmokeProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait(?Send)]
impl ChatProvider for SmokeProvider {
    async fn chat(
        &mut self,
        request: ChatRequest,
        sender: tokio::sync::mpsc::Sender<ProviderEvent>,
    ) -> Result<(), ProviderError> {
        self.turn += 1;
        let events = if request
            .messages
            .iter()
            .any(|message| matches!(message.role, Role::Tool))
            && !latest_user_task(&request).contains("fix failing tests")
        {
            vec![
                ProviderEvent::TextDelta("Done.".to_string()),
                ProviderEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                }),
                ProviderEvent::Done(StopReason::EndTurn),
            ]
        } else {
            let task = latest_user_task(&request);
            if task.contains("fix failing tests") {
                fix_failing_tests_events(tool_result_count(&request))
            } else if task.contains("list files") {
                vec![
                    ProviderEvent::ReasoningDelta("Need inspect workspace files.".to_string()),
                    ProviderEvent::TextDelta("I will list matching files.".to_string()),
                    ProviderEvent::ToolCallComplete(ToolCall {
                        call_id: "call_list_files_1".to_string(),
                        name: "ListFiles".to_string(),
                        input: serde_json::json!({"path": "."}),
                    }),
                    ProviderEvent::Usage(Usage {
                        input_tokens: 20,
                        output_tokens: 6,
                    }),
                    ProviderEvent::Done(StopReason::ToolUse),
                ]
            } else {
                vec![
                    ProviderEvent::TextDelta("Task stored for the next runtime stage.".to_string()),
                    ProviderEvent::Done(StopReason::EndTurn),
                ]
            }
        };
        for event in events {
            send_event(&sender, event).await?;
        }
        Ok(())
    }
}

fn latest_user_task(request: &ChatRequest) -> &str {
    request
        .messages
        .iter()
        .rev()
        .find_map(|message| {
            if message.role == Role::User {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            } else {
                None
            }
        })
        .unwrap_or_default()
}

fn tool_result_count(request: &ChatRequest) -> usize {
    request
        .messages
        .iter()
        .filter(|message| message.role == Role::Tool)
        .count()
}

fn fix_failing_tests_events(tool_results: usize) -> Vec<ProviderEvent> {
    match tool_results {
        0 => vec![
            ProviderEvent::ReasoningDelta("Inspect the failing Rust source.".to_string()),
            ProviderEvent::TextDelta("I will inspect the source before editing.".to_string()),
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_read_1".to_string(),
                name: "Read".to_string(),
                input: serde_json::json!({"path": "src/lib.rs"}),
            }),
            ProviderEvent::Done(StopReason::ToolUse),
        ],
        1 => vec![
            ProviderEvent::TextDelta(
                "I found the incorrect constant and will patch it.".to_string(),
            ),
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_patch_1".to_string(),
                name: "Edit".to_string(),
                input: serde_json::json!({
                    "path": "src/lib.rs",
                    "find": "pub fn answer() -> i32 {\n    41\n}\n",
                    "replace": "pub fn answer() -> i32 {\n    42\n}\n"
                }),
            }),
            ProviderEvent::Done(StopReason::ToolUse),
        ],
        2 => vec![
            ProviderEvent::TextDelta("Now I will run the test suite.".to_string()),
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_tests_1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({"command": "cargo test"}),
            }),
            ProviderEvent::Done(StopReason::ToolUse),
        ],
        3 => vec![
            ProviderEvent::TextDelta("Tests passed; I will collect the diff.".to_string()),
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_diff_1".to_string(),
                name: "Bash".to_string(),
                input: serde_json::json!({"command": "git diff --"}),
            }),
            ProviderEvent::Done(StopReason::ToolUse),
        ],
        _ => vec![
            ProviderEvent::TextDelta("Fixed the failing test and verified the diff.".to_string()),
            ProviderEvent::Done(StopReason::EndTurn),
        ],
    }
}
