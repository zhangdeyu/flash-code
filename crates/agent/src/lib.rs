use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use flash_core::{
    append_assistant_message_async, append_event_async, append_system_message_async,
    append_tool_result_message_async, append_user_message_async, create_session_async,
    finalize_session_async, CancellationToken, ContentBlock, Event, Message, Outcome,
    PermissionDecision, PermissionPolicy, Role, ToolContext, ToolError, ToolErrorKind,
    ToolExitStatus, ToolRegistry, ToolResultStatus, ToolRisk,
};
use flash_provider::{
    send_event, ChatProvider, ChatRequest, ProviderError, ProviderEvent, StopReason, ToolCall,
    ToolSpec, Usage,
};

pub trait EventObserver {
    fn on_event(&mut self, event: &Event);
}

impl<F> EventObserver for F
where
    F: FnMut(&Event),
{
    fn on_event(&mut self, event: &Event) {
        self(event);
    }
}

struct NoopObserver;

impl EventObserver for NoopObserver {
    fn on_event(&mut self, _event: &Event) {}
}

pub trait ApprovalController {
    fn approve(&mut self, request: &ApprovalRequest) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    pub call_id: String,
    pub name: String,
    pub input: String,
    pub risk: ToolRisk,
}

struct RejectingApproval;

impl ApprovalController for RejectingApproval {
    fn approve(&mut self, _request: &ApprovalRequest) -> bool {
        false
    }
}

pub struct AgentRuntime<P> {
    provider: P,
    tools: ToolRegistry,
    options: AgentOptions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOptions {
    pub model: String,
    pub max_turns: u32,
    pub permission_policy: PermissionPolicy,
    pub max_output_bytes: usize,
    pub max_prompt_bytes: usize,
}

impl<P> AgentRuntime<P>
where
    P: ChatProvider,
{
    pub fn new(provider: P, tools: ToolRegistry, options: AgentOptions) -> Self {
        Self {
            provider,
            tools,
            options,
        }
    }

    pub async fn run_task(
        &mut self,
        workspace_root: &Path,
        task: &str,
    ) -> Result<AgentRun, AgentError> {
        self.run_task_with_observer(workspace_root, task, NoopObserver)
            .await
    }

    pub async fn run_task_with_observer<O>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        mut observer: O,
    ) -> Result<AgentRun, AgentError>
    where
        O: EventObserver,
    {
        self.run_task_controlled(workspace_root, task, &mut observer, || false)
            .await
    }

