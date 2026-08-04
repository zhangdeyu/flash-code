use flash_core::Event;
use flash_provider::{ProviderEvent, StopReason, ToolCall, Usage};

use crate::error::AgentError;
use crate::finalize::emit_event;
use crate::hooks::EventObserver;
use crate::runtime::AgentRuntime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnResult {
    pub(crate) assistant_text: String,
    pub(crate) reasoning_text: String,
    pub(crate) tool_calls: Vec<ToolCall>,
    pub(crate) completion: TurnCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnCompletion {
    EndTurn,
    ToolUse,
    Cancelled,
    Failed,
}

impl<P> AgentRuntime<P>
where
    P: flash_provider::ChatProvider,
{
    pub(crate) async fn handle_provider_events(
        &self,
        session: &flash_core::storage::Session,
        request_id: &str,
        attempt: u32,
        provider_events: Vec<ProviderEvent>,
        observer: &mut impl EventObserver,
    ) -> Result<TurnResult, AgentError> {
        let mut assistant_text = String::new();
        let mut reasoning_text = String::new();
        let mut tool_calls = Vec::new();
        let mut stop_reason = None;

        for event in provider_events {
            match event {
                // ReasoningDelta and AssistantDelta were already streamed in
                // chat_with_retry_streaming; skip re-emitting them here.
                ProviderEvent::ReasoningDelta(text) => reasoning_text.push_str(&text),
                ProviderEvent::TextDelta(text) => assistant_text.push_str(&text),
                ProviderEvent::ToolCallComplete(call) => {
                    emit_event(
                        session,
                        Event::ToolCallRequested {
                            call_id: call.call_id.clone(),
                            name: call.name.clone(),
                        },
                        observer,
                    )
                    .await?;
                    tool_calls.push(call);
                }
                ProviderEvent::Usage(Usage {
                    input_tokens,
                    output_tokens,
                }) => {
                    emit_event(
                        session,
                        Event::UsageRecorded {
                            request_id: request_id.to_string(),
                            attempt,
                            input_tokens,
                            output_tokens,
                        },
                        observer,
                    )
                    .await?;
                }
                ProviderEvent::Done(reason) => stop_reason = Some(reason),
            }
        }

        let completion = match stop_reason {
            Some(StopReason::EndTurn) if tool_calls.is_empty() => TurnCompletion::EndTurn,
            Some(StopReason::EndTurn) => {
                emit_event(
                    session,
                    Event::Error {
                        message: "provider ended turn while returning tool calls".to_string(),
                    },
                    observer,
                )
                .await?;
                TurnCompletion::Failed
            }
            Some(StopReason::ToolUse) if !tool_calls.is_empty() => TurnCompletion::ToolUse,
            Some(StopReason::ToolUse) => {
                emit_event(
                    session,
                    Event::Error {
                        message: "provider requested tool use without tool calls".to_string(),
                    },
                    observer,
                )
                .await?;
                TurnCompletion::Failed
            }
            Some(StopReason::MaxTokens) => {
                emit_event(
                    session,
                    Event::Error {
                        message: "provider stopped at max tokens".to_string(),
                    },
                    observer,
                )
                .await?;
                TurnCompletion::Failed
            }
            Some(StopReason::Cancelled) => TurnCompletion::Cancelled,
            Some(StopReason::StopSequence) => {
                emit_event(
                    session,
                    Event::Error {
                        message: "provider stopped at a stop sequence".to_string(),
                    },
                    observer,
                )
                .await?;
                TurnCompletion::Failed
            }
            Some(StopReason::Refusal) => {
                emit_event(
                    session,
                    Event::Error {
                        message: "provider refused the request".to_string(),
                    },
                    observer,
                )
                .await?;
                TurnCompletion::Failed
            }
            Some(StopReason::Unknown(reason)) => {
                emit_event(
                    session,
                    Event::Error {
                        message: format!("provider returned unknown stop reason `{reason}`"),
                    },
                    observer,
                )
                .await?;
                TurnCompletion::Failed
            }
            None => {
                emit_event(
                    session,
                    Event::Error {
                        message: "model stream ended before done".to_string(),
                    },
                    observer,
                )
                .await?;
                TurnCompletion::Failed
            }
        };

        Ok(TurnResult {
            assistant_text,
            reasoning_text,
            tool_calls,
            completion,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{
        flash_tools_for_tests, prepared_workspace, InvalidCompletionProvider, LoopProvider,
    };
    use crate::{AgentOptions, AgentRuntime};
    use flash_core::{Outcome, PermissionPolicy, ToolRegistry};
    use flash_provider::StopReason;
    use std::fs;

    #[tokio::test]
    async fn run_task_should_reject_inconsistent_tool_completion() {
        for (name, provider) in [
            (
                "tool_use_without_call",
                InvalidCompletionProvider {
                    reason: StopReason::ToolUse,
                    include_tool_call: false,
                },
            ),
            (
                "end_turn_with_call",
                InvalidCompletionProvider {
                    reason: StopReason::EndTurn,
                    include_tool_call: true,
                },
            ),
        ] {
            let root = prepared_workspace(name);
            let runtime = AgentRuntime::new(
                provider,
                ToolRegistry::new(),
                AgentOptions {
                    model: "smoke".to_string(),
                    max_turns: 1,
                    permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                    max_output_bytes: 200_000,
                    max_prompt_bytes: 200_000,
                },
            );

            let run = runtime.run_task(&root, "invalid completion").await.unwrap();

            assert_eq!(run.outcome, Outcome::Failed, "{name}");
            let session_path = root.join(".flash").join("sessions").join(run.session_id);
            let metadata = fs::read_to_string(session_path.join("session.json")).unwrap();
            let events = fs::read_to_string(session_path.join("events.jsonl")).unwrap();
            assert!(metadata.contains("\"status\":\"failed\""), "{name}");
            assert_eq!(
                events.matches("\"type\":\"session_finished\"").count(),
                1,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn run_task_should_stop_at_max_turns() {
        let root = crate::test_support::temp_dir("max_turns");
        fs::create_dir_all(&root).unwrap();
        let runtime = AgentRuntime::new(
            LoopProvider,
            flash_tools_for_tests(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Confirm),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "loop").await.unwrap();

        assert_eq!(run.outcome, Outcome::Failed);
    }
}
