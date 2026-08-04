use std::path::Path;

use flash_core::{
    append_assistant_message_async, append_system_message_async, append_user_message_async,
    create_continuation_session_with_limits_async, create_session_with_limits_async,
    load_session_history_async, ArtifactLimits, CancellationToken, Event, Message, Outcome,
    PermissionPolicy, StorageLimits, ToolContext, ToolRegistry,
};
use flash_provider::{ChatProvider, ChatRequest, ToolSpec};

use crate::context::{project_history, validate_tool_turns};
use crate::control::{ExecutionControls, SessionStart};
use crate::error::AgentError;
use crate::finalize::{emit_event, finish_session, runtime_system_prompt};
use crate::hooks::{ApprovalController, EventObserver, NoopObserver, RejectingApproval};
use crate::turn::TurnCompletion;

pub struct AgentRuntime<P> {
    pub(crate) provider: P,
    pub(crate) tools: ToolRegistry,
    pub(crate) options: AgentOptions,
    pub(crate) storage_limits: StorageLimits,
    pub(crate) artifact_limits: ArtifactLimits,
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
            storage_limits: StorageLimits::default(),
            artifact_limits: ArtifactLimits::default(),
        }
    }

    pub fn with_resource_limits(
        mut self,
        storage_limits: StorageLimits,
        artifact_limits: ArtifactLimits,
    ) -> Self {
        self.storage_limits = storage_limits;
        self.artifact_limits = artifact_limits;
        self
    }

    pub async fn continue_task(
        &mut self,
        workspace_root: &Path,
        parent_session_id: &str,
        instruction: &str,
    ) -> Result<crate::error::AgentRun, AgentError> {
        self.continue_task_with_observer(
            workspace_root,
            parent_session_id,
            instruction,
            NoopObserver,
        )
        .await
    }

    pub async fn continue_task_with_observer<O>(
        &mut self,
        workspace_root: &Path,
        parent_session_id: &str,
        instruction: &str,
        mut observer: O,
    ) -> Result<crate::error::AgentRun, AgentError>
    where
        O: EventObserver,
    {
        self.continue_task_controlled(
            workspace_root,
            parent_session_id,
            instruction,
            &mut observer,
            || false,
        )
        .await
    }

    pub async fn continue_task_controlled<O, C>(
        &mut self,
        workspace_root: &Path,
        parent_session_id: &str,
        instruction: &str,
        observer: &mut O,
        should_cancel: C,
    ) -> Result<crate::error::AgentRun, AgentError>
    where
        O: EventObserver,
        C: FnMut() -> bool,
    {
        let mut approval = RejectingApproval;
        self.run_with_start_and_controls(
            workspace_root,
            instruction,
            observer,
            SessionStart::Continue(parent_session_id.to_string()),
            ExecutionControls {
                cancellation: CancellationToken::new(),
                should_cancel,
                approval: &mut approval,
            },
        )
        .await
    }

    pub async fn run_task_with_observer<O>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        mut observer: O,
    ) -> Result<crate::error::AgentRun, AgentError>
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
    ) -> Result<crate::error::AgentRun, AgentError>
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
    ) -> Result<crate::error::AgentRun, AgentError>
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
    ) -> Result<crate::error::AgentRun, AgentError>
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
        should_cancel: C,
        approval: &mut A,
    ) -> Result<crate::error::AgentRun, AgentError>
    where
        O: EventObserver,
        C: FnMut() -> bool,
        A: ApprovalController,
    {
        self.run_with_start_and_controls(
            workspace_root,
            task,
            observer,
            SessionStart::New,
            ExecutionControls {
                cancellation,
                should_cancel,
                approval,
            },
        )
        .await
    }

    async fn run_with_start_and_controls<O, C, A>(
        &mut self,
        workspace_root: &Path,
        task: &str,
        observer: &mut O,
        start: SessionStart,
        controls: ExecutionControls<'_, C, A>,
    ) -> Result<crate::error::AgentRun, AgentError>
    where
        O: EventObserver,
        C: FnMut() -> bool,
        A: ApprovalController,
    {
        let cancellation = controls.cancellation;
        let should_cancel = controls.should_cancel;
        let approval = controls.approval;
        let (session, inherited_history) = match start {
            SessionStart::New => (
                create_session_with_limits_async(workspace_root.to_path_buf(), self.storage_limits)
                    .await?,
                None,
            ),
            SessionStart::Continue(parent_session_id) => {
                let history = load_session_history_async(
                    workspace_root.to_path_buf(),
                    parent_session_id.clone(),
                )
                .await?;
                validate_tool_turns(&history)
                    .map_err(|error| AgentError::History(error.to_string()))?;
                let session = create_continuation_session_with_limits_async(
                    workspace_root.to_path_buf(),
                    parent_session_id,
                    self.storage_limits,
                )
                .await?;
                (session, Some(history))
            }
        };
        self.run_turns(
            &session,
            inherited_history,
            workspace_root,
            task,
            observer,
            cancellation,
            should_cancel,
            approval,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_turns<O, C, A>(
        &mut self,
        session: &flash_core::storage::Session,
        inherited_history: Option<Vec<Message>>,
        workspace_root: &Path,
        task: &str,
        observer: &mut O,
        cancellation: CancellationToken,
        mut should_cancel: C,
        approval: &mut A,
    ) -> Result<crate::error::AgentRun, AgentError>
    where
        O: EventObserver,
        C: FnMut() -> bool,
        A: ApprovalController,
    {
        let result = async {
            let mut history = if let Some(history) = inherited_history {
                history
            } else {
                vec![
                    append_system_message_async(
                        session.clone(),
                        runtime_system_prompt(workspace_root, &self.tools),
                    )
                    .await?,
                ]
            };
            let user = append_user_message_async(session.clone(), task.to_string()).await?;
            history.push(user);
            let context = ToolContext {
                workspace_root: workspace_root.to_path_buf(),
                cancellation: cancellation.clone(),
                artifact_dir: Some(session.path.join("artifacts")),
                artifact_stem: None,
                artifact_limits: Some(self.artifact_limits),
            };

            for turn in 1..=self.options.max_turns {
                if should_cancel() {
                    cancellation.cancel();
                    emit_event(
                        session,
                        Event::Error {
                            message: "run cancelled".to_string(),
                        },
                        observer,
                    )
                    .await?;
                    return finish_session(session, Outcome::Cancelled, observer).await;
                }

                let request_id = format!("request_{turn}");
                let projected_history =
                    match project_history(&history, self.options.max_prompt_bytes) {
                        Ok(history) => history,
                        Err(error) => {
                            emit_event(
                                session,
                                Event::Error {
                                    message: error.to_string(),
                                },
                                observer,
                            )
                            .await?;
                            return finish_session(session, Outcome::Failed, observer).await;
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
                    .chat_with_retry_streaming(session, &request_id, request, observer)
                    .await
                {
                    Ok(result) => result,
                    Err(AgentError::Provider(error)) => {
                        if error.is_cancelled() {
                            emit_event(
                                session,
                                Event::Error {
                                    message: "run cancelled".to_string(),
                                },
                                observer,
                            )
                            .await?;
                            return finish_session(session, Outcome::Cancelled, observer).await;
                        }
                        emit_event(
                            session,
                            Event::Error {
                                message: error.to_string(),
                            },
                            observer,
                        )
                        .await?;
                        finish_session(session, Outcome::Failed, observer).await?;
                        return Err(AgentError::Provider(error));
                    }
                    Err(error) => return Err(error),
                };
                let turn_result = self
                    .handle_provider_events(
                        session,
                        &request_id,
                        attempt,
                        provider_events,
                        observer,
                    )
                    .await?;
                match turn_result.completion {
                    TurnCompletion::EndTurn | TurnCompletion::ToolUse => {}
                    TurnCompletion::Cancelled => {
                        return finish_session(session, Outcome::Cancelled, observer).await;
                    }
                    TurnCompletion::Failed => {
                        return finish_session(session, Outcome::Failed, observer).await;
                    }
                }

                if should_cancel() {
                    cancellation.cancel();
                    emit_event(
                        session,
                        Event::Error {
                            message: "run cancelled".to_string(),
                        },
                        observer,
                    )
                    .await?;
                    return finish_session(session, Outcome::Cancelled, observer).await;
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
                    return finish_session(session, Outcome::Succeeded, observer).await;
                }

                for (index, call) in turn_result.tool_calls.iter().enumerate() {
                    if should_cancel() {
                        cancellation.cancel();
                        emit_event(
                            session,
                            Event::Error {
                                message: "run cancelled".to_string(),
                            },
                            observer,
                        )
                        .await?;
                        let cancelled = self
                            .cancel_tool_calls(session, &turn_result.tool_calls[index..], observer)
                            .await?;
                        history.extend(cancelled);
                        return finish_session(session, Outcome::Cancelled, observer).await;
                    }
                    let message = self
                        .execute_tool_call(session, &context, call, observer, approval)
                        .await?;
                    history.push(message);
                }
            }

            emit_event(
                session,
                Event::Error {
                    message: "max_turns exceeded".to_string(),
                },
                observer,
            )
            .await?;
            finish_session(session, Outcome::Failed, observer).await
        }
        .await;
        match result {
            Ok(run) => Ok(run),
            Err(primary) => match finish_session(session, Outcome::Failed, observer).await {
                Ok(_) => Err(primary),
                Err(finalize) => Err(AgentError::Finalization {
                    primary: primary.to_string(),
                    finalize: finalize.to_string(),
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::test_support::{
        flash_tools_for_tests, prepared_workspace, temp_dir, DeepSeekSseProvider, ErrorTool,
        ErrorToolProvider, LoopProvider, RecordingProvider, UnknownToolProvider,
    };
    use crate::{AgentError, AgentOptions, AgentRuntime, SmokeProvider};
    use flash_core::{ContentBlock, Event, Outcome, PermissionPolicy, ToolRegistry};
    use std::fs;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn continue_task_should_create_child_and_send_inherited_history() {
        let root = temp_dir("continue_history");
        fs::create_dir_all(&root).unwrap();
        let parent = flash_core::create_session(&root).unwrap();
        flash_core::append_system_message(&parent, "root system").unwrap();
        flash_core::append_user_message(&parent, "parent task").unwrap();
        flash_core::append_assistant_message(&parent, "", "parent answer", &[]).unwrap();
        flash_core::append_event(
            &parent,
            Event::ApprovalResolved {
                call_id: "historical_approval".to_string(),
                approved: true,
            },
        )
        .unwrap();
        flash_core::append_event(
            &parent,
            Event::SessionFinished {
                outcome: Outcome::Succeeded,
            },
        )
        .unwrap();
        flash_core::finalize_session(&parent, Outcome::Succeeded).unwrap();
        let parent_metadata = fs::read(parent.path.join("session.json")).unwrap();
        let parent_messages = fs::read(parent.path.join("messages.jsonl")).unwrap();
        let parent_events = fs::read(parent.path.join("events.jsonl")).unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let mut runtime = AgentRuntime::new(
            RecordingProvider {
                requests: Arc::clone(&requests),
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

        let run = runtime
            .continue_task(&root, &parent.id, "follow-up task")
            .await
            .unwrap();

        assert_eq!(run.outcome, Outcome::Succeeded);
        let child = flash_core::storage::load_session(&root, &run.session_id).unwrap();
        assert_eq!(child.parent_session_id.as_deref(), Some(parent.id.as_str()));
        assert_eq!(
            fs::read(parent.path.join("session.json")).unwrap(),
            parent_metadata
        );
        assert_eq!(
            fs::read(parent.path.join("messages.jsonl")).unwrap(),
            parent_messages
        );
        assert_eq!(
            fs::read(parent.path.join("events.jsonl")).unwrap(),
            parent_events
        );

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let texts = requests[0]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "root system",
                "parent task",
                "parent answer",
                "follow-up task"
            ]
        );
        assert!(!texts
            .iter()
            .any(|text| text.contains("historical_approval")));
        let child_messages = fs::read_to_string(child.path.join("messages.jsonl")).unwrap();
        assert!(child_messages.contains("follow-up task"));
        assert!(!child_messages.contains("parent task"));
    }

    #[tokio::test]
    async fn continue_task_should_reject_running_or_cross_workspace_parent() {
        let root = temp_dir("continue_rejections");
        let other = temp_dir("continue_rejections_other");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&other).unwrap();
        let parent = flash_core::create_session(&root).unwrap();
        let mut runtime = AgentRuntime::new(
            RecordingProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
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

        let running_error = runtime
            .continue_task(&root, &parent.id, "continue")
            .await
            .unwrap_err();
        assert!(matches!(
            running_error,
            AgentError::Storage(flash_core::storage::StorageError::SessionStillRunning(_))
        ));

        flash_core::finalize_session(&parent, Outcome::Failed).unwrap();
        let foreign_dir = other.join(".flash/sessions").join(&parent.id);
        fs::create_dir_all(&foreign_dir).unwrap();
        fs::copy(
            parent.path.join("session.json"),
            foreign_dir.join("session.json"),
        )
        .unwrap();
        let cross_workspace_error = runtime
            .continue_task(&other, &parent.id, "continue")
            .await
            .unwrap_err();
        assert!(matches!(
            cross_workspace_error,
            AgentError::Storage(flash_core::storage::StorageError::WorkspaceMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn continue_task_should_reject_incomplete_tool_turn_before_creating_child() {
        let root = temp_dir("continue_invalid_tool_turn");
        fs::create_dir_all(&root).unwrap();
        let parent = flash_core::create_session(&root).unwrap();
        flash_core::append_system_message(&parent, "system").unwrap();
        flash_core::append_user_message(&parent, "task").unwrap();
        flash_core::append_assistant_message(
            &parent,
            "",
            "",
            &[(
                "call_missing".to_string(),
                "Read".to_string(),
                serde_json::json!({"path": "README.md"}),
            )],
        )
        .unwrap();
        flash_core::finalize_session(&parent, Outcome::Failed).unwrap();
        let mut runtime = AgentRuntime::new(
            RecordingProvider {
                requests: Arc::new(Mutex::new(Vec::new())),
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

        let error = runtime
            .continue_task(&root, &parent.id, "continue")
            .await
            .unwrap_err();

        assert!(matches!(error, AgentError::History(_)));
        assert_eq!(
            fs::read_dir(root.join(".flash/sessions")).unwrap().count(),
            1
        );
    }

    #[tokio::test]
    async fn run_task_should_execute_search_tool_and_finish() {
        let root = temp_dir("search_loop");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        let runtime = AgentRuntime::new(
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
        let runtime = AgentRuntime::new(
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
        let runtime = AgentRuntime::new(
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
                    let mut observer = crate::hooks::NoopObserver;
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
        let runtime = AgentRuntime::new(
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
        let mut observer = crate::hooks::NoopObserver;

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
}