    pub async fn run_task_controlled<O, C>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        observer: &mut O,
        should_cancel: C,
    ) -> Result<AgentRun, AgentError>
    where
        O: EventObserver,
        C: FnMut() -> bool,
    {
        self.run_task_with_controls(
            workspace_root,
            task,
            observer,
            should_cancel,
            &mut RejectingApproval,
        )
        .await
    }

    pub async fn run_task_with_controls<O, C, A>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        observer: &mut O,
        should_cancel: C,
        approval: &mut A,
    ) -> Result<AgentRun, AgentError>
    where
        O: EventObserver,
        C: FnMut() -> bool,
        A: ApprovalController,
    {
        self.run_task_with_token_and_controls(
            workspace_root,
            task,
            observer,
            CancellationToken::new(),
            should_cancel,
            approval,
        )
        .await
    }

    pub async fn run_task_with_cancellation<O, A>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        observer: &mut O,
        cancellation: CancellationToken,
        approval: &mut A,
    ) -> Result<AgentRun, AgentError>
    where
        O: EventObserver,
        A: ApprovalController,
    {
        let observed_cancellation = cancellation.clone();
        self.run_task_with_token_and_controls(
            workspace_root,
            task,
            observer,
            cancellation,
            move || observed_cancellation.is_cancelled(),
            approval,
        )
        .await
    }

    async fn run_task_with_token_and_controls<O, C, A>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        observer: &mut O,
        cancellation: CancellationToken,
        mut should_cancel: C,
        approval: &mut A,
    ) -> Result<AgentRun, AgentError>
    where
        O: EventObserver,
        C: FnMut() -> bool,
        A: ApprovalController,
    {
        let session = create_session_async(workspace_root.to_path_buf()).await?;
        let result = async {
            let system = append_system_message_async(
                session.clone(),
                runtime_system_prompt(workspace_root, &self.tools),
            )
            .await?;
            let user = append_user_message_async(session.clone(), task.to_string()).await?;
            let mut history = vec![system, user];
            let context = ToolContext {
                workspace_root: workspace_root.to_path_buf(),
                cancellation: cancellation.clone(),
                artifact_dir: Some(session.path.join("artifacts")),
                artifact_stem: None,
            };

            for turn in 1..=self.options.max_turns {
                if should_cancel() {
                    cancellation.cancel();
                    emit_event(
                        &session,
                        Event::Error {
                            message: "run cancelled".to_string(),
                        },
                        observer,
                    )
                    .await?;
                    return finish_session(&session, Outcome::Cancelled, observer).await;
                }

                let request_id = format!("request_{turn}");
                let projected_history =
                    match project_history(&history, self.options.max_prompt_bytes) {
                        Ok(history) => history,
                        Err(error) => {
                            emit_event(
                                &session,
                                Event::Error {
                                    message: error.to_string(),
                                },
                                observer,
                            )
                            .await?;
                            return finish_session(&session, Outcome::Failed, observer).await;
                        }
                    };
                let request = ChatRequest {
                    messages: projected_history,
                    tools: self
                        .tools
                        .descriptors()
                        .map(|d| ToolSpec {
                            name: d.name,
                            description: d.description,
                            parameters: d.parameters,
                        })
                        .collect(),
                    model: self.options.model.clone(),
                    cancellation: cancellation.clone(),
                };
                let (provider_events, attempt) = match self
                    .chat_with_retry_streaming(&session, &request_id, request, observer)
                    .await
                {
                    Ok(result) => result,
                    Err(AgentError::Provider(error)) => {
                        if error.is_cancelled() {
                            emit_event(
                                &session,
                                Event::Error {
                                    message: "run cancelled".to_string(),
                                },
                                observer,
                            )
                            .await?;
                            return finish_session(&session, Outcome::Cancelled, observer).await;
                        }
                        emit_event(
                            &session,
                            Event::Error {
                                message: error.to_string(),
                            },
                            observer,
                        )
                        .await?;
                        finish_session(&session, Outcome::Failed, observer).await?;
                        return Err(AgentError::Provider(error));
                    }
                    Err(error) => return Err(error),
                };
                let turn_result = self
                    .handle_provider_events(
                        &session,
                        &request_id,
                        attempt,
                        provider_events,
                        observer,
                    )
                    .await?;
                match turn_result.completion {
                    TurnCompletion::EndTurn | TurnCompletion::ToolUse => {}
                    TurnCompletion::Cancelled => {
                        return finish_session(&session, Outcome::Cancelled, observer).await;
                    }
                    TurnCompletion::Failed => {
                        return finish_session(&session, Outcome::Failed, observer).await;
                    }
                }

                if should_cancel() {
                    cancellation.cancel();
                    emit_event(
                        &session,
                        Event::Error {
                            message: "run cancelled".to_string(),
                        },
                        observer,
                    )
                    .await?;
                    return finish_session(&session, Outcome::Cancelled, observer).await;
                }

                let assistant = append_assistant_message_async(
                    session.clone(),
                    turn_result.reasoning_text.clone(),
                    turn_result.assistant_text.clone(),
                    turn_result
                        .tool_calls
                        .iter()
                        .map(|call| (call.call_id.clone(), call.name.clone(), call.input.clone()))
                        .collect(),
                )
                .await?;
                history.push(assistant);

                if matches!(turn_result.completion, TurnCompletion::EndTurn) {
                    return finish_session(&session, Outcome::Succeeded, observer).await;
                }

                for (index, call) in turn_result.tool_calls.iter().enumerate() {
                    if should_cancel() {
                        cancellation.cancel();
                        emit_event(
                            &session,
                            Event::Error {
                                message: "run cancelled".to_string(),
                            },
                            observer,
                        )
                        .await?;
                        let cancelled = self
                            .cancel_tool_calls(&session, &turn_result.tool_calls[index..], observer)
                            .await?;
                        history.extend(cancelled);
                        return finish_session(&session, Outcome::Cancelled, observer).await;
                    }
                    let message = self
                        .execute_tool_call(&session, &context, call, observer, approval)
                        .await?;
                    history.push(message);
                }
            }

            emit_event(
                &session,
                Event::Error {
                    message: "max_turns exceeded".to_string(),
                },
                observer,
            )
            .await?;
            finish_session(&session, Outcome::Failed, observer).await
        }
        .await;
        match result {
            Ok(run) => Ok(run),
            Err(primary) => match finish_session(&session, Outcome::Failed, observer).await {
                Ok(_) => Err(primary),
                Err(finalize) => Err(AgentError::Finalization {
                    primary: primary.to_string(),
                    finalize: finalize.to_string(),
                }),
            },
        }
    }

    async fn handle_provider_events(
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

    async fn chat_with_retry_streaming<O: EventObserver>(
        &mut self,
        session: &flash_core::storage::Session,
        request_id: &str,
        request: ChatRequest,
        observer: &mut O,
    ) -> Result<(Vec<ProviderEvent>, u32), AgentError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            emit_event(
                session,
                Event::ModelRequestStarted {
                    request_id: request_id.to_string(),
                    attempt,
                    model: request.model.clone(),
                },
                observer,
            )
            .await?;
            let mut collected: Vec<ProviderEvent> = Vec::new();
            let mut published_delta = false;
            let (sender, mut events) = tokio::sync::mpsc::channel(64);
            let mut provider = Box::pin(self.provider.chat(request.clone(), sender));
            let result = loop {
                tokio::select! {
                    result = &mut provider => break result,
                    event = events.recv() => {
                        let Some(event) = event else {
                            break provider.await;
                        };
                        published_delta |= persist_provider_delta(
                            session,
                            request_id,
                            attempt,
                            &event,
                            observer,
                        ).await?;
                        collected.push(event);
                    }
                }
            };
            while let Some(event) = events.recv().await {
                published_delta |=
                    persist_provider_delta(session, request_id, attempt, &event, observer).await?;
                collected.push(event);
            }
            match result {
                Ok(()) => {
                    emit_event(
                        session,
                        Event::ModelAttemptCommitted {
                            request_id: request_id.to_string(),
                            attempt,
                        },
                        observer,
                    )
                    .await?;
                    return Ok((collected, attempt));
                }
                Err(error) => {
                    let will_retry = error.is_retryable() && attempt < 3 && !published_delta;
                    emit_event(
                        session,
                        Event::ModelAttemptFailed {
                            request_id: request_id.to_string(),
                            attempt,
                            retryable: will_retry,
                            message: error.to_string(),
                        },
                        observer,
                    )
                    .await?;
                    if will_retry {
                        continue;
                    }
                    return Err(AgentError::Provider(error));
                }
            }
        }
    }

    async fn execute_tool_call(
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
        let decision = self.options.permission_policy.decide(risk);
        match decision {
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
                    return match self
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
                    };
                }
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
    }

    async fn cancel_tool_calls(
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

    async fn materialize_output(
        &self,
        session: &flash_core::storage::Session,
        call_id: &str,
        stream: &str,
        text: &str,
    ) -> Result<MaterializedOutput, AgentError> {
        if text.len() <= self.options.max_output_bytes {
            return Ok(MaterializedOutput {
                text: text.to_string(),
                artifact: None,
                truncated: false,
            });
        }
        let safe_call_id = sanitize_artifact_component(call_id);
        let artifact = format!("artifacts/{safe_call_id}.{stream}.txt");
        let path: PathBuf = session.path.join(&artifact);
        let full_text = text.to_string();
        let artifact_text = full_text.clone();
        tokio::task::spawn_blocking(move || std::fs::write(&path, artifact_text))
            .await
            .map_err(|error| {
                AgentError::Storage(flash_core::storage::StorageError::TaskJoin(
                    error.to_string(),
                ))
            })?
            .map_err(flash_core::storage::StorageError::Io)?;
        Ok(MaterializedOutput {
            text: format!(
                "{}\n[full output: {}]",
                truncate(&full_text, self.options.max_output_bytes),
                artifact
            ),
            artifact: Some(artifact),
            truncated: true,
        })
    }
}

