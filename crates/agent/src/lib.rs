use std::fs;
use std::path::{Path, PathBuf};

use flash_core::{
    append_assistant_message, append_event, append_tool_result_message, append_user_message,
    create_session, ContentBlock, Event, Message, Outcome, PermissionDecision, PermissionPolicy,
    Role, ToolContext, ToolExitStatus, ToolRegistry, ToolResultStatus, ToolRisk,
};
use flash_provider::{
    ChatProvider, ChatRequest, ProviderError, ProviderEvent, StopReason, ToolCall, ToolSpec, Usage,
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

    pub fn run_task(&mut self, workspace_root: &Path, task: &str) -> Result<AgentRun, AgentError> {
        self.run_task_with_observer(workspace_root, task, NoopObserver)
    }

    pub fn run_task_with_observer<O>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        mut observer: O,
    ) -> Result<AgentRun, AgentError>
    where
        O: EventObserver,
    {
        self.run_task_controlled(workspace_root, task, &mut observer, || false)
    }

    pub fn run_task_controlled<O, C>(
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
    }

    pub fn run_task_with_controls<O, C, A>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        observer: &mut O,
        mut should_cancel: C,
        approval: &mut A,
    ) -> Result<AgentRun, AgentError>
    where
        O: EventObserver,
        C: FnMut() -> bool,
        A: ApprovalController,
    {
        let session = create_session(workspace_root)?;
        let user = append_user_message(&session, task)?;
        let mut history = vec![user];
        let context = ToolContext {
            workspace_root: workspace_root.to_path_buf(),
        };

        for turn in 1..=self.options.max_turns {
            if should_cancel() {
                emit_event(
                    &session,
                    Event::Error {
                        message: "run cancelled".to_string(),
                    },
                    observer,
                )?;
                emit_event(
                    &session,
                    Event::SessionFinished {
                        outcome: Outcome::Cancelled,
                    },
                    observer,
                )?;
                return Ok(AgentRun {
                    session_id: session.id,
                    outcome: Outcome::Cancelled,
                });
            }

            emit_event(
                &session,
                Event::ModelRequestStarted {
                    request_id: format!("request_{turn}"),
                    model: self.options.model.clone(),
                },
                observer,
            )?;
            let request = ChatRequest {
                messages: project_history(&history, self.options.max_prompt_bytes),
                tools: self
                    .tools
                    .names()
                    .map(|name| ToolSpec {
                        name: name.to_string(),
                    })
                    .collect(),
                model: self.options.model.clone(),
            };
            let provider_events = self.chat_with_retry(request)?;
            let turn_result = self.handle_provider_events(&session, provider_events, observer)?;
            let Some(turn_result) = turn_result else {
                return Ok(AgentRun {
                    session_id: session.id,
                    outcome: Outcome::Cancelled,
                });
            };

            if should_cancel() {
                emit_event(
                    &session,
                    Event::Error {
                        message: "run cancelled".to_string(),
                    },
                    observer,
                )?;
                emit_event(
                    &session,
                    Event::SessionFinished {
                        outcome: Outcome::Cancelled,
                    },
                    observer,
                )?;
                return Ok(AgentRun {
                    session_id: session.id,
                    outcome: Outcome::Cancelled,
                });
            }

            let assistant = append_assistant_message(
                &session,
                &turn_result.assistant_text,
                &turn_result
                    .tool_calls
                    .iter()
                    .map(|call| (call.call_id.clone(), call.name.clone()))
                    .collect::<Vec<_>>(),
            )?;
            history.push(assistant);

            if turn_result.tool_calls.is_empty() {
                emit_event(
                    &session,
                    Event::SessionFinished {
                        outcome: Outcome::Succeeded,
                    },
                    observer,
                )?;
                return Ok(AgentRun {
                    session_id: session.id,
                    outcome: Outcome::Succeeded,
                });
            }

            for call in turn_result.tool_calls {
                if should_cancel() {
                    emit_event(
                        &session,
                        Event::Error {
                            message: "run cancelled".to_string(),
                        },
                        observer,
                    )?;
                    emit_event(
                        &session,
                        Event::SessionFinished {
                            outcome: Outcome::Cancelled,
                        },
                        observer,
                    )?;
                    return Ok(AgentRun {
                        session_id: session.id,
                        outcome: Outcome::Cancelled,
                    });
                }
                let message =
                    self.execute_tool_call(&session, &context, &call, observer, approval)?;
                history.push(message);
            }
        }

        emit_event(
            &session,
            Event::Error {
                message: "max_turns exceeded".to_string(),
            },
            observer,
        )?;
        emit_event(
            &session,
            Event::SessionFinished {
                outcome: Outcome::Failed,
            },
            observer,
        )?;
        Ok(AgentRun {
            session_id: session.id,
            outcome: Outcome::Failed,
        })
    }

    fn handle_provider_events(
        &self,
        session: &flash_core::storage::Session,
        provider_events: Vec<ProviderEvent>,
        observer: &mut impl EventObserver,
    ) -> Result<Option<TurnResult>, AgentError> {
        let mut assistant_text = String::new();
        let mut tool_calls = Vec::new();
        let mut saw_done = false;

        for event in provider_events {
            match event {
                ProviderEvent::ReasoningDelta(text) => {
                    emit_event(session, Event::ReasoningDelta { text }, observer)?;
                }
                ProviderEvent::TextDelta(text) => {
                    assistant_text.push_str(&text);
                    emit_event(session, Event::AssistantDelta { text }, observer)?;
                }
                ProviderEvent::ToolCallComplete(call) => {
                    emit_event(
                        session,
                        Event::ToolCallRequested {
                            call_id: call.call_id.clone(),
                            name: call.name.clone(),
                        },
                        observer,
                    )?;
                    tool_calls.push(call);
                }
                ProviderEvent::Usage(Usage {
                    input_tokens,
                    output_tokens,
                }) => {
                    emit_event(
                        session,
                        Event::UsageRecorded {
                            input_tokens,
                            output_tokens,
                        },
                        observer,
                    )?;
                }
                ProviderEvent::Done(StopReason::EndTurn | StopReason::ToolUse) => {
                    saw_done = true;
                }
                ProviderEvent::Done(StopReason::MaxTokens) => {
                    saw_done = true;
                    emit_event(
                        session,
                        Event::Error {
                            message: "provider stopped at max tokens".to_string(),
                        },
                        observer,
                    )?;
                }
            }
        }

        if !saw_done {
            emit_event(
                session,
                Event::Error {
                    message: "model stream ended before done".to_string(),
                },
                observer,
            )?;
            emit_event(
                session,
                Event::SessionFinished {
                    outcome: Outcome::Cancelled,
                },
                observer,
            )?;
            return Ok(None);
        }

        Ok(Some(TurnResult {
            assistant_text,
            tool_calls,
        }))
    }

    fn chat_with_retry(&mut self, request: ChatRequest) -> Result<Vec<ProviderEvent>, AgentError> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            match self.provider.chat(request.clone()) {
                Ok(events) => return Ok(events),
                Err(error) if error.is_retryable() && attempts < 3 => continue,
                Err(error) => return Err(AgentError::Provider(error)),
            }
        }
    }

    fn execute_tool_call(
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
            )?;
            emit_event(
                session,
                Event::ToolFinished {
                    call_id: call.call_id.clone(),
                    status: ToolResultStatus::Error,
                },
                observer,
            )?;
            return append_tool_result_message(
                session,
                &call.call_id,
                ToolResultStatus::Error,
                "unknown tool",
            )
            .map_err(AgentError::Storage);
        };

        let risk = tool.risk(&call.input);
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
                )?;
                emit_event(
                    session,
                    Event::ToolStarted {
                        call_id: call.call_id.clone(),
                        name: call.name.clone(),
                    },
                    observer,
                )?;
                match tool.call(&call.input, context) {
                    Ok(output) => self.commit_tool_output(session, call, output, observer),
                    Err(error) => {
                        emit_event(
                            session,
                            Event::Error {
                                message: error.message.clone(),
                            },
                            observer,
                        )?;
                        emit_event(
                            session,
                            Event::ToolFinished {
                                call_id: call.call_id.clone(),
                                status: ToolResultStatus::Error,
                            },
                            observer,
                        )?;
                        append_tool_result_message(
                            session,
                            &call.call_id,
                            ToolResultStatus::Error,
                            &error.message,
                        )
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
                )?;
                let approved = approval.approve(&ApprovalRequest {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    input: call.input.clone(),
                    risk,
                });
                emit_event(
                    session,
                    Event::ApprovalResolved {
                        call_id: call.call_id.clone(),
                        approved,
                    },
                    observer,
                )?;
                if approved {
                    emit_event(
                        session,
                        Event::ToolStarted {
                            call_id: call.call_id.clone(),
                            name: call.name.clone(),
                        },
                        observer,
                    )?;
                    return match tool.call(&call.input, context) {
                        Ok(output) => self.commit_tool_output(session, call, output, observer),
                        Err(error) => {
                            emit_event(
                                session,
                                Event::Error {
                                    message: error.message.clone(),
                                },
                                observer,
                            )?;
                            emit_event(
                                session,
                                Event::ToolFinished {
                                    call_id: call.call_id.clone(),
                                    status: ToolResultStatus::Error,
                                },
                                observer,
                            )?;
                            append_tool_result_message(
                                session,
                                &call.call_id,
                                ToolResultStatus::Error,
                                &error.message,
                            )
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
                )?;
                append_tool_result_message(
                    session,
                    &call.call_id,
                    ToolResultStatus::Rejected,
                    "tool call rejected by permission policy",
                )
                .map_err(AgentError::Storage)
            }
            PermissionDecision::Deny => {
                emit_event(
                    session,
                    Event::ApprovalRequired {
                        call_id: call.call_id.clone(),
                    },
                    observer,
                )?;
                emit_event(
                    session,
                    Event::ApprovalResolved {
                        call_id: call.call_id.clone(),
                        approved: false,
                    },
                    observer,
                )?;
                emit_event(
                    session,
                    Event::ToolFinished {
                        call_id: call.call_id.clone(),
                        status: ToolResultStatus::Rejected,
                    },
                    observer,
                )?;
                append_tool_result_message(
                    session,
                    &call.call_id,
                    ToolResultStatus::Rejected,
                    "tool call rejected by permission policy",
                )
                .map_err(AgentError::Storage)
            }
        }
    }

    fn commit_tool_output(
        &self,
        session: &flash_core::storage::Session,
        call: &ToolCall,
        output: flash_core::ToolOutput,
        observer: &mut impl EventObserver,
    ) -> Result<Message, AgentError> {
        let stdout = self.materialize_output(session, &call.call_id, "stdout", &output.stdout)?;
        let stderr = self.materialize_output(session, &call.call_id, "stderr", &output.stderr)?;
        if !stdout.is_empty() {
            emit_event(
                session,
                Event::ToolOutputDelta {
                    call_id: call.call_id.clone(),
                    stream: "stdout".to_string(),
                    text: stdout.clone(),
                },
                observer,
            )?;
        }
        if !stderr.is_empty() {
            emit_event(
                session,
                Event::ToolOutputDelta {
                    call_id: call.call_id.clone(),
                    stream: "stderr".to_string(),
                    text: stderr.clone(),
                },
                observer,
            )?;
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
        )?;
        append_tool_result_message(
            session,
            &call.call_id,
            status,
            &format!("stdout:\n{stdout}\nstderr:\n{stderr}"),
        )
        .map_err(AgentError::Storage)
    }

    fn materialize_output(
        &self,
        session: &flash_core::storage::Session,
        call_id: &str,
        stream: &str,
        text: &str,
    ) -> Result<String, AgentError> {
        if text.len() <= self.options.max_output_bytes {
            return Ok(text.to_string());
        }
        let artifact = format!("artifacts/{call_id}.{stream}.txt");
        let path: PathBuf = session.path.join(&artifact);
        fs::write(&path, text).map_err(flash_core::storage::StorageError::Io)?;
        Ok(format!(
            "{}\n[full output: {}]",
            truncate(text, self.options.max_output_bytes),
            artifact
        ))
    }
}

