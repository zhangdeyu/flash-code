use flash_core::{
    append_tool_result_message_async, Event, Message, PermissionDecision, ToolContext, ToolError,
    ToolErrorKind, ToolExitStatus, ToolResultStatus,
};
use flash_provider::ToolCall;

use crate::artifact::sanitize_artifact_component;
use crate::error::AgentError;
use crate::finalize::emit_event;
use crate::hooks::{ApprovalController, ApprovalRequest, EventObserver};
use crate::runtime::AgentRuntime;

impl<P> AgentRuntime<P>
where
    P: flash_provider::ChatProvider,
{
    pub(crate) async fn execute_tool_call(
        &self,
        session: &flash_core::storage::Session,
        context: &ToolContext,
        call: &ToolCall,
        observer: &mut impl EventObserver,
        approval: &mut impl ApprovalController,
    ) -> Result<Message, AgentError> {
        let Some(tool) = self.tools.get(&call.name) else {
            emit_event(
                session,
                Event::Error {
                    message: format!("unknown tool `{}`", call.name),
                },
                observer,
            )
            .await?;
            emit_event(
                session,
                Event::ToolFinished {
                    call_id: call.call_id.clone(),
                    status: ToolResultStatus::Error,
                },
                observer,
            )
            .await?;
            return append_tool_result_message_async(
                session.clone(),
                call.call_id.clone(),
                ToolResultStatus::Error,
                "unknown tool".to_string(),
            )
            .await
            .map_err(AgentError::Storage);
        };

        let risk = match tool.risk(&call.input) {
            Ok(risk) => risk,
            Err(error) => {
                emit_event(
                    session,
                    Event::Error {
                        message: error.message.clone(),
                    },
                    observer,
                )
                .await?;
                emit_event(
                    session,
                    Event::ToolFinished {
                        call_id: call.call_id.clone(),
                        status: ToolResultStatus::Error,
                    },
                    observer,
                )
                .await?;
                return append_tool_result_message_async(
                    session.clone(),
                    call.call_id.clone(),
                    ToolResultStatus::Error,
                    format_tool_error(&error),
                )
                .await
                .map_err(AgentError::Storage);
            }
        };

        // Unified pipeline: decision -> approval -> execute -> submit.
        // The permission decision controls whether approval is auto-granted (Allow),
        // consulted (Ask), or auto-denied (Deny). Tool execution and ToolResult
        // submission are shared across the approved / denied outcomes rather than
        // duplicated per branch.
        let decision = self.options.permission_policy.decide(risk);
        let approved = match decision {
            PermissionDecision::Allow => {
                emit_event(
                    session,
                    Event::ApprovalResolved {
                        call_id: call.call_id.clone(),
                        approved: true,
                    },
                    observer,
                )
                .await?;
                true
            }
            PermissionDecision::Ask => {
                emit_event(
                    session,
                    Event::ApprovalRequired {
                        call_id: call.call_id.clone(),
                    },
                    observer,
                )
                .await?;
                let approved = approval.approve(&ApprovalRequest {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    input: call.input.to_string(),
                    risk,
                });
                emit_event(
                    session,
                    Event::ApprovalResolved {
                        call_id: call.call_id.clone(),
                        approved,
                    },
                    observer,
                )
                .await?;
                approved
            }
            PermissionDecision::Deny => {
                emit_event(
                    session,
                    Event::ApprovalRequired {
                        call_id: call.call_id.clone(),
                    },
                    observer,
                )
                .await?;
                emit_event(
                    session,
                    Event::ApprovalResolved {
                        call_id: call.call_id.clone(),
                        approved: false,
                    },
                    observer,
                )
                .await?;
                false
            }
        };

        if approved {
            emit_event(
                session,
                Event::ToolStarted {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                },
                observer,
            )
            .await?;
            match self
                .tools
                .call_blocking(
                    &call.name,
                    call.input.clone(),
                    tool_context_for_call(context, &call.call_id),
                )
                .await
                .expect("tool existence checked before call")
            {
                Ok(output) => {
                    self.commit_tool_output(session, call, output, observer)
                        .await
                }
                Err(error) => {
                    emit_event(
                        session,
                        Event::Error {
                            message: error.message.clone(),
                        },
                        observer,
                    )
                    .await?;
                    emit_event(
                        session,
                        Event::ToolFinished {
                            call_id: call.call_id.clone(),
                            status: tool_error_status(error.kind),
                        },
                        observer,
                    )
                    .await?;
                    append_tool_result_message_async(
                        session.clone(),
                        call.call_id.clone(),
                        tool_error_status(error.kind),
                        format_tool_error(&error),
                    )
                    .await
                    .map_err(AgentError::Storage)
                }
            }
        } else {
            emit_event(
                session,
                Event::ToolFinished {
                    call_id: call.call_id.clone(),
                    status: ToolResultStatus::Rejected,
                },
                observer,
            )
            .await?;
            append_tool_result_message_async(
                session.clone(),
                call.call_id.clone(),
                ToolResultStatus::Rejected,
                "tool call rejected by permission policy".to_string(),
            )
            .await
            .map_err(AgentError::Storage)
        }
    }

    pub(crate) async fn cancel_tool_calls(
        &self,
        session: &flash_core::storage::Session,
        calls: &[ToolCall],
        observer: &mut impl EventObserver,
    ) -> Result<Vec<Message>, AgentError> {
        let mut messages = Vec::with_capacity(calls.len());
        for call in calls {
            emit_event(
                session,
                Event::ToolFinished {
                    call_id: call.call_id.clone(),
                    status: ToolResultStatus::Cancelled,
                },
                observer,
            )
            .await?;
            messages.push(
                append_tool_result_message_async(
                    session.clone(),
                    call.call_id.clone(),
                    ToolResultStatus::Cancelled,
                    "tool call cancelled before execution".to_string(),
                )
                .await?,
            );
        }
        Ok(messages)
    }

    async fn commit_tool_output(
        &self,
        session: &flash_core::storage::Session,
        call: &ToolCall,
        mut output: flash_core::ToolOutput,
        observer: &mut impl EventObserver,
    ) -> Result<Message, AgentError> {
        let stdout = self
            .materialize_output(session, &call.call_id, "stdout", &output.stdout)
            .await?;
        let stderr = self
            .materialize_output(session, &call.call_id, "stderr", &output.stderr)
            .await?;
        output.truncated |= stdout.truncated || stderr.truncated;
        let artifacts = [
            output.artifact.as_deref(),
            stdout.artifact.as_deref(),
            stderr.artifact.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(", ");
        output.artifact = (!artifacts.is_empty()).then_some(artifacts);
        if !stdout.text.is_empty() {
            emit_event(
                session,
                Event::ToolOutputDelta {
                    call_id: call.call_id.clone(),
                    stream: "stdout".to_string(),
                    text: stdout.text.clone(),
                },
                observer,
            )
            .await?;
        }
        if !stderr.text.is_empty() {
            emit_event(
                session,
                Event::ToolOutputDelta {
                    call_id: call.call_id.clone(),
                    stream: "stderr".to_string(),
                    text: stderr.text.clone(),
                },
                observer,
            )
            .await?;
        }
        let status = match output.status {
            ToolExitStatus::Success => ToolResultStatus::Success,
            ToolExitStatus::Error => ToolResultStatus::Error,
            ToolExitStatus::Cancelled => ToolResultStatus::Cancelled,
        };
        emit_event(
            session,
            Event::ToolFinished {
                call_id: call.call_id.clone(),
                status,
            },
            observer,
        )
        .await?;
        append_tool_result_message_async(
            session.clone(),
            call.call_id.clone(),
            status,
            format!(
                "stdout:\n{}\nstderr:\n{}\nmetadata:\n{}",
                stdout.text,
                stderr.text,
                serde_json::json!({
                    "exit_code": output.exit_code,
                    "signal": output.signal,
                    "duration_ms": output.duration_ms,
                    "timed_out": output.timed_out,
                    "truncated": output.truncated,
                    "artifact": output.artifact,
                })
            ),
        )
        .await
        .map_err(AgentError::Storage)
    }
}

fn tool_error_status(kind: ToolErrorKind) -> ToolResultStatus {
    if kind == ToolErrorKind::Cancelled {
        ToolResultStatus::Cancelled
    } else {
        ToolResultStatus::Error
    }
}

fn format_tool_error(error: &ToolError) -> String {
    format!(
        "tool error: {}\nmetadata:\n{}",
        error.message,
        serde_json::json!({"kind": error.kind})
    )
}

fn tool_context_for_call(context: &ToolContext, call_id: &str) -> ToolContext {
    let mut context = context.clone();
    context.artifact_stem = Some(sanitize_artifact_component(call_id));
    context
}

#[cfg(test)]
mod tests {
    use crate::test_support::{
        only_session_path, prepared_workspace, temp_dir, ApprovingApproval, CancelTool,
        CancelToolProvider, DestructiveTool, DestructiveToolProvider, ErrorTool, ErrorToolProvider,
        ExecuteTool, ExecuteToolProvider, HugeDeltaProvider, MultipleToolProvider,
    };
    use crate::{AgentError, AgentOptions, AgentRuntime};
    use flash_core::{ArtifactLimits, Outcome, PermissionPolicy, StorageLimits, ToolRegistry};
    use std::fs;

    #[tokio::test]
    async fn event_limit_should_finalize_session_as_failed() {
        let root = prepared_workspace("event_limit");
        let runtime = AgentRuntime::new(
            HugeDeltaProvider,
            ToolRegistry::new(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        )
        .with_resource_limits(
            StorageLimits {
                max_event_bytes: 256,
                max_jsonl_bytes: 4096,
            },
            ArtifactLimits::default(),
        );

        let error = runtime.run_task(&root, "large event").await.unwrap_err();
        let session = only_session_path(&root);
        let metadata = fs::read_to_string(session.join("session.json")).unwrap();
        let events = fs::read_to_string(session.join("events.jsonl")).unwrap();

        assert!(matches!(
            error,
            AgentError::Storage(flash_core::storage::StorageError::ResourceLimit { .. })
        ));
        assert!(metadata.contains("\"status\":\"failed\""));
        assert!(!metadata.contains("\"status\":\"succeeded\""));
        assert_eq!(events.matches("\"type\":\"session_finished\"").count(), 1);
        assert!(events.contains("\"outcome\":\"failed\""));
    }

    #[tokio::test]
    async fn run_task_should_write_error_tool_result_for_unknown_tool() {
        let root = temp_dir("unknown_tool");
        fs::create_dir_all(&root).unwrap();
        let runtime = AgentRuntime::new(
            crate::test_support::UnknownToolProvider,
            ToolRegistry::new(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "use missing tool").await.unwrap();

        assert_eq!(run.outcome, Outcome::Failed);
    }

    #[tokio::test]
    async fn run_task_should_write_error_result_when_tool_fails() {
        let root = temp_dir("tool_error");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ErrorTool)).unwrap();
        let runtime = AgentRuntime::new(
            ErrorToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "error").await.unwrap();

        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(messages.contains("\"status\":\"error\""));
    }

    #[tokio::test]
    async fn run_task_should_write_rejected_result_when_policy_denies() {
        let root = temp_dir("tool_rejected");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ExecuteTool)).unwrap();
        let runtime = AgentRuntime::new(
            ExecuteToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Confirm),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "execute").await.unwrap();

        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(messages.contains("\"status\":\"rejected\""));
    }

    #[tokio::test]
    async fn run_task_with_controls_should_execute_approved_tool_call() {
        let root = temp_dir("tool_approved");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ExecuteTool)).unwrap();
        let mut runtime = AgentRuntime::new(
            ExecuteToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Confirm),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );
        let mut observer = crate::hooks::NoopObserver;
        let mut approval = ApprovingApproval;

        let run = runtime
            .run_task_with_controls(&root, "execute", &mut observer, || false, &mut approval)
            .await
            .unwrap();

        let events = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.contains("\"approved\":true"));
    }

    #[tokio::test]
    async fn run_task_should_require_approval_for_destructive_tool_even_in_yolo() {
        let root = temp_dir("destructive_yolo");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(DestructiveTool)).unwrap();
        let runtime = AgentRuntime::new(
            DestructiveToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "destructive").await.unwrap();

        let events = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.contains("\"type\":\"approval_required\""));
    }

    #[tokio::test]
    async fn run_task_should_write_cancelled_result_when_tool_cancels() {
        let root = temp_dir("tool_cancelled");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(CancelTool)).unwrap();
        let runtime = AgentRuntime::new(
            CancelToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "cancel").await.unwrap();

        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(messages.contains("\"status\":\"cancelled\""));
    }

    #[tokio::test]
    async fn run_task_should_cancel_every_pending_tool_call() {
        let root = temp_dir("pending_tool_cancel");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ExecuteTool)).unwrap();
        let mut runtime = AgentRuntime::new(
            MultipleToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );
        let mut observer = crate::hooks::NoopObserver;
        let mut checks = 0;

        let run = runtime
            .run_task_controlled(&root, "two tools", &mut observer, || {
                checks += 1;
                checks >= 4
            })
            .await
            .unwrap();

        assert_eq!(run.outcome, Outcome::Cancelled);
        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(messages.contains("\"call_id\":\"call_a\",\"status\":\"success\""));
        assert!(messages.contains("\"call_id\":\"call_b\",\"status\":\"cancelled\""));
    }
}