struct MaterializedOutput {
    text: String,
    artifact: Option<String>,
    truncated: bool,
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

fn sanitize_artifact_component(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let base = if sanitized.is_empty() {
        "call"
    } else {
        &sanitized
    };
    let hash = value
        .bytes()
        .fold(14_695_981_039_346_656_037_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(1_099_511_628_211)
        });
    format!("{base}_{hash:x}")
}

fn tool_context_for_call(context: &ToolContext, call_id: &str) -> ToolContext {
    let mut context = context.clone();
    context.artifact_stem = Some(sanitize_artifact_component(call_id));
    context
}

fn runtime_system_prompt(workspace_root: &Path, tools: &ToolRegistry) -> String {
    let available_tools = tools.names().collect::<Vec<_>>().join(", ");
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".to_string());
    format!(
        "{}\n\n# Runtime environment\n\n- Operating system: {}\n- Shell: {}\n- Working directory: {}\n- Available tools: {}",
        include_str!("../../../prompts/system_default.md").trim_end(),
        std::env::consts::OS,
        shell,
        workspace_root.display(),
        available_tools
    )
}

async fn persist_provider_delta(
    session: &flash_core::storage::Session,
    request_id: &str,
    attempt: u32,
    event: &ProviderEvent,
    observer: &mut impl EventObserver,
) -> Result<bool, AgentError> {
    let agent_event = match event {
        ProviderEvent::ReasoningDelta(text) => Some(Event::ReasoningDelta {
            request_id: request_id.to_string(),
            attempt,
            text: text.clone(),
        }),
        ProviderEvent::TextDelta(text) => Some(Event::AssistantDelta {
            request_id: request_id.to_string(),
            attempt,
            text: text.clone(),
        }),
        _ => None,
    };
    if let Some(event) = agent_event {
        emit_event(session, event, observer).await?;
        Ok(true)
    } else {
        Ok(false)
    }
}