fn emit_event(
    session: &flash_core::storage::Session,
    event: Event,
    observer: &mut impl EventObserver,
) -> Result<(), AgentError> {
    append_event(session, event.clone())?;
    observer.on_event(&event);
    Ok(())
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
}

impl std::fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(error) => write!(formatter, "{error}"),
            Self::Provider(error) => write!(formatter, "{error}"),
            Self::Tool(error) => write!(formatter, "tool error: {}", error.message),
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
    tool_calls: Vec<ToolCall>,
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

fn project_history(history: &[Message], max_bytes: usize) -> Vec<Message> {
    let mut projected = Vec::new();
    let mut used = 0;
    for message in history.iter().rev() {
        let size = message_size(message);
        if !projected.is_empty() && used + size > max_bytes {
            break;
        }
        used += size;
        projected.push(message.clone());
    }
    projected.reverse();
    projected
}

fn message_size(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } | ContentBlock::Reasoning { text } => text.len(),
            ContentBlock::ToolUse { call_id, name } => call_id.len() + name.len(),
            ContentBlock::ToolResult { call_id, .. } => call_id.len(),
        })
        .sum()
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

impl ChatProvider for SmokeProvider {
    fn chat(&mut self, request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
        self.turn += 1;
        if request
            .messages
            .iter()
            .any(|message| matches!(message.role, Role::Tool))
            && !latest_user_task(&request).contains("fix failing tests")
        {
            return Ok(vec![
                ProviderEvent::TextDelta("Done.".to_string()),
                ProviderEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                }),
                ProviderEvent::Done(StopReason::EndTurn),
            ]);
        }
        let task = latest_user_task(&request);
        if task.contains("fix failing tests") {
            return Ok(fix_failing_tests_events(tool_result_count(&request)));
        }
        if task.contains("list files") {
            return Ok(vec![
                ProviderEvent::ReasoningDelta("Need inspect workspace files.".to_string()),
                ProviderEvent::TextDelta("I will list matching files.".to_string()),
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_list_files_1".to_string(),
                    name: "ListFiles".to_string(),
                    input: ".".to_string(),
                }),
                ProviderEvent::Usage(Usage {
                    input_tokens: 20,
                    output_tokens: 6,
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ]);
        }
        Ok(vec![
            ProviderEvent::TextDelta("Task stored for the next runtime stage.".to_string()),
            ProviderEvent::Done(StopReason::EndTurn),
        ])
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
                input: "src/lib.rs".to_string(),
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
                input: concat!(
                    "src/lib.rs\n",
                    "---FIND---\n",
                    "pub fn answer() -> i32 {\n    41\n}\n",
                    "---REPLACE---\n",
                    "pub fn answer() -> i32 {\n    42\n}\n"
                )
                .to_string(),
            }),
            ProviderEvent::Done(StopReason::ToolUse),
        ],
        2 => vec![
            ProviderEvent::TextDelta("Now I will run the test suite.".to_string()),
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_tests_1".to_string(),
                name: "Bash".to_string(),
                input: "cargo test".to_string(),
            }),
            ProviderEvent::Done(StopReason::ToolUse),
        ],
        3 => vec![
            ProviderEvent::TextDelta("Tests passed; I will collect the diff.".to_string()),
            ProviderEvent::ToolCallComplete(ToolCall {
                call_id: "call_diff_1".to_string(),
                name: "Bash".to_string(),
                input: "git diff --".to_string(),
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use flash_core::{PermissionPolicy, Tool, ToolError, ToolOutput, ToolRisk};

    use super::*;

    #[test]
    fn run_task_should_execute_search_tool_and_finish() {
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

        let run = runtime.run_task(&root, "list files").unwrap();

        assert_eq!(run.outcome, Outcome::Succeeded);
    }

    #[test]
    fn run_task_should_write_error_tool_result_for_unknown_tool() {
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

        let run = runtime.run_task(&root, "use missing tool").unwrap();

        assert_eq!(run.outcome, Outcome::Failed);
    }

    #[test]
    fn run_task_should_not_commit_partial_assistant_without_done() {
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

        let run = runtime.run_task(&root, "partial").unwrap();

        assert_eq!(run.outcome, Outcome::Cancelled);
    }

    #[test]
    fn run_task_should_stop_at_max_turns() {
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

        let run = runtime.run_task(&root, "loop").unwrap();

        assert_eq!(run.outcome, Outcome::Failed);
    }

    #[test]
    fn run_task_should_retry_retryable_provider_errors() {
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

        let run = runtime.run_task(&root, "retry").unwrap();

        assert_eq!(run.outcome, Outcome::Succeeded);
    }

    #[test]
    fn run_task_should_write_large_tool_output_to_artifact() {
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

        let run = runtime.run_task(&root, "large").unwrap();

        let artifact = root
            .join(".flash")
            .join("sessions")
            .join(&run.session_id)
            .join("artifacts/call_large.stdout.txt");
        assert!(artifact.exists());
        assert_eq!(fs::read_to_string(&artifact).unwrap(), "abcdef");

        let session_dir = root.join(".flash").join("sessions").join(&run.session_id);
        let events = fs::read_to_string(session_dir.join("events.jsonl")).unwrap();
        let messages = fs::read_to_string(session_dir.join("messages.jsonl")).unwrap();
        assert!(events.contains("[full output: artifacts/call_large.stdout.txt]"));
        assert!(messages.contains("[full output: artifacts/call_large.stdout.txt]"));
    }

    #[test]
    fn run_task_should_write_error_result_when_tool_fails() {
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

        let run = runtime.run_task(&root, "error").unwrap();

        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(messages.contains("\"status\":\"error\""));
    }

    #[test]
    fn run_task_should_write_rejected_result_when_policy_denies() {
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

        let run = runtime.run_task(&root, "execute").unwrap();

        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(messages.contains("\"status\":\"rejected\""));
    }

    #[test]
    fn run_task_with_controls_should_execute_approved_tool_call() {
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

    #[test]
    fn run_task_should_require_approval_for_destructive_tool_even_in_yolo() {
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

        let run = runtime.run_task(&root, "destructive").unwrap();

        let events = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("events.jsonl"),
        )
        .unwrap();
        assert!(events.contains("\"type\":\"approval_required\""));
    }

    #[test]
    fn run_task_should_write_cancelled_result_when_tool_cancels() {
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

        let run = runtime.run_task(&root, "cancel").unwrap();

        let messages = fs::read_to_string(
            root.join(".flash")
                .join("sessions")
                .join(run.session_id)
                .join("messages.jsonl"),
        )
        .unwrap();
        assert!(messages.contains("\"status\":\"cancelled\""));
    }

    #[test]
    fn run_task_with_observer_should_emit_live_events_after_storage_commit() {
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

    #[test]
    fn mock_provider_scenarios_should_cover_loop_outcomes() {
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

    #[test]
    fn rust_fixture_smoke_should_fix_failing_test_and_keep_diff() {
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

        let run = runtime.run_task(&root, "fix failing tests").unwrap();

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

    #[test]
    fn run_task_controlled_should_cancel_without_committing_assistant_message() {
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

    #[test]
    fn project_history_should_keep_recent_messages_within_budget() {
        let old = test_message("old text");
        let recent = test_message("new");

        let projected = project_history(&[old, recent.clone()], 3);

        assert_eq!(projected, vec![recent]);
    }

    struct UnknownToolProvider;

    impl ChatProvider for UnknownToolProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_missing".to_string(),
                    name: "missing".to_string(),
                    input: String::new(),
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ])
        }
    }

    struct PartialProvider;

    impl ChatProvider for PartialProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![ProviderEvent::TextDelta("half".to_string())])
        }
    }

    struct LoopProvider;

    impl ChatProvider for LoopProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_read".to_string(),
                    name: "fake".to_string(),
                    input: String::new(),
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ])
        }
    }

    struct RetryProvider {
        calls: u32,
    }

    impl ChatProvider for RetryProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            self.calls += 1;
            if self.calls == 1 {
                return Err(ProviderError::RateLimited("rate limited".to_string()));
            }
            Ok(vec![
                ProviderEvent::TextDelta("ok".to_string()),
                ProviderEvent::Done(StopReason::EndTurn),
            ])
        }
    }

    struct LargeToolProvider;

    impl ChatProvider for LargeToolProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_large".to_string(),
                    name: "large".to_string(),
                    input: String::new(),
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ])
        }
    }

    struct ErrorToolProvider;

    impl ChatProvider for ErrorToolProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_error".to_string(),
                    name: "error".to_string(),
                    input: String::new(),
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ])
        }
    }

    struct ExecuteToolProvider;

    impl ChatProvider for ExecuteToolProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_execute".to_string(),
                    name: "execute".to_string(),
                    input: String::new(),
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ])
        }
    }

    struct CancelToolProvider;

    impl ChatProvider for CancelToolProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_cancel".to_string(),
                    name: "cancel".to_string(),
                    input: String::new(),
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ])
        }
    }

    struct DestructiveToolProvider;

    impl ChatProvider for DestructiveToolProvider {
        fn chat(&mut self, _request: ChatRequest) -> Result<Vec<ProviderEvent>, ProviderError> {
            Ok(vec![
                ProviderEvent::ToolCallComplete(ToolCall {
                    call_id: "call_destructive".to_string(),
                    name: "destructive".to_string(),
                    input: String::new(),
                }),
                ProviderEvent::Done(StopReason::ToolUse),
            ])
        }
    }

    struct FakeTool;

    impl Tool for FakeTool {
        fn name(&self) -> &str {
            "fake"
        }

        fn risk(&self, _input: &str) -> ToolRisk {
            ToolRisk::Read
        }

        fn call(&self, _input: &str, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::success("ok"))
        }
    }

    struct LargeTool;

    impl Tool for LargeTool {
        fn name(&self) -> &str {
            "large"
        }

        fn risk(&self, _input: &str) -> ToolRisk {
            ToolRisk::Read
        }

        fn call(&self, _input: &str, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::success("abcdef"))
        }
    }

    struct ErrorTool;

    impl Tool for ErrorTool {
        fn name(&self) -> &str {
            "error"
        }

        fn risk(&self, _input: &str) -> ToolRisk {
            ToolRisk::Read
        }

        fn call(&self, _input: &str, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
            Err(ToolError::new("tool failed"))
        }
    }

    struct ExecuteTool;

    impl Tool for ExecuteTool {
        fn name(&self) -> &str {
            "execute"
        }

        fn risk(&self, _input: &str) -> ToolRisk {
            ToolRisk::Execute
        }

        fn call(&self, _input: &str, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
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

        fn risk(&self, _input: &str) -> ToolRisk {
            ToolRisk::Read
        }

        fn call(&self, _input: &str, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput {
                stdout: String::new(),
                stderr: "cancelled".to_string(),
                status: ToolExitStatus::Cancelled,
            })
        }
    }

    struct DestructiveTool;

    impl Tool for DestructiveTool {
        fn name(&self) -> &str {
            "destructive"
        }

        fn risk(&self, _input: &str) -> ToolRisk {
            ToolRisk::Destructive
        }

        fn call(&self, _input: &str, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
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

        fn risk(&self, _input: &str) -> ToolRisk {
            ToolRisk::Read
        }

        fn call(&self, _input: &str, _context: &ToolContext) -> Result<ToolOutput, ToolError> {
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