async fn emit_event(
    session: &flash_core::storage::Session,
    event: Event,
    observer: &mut impl EventObserver,
) -> Result<(), AgentError> {
    append_event_async(session.clone(), event.clone()).await?;
    observer.on_event(&event);
    Ok(())
}

async fn finish_session(
    session: &flash_core::storage::Session,
    outcome: Outcome,
    observer: &mut impl EventObserver,
) -> Result<AgentRun, AgentError> {
    if session.begin_finalize() {
        let event_result = emit_event(session, Event::SessionFinished { outcome }, observer).await;
        let metadata_result = finalize_session_async(session.clone(), outcome).await;
        event_result?;
        metadata_result?;
    }
    Ok(AgentRun {
        session_id: session.id.clone(),
        outcome,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRun {
    pub session_id: String,
    pub outcome: Outcome,
}

#[derive(Debug)]
pub enum AgentError {
    Storage(flash_core::storage::StorageError),
    Provider(ProviderError),
    Tool(flash_core::ToolError),
    Finalization { primary: String, finalize: String },
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "{error}"),
            Self::Provider(error) => write!(formatter, "{error}"),
            Self::Tool(error) => write!(formatter, "tool error: {}", error.message),
            Self::Finalization { primary, finalize } => {
                write!(
                    formatter,
                    "{primary}; session finalization also failed: {finalize}"
                )
            }
        }
    }
}

impl std::error::Error for AgentError {}

impl From<flash_core::storage::StorageError> for AgentError {
    fn from(error: flash_core::storage::StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ProviderError> for AgentError {
    fn from(error: ProviderError) -> Self {
        Self::Provider(error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TurnResult {
    assistant_text: String,
    reasoning_text: String,
    tool_calls: Vec<ToolCall>,
    completion: TurnCompletion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnCompletion {
    EndTurn,
    ToolUse,
    Cancelled,
    Failed,
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let truncated = value
        .chars()
        .scan(0, |used, ch| {
            let len = ch.len_utf8();
            if *used + len > max_bytes {
                None
            } else {
                *used += len;
                Some(ch)
            }
        })
        .collect::<String>();
    format!("{truncated}...[truncated]")
}

fn project_history(
    history: &[Message],
    max_bytes: usize,
) -> Result<Vec<Message>, HistoryProjectionError> {
    let turns = conversation_turns(history)?;
    let mut projected_turns = Vec::new();
    let mut used = 0;
    for turn in turns.iter().rev() {
        let size = turn.iter().map(message_size).sum::<usize>();
        if !projected_turns.is_empty() && used + size > max_bytes {
            break;
        }
        used += size;
        projected_turns.push(turn);
    }
    projected_turns.reverse();
    let projected = projected_turns
        .into_iter()
        .flat_map(|turn| turn.iter().cloned())
        .collect::<Vec<_>>();
    validate_tool_turns(&projected)?;
    Ok(projected)
}

fn conversation_turns(history: &[Message]) -> Result<Vec<Vec<Message>>, HistoryProjectionError> {
    validate_tool_turns(history)?;
    let mut turns = Vec::<Vec<Message>>::new();
    for message in history {
        match message.role {
            Role::System | Role::User => turns.push(vec![message.clone()]),
            Role::Assistant => {
                if turns
                    .last()
                    .and_then(|turn| turn.last())
                    .is_some_and(|previous| previous.role == Role::User)
                {
                    if let Some(turn) = turns.last_mut() {
                        turn.push(message.clone());
                    }
                } else {
                    turns.push(vec![message.clone()]);
                }
            }
            Role::Tool => {
                let Some(turn) = turns.last_mut() else {
                    return Err(HistoryProjectionError::ToolMessageWithoutAssistant);
                };
                if !turn
                    .iter()
                    .any(|candidate| candidate.role == Role::Assistant)
                {
                    return Err(HistoryProjectionError::ToolMessageWithoutAssistant);
                }
                turn.push(message.clone());
            }
        }
    }
    Ok(turns)
}

fn validate_tool_turns(history: &[Message]) -> Result<(), HistoryProjectionError> {
    let mut pending = BTreeSet::new();
    for message in history {
        match message.role {
            Role::Assistant => {
                if !pending.is_empty() {
                    return Err(HistoryProjectionError::MissingToolResults(
                        pending.into_iter().collect(),
                    ));
                }
                for block in &message.content {
                    if let ContentBlock::ToolUse { call_id, .. } = block {
                        if !pending.insert(call_id.clone()) {
                            return Err(HistoryProjectionError::DuplicateToolUse(call_id.clone()));
                        }
                    }
                }
            }
            Role::Tool => {
                for block in &message.content {
                    if let ContentBlock::ToolResult { call_id, .. } = block {
                        if !pending.remove(call_id) {
                            return Err(HistoryProjectionError::OrphanToolResult(call_id.clone()));
                        }
                    }
                }
            }
            Role::System | Role::User => {
                if !pending.is_empty() {
                    return Err(HistoryProjectionError::MissingToolResults(
                        pending.into_iter().collect(),
                    ));
                }
            }
        }
    }
    if pending.is_empty() {
        Ok(())
    } else {
        Err(HistoryProjectionError::MissingToolResults(
            pending.into_iter().collect(),
        ))
    }
}

fn message_size(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len(),
            ContentBlock::ToolUse {
                call_id,
                name,
                input,
            } => call_id.len() + name.len() + input.to_string().len(),
            ContentBlock::ToolResult { call_id, .. } => call_id.len(),
        })
        .sum()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HistoryProjectionError {
    ToolMessageWithoutAssistant,
    DuplicateToolUse(String),
    OrphanToolResult(String),
    MissingToolResults(Vec<String>),
}

impl std::fmt::Display for HistoryProjectionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ToolMessageWithoutAssistant => {
                write!(
                    formatter,
                    "history contains a tool message without an assistant"
                )
            }
            Self::DuplicateToolUse(call_id) => {
                write!(formatter, "history contains duplicate tool use `{call_id}`")
            }
            Self::OrphanToolResult(call_id) => {
                write!(formatter, "history contains orphan tool result `{call_id}`")
            }
            Self::MissingToolResults(call_ids) => {
                write!(
                    formatter,
                    "history is missing tool results for {}",
                    call_ids.join(", ")
                )
            }
        }
    }
}

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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    use flash_core::{PermissionPolicy, Tool, ToolError, ToolOutput, ToolRisk};
    use serde_json::{json, Value};

    use super::*;

    #[tokio::test]
    async fn run_task_should_execute_search_tool_and_finish() {
        let root = temp_dir("search_loop");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        let mut runtime = AgentRuntime::new(
            SmokeProvider::new(),
            flash_tools_for_tests(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 3,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Confirm),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "list files").await.unwrap();

        assert_eq!(run.outcome, Outcome::Succeeded);
    }

    #[tokio::test]
    async fn deepseek_sse_tool_call_should_execute_real_read_tool_and_return_result() {
        let root = temp_dir("deepseek_read_e2e");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("README.md"), "hello from tool").unwrap();
        let mut runtime = AgentRuntime::new(
            DeepSeekSseProvider { turn: 0 },
            flash_tools::builtin_registry().unwrap(),
            AgentOptions {
                model: "deepseek-chat".to_string(),
                max_turns: 2,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "read README").await.unwrap();

        assert_eq!(run.outcome, Outcome::Succeeded);
        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(messages.contains("hello from tool"));
    }

    #[tokio::test]
    async fn run_task_should_update_session_metadata_on_success() {
        let root = temp_dir("session_success_status");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        let mut runtime = AgentRuntime::new(
            SmokeProvider::new(),
            flash_tools_for_tests(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 3,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Confirm),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "list files").await.unwrap();

        let session_json = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("session.json"),
        )
        .unwrap();
        assert!(session_json.contains("\"status\":\"succeeded\""));
    }

    #[tokio::test]
    async fn run_task_should_write_error_tool_result_for_unknown_tool() {
        let root = temp_dir("unknown_tool");
        fs::create_dir_all(&root).unwrap();
        let mut runtime = AgentRuntime::new(
            UnknownToolProvider,
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
    async fn run_task_should_not_commit_partial_assistant_without_done() {
        let root = temp_dir("partial");
        fs::create_dir_all(&root).unwrap();
        let mut runtime = AgentRuntime::new(
            PartialProvider,
            ToolRegistry::new(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "partial").await.unwrap();

        assert_eq!(run.outcome, Outcome::Failed);
    }

    #[tokio::test]
    async fn run_task_should_fail_when_provider_stops_at_max_tokens() {
        let root = temp_dir("max_tokens");
        fs::create_dir_all(&root).unwrap();
        let mut runtime = AgentRuntime::new(
            MaxTokensProvider,
            ToolRegistry::new(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "max tokens").await.unwrap();

        assert_eq!(run.outcome, Outcome::Failed);
    }

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
            let mut runtime = AgentRuntime::new(
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
        let root = temp_dir("max_turns");
        fs::create_dir_all(&root).unwrap();
        let mut runtime = AgentRuntime::new(
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

    #[tokio::test]
    async fn run_task_should_retry_retryable_provider_errors() {
        let root = temp_dir("retry");
        fs::create_dir_all(&root).unwrap();
        let mut runtime = AgentRuntime::new(
            RetryProvider { calls: 0 },
            ToolRegistry::new(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "retry").await.unwrap();

        assert_eq!(run.outcome, Outcome::Succeeded);
    }

    #[tokio::test]
    async fn run_task_should_not_retry_after_publishing_delta() {
        let root = temp_dir("published_delta_retry");
        fs::create_dir_all(&root).unwrap();
        let calls = Arc::new(AtomicU32::new(0));
        let mut runtime = AgentRuntime::new(
            PartialRetryProvider {
                calls: Arc::clone(&calls),
            },
            ToolRegistry::new(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let error = runtime.run_task(&root, "retry").await.unwrap_err();

        assert!(matches!(error, AgentError::Provider(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let session_path = only_session_path(&root);
        let metadata = fs::read_to_string(session_path.join("session.json")).unwrap();
        assert!(metadata.contains("\"status\":\"failed\""));
        let events = fs::read_to_string(session_path.join("events.jsonl")).unwrap();
        assert_eq!(events.matches("\"type\":\"assistant_delta\"").count(), 1);
        assert_eq!(events.matches("\"type\":\"session_finished\"").count(), 1);
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
        let mut observer = NoopObserver;
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

    #[tokio::test]
    async fn run_task_with_cancellation_should_interrupt_provider_stream() {
        let root = temp_dir("provider_cancel");
        fs::create_dir_all(&root).unwrap();
        let mut runtime = AgentRuntime::new(
            CancellableProvider,
            ToolRegistry::new(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );
        let cancellation = CancellationToken::new();
        let cancel_from_thread = cancellation.clone();
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(25));
            cancel_from_thread.cancel();
        });
        let mut observer = NoopObserver;
        let mut approval = RejectingApproval;

        let run = runtime
            .run_task_with_cancellation(
                &root,
                "cancel provider",
                &mut observer,
                cancellation,
                &mut approval,
            )
            .await
            .unwrap();
        cancel_thread.join().unwrap();

        assert_eq!(run.outcome, Outcome::Cancelled);
        let session_path = root.join(".flash").join("sessions").join(&run.session_id);
        let metadata = fs::read_to_string(session_path.join("session.json")).unwrap();
        let events = fs::read_to_string(session_path.join("events.jsonl")).unwrap();
        assert!(metadata.contains("\"status\":\"cancelled\""));
        assert_eq!(events.matches("\"type\":\"session_finished\"").count(), 1);
        assert!(events.contains("\"outcome\":\"cancelled\""));
    }

    #[tokio::test]
    async fn run_task_should_write_large_tool_output_to_artifact() {
        let root = temp_dir("artifact");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(LargeTool)).unwrap();
        let mut runtime = AgentRuntime::new(
            LargeToolProvider,
            registry,
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 1,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 4,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "large").await.unwrap();

        let artifact_name = format!("{}.stdout.txt", sanitize_artifact_component("call_large"));
        let artifact = root
            .join(".flash")
            .join("sessions")
            .join(&run.session_id)
            .join("artifacts")
            .join(&artifact_name);
        assert!(artifact.exists());
        assert_eq!(fs::read_to_string(&artifact).unwrap(), "abcdef");

        let session_dir = root.join(".flash").join("sessions").join(&run.session_id);
        let events = fs::read_to_string(session_dir.join("events.jsonl")).unwrap();
        let messages = fs::read_to_string(session_dir.join("messages.jsonl")).unwrap();
        assert!(events.contains(&format!("[full output: artifacts/{artifact_name}]")));
        assert!(messages.contains(&format!("[full output: artifacts/{artifact_name}]")));
    }

    #[test]
    fn artifact_name_should_not_allow_path_traversal() {
        let name = sanitize_artifact_component("../../outside/evil");

        assert!(!name.contains('/'));
        assert!(!name.contains(".."));
        assert!(name.starts_with("______outside_evil_"));
    }

    #[tokio::test]
    async fn run_task_should_write_error_result_when_tool_fails() {
        let root = temp_dir("tool_error");
        fs::create_dir_all(&root).unwrap();
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(ErrorTool)).unwrap();
        let mut runtime = AgentRuntime::new(
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
        let mut observer = NoopObserver;
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
        let mut runtime = AgentRuntime::new(
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
        let mut runtime = AgentRuntime::new(
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
    async fn run_task_with_observer_should_emit_live_events_after_storage_commit() {
        let root = temp_dir("observer");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        let mut runtime = AgentRuntime::new(
            SmokeProvider::new(),
            flash_tools_for_tests(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 3,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Confirm),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );
        let mut observed = Vec::new();

        let run = runtime
            .run_task_with_observer(&root, "list files", |event: &Event| {
                observed.push(event.event_type().to_string());
            })
            .await
            .unwrap();

        let events = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.contains(observed.first().unwrap()));
    }

    #[tokio::test]
    async fn mock_provider_scenarios_should_cover_loop_outcomes() {
        let scenarios = [
            (
                "success",
                AgentRuntime::new(
                    SmokeProvider::new(),
                    flash_tools_for_tests(),
                    AgentOptions {
                        model: "smoke".to_string(),
                        max_turns: 3,
                        permission_policy: PermissionPolicy::new(
                            flash_core::tools::ApprovalMode::Confirm,
                        ),
                        max_output_bytes: 200_000,
                        max_prompt_bytes: 200_000,
                    },
                )
                .run_task(&prepared_workspace("scenario_success"), "list files")
                .await
                .unwrap()
                .outcome,
                Outcome::Succeeded,
            ),
            (
                "unknown_tool",
                AgentRuntime::new(
                    UnknownToolProvider,
                    ToolRegistry::new(),
                    AgentOptions {
                        model: "smoke".to_string(),
                        max_turns: 1,
                        permission_policy: PermissionPolicy::new(
                            flash_core::tools::ApprovalMode::Yolo,
                        ),
                        max_output_bytes: 200_000,
                        max_prompt_bytes: 200_000,
                    },
                )
                .run_task(
                    &prepared_workspace("scenario_unknown_tool"),
                    "use missing tool",
                )
                .await
                .unwrap()
                .outcome,
                Outcome::Failed,
            ),
            (
                "tool_failure",
                {
                    let root = prepared_workspace("scenario_tool_failure");
                    let mut registry = ToolRegistry::new();
                    registry.register(Box::new(ErrorTool)).unwrap();
                    AgentRuntime::new(
                        ErrorToolProvider,
                        registry,
                        AgentOptions {
                            model: "smoke".to_string(),
                            max_turns: 1,
                            permission_policy: PermissionPolicy::new(
                                flash_core::tools::ApprovalMode::Yolo,
                            ),
                            max_output_bytes: 200_000,
                            max_prompt_bytes: 200_000,
                        },
                    )
                    .run_task(&root, "error")
                    .await
                    .unwrap()
                    .outcome
                },
                Outcome::Failed,
            ),
            (
                "max_turns",
                AgentRuntime::new(
                    LoopProvider,
                    flash_tools_for_tests(),
                    AgentOptions {
                        model: "smoke".to_string(),
                        max_turns: 1,
                        permission_policy: PermissionPolicy::new(
                            flash_core::tools::ApprovalMode::Confirm,
                        ),
                        max_output_bytes: 200_000,
                        max_prompt_bytes: 200_000,
                    },
                )
                .run_task(&prepared_workspace("scenario_max_turns"), "loop")
                .await
                .unwrap()
                .outcome,
                Outcome::Failed,
            ),
            (
                "cancel",
                {
                    let root = prepared_workspace("scenario_cancel");
                    let mut runtime = AgentRuntime::new(
                        SmokeProvider::new(),
                        flash_tools_for_tests(),
                        AgentOptions {
                            model: "smoke".to_string(),
                            max_turns: 3,
                            permission_policy: PermissionPolicy::new(
                                flash_core::tools::ApprovalMode::Confirm,
                            ),
                            max_output_bytes: 200_000,
                            max_prompt_bytes: 200_000,
                        },
                    );
                    let mut observer = NoopObserver;
                    runtime
                        .run_task_controlled(&root, "list files", &mut observer, || true)
                        .await
                        .unwrap()
                        .outcome
                },
                Outcome::Cancelled,
            ),
        ];

        for (name, actual, expected) in scenarios {
            assert_eq!(actual, expected, "scenario {name}");
        }
    }

    #[tokio::test]
    async fn rust_fixture_smoke_should_fix_failing_test_and_keep_diff() {
        let root = temp_dir("fixture_fix");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"fixture_fix\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub fn answer() -> i32 {\n    41\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn answer_should_be_42() {\n        assert_eq!(answer(), 42);\n    }\n}\n",
        )
        .unwrap();
        let git_init = std::process::Command::new("git")
            .args(["init"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(git_init.status.success());
        let git_add = std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(git_add.status.success());
        let git_commit = std::process::Command::new("git")
            .args(["commit", "-m", "baseline"])
            .env("GIT_AUTHOR_NAME", "Flash Test")
            .env("GIT_AUTHOR_EMAIL", "flash@example.com")
            .env("GIT_COMMITTER_NAME", "Flash Test")
            .env("GIT_COMMITTER_EMAIL", "flash@example.com")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(git_commit.status.success());
        let mut runtime = AgentRuntime::new(
            SmokeProvider::new(),
            flash_tools::builtin_registry().unwrap(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 8,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Yolo),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );

        let run = runtime.run_task(&root, "fix failing tests").await.unwrap();

        assert_eq!(run.outcome, Outcome::Succeeded);
        let test_output = std::process::Command::new("cargo")
            .arg("test")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(test_output.status.success());
        let diff = std::process::Command::new("git")
            .args(["diff", "--", "src/lib.rs"])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&diff.stdout).contains("+    42"),
            "diff should include the fixture fix"
        );
    }

    #[tokio::test]
    async fn run_task_controlled_should_cancel_without_committing_assistant_message() {
        let root = temp_dir("controlled_cancel");
        fs::create_dir_all(&root).unwrap();
        let mut runtime = AgentRuntime::new(
            SmokeProvider::new(),
            flash_tools_for_tests(),
            AgentOptions {
                model: "smoke".to_string(),
                max_turns: 3,
                permission_policy: PermissionPolicy::new(flash_core::tools::ApprovalMode::Confirm),
                max_output_bytes: 200_000,
                max_prompt_bytes: 200_000,
            },
        );
        let mut observer = NoopObserver;

        let run = runtime
            .run_task_controlled(&root, "list files", &mut observer, || true)
            .await
            .unwrap();

        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(!messages.contains("\"role\":\"assistant\""));
    }

    #[tokio::test]
    async fn project_history_should_keep_recent_messages_within_budget() {
        let old = test_message("old text");
        let recent = test_message("new");

        let projected = project_history(&[old, recent.clone()], 3).unwrap();

        assert_eq!(projected, vec![recent]);
    }

    #[test]
    fn project_history_should_keep_complete_tool_turn_when_budget_is_tiny() {
        let old = test_message("old");
        let user = test_message("task");
        let assistant = Message {
            id: "assistant".to_string(),
            role: Role::Assistant,
            created_at: "0".to_string(),
            content: vec![ContentBlock::ToolUse {
                call_id: "call_1".to_string(),
                name: "Read".to_string(),
                input: json!({"path": "README.md"}),
            }],
        };
        let tool = Message {
            id: "tool".to_string(),
            role: Role::Tool,
            created_at: "0".to_string(),
            content: vec![
                ContentBlock::ToolResult {
                    call_id: "call_1".to_string(),
                    status: ToolResultStatus::Success,
                },
                ContentBlock::Text {
                    text: "result".to_string(),
                },
            ],
        };

        let projected =
            project_history(&[old, user.clone(), assistant.clone(), tool.clone()], 1).unwrap();

        assert_eq!(projected, vec![user, assistant, tool]);
    }

    #[test]
    fn project_history_should_reject_orphan_tool_result() {
        let tool = Message {
            id: "tool".to_string(),
            role: Role::Tool,
            created_at: "0".to_string(),
            content: vec![ContentBlock::ToolResult {
                call_id: "call_1".to_string(),
                status: ToolResultStatus::Success,
            }],
        };

        let error = project_history(&[tool], 100).unwrap_err();

        assert_eq!(
            error,
            HistoryProjectionError::OrphanToolResult("call_1".to_string())
        );
    }

    struct UnknownToolProvider;

    struct DeepSeekSseProvider {
        turn: u32,
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

    struct PartialProvider;

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

    struct LoopProvider;

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

    struct MaxTokensProvider;

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

    struct InvalidCompletionProvider {
        reason: StopReason,
        include_tool_call: bool,
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

    struct RetryProvider {
        calls: u32,
    }

    struct PartialRetryProvider {
        calls: Arc<AtomicU32>,
    }

    struct CancellableProvider;

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
            Err(ProviderError::Server("stream failed".to_string()))
        }
    }

    struct MultipleToolProvider;

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
                return Err(ProviderError::RateLimited("rate limited".to_string()));
            }
            send_event(&events, ProviderEvent::TextDelta("ok".to_string())).await?;
            send_event(&events, ProviderEvent::Done(StopReason::EndTurn)).await?;
            Ok(())
        }
    }

    struct LargeToolProvider;

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

    struct ErrorToolProvider;

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

    struct ExecuteToolProvider;

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

    struct CancelToolProvider;

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

    struct DestructiveToolProvider;

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

    struct FakeTool;

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

    struct LargeTool;

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

    struct ErrorTool;

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

    struct ExecuteTool;

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

    struct ApprovingApproval;

    impl ApprovalController for ApprovingApproval {
        fn approve(&mut self, _request: &ApprovalRequest) -> bool {
            true
        }
    }

    struct CancelTool;

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

    struct DestructiveTool;

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

    fn flash_tools_for_tests() -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(FakeTool)).unwrap();
        registry.register(Box::new(SearchFakeTool)).unwrap();
        registry
    }

    struct SearchFakeTool;

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

    fn temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("flash_agent_{name}_{nanos}"))
    }

    fn prepared_workspace(name: &str) -> PathBuf {
        let root = temp_dir(name);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        root
    }

    fn only_session_path(root: &Path) -> PathBuf {
        fs::read_dir(root.join(".flash/sessions"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
    }

    fn test_message(text: &str) -> Message {
        Message {
            id: text.to_string(),
            role: Role::User,
            created_at: "0".to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
        }
    }
}
